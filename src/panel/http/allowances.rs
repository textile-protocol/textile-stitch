//! Per-token Permit2 allowance status for one bot.
//!
//! `stitch approve` already covers every `[[pools]]` entry, but until now the
//! panel could not say *which* tokens were short — so an operator who added a
//! second corridor saw the bot refuse to start over a missing approval with no
//! way to tell which token it meant, or whether approving had fixed it.
//!
//! This is a read, so it runs in the panel rather than in a throwaway container:
//! `allowance(owner, permit2)` needs no signer, and going through the bot image
//! would make the answer depend on that image being new enough to have the
//! subcommand.

use alloy_primitives::{Address, Bytes, U256};
use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use super::settings::config_path;
use super::{ApiError, AppState};
use crate::approve::{approval_action, required_approvals, ApprovalAction, ApprovalMode};
use crate::closer::executor::encode_allowance;
use crate::config::Config;
use crate::rpc::Rpc;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenAllowanceBody {
    pub token: String,
    /// Ticker when the pair is a known corridor, else a shortened address.
    pub symbol: String,
    /// Every corridor on this bot that spends the token, by display name.
    pub corridors: Vec<String>,
    /// Which legs need it, e.g. "debt (buy side)".
    pub reasons: Vec<String>,
    /// Committed liquidity in the token's atomic units, as a decimal string.
    pub required: String,
    /// A side commits `"max"`, so no fixed amount can cover it.
    pub uses_max_liquidity: bool,
    /// Current Permit2 allowance, decimal. `null` when the read failed.
    pub allowance: Option<String>,
    /// Whether the current allowance satisfies what the config commits — the
    /// same test the bot's own live-start preflight applies.
    pub approved: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowancesBody {
    pub operator_address: Option<String>,
    pub permit2: String,
    pub chain_id: u64,
    pub tokens: Vec<TokenAllowanceBody>,
    /// Set when the chain could not be reached, so the UI can say "unknown"
    /// rather than "not approved" — the difference matters when the answer
    /// decides whether someone sends transactions.
    pub read_error: Option<String>,
}

/// `GET /api/bots/{name}/allowances`
pub async fn allowances(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    let toml = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::internal(&anyhow::anyhow!(e).context(format!("reading {}", path.display())))
    })?;
    let cfg = Config::from_toml(&toml).map_err(ApiError::bad_request)?;

    let required = required_approvals(&cfg).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let symbols = token_symbols(&cfg);
    let corridors = token_corridors(&cfg);
    let owner = bot
        .config
        .as_ref()
        .and_then(|c| c.operator_address.clone())
        .and_then(|a| a.parse::<Address>().ok());

    // One read error covers the whole request: they all hit the same node, so
    // reporting per token would just repeat the same failure.
    let mut read_error = None;
    let mut current: Vec<Option<U256>> = Vec::with_capacity(required.len());
    match (owner, cfg.permit2.parse::<Address>()) {
        (Some(owner), Ok(permit2)) => {
            let rpc = Rpc::new(cfg.rpc_url.clone());
            for req in &required {
                match read_allowance(&rpc, req.token, owner, permit2).await {
                    Ok(v) => current.push(Some(v)),
                    Err(e) => {
                        read_error.get_or_insert(format!("{e:#}"));
                        current.push(None);
                    }
                }
            }
        }
        (None, _) => {
            read_error = Some(
                "this bot has no operator address the panel can read, so allowances can't be \
                 checked."
                    .to_string(),
            );
            current.resize(required.len(), None);
        }
        (_, Err(e)) => {
            read_error = Some(format!("the config's permit2 address is not valid: {e}"));
            current.resize(required.len(), None);
        }
    }

    let tokens = required
        .iter()
        .zip(current)
        .map(|(req, allowance)| {
            let key = format!("{:#x}", req.token);
            TokenAllowanceBody {
                symbol: symbols
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| short_token(&key)),
                corridors: corridors.get(&key).cloned().unwrap_or_default(),
                token: key,
                // `required_approvals` repeats a reason per pool, so a token two
                // pools buy with says "debt (buy side)" twice. The corridor list
                // above already carries the per-pool detail.
                reasons: dedupe(&req.reasons),
                required: req.required.to_string(),
                uses_max_liquidity: req.uses_max_liquidity,
                approved: allowance.map(|a| {
                    approval_action(a, req.required, req.uses_max_liquidity, ApprovalMode::Max)
                        == ApprovalAction::AlreadyApproved
                }),
                allowance: allowance.map(|a| a.to_string()),
            }
        })
        .collect();

    Ok(Json(AllowancesBody {
        operator_address: bot.config.as_ref().and_then(|c| c.operator_address.clone()),
        permit2: cfg.permit2.clone(),
        chain_id: cfg.chain_id,
        tokens,
        read_error,
    })
    .into_response())
}

async fn read_allowance(
    rpc: &Rpc,
    token: Address,
    owner: Address,
    permit2: Address,
) -> anyhow::Result<U256> {
    let data = Bytes::from(encode_allowance(owner, permit2));
    let out = rpc.eth_call(token, &data).await?;
    anyhow::ensure!(
        out.len() >= 32,
        "allowance() on {token} returned {} bytes, not a uint256 — is that address an ERC-20 on \
         this chain?",
        out.len()
    );
    Ok(U256::from_be_slice(&out[out.len() - 32..]))
}

