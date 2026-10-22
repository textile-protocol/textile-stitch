// SPDX-License-Identifier: AGPL-3.0-or-later
use super::{ApiError, AppState};
use crate::{
    config::Config,
    perps::{self, PerpsConfig},
};
use axum::{
    extract::{Path, State},
    response::Response,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub async fn show(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let bot = state.bot(&name).await?;
    let path = super::settings::config_path(&bot)?;
    let cfg =
        Config::from_toml(&super::settings::read_toml(&path)?).map_err(ApiError::bad_request)?;
    let p = cfg.perps.clone().unwrap_or_default();
    let remote = if p.assignment_id.is_empty() {
        Ok(None)
    } else {
        perps::request(
            &cfg,
            path.parent().expect("config parent"),
            reqwest::Method::GET,
            &p.assignment_id,
            "",
            None,
        )
        .await
        .map(Some)
    };
    let unavailable = remote.is_err();
    Ok(Json(
        json!({"enabled":p.enabled,"assignmentId":p.assignment_id,
        "remote":remote.ok().flatten(),"unavailable":unavailable}),
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Update {
    assignment_id: String,
    mode: String,
}
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Extension(identity): Extension<crate::panel::auth::Identity>,
    Json(body): Json<Update>,
) -> Result<Response, ApiError> {
    let (_lock, bot) = super::bots::lock_config(&name, &state).await?;
    let origin = super::changes::origin(&identity, crate::changelog::Source::PanelPerps, &bot.name);
    super::require_editable(&bot)?;
    if !["off", "preview", "live"].contains(&body.mode.as_str()) {
        return Err(ApiError::bad_request("Choose off, preview or live"));
    }
    let path = super::settings::config_path(&bot)?;
    let raw = super::settings::read_toml(&path)?;
    let cfg = Config::from_toml(&raw).map_err(ApiError::bad_request)?;
    let dir = path.parent().expect("config parent");
    if cfg
        .perps
        .as_ref()
        .is_some_and(|p| p.enabled && p.assignment_id != body.assignment_id)
    {
        return Err(ApiError::bad_request(
            "Turn the existing FX hedge off before changing assignments",
        ));
    }
    // Off must be available during a venue outage. The running task also reads
    // the saved gate before each heartbeat, including when restart fails.
    if body.mode == "off" {
        let current = cfg
            .perps
            .as_ref()
            .ok_or_else(|| ApiError::bad_request("No hedge configured"))?;
        if current.assignment_id != body.assignment_id {
            return Err(ApiError::bad_request("Disable the configured assignment"));
        }
        let stopped = perps::request(
            &cfg,
            dir,
            reqwest::Method::PUT,
            &body.assignment_id,
            "",
            Some(json!({"mode":"off"})),
        )
        .await
        .is_ok();
        let mut doc = raw
            .parse::<toml_edit::DocumentMut>()
            .map_err(ApiError::bad_request)?;
        doc["perps"]["enabled"] = toml_edit::value(false);
        let note = if stopped {
            "FX hedge disabled. Existing positions retain exit protection."
        } else {
            "Disabled locally. Venue unavailable; the last lease expires within 20 seconds. Existing positions retain exit protection."
        };
        return super::settings::save_and_restart(
            &state,
            &bot,
            &path,
            &doc.to_string(),
            &origin,
            0,
            Some(json!({"note":note})),
        )
        .await;
    }
    let remote = perps::request(
        &cfg,
        dir,
        reqwest::Method::GET,
        &body.assignment_id,
        "",
        None,
    )
    .await
    .map_err(ApiError::bad_request)?;
    let summary = bot
        .config
        .as_ref()
        .ok_or_else(|| ApiError::bad_request("Bot wallet unavailable"))?;
    let funding = summary
        .vault_address
        .as_ref()
        .or(summary.operator_address.as_ref())
        .ok_or_else(|| ApiError::bad_request("Bot wallet unavailable"))?;
    if !assignment_matches(&remote["assignment"], &name, &cfg, funding) {
        return Err(ApiError::bad_request(
            "Hedge assignment must match this Stitch, funding wallet and corridor",
        ));
    }
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(ApiError::bad_request)?;
    let enabled = body.mode != "off";
    // Validate locally before requesting an authorization revision.
    let candidate = PerpsConfig {
        enabled,
        assignment_id: body.assignment_id.clone(),
        bot_id: name,
        revision: "0".repeat(36),
    };
    let block = toml::to_string(&candidate)
        .map_err(ApiError::bad_request)?
        .parse::<toml_edit::DocumentMut>()
        .map_err(ApiError::bad_request)?;
    doc["perps"] = toml_edit::Item::Table(block.as_table().clone());
    Config::from_toml(&doc.to_string()).map_err(ApiError::bad_request)?;
    let control = perps::request(
        &cfg,
        dir,
        reqwest::Method::PUT,
        &body.assignment_id,
        "",
        Some(json!({"mode":body.mode})),
    )
    .await
    .map_err(ApiError::bad_request)?;
    let revision = control["revision"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("Missing authorization revision"))?;
    doc["perps"]["revision"] = toml_edit::value(revision);
    // The service requires heartbeats carrying this revision. A failed save or
    // restart cannot leave an old bot authorized to increase a position.
    super::settings::save_and_restart(&state,&bot,&path,&doc.to_string(),&origin,0,Some(json!({"note":"FX hedge authorization saved. Off stops increases; existing positions retain exit and liquidation protection."}))).await
}

fn assignment_matches(a: &Value, name: &str, cfg: &Config, funding: &str) -> bool {
    a["botId"].as_str() == Some(name)
        && a["chainId"].as_u64() == Some(cfg.chain_id)
        && a["fundingWallet"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(funding))
        && cfg.pools.iter().any(|p| {
            a["inventoryToken"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(&p.collateral))
                && a["inventoryDecimals"].as_u64() == Some(u64::from(p.collateral_decimals))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assignment_binds_bot_chain_funding_wallet_and_inventory_units() {
        let cfg: Config = toml::from_str(include_str!("../../../stitch.example.toml")).unwrap();
        let a = json!({"botId":"one","chainId":cfg.chain_id,"fundingWallet":"0xabc",
            "inventoryToken":cfg.pools[0].collateral,"inventoryDecimals":cfg.pools[0].collateral_decimals});
        assert!(assignment_matches(&a, "one", &cfg, "0xABC"));
        for (field, value) in [
            ("botId", json!("other")),
            ("chainId", json!(999)),
            ("fundingWallet", json!("0xdef")),
            ("inventoryToken", json!("0xdef")),
            ("inventoryDecimals", json!(99)),
        ] {
            let mut wrong = a.clone();
            wrong[field] = value;
            assert!(!assignment_matches(&wrong, "one", &cfg, "0xabc"));
        }
    }
}
