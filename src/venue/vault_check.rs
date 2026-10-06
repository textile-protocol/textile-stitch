// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! `GET /v2/maker/vault-check`: would enroll accept this OperatorVault for this
//! signer, and which `[vault].order_executor` goes with it.
//!
//! Advice, not a grant. Enroll runs the same rules and is what actually
//! decides; this answers the same questions as sentences up front, so the
//! panel can show every problem before it rewrites a config. Public and
//! unsigned on the venue side, IP-throttled.

use std::time::Duration;

use alloy_primitives::Address;
use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::enroll::{maker_venue_origin, venue_error_message};

/// One round trip with an operator watching, so short. The venue reads the
/// chain to answer, so it gets longer than the panel's own per-read budget.
const VAULT_CHECK_TIMEOUT_SECS: u64 = 15;

/// The venue's verdict. Fields the venue could not determine are `None`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VaultCheckResponse {
    /// From an OperatorVaultFactory Textile trusts on this chain.
    #[serde(default)]
    pub registered: bool,
    #[serde(default)]
    pub risk_signer_ok: Option<bool>,
    /// Reactor, Permit2 and preferred-filler validation match the venue's.
    #[serde(default)]
    pub wiring_ok: Option<bool>,
    #[serde(default)]
    pub strategy_signer_matches: Option<bool>,
    /// The chain's active VaultOrderExecutor when this vault stakes idle
    /// settlement and the venue routes vault fills through it; `None`
    /// otherwise. This is what `[vault].order_executor` should be.
    #[serde(default)]
    pub order_executor: Option<String>,
    /// Every reason enroll would refuse, as operator sentences. Empty means
    /// enroll would accept.
    #[serde(default)]
    pub issues: Vec<String>,
}

/// Where the check lives, from a stream URL or an API origin.
pub fn maker_vault_check_url(stream_or_origin: &str) -> String {
    format!(
        "{}/v2/maker/vault-check",
        maker_venue_origin(stream_or_origin)
    )
}

/// Ask the venue about `vault` for a bot that signs as `signing_address`.
///
/// `Ok(None)` when the venue has no such route (404): an API older than the
/// endpoint. Enroll still runs the same checks, so a caller can carry on
/// without this answer; it just has no `order_executor` to configure.
pub async fn check_vault(
    origin: &str,
    chain_id: u64,
    vault: Address,
    signing_address: Address,
) -> Result<Option<VaultCheckResponse>> {
    let url = maker_vault_check_url(origin);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(VAULT_CHECK_TIMEOUT_SECS))
        .build()
        .context("building the vault-check HTTP client")?;
    let response = client
        .get(&url)
        .query(&[
            ("chainId", chain_id.to_string()),
            ("vault", vault.to_checksum(None)),
            ("signingAddress", signing_address.to_checksum(None)),
        ])
        .send()
        .await
        .with_context(|| format!("could not reach Textile at {url}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("Textile's vault check returned an unreadable body")?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        match venue_error_message(&text) {
            Some(message) => bail!("{message}"),
            None => bail!("Textile's vault check failed ({status})"),
        }
    }
    let answer =
        serde_json::from_str(&text).context("Textile's vault check returned an unexpected body")?;
    Ok(Some(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_hangs_off_the_same_origin_as_enroll() {
        assert_eq!(
            maker_vault_check_url("wss://api.textilecredit.com/v2/maker/stream"),
            "https://api.textilecredit.com/v2/maker/vault-check"
        );
        assert_eq!(
            maker_vault_check_url("http://127.0.0.1:9/v2/maker/enroll"),
            "http://127.0.0.1:9/v2/maker/vault-check"
        );
    }

    #[test]
    fn a_partial_answer_parses_with_unknowns_left_empty() {
        let answer: VaultCheckResponse = serde_json::from_str(
            r#"{"chainId":56,"vault":"0x00000000000000000000000000000000000000aa",
                "registered":true,"riskSignerOk":null,"orderExecutor":null,"issues":[]}"#,
        )
        .unwrap();
        assert!(answer.registered);
        assert_eq!(answer.risk_signer_ok, None);
        assert_eq!(answer.order_executor, None);
        assert!(answer.issues.is_empty());
    }
}
