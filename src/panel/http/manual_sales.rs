// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use super::{ApiError, AppState};
use crate::{
    config::Config,
    modules::{config::RebalanceMethod, manual, Mode},
};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

async fn venue(
    cfg: &Config,
    dir: &std::path::Path,
    method: reqwest::Method,
    route: &str,
    body: Option<Value>,
) -> Result<Value, ApiError> {
    let key = crate::rfq::load_rfq_api_key(
        &cfg.rfq
            .as_ref()
            .ok_or_else(|| ApiError::bad_request("RFQ is not configured"))?
            .api_key_env,
        Some(dir),
    )
    .map_err(|_| ApiError::bad_request("Connect this bot to Textile first"))?;
    let origin = crate::venue::enroll::venue_origin_from_config(cfg, None);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(ApiError::bad_request)?;
    let request = client
        .request(
            method,
            format!("{}/v2/{route}", origin.trim_end_matches('/')),
        )
        .bearer_auth(key);
    let response = if let Some(body) = body {
        request.json(&body)
    } else {
        request
    }
    .send()
    .await
    .map_err(ApiError::bad_request)?;
    let status = response.status();
    let text = response.text().await.map_err(ApiError::bad_request)?;
    if !status.is_success() {
        return Err(ApiError::bad_request(
            crate::venue::enroll::venue_error_message(&text)
                .unwrap_or_else(|| format!("Textile could not process the sale ({status})")),
        ));
    }
    let value: Value = serde_json::from_str(&text).map_err(ApiError::bad_request)?;
    value
        .get("data")
        .cloned()
        .ok_or_else(|| ApiError::bad_request("Textile returned an unreadable sale response"))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Create {
    #[serde(with = "manual::address")]
    pub taker: alloy_primitives::Address,
    #[serde(with = "crate::modules::atomic")]
    pub corridor_amount: alloy_primitives::U256,
    #[serde(with = "crate::modules::atomic")]
    pub min_settlement: alloy_primitives::U256,
}
pub async fn create(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Create>,
) -> Result<Json<Value>, ApiError> {
    let (_lock, bot) = super::bots::lock_config(&name, &state).await?;
    super::require_editable(&bot)?;
    let (path, _, cfg) = super::modules::read(&bot)?;
    if cfg.modules.mode != Mode::Live
        || !cfg.modules.rebalance.enabled
        || cfg.modules.rebalance.method != RebalanceMethod::Manual
    {
        return Err(ApiError::bad_request(
            "Enable spot rebalancing in Live mode and save before creating a sale",
        ));
    }
    let pool = &cfg.pools[0];
    let sale = manual::Sale {
        id: alloy_primitives::hex::encode(rand::random::<[u8; 32]>()),
        chain_id: cfg.chain_id,
        vault: cfg
            .vault
            .as_ref()
            .ok_or_else(|| ApiError::bad_request("An operator vault is required"))?
            .address
            .parse()
            .map_err(ApiError::bad_request)?,
        taker: body.taker,
        corridor_token: pool.collateral.parse().map_err(ApiError::bad_request)?,
        settlement_token: pool.debt.parse().map_err(ApiError::bad_request)?,
        corridor_amount: body.corridor_amount,
        min_settlement: body.min_settlement,
        expires_at: crate::time::unix_now() + 86_400,
        closed: false,
    };
    sale.validate(crate::time::unix_now())
        .map_err(ApiError::bad_request)?;
    let dir = path.parent().expect("config parent");
    // Durable local authorization precedes publishing its unsigned link.
    manual::write(dir, &sale).map_err(ApiError::bad_request)?;
    let mut payload = serde_json::to_value(&sale).map_err(ApiError::bad_request)?;
    payload
        .as_object_mut()
        .expect("sale object")
        .remove("closed");
    let result = venue(
        &cfg,
        dir,
        reqwest::Method::POST,
        "maker/manual-sales",
        Some(payload),
    )
    .await?;
    Ok(Json(result))
}
pub async fn list(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let bot = state.bot(&name).await?;
    let (path, _, cfg) = super::modules::read(&bot)?;
    let vault = &cfg
        .vault
        .as_ref()
        .ok_or_else(|| ApiError::bad_request("An operator vault is required"))?
        .address;
    Ok(Json(
        venue(
            &cfg,
            path.parent().expect("config parent"),
            reqwest::Method::GET,
            &format!("maker/manual-sales?vault={vault}"),
            None,
        )
        .await?,
    ))
}
pub async fn close(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (_lock, bot) = super::bots::lock_config(&name, &state).await?;
    super::require_editable(&bot)?;
    let (path, _, cfg) = super::modules::read(&bot)?;
    let dir = path.parent().expect("config parent");
    let mut sale = manual::read(dir, &id).map_err(ApiError::bad_request)?;
    sale.closed = true;
    // Stop the signer first, including when the venue is temporarily unavailable.
    manual::write(dir, &sale).map_err(ApiError::bad_request)?;
    Ok(Json(
        venue(
            &cfg,
            dir,
            reqwest::Method::POST,
            &format!("maker/manual-sales/{id}/close"),
            Some(json!({})),
        )
        .await?,
    ))
}
