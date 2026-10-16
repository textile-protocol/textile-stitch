// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use super::{ApiError, AppState};
use crate::{
    config::Config,
    modules::{
        runtime::{Status, STATUS_FILE},
        ModulesConfig,
    },
    panel::inventory::Bot,
    pricing::quote::Spread,
};
use axum::{
    extract::{Path, State},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) fn read(bot: &Bot) -> Result<(std::path::PathBuf, String, Config), ApiError> {
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
/// One side of the corridor's spread as configured, before any module adjusts
/// it. Bps as written; an absolute offset is in collateral per debt, which the
/// panel can only turn into bps at a price.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum CorridorSpread {
    Bps(u32),
    Abs(f64),
}
impl From<Spread> for CorridorSpread {
    fn from(s: Spread) -> Self {
        match s {
            Spread::Bps(b) => Self::Bps(b),
            Spread::Abs(d) => Self::Abs(d),
        }
    }
}
/// The spreads the modules start from. `None` is a side the corridor leaves off,
/// which no module turns back on.
#[derive(Serialize, Debug, PartialEq)]
pub struct CorridorSpreads {
    pub buy: Option<CorridorSpread>,
    pub sell: Option<CorridorSpread>,
}
#[derive(Serialize)]
pub struct View {
    pub config: ModulesConfig,
    pub revision: String,
    pub status: Option<Status>,
    pub running: bool,
    pub settlement_decimals: u8,
    pub corridor_decimals: u8,
    pub corridor_spreads: CorridorSpreads,
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
    Ok(Json(View {
        revision: revision(&raw),
        status,
        running: bot.state.is_running(),
        settlement_decimals: cfg.pools[0].debt_decimals,
        corridor_decimals: cfg.pools[0].collateral_decimals,
        // Modules require exactly one pool, so this is the corridor they adjust.
        corridor_spreads: CorridorSpreads {
            buy: cfg.pools[0].buy_spread().map(Into::into),
            sell: cfg.pools[0].sell_spread().map(Into::into),
        },
        config: cfg.modules,
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
    place_last(&mut doc, "modules");
    let edited = doc.to_string();
    Config::from_toml(&edited).map_err(ApiError::bad_request)?;
    super::settings::save_and_restart(&state, &bot, &path, &edited, 0, None).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enabled {
    pub enabled: bool,
}
/// Turn the module feature flag on or off. Separate from `update` because
/// `read` refuses every module route while the flag is off. The loader decides
/// whether this config can run modules, so a blocked bot gets its reason back
/// and nothing is written.
pub async fn set_enabled(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Enabled>,
) -> Result<Response, ApiError> {
    let (_lock, bot) = super::bots::lock_config(&name, &state).await?;
    super::require_editable(&bot)?;
    let path = super::settings::config_path(&bot)?;
    let raw = super::settings::read_toml(&path)?;
    let turned_off = toml::from_str::<Config>(&raw)
        .map(|current| current.module_conflicts())
        .unwrap_or_default();
    let edited = with_modules_flag(&raw, body.enabled).map_err(ApiError::bad_request)?;
    let cfg = Config::from_toml(&edited).map_err(ApiError::bad_request)?;
    if body.enabled {
        if let Some(blocker) = blockers(&cfg, &path, state.cfg.runtime).into_iter().next() {
            return Err(ApiError::bad_request(blocker));
        }
    }
    let note = match (body.enabled, turned_off.is_empty()) {
        (false, _) => "The Modules tab is hidden. Its settings stay in stitch.toml for when you turn modules back on.".to_string(),
        (true, true) => "Modules start in Preview: they record what they would do without changing quotes. Configure them in the Modules tab.".to_string(),
        (true, false) => format!(
            "Modules start in Preview: they record what they would do without changing quotes. Configure them in the Modules tab. Turned off, since modules can't run alongside: {}.",
            and_list(&turned_off)
        ),
    };
    super::settings::save_and_restart(
        &state,
        &bot,
        &path,
        &edited,
        0,
        Some(serde_json::json!({ "note": note })),
    )
    .await
}

const NO_RFQ_CREDENTIAL: &str =
    "modules require a Textile connection; no RFQ credential reaches this bot";

/// What stops modules running on this bot, in the order they're reported: what
/// the loader refuses, then the RFQ credential. The credential lives beside the
/// config rather than in it, and the module runtime is built inside the RFQ
/// runtime, which never starts without one: the flag would be on and nothing
/// would run. Checked with the same rule as the Start guard.
pub(super) fn blockers(
    cfg: &Config,
    path: &std::path::Path,
    runtime: crate::panel::PanelRuntime,
) -> Vec<String> {
    let no_credential =
        cfg.rfq_active() && !crate::panel::inventory::rfq_key_reaches_bot(cfg, path, runtime);
    cfg.modules_blockers()
        .into_iter()
        .map(String::from)
        .chain(no_credential.then(|| NO_RFQ_CREDENTIAL.to_string()))
        .collect()
}

/// What turning modules on would still run into, and what it turns off on the
/// way. `blockers` is read from the config it would write, so a conflict it
/// clears isn't listed there; `turns_off` names those instead.
#[derive(Debug, Default)]
pub(super) struct Readiness {
    pub blockers: Vec<String>,
    pub turns_off: Vec<String>,
}

pub(super) fn readiness(
    raw: &str,
    path: &std::path::Path,
    runtime: crate::panel::PanelRuntime,
) -> Readiness {
    let Ok(current) = toml::from_str::<Config>(raw) else {
        return Readiness::default();
    };
    let candidate =
        with_modules_flag(raw, true).and_then(|toml| Ok(toml::from_str::<Config>(&toml)?));
    Readiness {
        blockers: match candidate {
            Ok(candidate) => blockers(&candidate, path, runtime),
            Err(e) => vec![format!("{e:#}")],
        },
        turns_off: current
            .module_conflicts()
            .into_iter()
            .map(String::from)
            .collect(),
    }
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `raw` with the root `modules_enabled` flag set. A root key prints above the
/// first table, where the loader reads it; everything else, comments
/// included, stays as written. Turning modules on first turns off what they
/// can't run beside ([`crate::setup::turn_off_module_conflicts`]), and puts a
/// saved `[modules].mode` back to `shadow`: turning modules off keeps that
/// table, so a bot last run in Live would otherwise restart straight into Live
/// and change quotes. An absent mode already defaults to `shadow`.
pub fn with_modules_flag(raw: &str, enabled: bool) -> anyhow::Result<String> {
    let raw = if enabled {
        crate::setup::turn_off_module_conflicts(raw)?
    } else {
        raw.to_string()
    };
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    doc["modules_enabled"] = toml_edit::value(enabled);
    if enabled {
        if let Some(modules) = doc
            .get_mut("modules")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            modules.insert("mode", toml_edit::value("shadow"));
        }
    }
    Ok(doc.to_string())
}

/// Print the table at `key`, and every table under it, as one block after the
/// rest of the document. A table built from a separate document keeps that
/// document's positions, which start at zero, so `[modules.inventory]` would
/// otherwise land above `[[pools]]` and `[modules]` itself at the very end.
fn place_last(doc: &mut toml_edit::DocumentMut, key: &str) {
    let mut next = doc
        .iter()
        .filter(|(k, _)| *k != key)
        .filter_map(|(_, item)| last_position(item))
        .max()
        .map_or(0, |p| p + 1);
    if let Some(table) = doc.get_mut(key).and_then(toml_edit::Item::as_table_mut) {
        renumber(table, &mut next);
    }
}
/// The highest document position of `item` or anything nested in it.
fn last_position(item: &toml_edit::Item) -> Option<usize> {
    let within = |t: &toml_edit::Table| {
        t.iter()
            .filter_map(|(_, child)| last_position(child))
            .chain(t.position())
            .max()
    };
    match item {
        toml_edit::Item::Table(t) => within(t),
        toml_edit::Item::ArrayOfTables(a) => a.iter().filter_map(within).max(),
        _ => None,
    }
}
fn renumber(table: &mut toml_edit::Table, next: &mut usize) {
    table.set_position(*next);
    *next += 1;
    for (_, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Table(t) => renumber(t, next),
            toml_edit::Item::ArrayOfTables(a) => a.iter_mut().for_each(|t| renumber(t, next)),
            _ => {}
        }
    }
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
        crate::setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_module_tests").unwrap();
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
    async fn feature_flag_hides_read_and_write_routes() {
        let h = harness("module-gate");
        seed(&h, false);
        assert_eq!(
            h.get("/api/bots/bot-a/modules").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            h.get("/api/bots/bot-a/modules/manual-sales").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(h.post_json("/api/bots/bot-a/modules/manual-sales", serde_json::json!({
            "taker": "0x2222222222222222222222222222222222222222", "corridorAmount": "100", "minSettlement": "1"
        })).await.0, StatusCode::NOT_FOUND);
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
        assert_eq!(view["config"]["spreads"]["inventory_aware"], false);
        let mut config = view["config"].clone();
        config["inventory"]["target_bps"] = serde_json::json!(2500);
        config["spreads"]["inventory_aware"] = serde_json::json!(true);
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
        assert!(stored.modules.spreads.inventory_aware);
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
    async fn the_view_carries_the_corridor_spreads_modules_start_from() {
        let h = harness("module-corridor-spreads");
        seed(&h, true);
        let path = h.root.join("bot-a/stitch.toml");
        let mut doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        doc["pools"][0]["buy_offset_bps"] = toml_edit::value(30);
        doc["pools"][0]
            .as_table_like_mut()
            .unwrap()
            .remove("sell_offset_bps");
        doc["pools"][0]["sell_offset_abs"] = toml_edit::value(2.5);
        std::fs::write(&path, doc.to_string()).unwrap();
        let (status, body) = h.get("/api/bots/bot-a/modules").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            Harness::parse(&body)["corridor_spreads"],
            serde_json::json!({
                "buy": {"kind": "bps", "value": 30},
                "sell": {"kind": "abs", "value": 2.5},
            })
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

    fn table_headers(toml: &str) -> Vec<String> {
        toml.lines()
            .map(str::trim)
            .filter(|l| l.starts_with('['))
            .map(String::from)
            .collect()
    }
    #[tokio::test]
    async fn turning_modules_on_and_off_flips_only_the_root_flag() {
        let h = harness("module-enable");
        seed(&h, false);
        let path = h.root.join("bot-a/stitch.toml");
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            h.get("/api/bots/bot-a/modules").await.0,
            StatusCode::NOT_FOUND
        );

        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules/enabled",
                serde_json::json!({"enabled": true}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let res = Harness::parse(&body);
        assert_eq!(res["settings"]["modulesEnabled"], true);
        assert!(res["message"].as_str().unwrap().contains("Preview"));
        let on = std::fs::read_to_string(&path).unwrap();
        let flag = on.find("modules_enabled = true").expect("flag written");
        assert!(
            flag < on.find("\n[").unwrap(),
            "root keys must precede the first table"
        );
        assert_eq!(
            table_headers(&on),
            table_headers(&before),
            "no tables added or moved"
        );
        assert_eq!(h.get("/api/bots/bot-a/modules").await.0, StatusCode::OK);

        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules/enabled",
                serde_json::json!({"enabled": false}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("modules_enabled = false"));
        assert_eq!(
            h.get("/api/bots/bot-a/modules").await.0,
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn turning_modules_back_on_after_live_starts_in_preview() {
        let h = harness("module-reenable");
        seed(&h, true);
        let (_, body) = h.get("/api/bots/bot-a/modules").await;
        let view = Harness::parse(&body);
        let mut config = view["config"].clone();
        config["mode"] = serde_json::json!("live");
        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules",
                serde_json::json!({"revision": view["revision"], "config": config}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for enabled in [false, true] {
            let (status, body) = h
                .put_json(
                    "/api/bots/bot-a/modules/enabled",
                    serde_json::json!({"enabled": enabled}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (_, body) = h.get("/api/bots/bot-a/modules").await;
        let view = Harness::parse(&body);
        assert_eq!(view["config"]["mode"], "shadow");
        assert_eq!(
            view["config"]["inventory"]["target_bps"],
            config["inventory"]["target_bps"]
        );
    }
    #[tokio::test]
    async fn a_bot_modules_cannot_run_on_says_why_and_keeps_its_config() {
        let h = harness("module-enable-blocked");
        seed(&h, false);
        let path = h.root.join("bot-a/stitch.toml");
        let mut doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        doc.remove("vault");
        std::fs::write(&path, doc.to_string()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let (_, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(
            Harness::parse(&body)["modulesBlockers"],
            serde_json::json!(["modules require an OperatorVault, one corridor and enabled RFQ"])
        );
        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules/enabled",
                serde_json::json!({"enabled": true}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("OperatorVault"), "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
    #[tokio::test]
    async fn modules_stay_off_until_an_rfq_credential_reaches_the_bot() {
        let h = harness("module-enable-no-key");
        seed(&h, false);
        std::fs::remove_file(h.root.join("bot-a").join(crate::setup::RFQ_API_KEY_FILE)).unwrap();
        let path = h.root.join("bot-a/stitch.toml");
        let before = std::fs::read_to_string(&path).unwrap();

        let (_, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(
            Harness::parse(&body)["modulesBlockers"],
            serde_json::json!([NO_RFQ_CREDENTIAL])
        );
        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules/enabled",
                serde_json::json!({"enabled": true}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("no RFQ credential"), "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
    #[test]
    fn the_conflict_switch_turns_off_exactly_what_it_names() {
        let h = harness("module-conflicts");
        seed(&h, false);
        let raw = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let mut doc = raw.parse::<toml_edit::DocumentMut>().unwrap();
        doc["book_enabled"] = toml_edit::value(true);
        let pool = &mut doc["pools"][0];
        pool["limit_taker_enabled"] = toml_edit::value(true);
        pool["closer_pool"] = toml_edit::value("0x0000000000000000000000000000000000000003");
        pool["floor_ray"] = toml_edit::value("1000000000000000000000000000");
        pool["lean_shadow"] = toml_edit::value(true);
        pool["lean_floor_bps"] = toml_edit::value(3.0);
        pool["twap_window_secs"] = toml_edit::value(60);
        pool["twap_max_deviation_bps"] = toml_edit::value(50);
        let on = doc.to_string();
        let before: Config = toml::from_str(&on).unwrap();
        assert_eq!(
            before.module_conflicts(),
            [
                "the public ladder",
                "the taker leg",
                "the closer",
                "lean",
                "TWAP"
            ]
        );

        let off = crate::setup::turn_off_module_conflicts(&on).unwrap();
        let after: Config = toml::from_str(&off).unwrap();
        assert!(after.module_conflicts().is_empty());
        assert!(after
            .modules_blockers()
            .iter()
            .all(|b| !b.contains("RFQ-only") && !b.contains("lean or TWAP")));
        // Only the switches: the closer's and lean's other values stay.
        assert_eq!(after.pools[0].floor_ray, before.pools[0].floor_ray);
        assert_eq!(
            after.pools[0].lean_floor_bps,
            before.pools[0].lean_floor_bps
        );
        assert_eq!(
            after.pools[0].buy_offset_bps,
            before.pools[0].buy_offset_bps
        );
    }
    #[tokio::test]
    async fn turning_modules_on_turns_off_what_they_cannot_run_beside() {
        let h = harness("module-enable-conflicts");
        seed(&h, false);
        let path = h.root.join("bot-a/stitch.toml");
        let mut doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let pool = &mut doc["pools"][0];
        pool["twap_window_secs"] = toml_edit::value(60);
        pool["lean_shadow"] = toml_edit::value(true);
        pool["lean_floor_bps"] = toml_edit::value(3.0);
        std::fs::write(&path, doc.to_string()).unwrap();

        let (_, body) = h.get("/api/bots/bot-a/settings").await;
        let settings = Harness::parse(&body);
        assert_eq!(settings["modulesBlockers"], serde_json::json!([]));
        assert_eq!(
            settings["modulesTurnsOff"],
            serde_json::json!(["lean", "TWAP"])
        );

        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules/enabled",
                serde_json::json!({"enabled": true}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(Harness::parse(&body)["message"]
            .as_str()
            .unwrap()
            .contains("lean and TWAP"));
        let saved = Config::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.modules_enabled);
        assert!(saved.module_conflicts().is_empty());
        assert_eq!(saved.pools[0].lean_floor_bps, Some(3.0));
    }
    #[test]
    fn blockers_match_what_the_loader_refuses_in_order() {
        let h = harness("module-blockers");
        seed(&h, true);
        let raw = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(Config::from_toml(&raw)
            .unwrap()
            .modules_blockers()
            .is_empty());
        let mut doc = raw.parse::<toml_edit::DocumentMut>().unwrap();
        doc["pools"][0]["limit_taker_enabled"] = toml_edit::value(true);
        doc["pools"][0]["twap_window_secs"] = toml_edit::value(60);
        let blocked = doc.to_string();
        let parsed: Config = toml::from_str(&blocked).unwrap();
        assert_eq!(
            parsed.modules_blockers(),
            [
                "modules require RFQ-only operation; disable the ladder, limit taker and closer",
                "modules cannot be combined with legacy lean or TWAP settings",
            ]
        );
        assert_eq!(
            Config::from_toml(&blocked).unwrap_err().to_string(),
            parsed.modules_blockers()[0]
        );
    }
    #[tokio::test]
    async fn saved_module_settings_print_as_one_block_after_the_other_tables() {
        let h = harness("module-layout");
        seed(&h, true);
        let path = h.root.join("bot-a/stitch.toml");
        let before = table_headers(&std::fs::read_to_string(&path).unwrap());
        for target in [2500, 2600] {
            let (_, body) = h.get("/api/bots/bot-a/modules").await;
            let view = Harness::parse(&body);
            let mut config = view["config"].clone();
            config["inventory"]["target_bps"] = serde_json::json!(target);
            let (status, body) = h
                .put_json(
                    "/api/bots/bot-a/modules",
                    serde_json::json!({"revision": view["revision"], "config": config}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let headers = table_headers(&std::fs::read_to_string(&path).unwrap());
            let block = [
                "[modules]",
                "[modules.inventory]",
                "[modules.spreads]",
                "[modules.rebalance]",
            ];
            assert_eq!(
                &headers[..before.len()],
                &before[..],
                "operator tables keep their order"
            );
            assert_eq!(&headers[before.len()..], &block[..], "{headers:?}");
        }
    }
}
