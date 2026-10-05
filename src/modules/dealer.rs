// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A dealer buys corridor tokens by filling a normal dual-signed vault order.
//! It obtains Warp's co-signature and pays gas. HTTP acceptance is NOT a fill.
use super::{atomic, config::DealerConfig};
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
fn post(cfg: &DealerConfig, route: &str) -> Result<reqwest::RequestBuilder> {
    let mut req = crate::net::http_client()
        .post(format!("{}/{}", cfg.url.trim_end_matches('/'), route))
        .timeout(std::time::Duration::from_secs(5));
    if let Some(env) = &cfg.api_key_env {
        let key =
            std::env::var(env).context("dealer credential environment variable is missing")?;
        ensure!(!key.trim().is_empty(), "dealer credential is empty");
        req = req.bearer_auth(key);
    }
    Ok(req)
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
