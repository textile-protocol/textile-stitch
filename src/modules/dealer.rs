// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A dealer buys corridor tokens by filling a normal dual-signed vault order.
//! It obtains Warp's co-signature and pays gas. HTTP acceptance is NOT a fill.
use super::{
    atomic,
    config::{DealerConfig, DEALER_API_KEY_ENV},
};
use alloy_primitives::{Address, U256};
use anyhow::{ensure, Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuoteRequest {
    pub request_id: String,
    pub chain_id: u64,
    pub vault: String,
    pub sell_token: String,
    pub buy_token: String,
    #[serde(with = "atomic")]
    pub sell_amount: U256,
    #[serde(with = "atomic")]
    pub min_buy_amount: U256,
    pub deadline: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Quote {
    pub request_id: String,
    pub taker: String,
    #[serde(with = "atomic")]
    pub sell_amount: U256,
    /// Net amount delivered to the vault, after all dealer costs.
    #[serde(with = "atomic")]
    pub buy_amount: U256,
    pub expires_at: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Execute<'a> {
    pub request_id: &'a str,
    pub chain_id: u64,
    pub encoded_order: String,
    pub strategy_signature: String,
}

pub fn validate_quote(
    cfg: &DealerConfig,
    request: &QuoteRequest,
    quote: &Quote,
    now: u64,
) -> Result<()> {
    ensure!(
        quote.request_id == request.request_id,
        "dealer returned a different request id"
    );
    ensure!(
        quote.taker.parse::<Address>()? == cfg.taker.parse::<Address>()?,
        "dealer changed the configured taker"
    );
    ensure!(
        !quote.sell_amount.is_zero() && quote.sell_amount == request.sell_amount,
        "dealer changed the sell amount"
    );
    ensure!(
        !quote.buy_amount.is_zero() && quote.buy_amount >= request.min_buy_amount,
        "dealer price is below the minimum net proceeds"
    );
    ensure!(
        quote.expires_at > now.saturating_add(5) && quote.expires_at <= request.deadline,
        "dealer quote expiry is outside the requested window"
    );
    Ok(())
}
/// A request to `{url}/{route}`, with the dealer credential as a bearer token
/// when `api_key_env` is set (see [`DealerConfig::api_key_env`]).
fn post(cfg: &DealerConfig, route: &str) -> Result<reqwest::RequestBuilder> {
    let req = crate::net::http_client()
        .post(format!("{}/{}", cfg.url.trim_end_matches('/'), route))
        .timeout(std::time::Duration::from_secs(5));
    Ok(match cfg.api_key_env {
        Some(_) => req.bearer_auth(load_api_key(DEALER_API_KEY_ENV)?),
        None => req,
    })
}
/// `{raw_env}_FILE` (preferred, same as the wallet), then `{raw_env}`. Only
/// ever called with [`DEALER_API_KEY_ENV`]; tests pass their own name.
fn load_api_key(raw_env: &str) -> Result<String> {
    let file_env = format!("{raw_env}_FILE");
    let key = crate::signer::read_env_secret(&file_env, raw_env)
        .context("the dealer credential is missing")?;
    ensure!(
        !key.is_empty(),
        "the dealer credential is empty: set {file_env} or {raw_env}"
    );
    Ok(key)
}
pub async fn quote(cfg: &DealerConfig, request: &QuoteRequest) -> Result<Quote> {
    let mut response = post(cfg, "quote")?
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 16_384,
            "dealer quote response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let quote = serde_json::from_slice(&bytes)?;
    validate_quote(cfg, request, &quote, crate::time::unix_now())?;
    Ok(quote)
}
pub async fn execute(cfg: &DealerConfig, payload: &Execute<'_>) -> Result<()> {
    post(cfg, "execute")?
        .json(payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dealer(api_key_env: Option<&str>) -> DealerConfig {
        DealerConfig {
            url: "https://collector.attacker.example/stitch/v1".into(),
            taker: "0x0000000000000000000000000000000000000003".into(),
            api_key_env: api_key_env.map(Into::into),
        }
    }

    fn bearer(cfg: &DealerConfig) -> Option<String> {
        let req = post(cfg, "quote").ok()?.build().unwrap();
        req.headers()
            .get(reqwest::header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn a_config_naming_another_variable_never_sends_it() {
        // A config that skipped validation still can't pick the variable:
        // whatever `api_key_env` says, only STITCH_DEALER_API_KEY is read.
        let var = "STITCH_DEALER_EXFIL_STANDIN_TEST";
        std::env::set_var(var, "tx_live_maker_secret");
        let sent = bearer(&dealer(Some(var)));
        std::env::remove_var(var);
        assert!(
            !sent.is_some_and(|h| h.contains("tx_live_maker_secret")),
            "the named variable reached the dealer"
        );
    }

    #[test]
    fn no_api_key_env_sends_no_credential() {
        assert_eq!(bearer(&dealer(None)), None);
    }

    #[test]
    fn a_blank_or_missing_credential_is_refused() {
        let var = "STITCH_DEALER_API_KEY_LOADER_TEST";
        std::env::set_var(var, "  ");
        assert!(load_api_key(var).is_err());
        std::env::remove_var(var);
        let err = format!("{:#}", load_api_key(var).unwrap_err());
        assert!(err.contains(&format!("{var}_FILE")), "{err}");
    }
}
