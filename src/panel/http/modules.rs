// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use super::{ApiError, AppState};
use crate::{
    config::Config,
    modules::{
        replay::{self, Dataset, Report},
        runtime::{Status, STATUS_FILE},
        ModulesConfig,
    },
    panel::inventory::Bot,
};
use axum::{
    extract::{Path, State},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn read(bot: &Bot) -> Result<(std::path::PathBuf, String, Config), ApiError> {
    let path = bot
        .config_panel_path
        .clone()
        .ok_or_else(|| ApiError::not_found("Config unavailable"))?;
    let raw = std::fs::read_to_string(&path).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let cfg = Config::from_toml(&raw).map_err(ApiError::bad_request)?;
    if !cfg.modules_enabled {
        return Err(ApiError::not_found("Modules are disabled in stitch.toml"));
    }
    Ok((path, raw, cfg))
}
fn revision(raw: &str) -> String {
    alloy_primitives::hex::encode(Sha256::digest(raw.as_bytes()))
}
#[derive(Serialize)]
pub struct View {
    pub config: ModulesConfig,
    pub revision: String,
    pub status: Option<Status>,
    pub running: bool,
    pub settlement_decimals: u8,
    pub dataset_template: serde_json::Value,
}
pub async fn show(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<View>, ApiError> {
    let bot = state.bot(&name).await?;
    let (path, raw, cfg) = read(&bot)?;
    let status_path = path.parent().expect("config parent").join(STATUS_FILE);
    let status = if status_path.exists() {
        let size = std::fs::metadata(&status_path)
            .map_err(|e| ApiError::bad_request(e.to_string()))?
            .len();
        if size > 2_000_000 {
            return Err(ApiError::conflict("Module status file exceeds its limit"));
        }
        Some(
            serde_json::from_str(
                &std::fs::read_to_string(status_path)
                    .map_err(|e| ApiError::bad_request(e.to_string()))?,
            )
            .map_err(ApiError::bad_request)?,
        )
    } else {
        None
    };
    let dataset_template = serde_json::json!({
        "version": 1, "chain_id": cfg.chain_id, "corridor_token": cfg.pools[0].collateral, "settlement_token": cfg.pools[0].debt,
        "corridor_decimals": cfg.pools[0].collateral_decimals, "settlement_decimals": cfg.pools[0].debt_decimals,
        "initial_settlement": "0", "initial_corridor": "0", "max_order_settlement": "0", "max_order_corridor": "0",
        "reserve_settlement": "0", "reserve_corridor": "0", "cost_per_trade": "0", "events": []
    });
    Ok(Json(View {
        dataset_template,
        revision: revision(&raw),
        config: cfg.modules,
        status,
        running: bot.state.is_running(),
        settlement_decimals: cfg.pools[0].debt_decimals,
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub revision: String,
    pub config: ModulesConfig,
}
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Update>,
) -> Result<Response, ApiError> {
    let (_lock, bot) = super::bots::lock_config(&name, &state).await?;
    super::require_editable(&bot)?;
    let (path, raw, _) = read(&bot)?;
    if revision(&raw) != body.revision {
        return Err(ApiError::conflict(
            "Config changed since you opened Modules. Reload before saving.",
        ));
    }
    body.config.validate().map_err(ApiError::bad_request)?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(ApiError::bad_request)?;
    let block = toml::to_string(&body.config)
        .map_err(ApiError::bad_request)?
        .parse::<toml_edit::DocumentMut>()
        .map_err(ApiError::bad_request)?;
    doc["modules"] = toml_edit::Item::Table(block.as_table().clone());
    let edited = doc.to_string();
    Config::from_toml(&edited).map_err(ApiError::bad_request)?;
    super::settings::save_and_restart(&state, &bot, &path, &edited, 0, None).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub config: ModulesConfig,
    pub dataset: Dataset,
}
pub async fn simulate(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Simulation>,
) -> Result<Json<Report>, ApiError> {
    let (_lock, bot) = super::bots::lock_config(&name, &state).await?;
    let (_, _, cfg) = read(&bot)?;
    if body.dataset.chain_id != cfg.chain_id {
        return Err(ApiError::bad_request(
            "Historical data belongs to a different chain",
        ));
    }
    let book = crate::rfq::responder::book_from_pool(
        &cfg.pools[0],
        &cfg.feed.url,
        crate::config::rfq_staleness_secs_for_pool(&cfg.feed, &cfg.pools[0]),
    )
    .map_err(ApiError::bad_request)?
    .ok_or_else(|| ApiError::bad_request("No quotable corridor"))?;
    // One replay per bot, bounded to 10000 rows by the engine and 2 MiB by Axum.
    let report =
        tokio::task::spawn_blocking(move || replay::run(&body.dataset, &body.config, &book))
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?
            .map_err(ApiError::bad_request)?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{harness, Harness, TEST_KEY};
    use super::*;
    use axum::http::StatusCode;
    fn seed(h: &Harness, enabled: bool) {
        crate::setup::write_config(
            h.root.join("bot-a"),
            crate::setup::find_corridor("cngn-usdt-bsc").unwrap(),
            TEST_KEY,
        )
        .unwrap();
        let path = h.root.join("bot-a/stitch.toml");
        let mut doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        doc["modules_enabled"] = toml_edit::value(enabled);
        doc["vault"] = toml_edit::table();
        doc["vault"]["address"] = toml_edit::value("0x00000000000000000000000000000000000000aa");
        doc["rfq"] = toml_edit::table();
        for (k, v) in [
            ("url", "wss://localhost/v2/maker/stream"),
            ("maker_id", "test"),
            (
                "validation_contract",
                "0x00000000000000000000000000000000000000bb",
            ),
        ] {
            doc["rfq"][k] = toml_edit::value(v);
        }
        doc["rfq"]["enabled"] = toml_edit::value(true);
        std::fs::write(path, doc.to_string()).unwrap();
    }
    #[tokio::test]
    async fn feature_flag_hides_read_write_and_simulation_routes() {
        let h = harness("module-gate");
        seed(&h, false);
        assert_eq!(
            h.get("/api/bots/bot-a/modules").await.0,
            StatusCode::NOT_FOUND
        );
        let config = serde_json::to_value(ModulesConfig::default()).unwrap();
        assert_eq!(
            h.put_json(
                "/api/bots/bot-a/modules",
                serde_json::json!({"revision":"x", "config":config})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (_, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(Harness::parse(&body)["config"]["modulesEnabled"], false);
    }
    #[tokio::test]
    async fn one_property_enables_defaults_and_saves_preserve_other_config() {
        let h = harness("module-settings");
        seed(&h, true);
        let (status, body) = h.get("/api/bots/bot-a/modules").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let view = Harness::parse(&body);
        assert_eq!(view["config"]["mode"], "shadow");
        let mut config = view["config"].clone();
        config["inventory"]["target_bps"] = serde_json::json!(2500);
        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules",
                serde_json::json!({"revision":view["revision"], "config":config}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["restarted"], false);
        let stored =
            Config::from_toml(&std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap())
                .unwrap();
        assert_eq!(stored.modules.inventory.target_bps, 2500);
        assert!(stored.modules_enabled);
        assert!(stored.vault.is_some());
        assert_eq!(stored.chain_id, 56);
        let (status, _) = h
            .put_json(
                "/api/bots/bot-a/modules",
                serde_json::json!({"revision":view["revision"], "config":config}),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "stale edits must not overwrite newer settings"
        );
    }
    #[tokio::test]
    async fn invalid_module_parameters_never_write_config() {
        let h = harness("module-invalid");
        seed(&h, true);
        let (_, body) = h.get("/api/bots/bot-a/modules").await;
        let view = Harness::parse(&body);
        let path = h.root.join("bot-a/stitch.toml");
        let before = std::fs::read_to_string(&path).unwrap();
        let mut cfg = view["config"].clone();
        cfg["inventory"]["max_bps"] = serde_json::json!(1);
        let (status, _) = h
            .put_json(
                "/api/bots/bot-a/modules",
                serde_json::json!({"revision":view["revision"], "config":cfg}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    }
}
