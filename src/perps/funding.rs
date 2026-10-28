// SPDX-License-Identifier: AGPL-3.0-or-later
use super::{request, PerpsConfig};
use crate::{
    chain::rpc::{read_external_intent, Wallet},
    config::Config,
    signer::DynSigner,
};
use alloy_primitives::{Address, Bytes, U256};
use serde_json::json;
use std::path::Path;

/// Owner-configured allocation ceiling, separate from ordinary liquid quoting.
/// The venue reserves uncommitted atoms first. Stitch signs only the exact token
/// transfer and the perps worker journals/broadcasts it. No keys leave Stitch.
pub async fn tick(
    cfg: &Config,
    dir: &Path,
    p: &PerpsConfig,
    signer: DynSigner,
) -> anyhow::Result<()> {
    // Retry the original bytes even if owner allocation changed since signing.
    // No replacement transaction is signed around an uncertain HTTP response.
    let journal_path = dir.join("perps-funding-pending.json");
    if let Some(pending) = read_external_intent(&journal_path)? {
        request(
            cfg,
            dir,
            reqwest::Method::POST,
            &p.assignment_id,
            "/funding/journal",
            Some(json!({
                "operationId": pending.operation, "signedTransaction": pending.raw
            })),
        )
        .await?;
        let wallet = Wallet::new(cfg.rpc_url.clone(), signer.clone(), cfg.chain_id);
        if wallet
            .rpc()
            .latest_transaction_count(signer.address())
            .await?
            > pending.nonce
        {
            std::fs::remove_file(journal_path)?;
        }
        return Ok(());
    }
    if !p.enabled
        || !p.recall_enabled
        || p.funding_target_atoms.is_empty()
        || p.funding_target_atoms == "0"
    {
        return Ok(());
    }
    anyhow::ensure!(cfg.vault.is_none() && signer.can_sign_transactions(), "Automated funding requires an owner transaction signer; contract wallets can fund through their normal owner flow");
    let target = p.funding_target_atoms.parse::<U256>()?;
    let remote = request(cfg, dir, reqwest::Method::GET, &p.assignment_id, "", None).await?;
    if remote["effectiveMode"] != "live"
        || remote["control"]["revision"] != p.revision
        || remote["recall"]["enabled"] != true
    {
        return Ok(());
    }
    let m = &remote["recall"]["manifest"];
    let position = &remote["recall"]["position"];
    let atoms = |name: &str| -> anyhow::Result<U256> {
        Ok(position[name]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Inventory snapshot missing"))?
            .parse()?)
    };
    let allocated = atoms("collateralAtoms")?
        .checked_add(atoms("marginIdleAtoms")?)
        .ok_or_else(|| anyhow::anyhow!("Inventory overflow"))?;
    let needed = target.saturating_sub(allocated).min(atoms("liquidAtoms")?);
    if needed.is_zero() {
        return Ok(());
    }
    anyhow::ensure!(
        m["assignment"]["fundingWallet"]
            .as_str()
            .and_then(|s| s.parse::<Address>().ok())
            == Some(signer.address()),
        "Funding owner mismatch"
    );
    let token: Address = m["assets"]["collateral"]["address"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing token"))?
        .parse()?;
    let margin: Address = m["assignment"]["marginAccount"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing margin account"))?
        .parse()?;
    let intent = request(
        cfg,
        dir,
        reqwest::Method::POST,
        &p.assignment_id,
        "/funding",
        Some(json!({"amountAtoms": needed.to_string()})),
    )
    .await?;
    if intent["submitted"] == true {
        return Ok(());
    }
    let tx = &intent["transaction"];
    let amount: U256 = tx["amount"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing funding amount"))?
        .parse()?;
    anyhow::ensure!(
        amount <= needed
            && amount > U256::ZERO
            && tx["chainId"] == cfg.chain_id
            && tx["value"] == "0",
        "Funding exceeds owner allocation"
    );
    let to: Address = tx["to"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing transfer token"))?
        .parse()?;
    let data: Bytes = tx["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing transfer data"))?
        .parse()?;
    let mut expected = alloy_primitives::keccak256("transfer(address,uint256)")[..4].to_vec();
    expected.extend_from_slice(&margin.into_word().0);
    expected.extend_from_slice(&amount.to_be_bytes::<32>());
    anyhow::ensure!(
        to == token && data.as_ref() == expected.as_slice(),
        "Funding target mismatch"
    );
    let operation = intent["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing operation ID"))?
        .to_owned();
    let wallet =
        Wallet::new(cfg.rpc_url.clone(), signer, cfg.chain_id).with_gas_caps(cfg.gas_caps());
    wallet.journal_transaction(&operation.clone(), to, data, U256::ZERO, |raw| async move {
        // Repeating these exact bytes is safe if the journal response is lost.
        request(cfg, dir, reqwest::Method::POST, &p.assignment_id, "/funding/journal", Some(json!({"operationId": operation, "signedTransaction": alloy_primitives::hex::encode_prefixed(raw)}))).await?;
        Ok(())
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};
    struct RecoveryOnly;
    #[async_trait::async_trait]
    impl crate::signer::Signer for RecoveryOnly {
        async fn sign_digest(&self, _: alloy_primitives::B256) -> anyhow::Result<[u8; 65]> {
            panic!("Disabled funding must not sign anything")
        }
        fn address(&self) -> Address {
            Address::repeat_byte(1)
        }
    }
    #[tokio::test]
    async fn disabled_funding_retries_original_bytes_after_failure_without_new_authority() {
        let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        async fn journal(
            State(seen): State<Arc<Mutex<Vec<serde_json::Value>>>>,
            Json(body): Json<serde_json::Value>,
        ) -> (StatusCode, Json<serde_json::Value>) {
            let mut seen = seen.lock().unwrap();
            seen.push(body);
            if seen.len() == 1 {
                (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
            } else {
                (StatusCode::OK, Json(json!({"data":{"hash":"fixture"}})))
            }
        }
        async fn rpc(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            assert_eq!(body["method"], "eth_getTransactionCount");
            Json(json!({"jsonrpc":"2.0","id":body["id"],"result":"0x1"}))
        }
        let app = Router::new()
            .route("/v2/maker/perps/recall-test/funding/journal", post(journal))
            .route("/", post(rpc))
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = std::env::temp_dir().join(format!(
            "recall-recovery-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("rfq-api.key"), "local-test-only").unwrap();
        let path = dir.join("perps-funding-pending.json");
        let raw = "0x000000fixture-signed-bytes";
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"operation":"op-original","nonce":0,"raw":raw})).unwrap(),
        )
        .unwrap();
        let mut cfg: Config = toml::from_str(include_str!("../../stitch.example.toml")).unwrap();
        cfg.rpc_url = origin.clone();
        cfg.rfq = Some(crate::config::RfqConfig {
            enabled: false,
            url: origin,
            maker_id: "fixture".into(),
            api_key_env: "UNSET_RECALL_RECOVERY_TEST".into(),
            validation_contract: format!("{}", Address::repeat_byte(2)),
            instance_id: None,
        });
        let p = PerpsConfig {
            assignment_id: "recall-test".into(),
            ..Default::default()
        };
        assert!(tick(&cfg, &dir, &p, Arc::new(RecoveryOnly)).await.is_err());
        assert!(path.exists());
        tick(&cfg, &dir, &p, Arc::new(RecoveryOnly)).await.unwrap();
        assert!(!path.exists());
        let calls = received.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_eq!(calls[0]["signedTransaction"], raw);
        std::fs::remove_dir_all(dir).unwrap();
        task.abort();
    }
}
