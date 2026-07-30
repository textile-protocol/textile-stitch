// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Per-bot settings.
//!
//! Two ways to edit: the structured form, backed by
//! [`setup::read_settings_at`] / [`setup::apply_settings`], and a raw TOML editor
//! for the long tail of pool fields the form doesn't show. Both validate through
//! `Config::from_toml` before anything is written, so a save can't produce a file
//! the bot then refuses to start on.
//!
//! A successful save restarts the container when the bot is running, because the
//! bot reads its config once at startup. A stopped bot is left stopped: editing
//! settings is not a request to go live. If the write lands and the restart fails,
//! the response says exactly that — the alternative is an operator staring at a
//! "saved" toast while the bot keeps quoting the old spread.

use std::path::PathBuf;

use axum::extract::{Path as UrlPath, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{require_editable, ApiError, AppState};
use crate::config::Config;
use crate::panel::docker::STOP_GRACE_SECS;
use crate::panel::inventory::Bot;
use crate::setup::{
    self, PoolPair, SettingsPatch, SettingsView, SideSizing, SpreadEdit, SpreadKind,
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SpreadBody {
    /// `bps` or `abs`, matching whichever form the config already uses.
    pub kind: String,
    pub value: String,
}

impl From<&SpreadEdit> for SpreadBody {
    fn from(s: &SpreadEdit) -> Self {
        Self {
            kind: match s.kind {
                SpreadKind::Bps => "bps".to_string(),
                SpreadKind::Abs => "abs".to_string(),
            },
            value: s.value.clone(),
        }
    }
}

impl SpreadBody {
    fn to_edit(&self) -> Result<SpreadEdit, ApiError> {
        let kind = match self.kind.as_str() {
            "bps" => SpreadKind::Bps,
            "abs" => SpreadKind::Abs,
            other => {
                return Err(ApiError::bad_request(format!(
                    "spread kind must be \"bps\" or \"abs\", not \"{other}\""
                )))
            }
        };
        Ok(SpreadEdit {
            kind,
            value: self.value.trim().to_string(),
        })
    }
}

/// Amounts stay strings the whole way through: they're atomic-unit integers, and
/// a JSON number would round a large inventory into a wrong one.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SizingBody {
    pub total_liquidity: String,
    pub min_slice_debt: String,
    pub order_size: String,
    pub max_orders: String,
}

impl From<&SideSizing> for SizingBody {
    fn from(s: &SideSizing) -> Self {
        Self {
            total_liquidity: s.total_liquidity.clone(),
            min_slice_debt: s.min_slice_debt.clone(),
            order_size: s.order_size.clone(),
            max_orders: s.max_orders.clone(),
        }
    }
}

impl SizingBody {
    fn to_sizing(&self) -> SideSizing {
        SideSizing {
            total_liquidity: self.total_liquidity.trim().to_string(),
            min_slice_debt: self.min_slice_debt.trim().to_string(),
            order_size: self.order_size.trim().to_string(),
            max_orders: self.max_orders.trim().to_string(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairBody {
    pub collateral: String,
    pub collateral_decimals: u8,
    pub debt: String,
    pub debt_decimals: u8,
}

impl From<&PoolPair> for PairBody {
    fn from(p: &PoolPair) -> Self {
        Self {
            collateral: p.collateral.clone(),
            collateral_decimals: p.collateral_decimals,
            debt: p.debt.clone(),
            debt_decimals: p.debt_decimals,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsBody {
    pub rpc_url: String,
    pub feed_url: String,
    pub buy: SpreadBody,
    pub sell: SpreadBody,
    pub taker_enabled: bool,
    pub pool_index: usize,
    pub pool_count: usize,
    pub pair: PairBody,
    pub buy_sizing: SizingBody,
    pub sell_sizing: SizingBody,
    pub ttl_secs: u64,
    pub tick_interval_secs: u64,
    /// Whether saving will be accepted. False for a bot whose config the panel can
    /// see but not write.
    pub editable: bool,
}

impl SettingsBody {
    fn from_view(v: &SettingsView, editable: bool) -> Self {
        Self {
            rpc_url: v.rpc_url.clone(),
            feed_url: v.feed_url.clone(),
            buy: SpreadBody::from(&v.buy),
            sell: SpreadBody::from(&v.sell),
            taker_enabled: v.taker_enabled,
            pool_index: v.pool_index,
            pool_count: v.pool_count,
            pair: PairBody::from(&v.pair),
            buy_sizing: SizingBody::from(&v.buy_sizing),
            sell_sizing: SizingBody::from(&v.sell_sizing),
            ttl_secs: v.ttl_secs,
            tick_interval_secs: v.tick_interval_secs,
            editable,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolQuery {
    /// Which pool to read. Defaults to the first, like the desktop app.
    #[serde(default)]
    pub pool: usize,
}

/// The path the panel reads and writes a bot's config at, refusing bots it can't.
fn config_path(bot: &Bot) -> Result<PathBuf, ApiError> {
    require_editable(bot)?;
    bot.config_panel_path
        .clone()
        .ok_or_else(|| ApiError::conflict(format!("{}'s config isn't readable", bot.name)))
}

/// One lock per config file, so a read-modify-write can't interleave with another.
///
/// `update` reads the whole TOML, applies a partial patch to it, and writes the whole
/// file back. Two of those against one bot — two tabs, or a structured save racing the
/// raw editor — both read the same starting text and both write a complete file, so
/// whichever lands second silently drops the other's edit. That breaks the thing the
/// partial-patch API promises: fields you don't mention are left alone.
///
/// Keyed by path rather than one global lock because the write is followed by a
/// restart with a 30s grace period, and a save for an unrelated bot has no business
/// waiting behind that.
///
/// It does not serialize against the desktop app or a hand edit — nothing in-process
/// can. It covers the panel racing itself, which is what having two tabs makes easy.
#[derive(Debug, Default)]
pub struct ConfigLocks {
    live: std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >,
}

impl ConfigLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock for one config path, created on first use. Bounded by the number of
    /// bots on the host, so there is nothing to evict.
    pub fn for_path(&self, path: &std::path::Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::sync::Arc::clone(live.entry(path.to_path_buf()).or_default())
    }
}

fn read_toml(path: &std::path::Path) -> Result<String, ApiError> {
    std::fs::read_to_string(path).map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("couldn't read {}: {e}", path.display()),
        )
    })
}

pub async fn show(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Query(query): Query<PoolQuery>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    let toml = read_toml(&path)?;
    // A pool index out of range is the caller's mistake, not a server fault.
    let view = setup::read_settings_at(&toml, query.pool).map_err(ApiError::bad_request)?;
    Ok(Json(SettingsBody::from_view(&view, bot.is_editable())).into_response())
}

/// A partial update. Anything left out keeps its current value, so a UI that only
/// edits spreads doesn't have to know what the sizing is.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdate {
    #[serde(default)]
    pub pool: Option<usize>,
    #[serde(default)]
    pub rpc_url: Option<String>,
    #[serde(default)]
    pub feed_url: Option<String>,
    #[serde(default)]
    pub buy: Option<SpreadBody>,
    #[serde(default)]
    pub sell: Option<SpreadBody>,
    #[serde(default)]
    pub taker_enabled: Option<bool>,
    #[serde(default)]
    pub buy_sizing: Option<SizingBody>,
    #[serde(default)]
    pub sell_sizing: Option<SizingBody>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub tick_interval_secs: Option<u64>,
}

impl SettingsUpdate {
    /// Fold the update onto the config as it is now.
    fn onto(&self, current: &SettingsView) -> Result<SettingsPatch, ApiError> {
        let mut patch = current.to_patch();
        if let Some(pool) = self.pool {
            patch.pool_index = pool;
        }
        if let Some(v) = &self.rpc_url {
            patch.rpc_url = v.trim().to_string();
        }
        if let Some(v) = &self.feed_url {
            patch.feed_url = v.trim().to_string();
        }
        if let Some(v) = &self.buy {
            patch.buy = v.to_edit()?;
        }
        if let Some(v) = &self.sell {
            patch.sell = v.to_edit()?;
        }
        if let Some(v) = self.taker_enabled {
            patch.taker_enabled = v;
        }
        if let Some(v) = &self.buy_sizing {
            patch.buy_sizing = Some(v.to_sizing());
        }
        if let Some(v) = &self.sell_sizing {
            patch.sell_sizing = Some(v.to_sizing());
        }
        if let Some(v) = self.ttl_secs {
            patch.ttl_secs = Some(v);
        }
        if let Some(v) = self.tick_interval_secs {
            patch.tick_interval_secs = Some(v);
        }
        Ok(patch)
    }
}

pub async fn update(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<SettingsUpdate>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    // Held across read → patch → write, so a concurrent save can't read the same
    // starting text and overwrite this one's edit with a complete file of its own.
    let lock = state.config_locks.for_path(&path);
    let _saving = lock.lock().await;
    let current_toml = read_toml(&path)?;

    let pool = body.pool.unwrap_or(0);
    let current = setup::read_settings_at(&current_toml, pool).map_err(ApiError::bad_request)?;
    let patch = body.onto(&current)?;
    // `apply_settings` re-validates through the real loader, so an invalid value
    // fails here and nothing is written.
    let edited = setup::apply_settings(&current_toml, &patch).map_err(ApiError::bad_request)?;

    save_and_restart(&state, &bot, &path, &edited, pool).await
}

/// The raw config, for the fields the form doesn't cover.
pub async fn raw(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    let toml = read_toml(&path)?;
    Ok(Json(serde_json::json!({
        "toml": toml,
        "path": path.display().to_string(),
        "editable": bot.is_editable(),
    }))
    .into_response())
}

#[derive(Deserialize)]
pub struct RawUpdate {
    pub toml: String,
}

/// Replace a bot's config wholesale, after validating it the way the bot would.
pub async fn save_raw(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<RawUpdate>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    // Same lock as the structured save: a raw write is a whole-file write, so it can
    // clobber a partial patch just as easily as the other way round.
    let lock = state.config_locks.for_path(&path);
    let _saving = lock.lock().await;
    // The same parse the bot does at startup. Rejecting here is the whole point of
    // the escape hatch being server-validated rather than a blind file write.
    Config::from_toml(&body.toml).map_err(|e| {
        ApiError::bad_request(format!(
            "that config isn't valid, and the bot would fail to start on it: {e:#}"
        ))
    })?;
    save_and_restart(&state, &bot, &path, &body.toml, 0).await
}

/// Write the file, then bounce the container.
///
/// The write is atomic (`write_toml_atomic`), so a bot reading its config at the
/// same moment sees either the old file or the new one, never a truncated one.
async fn save_and_restart(
    state: &AppState,
    bot: &Bot,
    path: &std::path::Path,
    toml: &str,
    pool: usize,
) -> Result<Response, ApiError> {
    setup::write_toml_atomic(path, toml).map_err(|e| ApiError::internal(&e))?;
    tracing::info!(bot = %bot.name, path = %path.display(), "config saved");

    // Only a bot that was running gets bounced. `docker restart` on a stopped or
    // never-started container starts it, which would turn "I tweaked a spread"
    // into "I put a bot on the book" — including for a bot straight out of the
    // wizard, which deliberately isn't started until the allowance is approved.
    //
    // The restart is reported, not asserted. A bot that saved but didn't come back
    // is exactly the case an operator must not be lied to about.
    //
    // And it's a bot launch like any other, so it takes the same wallet reservation.
    // The save that got here may have enabled a taker or a closer, which turns a
    // maker an approval was legitimately running alongside into one that broadcasts.
    // Without this the exclusion the docs promise for Restart has a side door.
    //
    // Re-discovered from disk first, because `bot` describes the config as it was
    // *before* the write. A raw-config save can change `chain_id` or an MPC operator
    // address, so the stale view names the wallet the bot is leaving — reserving that
    // one and restarting into another is a lock on the wrong door.
    let restarting = state.bot(&bot.name).await.unwrap_or_else(|_| bot.clone());
    let (restarted, restart_error) = match restarting.container_name.as_deref() {
        Some(container) if restarting.state.is_running() => {
            match state.reservations.reserve_for(&restarting) {
                // Held across the restart, then dropped with `_wallet`.
                Some(_wallet) => match state.docker.restart(container, STOP_GRACE_SECS).await {
                    Ok(()) => (true, None),
                    Err(e) => {
                        tracing::error!(bot = %bot.name, "config saved but the restart failed: {e:#}");
                        (false, Some(format!("{e:#}")))
                    }
                },
                None => {
                    tracing::warn!(bot = %bot.name, "config saved but the wallet is busy");
                    (
                        false,
                        Some(
                            "an approval is running against its operator wallet, so restarting it \
                             now would put two signers on the same nonce"
                                .to_string(),
                        ),
                    )
                }
            }
        }
        _ => (false, None),
    };

    let fresh = read_toml(path)?;
    let view = setup::read_settings_at(&fresh, pool).map_err(ApiError::bad_request)?;
    let message = match &restart_error {
        Some(e) => format!(
            "The config was saved, but restarting {} failed: {e}. It is still running the old \
             config — restart it yourself to apply the change.",
            bot.name
        ),
        None if restarted => format!(
            "Saved and restarted {}. Orders it signed under the old settings stay on the book \
             until they expire.",
            bot.name
        ),
        // A container that is neither running nor terminal — paused, above all — still
        // holds a process, and that process has the *old* settings in memory. Stitch
        // reads its config at startup and never re-reads it, so unpausing resumes the
        // old ones: the file on disk and the bot's behaviour disagree until it is
        // restarted. Saying "it picks the new config up when you start it" would be
        // wrong twice over, since the UI offers Stop for a paused bot, not Start.
        //
        // Not restarted here on purpose. A paused process can't act on SIGTERM, so a
        // graceful stop degenerates into a kill after the grace period — the same
        // reason the layout migration refuses a paused container rather than quietly
        // killing one.
        None if !restarting.state.is_terminal() && restarting.container_name.is_some() => {
            format!(
                "Saved, but {} is {} so it was not restarted, and it is still running the old \
                 settings. Stitch only reads its config at startup: restart it to apply this.",
                bot.name,
                restarting.state.as_str()
            )
        }
        None if bot.container_name.is_some() => format!(
            "Saved. {} isn't running, so there was nothing to restart — it picks the new config \
             up when you start it.",
            bot.name
        ),
        None => format!(
            "Saved. {} has no container, so there was nothing to restart.",
            bot.name
        ),
    };

    Ok(Json(serde_json::json!({
        "settings": SettingsBody::from_view(&view, true),
        "restarted": restarted,
        "restartError": restart_error,
        "message": message,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{harness, Harness, TEST_KEY};
    use crate::panel::docker::fake::{container, dir_layout_mounts, Call};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::LABEL_BOT;
    use crate::setup;
    use axum::http::StatusCode;
    use serde_json::json;

    /// A running panel-native bot with a real config on disk.
    fn seed(h: &Harness, name: &str) {
        seed_in_state(h, name, ContainerState::Running);
    }

    fn seed_in_state(h: &Harness, name: &str, state: ContainerState) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let mut c = container(&format!("stitch-{name}"), state);
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    #[tokio::test]
    async fn settings_are_read_from_the_bots_own_config() {
        let h = harness("settings-read");
        seed(&h, "bot-a");
        let (status, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["rpcUrl"].as_str().unwrap().starts_with("http"));
        assert_eq!(v["poolIndex"], 0);
        assert_eq!(v["editable"], true);
        assert_eq!(v["pair"]["collateral"].as_str().unwrap().len(), 42);
        assert!(v["ttlSecs"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn a_patch_edits_one_field_and_leaves_the_rest() {
        let h = harness("settings-patch");
        seed(&h, "bot-a");
        let (_, before) = h.get("/api/bots/bot-a/settings").await;
        let before = Harness::parse(&before);

        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "buy": { "kind": "bps", "value": "42" } }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["buy"]["value"], "42");
        // Everything the patch didn't mention survived.
        assert_eq!(v["settings"]["rpcUrl"], before["rpcUrl"]);
        assert_eq!(v["settings"]["sell"], before["sell"]);
        assert_eq!(v["settings"]["ttlSecs"], before["ttlSecs"]);
        assert_eq!(v["settings"]["buySizing"], before["buySizing"]);
    }

    #[tokio::test]
    async fn a_save_restarts_the_bot_and_says_orders_outlive_it() {
        let h = harness("settings-restart");
        seed(&h, "bot-a");
        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], true);
        assert!(v["message"].as_str().unwrap().contains("until they expire"));
        assert!(h.docker.calls().contains(&Call::Restart {
            name: "stitch-bot-a".into(),
            grace_secs: 30,
        }));
    }

    #[tokio::test]
    async fn saving_settings_never_starts_a_bot_that_was_not_running() {
        // `docker restart` starts a stopped container. A bot fresh out of the
        // wizard is deliberately not started until its allowance is approved, and a
        // paused bot was paused on purpose: neither should go live because someone
        // adjusted a spread. What they're *told* differs — see the message assertion
        // below — but no container is touched either way.
        for state in [
            ContainerState::Created,
            ContainerState::Exited,
            ContainerState::Paused,
        ] {
            let h = harness("settings-no-start");
            seed_in_state(&h, "bot-a", state);
            let (status, body) = h
                .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
                .await;
            assert_eq!(status, StatusCode::OK, "{state:?}: {body}");
            let v = Harness::parse(&body);
            assert_eq!(v["restarted"], false, "{state:?}");
            assert!(v["restartError"].is_null(), "{state:?}: {body}");
            // Terminal really does mean "start it and it reads the file". Paused does
            // not: the process is frozen with the old settings in memory and unpausing
            // resumes those, so it gets told the save isn't applied yet.
            let expect = if state.is_terminal() {
                "isn't running"
            } else {
                "still running the old settings"
            };
            assert!(
                v["message"].as_str().unwrap().contains(expect),
                "{state:?}: {body}"
            );
            // The save itself must still have happened.
            assert_eq!(v["settings"]["takerEnabled"], true, "{state:?}");
            // And nothing touched the container.
            assert!(
                h.docker.calls().iter().all(|c| !matches!(
                    c,
                    Call::Restart { .. } | Call::Start { .. } | Call::Stop { .. }
                )),
                "{state:?}: {:?}",
                h.docker.calls()
            );
        }
    }

    #[tokio::test]
    async fn a_failed_restart_is_reported_and_the_save_still_stands() {
        // The dishonest version of this reports success and leaves the operator
        // believing a spread change took effect.
        let h = harness("settings-restart-fail");
        seed(&h, "bot-a");
        h.docker.fail_next("daemon is unreachable");
        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false);
        assert!(v["restartError"].as_str().unwrap().contains("unreachable"));
        assert!(v["message"].as_str().unwrap().contains("old config"));
        // The file really was written.
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(toml.contains("limit_taker_enabled = true"));
    }

    #[tokio::test]
    async fn a_save_does_not_restart_into_an_approval_holding_the_wallet() {
        // The side door: an approval is legitimately allowed alongside a running
        // maker-only bot, and a save that switches the taker on turns that bot into one
        // that broadcasts. Restarting it here would put two signers on one nonce while
        // bypassing the exclusion the Restart button promises.
        let h = harness("settings-approval");
        seed(&h, "bot-a");
        let bot = h.state.bot("bot-a").await.unwrap();
        let _approval = h
            .state
            .reservations
            .reserve(bot.wallet().unwrap())
            .expect("nothing else holds it");

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        // The save stands — the file is the operator's intent and it's already valid.
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false);
        assert!(
            v["restartError"].as_str().unwrap().contains("approval"),
            "{body}"
        );
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(toml.contains("limit_taker_enabled = true"));
        // And nothing was bounced.
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn the_reservation_follows_the_wallet_the_save_selects() {
        // The `Bot` this function is handed describes the config as it was *before* the
        // write. A raw-config save can move the bot to another chain — or another MPC
        // operator address — so reserving from the stale view locks the wallet the bot
        // is leaving and restarts into an unguarded one.
        let h = harness("settings-new-wallet");
        seed(&h, "bot-a");
        let before = h.state.bot("bot-a").await.unwrap();
        let old_wallet = before.wallet().expect("a hot wallet has an address");

        // An approval owns the wallet the *new* config will select: same key, chain 1.
        let new_wallet = crate::panel::inventory::WalletId {
            chain_id: 1,
            address: old_wallet.address.clone(),
        };
        let _approval = h
            .state
            .reservations
            .reserve(new_wallet)
            .expect("nothing else holds it");

        // Save a raw config that switches the chain.
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml"))
            .unwrap()
            .replace("chain_id        = 56", "chain_id = 1");
        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": toml }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false, "{body}");
        assert!(
            v["restartError"].as_str().unwrap().contains("approval"),
            "the new wallet's reservation has to be the one that counts: {body}"
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn a_paused_bot_is_told_the_save_is_not_applied_yet() {
        // Stitch reads its config at startup and never again, so a frozen process keeps
        // the settings it started with. Unpausing resumes those, not the file — and the
        // old message promised the opposite ("picks the new config up when you start
        // it") for a bot the UI offers Stop for, not Start.
        let h = harness("settings-paused");
        seed_in_state(&h, "bot-a", ContainerState::Paused);

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false);
        let msg = v["message"].as_str().unwrap();
        assert!(msg.contains("paused"), "{msg}");
        assert!(msg.contains("still running the old settings"), "{msg}");
        assert!(msg.contains("restart it"), "{msg}");
        assert!(
            !msg.contains("when you start it"),
            "a paused bot is not started, it is restarted: {msg}"
        );
        // Not killed to force the point: a paused process can't act on SIGTERM.
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "{:?}",
            h.docker.calls()
        );
        // The save itself still stands.
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(toml.contains("limit_taker_enabled = true"));
    }

    #[tokio::test]
    async fn an_invalid_value_is_refused_without_touching_the_file() {
        let h = harness("settings-invalid");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a/stitch.toml");
        let before = std::fs::read_to_string(&path).unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "rpcUrl": "   " }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // And nothing was restarted on a refused save.
        assert!(h.docker.calls().is_empty());
    }

    #[tokio::test]
    async fn a_bad_spread_kind_is_refused_by_name() {
        let h = harness("settings-badkind");
        seed(&h, "bot-a");
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "buy": { "kind": "percent", "value": "1" } }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("percent"), "{body}");
    }

    #[tokio::test]
    async fn comments_in_the_config_survive_a_save() {
        // The shipped templates are heavily commented, and an operator who edits
        // one spread should not lose the documentation around it.
        let h = harness("settings-comments");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a/stitch.toml");
        let comments_before = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .count();
        h.patch_json(
            "/api/bots/bot-a/settings",
            json!({ "buy": { "kind": "bps", "value": "17" } }),
        )
        .await;
        let after = std::fs::read_to_string(&path).unwrap();
        let comments_after = after
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .count();
        assert_eq!(comments_before, comments_after);
    }

    #[tokio::test]
    async fn the_raw_editor_returns_the_file_and_its_path() {
        let h = harness("raw-read");
        seed(&h, "bot-a");
        let (status, body) = h.get("/api/bots/bot-a/config").await;
        assert_eq!(status, StatusCode::OK);
        let v = Harness::parse(&body);
        assert!(v["toml"].as_str().unwrap().contains("[[pools]]"));
        assert!(v["path"].as_str().unwrap().ends_with("bot-a/stitch.toml"));
    }

    #[tokio::test]
    async fn the_raw_editor_refuses_a_config_the_bot_would_reject() {
        let h = harness("raw-invalid");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a/stitch.toml");
        let before = std::fs::read_to_string(&path).unwrap();

        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/config",
                json!({ "toml": "this is not toml [[[" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("would fail to start"), "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn the_raw_editor_writes_a_valid_config_and_restarts() {
        let h = harness("raw-write");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a/stitch.toml");
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("tick_interval_secs = 5", "tick_interval_secs = 45");
        assert!(
            edited.contains("tick_interval_secs = 45"),
            "the fixture must have that key"
        );

        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": edited }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["tickIntervalSecs"], 45);
        assert_eq!(v["restarted"], true);
    }

    #[tokio::test]
    async fn a_bot_whose_config_is_outside_the_root_cannot_be_edited() {
        let h = harness("settings-foreign");
        let mut c = container("stitch-adopted", ContainerState::Running);
        c.mounts = dir_layout_mounts("/srv/elsewhere/adopted");
        h.docker.add_container(c);

        let (status, body) = h.get("/api/bots/adopted/settings").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("outside"), "{body}");
    }

    #[tokio::test]
    async fn a_pool_index_out_of_range_is_a_bad_request_not_a_crash() {
        let h = harness("settings-badpool");
        seed(&h, "bot-a");
        let (status, body) = h.get("/api/bots/bot-a/settings?pool=9").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("pool"), "{body}");
    }
}