/// Ticker per token address, taken from the corridor catalog.
///
/// A corridor's `display_name` is `"<collateral> / <debt>"`, which is the only
/// place the panel knows tickers at all — the config carries addresses. A pool
/// the catalog doesn't recognise contributes nothing and falls back to an
/// address.
fn token_symbols(cfg: &Config) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pool in &cfg.pools {
        let Some(corridor) =
            crate::setup::identify_pair(cfg.chain_id, &pool.collateral, &pool.debt)
        else {
            continue;
        };
        if let Some((collateral, debt)) = corridor.display_name.split_once(" / ") {
            out.insert(
                pool.collateral.to_lowercase(),
                collateral.trim().to_string(),
            );
            out.insert(pool.debt.to_lowercase(), debt.trim().to_string());
        }
    }
    out
}

/// Which corridors spend each token. A shared token (USDT on both cNGN and
/// wBRL) lists both, so it is obvious that one approval serves several pairs.
fn token_corridors(cfg: &Config) -> std::collections::HashMap<String, Vec<String>> {
    let mut out: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for pool in &cfg.pools {
        let label = crate::setup::identify_pair(cfg.chain_id, &pool.collateral, &pool.debt)
            .map(|c| c.display_name.to_string())
            .unwrap_or_else(|| {
                format!(
                    "{} / {}",
                    short_token(&pool.collateral),
                    short_token(&pool.debt)
                )
            });
        for token in [&pool.collateral, &pool.debt] {
            let entry = out.entry(token.to_lowercase()).or_default();
            if !entry.contains(&label) {
                entry.push(label.clone());
            }
        }
    }
    out
}

fn dedupe(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        if !out.contains(v) {
            out.push(v.clone());
        }
    }
    out
}

fn short_token(addr: &str) -> String {
    // Character indices, not bytes: a custom pool's token string need not be an
    // ASCII address, and a label must not panic mid-codepoint.
    let chars: Vec<char> = addr.chars().collect();
    if chars.len() <= 10 {
        return addr.to_string();
    }
    format!(
        "{}…{}",
        chars[..6].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::ContainerState;
    use crate::panel::http::testkit::{harness, Harness, TEST_KEY};
    use crate::setup;
    use axum::http::StatusCode;

    fn two_pool_bot(h: &Harness, name: &str, rpc_url: &str) {
        let corridor = setup::find_corridor("cngn-usdt-celo").unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let path = h.root.join(name).join("stitch.toml");
        let one = std::fs::read_to_string(&path).unwrap();
        let two = setup::add_pool_from_template(
            &one,
            setup::find_corridor("wbrl-usdt-celo")
                .unwrap()
                .toml_template,
        )
        .unwrap();
        // Point the config at a closed port: these tests must not reach a chain,
        // and the unreachable case is the one worth pinning anyway.
        let two = two.replace(
            "rpc_url         = \"https://forno.celo.org\"",
            &format!("rpc_url = \"{rpc_url}\""),
        );
        std::fs::write(&path, two).unwrap();
        let mut c = crate::panel::docker::fake::container(
            &format!("stitch-{name}"),
            ContainerState::Exited,
        );
        c.image = h.state.cfg.bot_image.clone();
        c.labels.insert(
            crate::panel::naming::LABEL_BOT.to_string(),
            name.to_string(),
        );
        c.mounts =
            crate::panel::docker::fake::dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    /// One approval covers a token across every pool that pays it, so a bot on
    /// cNGN/USDT and wBRL/USDT needs three, not four — and the shared USDT row
    /// has to name both pairs or the operator can't tell why it matters.
    #[tokio::test]
    async fn shared_tokens_are_listed_once_against_every_corridor() {
        let h = harness("allowances-shared-token");
        two_pool_bot(&h, "bot-a", "http://127.0.0.1:1");

        let (status, body) = h.get("/api/bots/bot-a/allowances").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        let tokens = v["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 3, "cNGN, wBRL and the shared USDT: {body}");

        let usdt = tokens
            .iter()
            .find(|t| t["symbol"] == "USDT")
            .unwrap_or_else(|| panic!("no USDT row: {body}"));
        let corridors: Vec<&str> = usdt["corridors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(corridors.len(), 2, "{body}");
        assert!(corridors.contains(&"cNGN / USDT"), "{body}");
        assert!(corridors.contains(&"wBRL / USDT"), "{body}");

        // The other two are one corridor each.
        for symbol in ["cNGN", "wBRL"] {
            let row = tokens
                .iter()
                .find(|t| t["symbol"] == symbol)
                .unwrap_or_else(|| panic!("no {symbol} row: {body}"));
            assert_eq!(row["corridors"].as_array().unwrap().len(), 1, "{body}");
        }
    }

    /// A chain we can't reach means unknown, not unapproved. Saying "not
    /// approved" would send someone to spend gas re-approving tokens that are
    /// already fine.
    #[tokio::test]
    async fn an_unreachable_chain_reads_as_unknown_rather_than_unapproved() {
        let h = harness("allowances-no-chain");
        two_pool_bot(&h, "bot-a", "http://127.0.0.1:1");

        let (status, body) = h.get("/api/bots/bot-a/allowances").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(!v["readError"].is_null(), "{body}");
        for token in v["tokens"].as_array().unwrap() {
            assert!(token["approved"].is_null(), "{body}");
            assert!(token["allowance"].is_null(), "{body}");
        }
        // The operator address still comes back, so the UI can name the wallet
        // the approvals would come from.
        assert!(v["operatorAddress"].as_str().is_some_and(|a| a.len() == 42));
    }

    #[test]
    fn an_unrecognised_pair_falls_back_to_shortened_addresses() {
        assert_eq!(
            short_token("0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e"),
            "0x4806…3D5e"
        );
        // Multibyte, and shorter than the elision window.
        assert_eq!(short_token("cNGN₦"), "cNGN₦");
    }
}
