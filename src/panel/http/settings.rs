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

use super::logs::{self, WalletClaim};
use super::{require_editable, ApiError, AppState};
use crate::config::Config;
use crate::panel::docker::{ContainerState, STOP_GRACE_SECS};
use crate::panel::inventory::{Bot, WalletId};
use crate::panel::naming::{LABEL_RFQ_RESERVATIONS, RFQ_RESERVATIONS_TOKEN};
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
pub struct PoolSummaryBody {
    pub index: usize,
    pub pair: String,
    pub corridor_id: Option<String>,
    pub corridor_label: Option<String>,
    pub collateral: String,
    pub debt: String,
}

impl From<&setup::PoolSummary> for PoolSummaryBody {
    fn from(p: &setup::PoolSummary) -> Self {
        Self {
            index: p.index,
            pair: p.pair.clone(),
            corridor_id: p.corridor_id.clone(),
            corridor_label: p.corridor_label.clone(),
            collateral: p.collateral.clone(),
            debt: p.debt.clone(),
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
    pub pools: Vec<PoolSummaryBody>,
    pub pair: PairBody,
    pub buy_sizing: SizingBody,
    pub sell_sizing: SizingBody,
    pub ttl_secs: u64,
    pub refresh_threshold_bps: u32,
    pub tick_interval_secs: u64,
    /// Rolling TWAP window in seconds. Empty = quote off the instantaneous feed.
    pub twap_window_secs: String,
    /// Spot-deviation guard in bps. Empty = bot default when TWAP is on.
    pub twap_max_deviation_bps: String,
    pub lean_enabled: bool,
    pub lean_shadow: bool,
    pub lean_floor_bps: String,
    pub lean_base_bps: String,
    pub lean_wide_bps: String,
    /// Whether saving will be accepted. False for a bot whose config the panel can
    /// see but not write.
    pub editable: bool,
    /// Raw-config or fleet gate. The Settings RFQ card stays hidden until this is true.
    pub rfq_panel_unlocked: bool,
    /// RFQ-as-default rollout. New wording, migrate-to-RFQ nudge, RFQ-only Connect.
    pub rfq_default_unlocked: bool,
    /// Public ladder. False is RFQ-only.
    pub book_enabled: bool,
    pub rfq_enabled: bool,
    pub rfq_url: String,
    pub rfq_maker_id: String,
    pub rfq_validation_contract: String,
    pub rfq_corridor: String,
    /// A maker API key is stored in `rfq-api.key`. Never the secret itself.
    pub rfq_api_key_set: bool,
}

impl SettingsBody {
    fn from_view(
        v: &SettingsView,
        editable: bool,
        rfq_api_key_set: bool,
        rfq_panel_unlocked: bool,
        rfq_default_unlocked: bool,
    ) -> Self {
        Self {
            rpc_url: v.rpc_url.clone(),
            feed_url: v.feed_url.clone(),
            buy: SpreadBody::from(&v.buy),
            sell: SpreadBody::from(&v.sell),
            taker_enabled: v.taker_enabled,
            pool_index: v.pool_index,
            pool_count: v.pool_count,
            pools: v.pools.iter().map(PoolSummaryBody::from).collect(),
            pair: PairBody::from(&v.pair),
            buy_sizing: SizingBody::from(&v.buy_sizing),
            sell_sizing: SizingBody::from(&v.sell_sizing),
            ttl_secs: v.ttl_secs,
            refresh_threshold_bps: v.refresh_threshold_bps,
            tick_interval_secs: v.tick_interval_secs,
            twap_window_secs: v.twap_window_secs.clone(),
            twap_max_deviation_bps: v.twap_max_deviation_bps.clone(),
            lean_enabled: v.lean_enabled,
            lean_shadow: v.lean_shadow,
            lean_floor_bps: v.lean_floor_bps.clone(),
            lean_base_bps: v.lean_base_bps.clone(),
            lean_wide_bps: v.lean_wide_bps.clone(),
            editable,
            rfq_panel_unlocked,
            rfq_default_unlocked,
            book_enabled: v.book_enabled,
            rfq_enabled: v.rfq_enabled,
            rfq_url: v.rfq_url.clone(),
            rfq_maker_id: v.rfq_maker_id.clone(),
            rfq_validation_contract: v.rfq_validation_contract.clone(),
            rfq_corridor: v.rfq_corridor.clone(),
            rfq_api_key_set,
        }
    }
}

fn rfq_key_is_set(config_path: &std::path::Path) -> bool {
    config_path.parent().is_some_and(setup::rfq_api_key_is_set)
}

fn rfq_surface(toml: &str, bots_dir: &std::path::Path) -> (bool, bool) {
    let fleet = crate::config::rfq_default_flag_in_dir(bots_dir);
    match Config::from_toml(toml) {
        Ok(c) => {
            let default = c.rfq_default_unlocked() || fleet;
            (c.rfq_panel_unlocked() || fleet, default)
        }
        Err(_) => (fleet, fleet),
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
pub(super) fn config_path(bot: &Bot) -> Result<PathBuf, ApiError> {
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

pub(super) fn read_toml(path: &std::path::Path) -> Result<String, ApiError> {
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
    let (rfq_panel, rfq_default) = rfq_surface(&toml, &state.cfg.bots_dir);
    Ok(Json(SettingsBody::from_view(
        &view,
        bot.is_editable(),
        rfq_key_is_set(&path),
        rfq_panel,
        rfq_default,
    ))
    .into_response())
}

/// A partial update. Anything left out keeps its current value, so a UI that only
/// edits spreads doesn't have to know what the sizing is.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdate {
    #[serde(default)]
    pub pool: Option<usize>,
    /// The pair the client believes `pool` names. Required once a bot has more
    /// than one, for the same reason the delete takes it: an index only means
    /// something against the list the client read, and a concurrent add, remove
    /// or replace renumbers it under them.
    #[serde(default)]
    pub collateral: Option<String>,
    #[serde(default)]
    pub debt: Option<String>,
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
    pub refresh_threshold_bps: Option<u32>,
    #[serde(default)]
    pub tick_interval_secs: Option<u64>,
    #[serde(default)]
    pub twap_window_secs: Option<String>,
    #[serde(default)]
    pub twap_max_deviation_bps: Option<String>,
    #[serde(default)]
    pub lean_enabled: Option<bool>,
    #[serde(default)]
    pub lean_shadow: Option<bool>,
    #[serde(default)]
    pub lean_floor_bps: Option<String>,
    #[serde(default)]
    pub lean_base_bps: Option<String>,
    #[serde(default)]
    pub lean_wide_bps: Option<String>,
    #[serde(default)]
    pub rfq_enabled: Option<bool>,
    #[serde(default)]
    pub rfq_url: Option<String>,
    #[serde(default)]
    pub rfq_maker_id: Option<String>,
    #[serde(default)]
    pub rfq_validation_contract: Option<String>,
    #[serde(default)]
    pub rfq_corridor: Option<String>,
    /// Write-only. Omitted or empty leaves the stored key alone. Never returned.
    #[serde(default)]
    pub rfq_api_key: Option<String>,
    #[serde(default)]
    pub book_enabled: Option<bool>,
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
        if let Some(v) = self.refresh_threshold_bps {
            patch.refresh_threshold_bps = Some(v);
        }
        if let Some(v) = self.tick_interval_secs {
            patch.tick_interval_secs = Some(v);
        }
        // A partial UI patch must leave untouched experimental fields alone —
        // otherwise a spread-only save would rewrite them from a stale form.
        // So start from a "don't touch" patch for those and only fold in what
        // the request named.
        patch.twap_window_secs = None;
        patch.twap_max_deviation_bps = None;
        patch.lean_enabled = None;
        patch.lean_shadow = None;
        patch.lean_floor_bps = None;
        patch.lean_base_bps = None;
        patch.lean_wide_bps = None;
        if let Some(v) = &self.twap_window_secs {
            patch.twap_window_secs = Some(v.trim().to_string());
        }
        if let Some(v) = &self.twap_max_deviation_bps {
            patch.twap_max_deviation_bps = Some(v.trim().to_string());
        }
        if let Some(v) = self.lean_enabled {
            patch.lean_enabled = Some(v);
        }
        if let Some(v) = self.lean_shadow {
            patch.lean_shadow = Some(v);
        }
        if let Some(v) = &self.lean_floor_bps {
            patch.lean_floor_bps = Some(v.trim().to_string());
        }
        if let Some(v) = &self.lean_base_bps {
            patch.lean_base_bps = Some(v.trim().to_string());
        }
        if let Some(v) = &self.lean_wide_bps {
            patch.lean_wide_bps = Some(v.trim().to_string());
        }
        if let Some(v) = self.rfq_enabled {
            patch.rfq_enabled = Some(v);
        }
        if let Some(v) = &self.rfq_url {
            patch.rfq_url = Some(v.trim().to_string());
        }
        if let Some(v) = &self.rfq_maker_id {
            patch.rfq_maker_id = Some(v.trim().to_string());
        }
        if let Some(v) = &self.rfq_validation_contract {
            patch.rfq_validation_contract = Some(v.trim().to_string());
        }
        if let Some(v) = &self.rfq_corridor {
            patch.rfq_corridor = Some(v.trim().to_string());
        }
        if let Some(v) = self.book_enabled {
            patch.book_enabled = Some(v);
        }
        Ok(patch)
    }
}

/// Turning the legacy public ladder back on only means something if a side can
/// actually post. `quote_side` needs a spread *and* a size (an order size, or a
/// total-liquidity + min-slice ladder) — see `PoolConfig::buy_enabled`. Flipping
/// `book_enabled` on a pool with neither would leave the bot running, restarted,
/// and quoting nothing, which is exactly the silent-idle failure the Start guard
/// exists to prevent.
///
/// Only fires when the operator asks to turn it *on* (`Some(true)`). An ordinary
/// save on a bot that already has the ladder on is none of this function's
/// business — refusing there would block edits to an existing config.
fn refuse_book_on_without_a_postable_side(
    patch: &setup::SettingsPatch,
    edited: &str,
) -> Result<(), ApiError> {
    if patch.book_enabled != Some(true) {
        return Ok(());
    }
    let cfg = crate::config::Config::from_toml(edited)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    // Any pool, not just the edited one: `book_enabled` is bot-wide, so a
    // multi-pool bot with one postable side does post.
    if cfg
        .pools
        .iter()
        .any(|p| p.tokens_parse() && (p.buy_postable() || p.sell_postable()))
    {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "The public ladder needs a spread and a positive size on at least one side before it can \
         post anything. Set the buy/sell sizing on the Raw config tab (buy_total_liquidity_debt + \
         buy_min_slice_debt, or buy_order_size_debt — same for the sell side), then turn the \
         ladder on."
            .to_string(),
    ))
}

/// The mirror of [`refuse_book_on_without_a_postable_side`]: turning the ladder
/// *off* on a bot whose only working leg was the ladder.
///
/// The save restarts the bot, so it comes back with nothing to do — running,
/// and quoting nothing. Start and Restart already refuse to put a bot in that
/// state; a Settings save is the third door into it, and the Legacy card makes
/// it a one-click door.
///
/// Scoped to the ladder on→off transition on purpose, and *not* generalised to
/// every edit that removes a leg. Switching the ladder off is presented as a
/// migration — "Switch to RFQ only" — where the operator's intent is to keep
/// trading, so landing dead contradicts what they asked for. Explicitly
/// toggling a leg off (the RFQ switch, the taker switch) is the opposite: the
/// operator is saying "stop doing this", and refusing would trap them into
/// giving the bot another leg first. `disabling_a_taker_next_to_a_live_sibling`
/// is the case that keeps this honest.
///
/// Keyed on the transition, not the resulting state: a bot already RFQ-only
/// sends `bookEnabled: false` on every ordinary save, and refusing those would
/// trap the operator in the one screen where they would fix it.
fn refuse_book_off_that_kills_the_last_leg(
    patch: &setup::SettingsPatch,
    current: &str,
    edited: &str,
    path: &std::path::Path,
    runtime: crate::panel::PanelRuntime,
    incoming_key: bool,
) -> Result<(), ApiError> {
    if patch.book_enabled != Some(false) {
        return Ok(());
    }
    let ladder_was_on = crate::config::Config::from_toml(current).is_ok_and(|c| c.book_enabled);
    if !ladder_was_on {
        return Ok(());
    }
    let cfg = crate::config::Config::from_toml(edited)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    // The credential is written further down, after these guards, so a save
    // that pastes the key *and* leaves the ladder in one go would otherwise be
    // judged against the disk as it was before the request. That save is the
    // whole migration in one click, and refusing it would be wrong. The key
    // only stands in for the credential — `[rfq]` still has to be active and
    // quotable, or some other leg runnable.
    if super::bots::config_has_a_live_leg_with(&cfg, path, runtime, incoming_key) {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "Turning the public ladder off would leave this bot with nothing to do: RFQ has no maker \
         credential yet and no other leg is configured. Connect it to Textile first, then switch \
         the ladder off."
            .to_string(),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<SettingsUpdate>,
) -> Result<Response, ApiError> {
    // Lock and re-read *under the lock* via `lock_config`. Locking a path read before the
    // lock is a race: a migration can win the lock first and move the authoritative config
    // to the per-bot layout, leaving our `path` pointing at the flat file it orphaned — the
    // write would then succeed against a dead file and report success while the live config
    // never changed. `lock_config` re-reads under the lock and re-locks the moved path, so
    // `bot`/`path` below are the ones the save actually writes. Held across read → patch →
    // write, so a concurrent save can't read the same starting text and overwrite this
    // one's edit with a complete file of its own.
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;

    let pool = body.pool.unwrap_or(0);
    confirm_pool_identity(
        &current_toml,
        pool,
        body.collateral.as_deref(),
        body.debt.as_deref(),
    )?;
    let current = setup::read_settings_at(&current_toml, pool).map_err(ApiError::bad_request)?;
    let patch = body.onto(&current)?;
    // `apply_settings` re-validates through the real loader, so an invalid value
    // fails here and nothing is written. Use the full anyhow chain — the outer
    // "edited config is not valid" alone doesn't name the field that failed.
    let edited = setup::apply_settings(&current_toml, &patch)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    refuse_book_on_without_a_postable_side(&patch, &edited)?;
    let incoming_rfq_key = body
        .rfq_api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|k| !k.is_empty());
    refuse_book_off_that_kills_the_last_leg(
        &patch,
        &current_toml,
        &edited,
        &path,
        state.cfg.runtime,
        incoming_rfq_key,
    )?;

    if let Some(key) = body
        .rfq_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        let dir = path.parent().ok_or_else(|| {
            ApiError::internal(&anyhow::anyhow!(
                "{}'s config has no parent directory",
                bot.name
            ))
        })?;
        setup::write_rfq_api_key(dir, key).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
        crate::panel::provision::hand_over_paths_to_bot(
            dir,
            &[
                setup::RFQ_API_KEY_FILE.to_string(),
                "stitch.env".to_string(),
            ],
            state.cfg.bot_uid,
        )
        .map_err(|e| ApiError::internal(&e))?;
    }

    save_and_restart(&state, &bot, &path, &edited, pool, None).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddPoolBody {
    pub corridor_id: String,
}

/// Append a same-chain catalog corridor as another `[[pools]]` entry. Restarts
/// a running bot so it quotes the new pair; a stopped bot stays stopped.
pub async fn add_pool(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<AddPoolBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;

    let corridor = setup::find_corridor(&body.corridor_id).ok_or_else(|| {
        ApiError::bad_request(format!(
            "there is no corridor called \"{}\". Ask /api/corridors for the list.",
            body.corridor_id
        ))
    })?;
    if corridor.pending_deploy {
        return Err(ApiError::bad_request(format!(
            "the {} corridor on {} isn't deployed yet, so a bot can't quote it.",
            corridor.display_name, corridor.network_label
        )));
    }
    if corridor.chain_id == 0 {
        return Err(ApiError::bad_request(
            "that corridor has no chain id — 0 is not a network".to_string(),
        ));
    }

    let edited = setup::add_pool_from_template(&current_toml, corridor.toml_template)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    require_token_aware_image(&state, &bot).await?;
    let new_index = Config::from_toml(&edited)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?
        .pools
        .len()
        .saturating_sub(1);
    let note = format!(
        "Added {} on {}. Next: approve its tokens under Tools → Permit2 allowances, then enroll \
         this bot's maker key on the corridor so RFQ quotes it.",
        corridor.display_name, corridor.network_label
    );
    save_and_restart(
        &state,
        &bot,
        &path,
        &edited,
        new_index,
        Some(serde_json::json!({ "note": note })),
    )
    .await
}

/// Refuse a pool-scoped write whose index no longer names the pair the client
/// meant.
///
/// A pool index is only meaningful against the list the client last read: an
/// add, a remove or a config replace renumbers it, so a write that names only
/// an index can land on a corridor the operator never opened — writing one
/// pair's spreads onto another. Single-pool bots can't be renumbered into
/// (removing the last pool is refused), so they may still omit the pair.
fn confirm_pool_identity(
    toml_str: &str,
    pool: usize,
    collateral: Option<&str>,
    debt: Option<&str>,
) -> Result<(), ApiError> {
    let cfg = Config::from_toml(toml_str).map_err(ApiError::bad_request)?;
    if cfg.pools.len() < 2 {
        return Ok(());
    }
    let target = cfg.pools.get(pool).ok_or_else(|| {
        ApiError::bad_request(format!(
            "config has {} pools, so there is no pool {pool}",
            cfg.pools.len()
        ))
    })?;
    let (Some(collateral), Some(debt)) = (collateral, debt) else {
        return Err(ApiError::bad_request(
            "this bot quotes more than one corridor, so a settings write has to say which pair \
             it is editing. Reload the page and try again."
                .to_string(),
        ));
    };
    if !target.collateral.eq_ignore_ascii_case(collateral.trim())
        || !target.debt.eq_ignore_ascii_case(debt.trim())
    {
        return Err(ApiError::conflict(
            "the corridor at that position is not the pair you were editing — this bot's \
             corridors changed since the page loaded. Nothing was written; reload and try again."
                .to_string(),
        ));
    }
    Ok(())
}

/// The pairs a config quotes, in order, for comparing one config against
/// another. Lowercased so a re-typed address doesn't read as a different pool.
fn pool_pairs(cfg: Config) -> Vec<(String, String)> {
    cfg.pools
        .iter()
        .map(|p| (p.collateral.to_lowercase(), p.debt.to_lowercase()))
        .collect()
}

/// Refuse a change to a bot's pool list unless the image it would run declares
/// that it reserves RFQ capacity per wallet token.
///
/// Both directions need this, for the same reason: add and remove only
/// **restart** the container, so the binary that comes back is the one that was
/// already there. An older responder reserves per corridor slug and ignores the
/// `input_token` field, which is wrong in both directions — a new pool on a
/// shared token (USDT) lets it sign the full wallet balance once per corridor,
/// and a removed pool's live claims stay invisible to the corridors that
/// remain, however carefully the panel stamped them on the way out. Recreate is
/// the path that swaps the image, and it drops the in-container slot-nonce
/// ledger, so we never do it as a side effect here: the operator goes through
/// Update.
///
/// The question is what the binary *can do*, so ask the image, not the tag.
/// Comparing the container against `STITCH_PANEL_BOT_IMAGE` only proves it runs
/// what was configured, and production pins that to a `sha-*` tag which can be
/// older than the panel by design — an equal digest there would wave through
/// exactly the responder this refuses.
async fn require_token_aware_image(state: &AppState, bot: &Bot) -> Result<(), ApiError> {
    // The process runtime runs one binary — this panel's — so there is no
    // image to interrogate, and no older binary to come back.
    if state.cfg.runtime != crate::panel::PanelRuntime::Docker {
        return Ok(());
    }
    // A restart keeps the container's own image. With no container, Start
    // creates one from the configured image, so that is the binary at stake —
    // pull it if it isn't here yet, or its labels can't be read at all.
    let image = match bot.image_id.as_deref().filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => match bot.image.as_deref().filter(|s| !s.is_empty()) {
            Some(image) => image.to_string(),
            None => {
                state
                    .docker
                    .ensure_image(&state.cfg.bot_image, false)
                    .await
                    .map_err(|e| {
                        ApiError::conflict(format!(
                            "couldn't fetch {} to check what it supports ({e:#}). Try again when \
                             the registry is reachable.",
                            state.cfg.bot_image
                        ))
                    })?;
                state.cfg.bot_image.clone()
            }
        },
    };
    let labels = state
        .docker
        .local_image_labels(&image)
        .await
        .map_err(|e| ApiError::conflict(format!("couldn't inspect {image} ({e:#})")))?;
    if labels.get(LABEL_RFQ_RESERVATIONS).map(String::as_str) == Some(RFQ_RESERVATIONS_TOKEN) {
        return Ok(());
    }
    Err(ApiError::conflict(format!(
        "{} runs a stitch image that doesn't declare per-token RFQ reservations, so it may track \
         them per corridor and sign against inventory another corridor already committed. Use \
         Update on this bot first — a restart keeps the image it has.",
        bot.name
    )))
}

/// Which pool the caller believes it is deleting.
///
/// An index is not a stable name: removing a pool renumbers the rest, so two
/// clients that both confirmed "remove pool 0" against a three-pool config
/// would, serialized behind the config lock, delete the original 0 and 1. The
/// tokens come from the `pools` list the client rendered, and a mismatch means
/// that list is stale.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovePoolQuery {
    pub collateral: String,
    pub debt: String,
}

/// Drop one `[[pools]]` entry. A bot must keep at least one.
pub async fn remove_pool(
    State(state): State<AppState>,
    UrlPath((name, index)): UrlPath<(String, usize)>,
    Query(expected): Query<RemovePoolQuery>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let current = Config::from_toml(&current_toml).map_err(ApiError::bad_request)?;
    let removed = current.pools.get(index).ok_or_else(|| {
        ApiError::bad_request(format!(
            "config has {} pools, so there is no pool {index}",
            current.pools.len()
        ))
    })?;
    let labelled = setup::identify_pair(current.chain_id, &removed.collateral, &removed.debt)
        .map(|c| c.display_name.to_string())
        .unwrap_or_else(|| format!("pool {index}"));

    // The index is resolved under the config lock, but it was chosen against
    // whatever list the client last read. An earlier removal renumbers the
    // rest, so confirm this is still the pool they meant.
    if !removed
        .collateral
        .eq_ignore_ascii_case(expected.collateral.trim())
        || !removed.debt.eq_ignore_ascii_case(expected.debt.trim())
    {
        return Err(ApiError::conflict(format!(
            "pool {index} is {labelled} now, not the pair you asked to remove — {}'s corridors \
             changed since this page loaded. Nothing was removed; reload and try again.",
            bot.name
        )));
    }

    // Edit the TOML before touching the container. `remove_pool` is what
    // enforces "a bot keeps at least one pool", and stopping first would leave
    // the bot down on a request that removes nothing. Same for the image
    // check — refuse before the stop, not after it.
    let edited = setup::remove_pool(&current_toml, index)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    require_token_aware_image(&state, &bot).await?;

    // The running process has its own in-memory ledger and will persist it.
    // Tagging on disk while it is alive can be overwritten by a quote/prune
    // before the later restart. Stop first, tag, write, then start back.
    //
    // A paused bot is refused instead: its claims are still in memory, a paused
    // process can't act on SIGTERM so the stop degenerates into a kill after
    // the grace period, and unpausing would write the pre-tag ledger back over
    // the stamp anyway. The layout migration refuses a paused container for the
    // same reason.
    if matches!(bot.state, ContainerState::Paused) {
        return Err(ApiError::conflict(format!(
            "{} is paused, so its RFQ claims are still in memory and it can't be shut down \
             gracefully. Stop it before removing a corridor — nothing was removed.",
            bot.name
        )));
    }
    let container = bot.container_name.clone();
    // Every state we stop here wants to be up, so "we stopped it" always
    // implies "start it back" — on the happy path and on the rollback.
    let must_quiesce = bot.state.wants_to_be_up();
    let _wallet = if must_quiesce {
        match state.wallet_locks.try_claim_for(&bot) {
            Some(held) => held,
            None => {
                return Err(ApiError::conflict(format!(
                    "{}'s operator wallet is busy — an approval or launch is running against it. \
                     Nothing was removed; wait for that to finish and try again.",
                    bot.name
                )));
            }
        }
    } else {
        None
    };
    if must_quiesce {
        let name = container
            .as_deref()
            .ok_or_else(|| ApiError::conflict(format!("{} has no container to stop", bot.name)))?;
        state
            .docker
            .stop(name, STOP_GRACE_SECS)
            .await
            .map_err(|e| {
                ApiError::conflict(format!(
                    "couldn't stop {} before tagging RFQ reservations: {e:#}",
                    bot.name
                ))
            })?;
    }

    // From here on the bot may be stopped, so every failure has to put it back
    // before it answers — otherwise a request that changed nothing leaves the
    // bot down.
    let stopped = must_quiesce.then_some(container.as_deref()).flatten();
    if let Err(e) = tag_reservations_before_pool_removal(&path, &current, index) {
        return Err(start_back_after_failure(&state, &bot, stopped, e).await);
    }

    let next_index = index.saturating_sub(1).min(
        Config::from_toml(&edited)
            .map(|c| c.pools.len().saturating_sub(1))
            .unwrap_or(0),
    );
    if let Err(e) = setup::write_toml_atomic(&path, &edited) {
        return Err(start_back_after_failure(&state, &bot, stopped, format!("{e:#}")).await);
    }

    let mut restarted = false;
    let mut restart_error = None;
    if must_quiesce {
        if let Some(name) = container.as_deref() {
            match state.docker.start(name).await {
                Ok(()) => restarted = true,
                Err(e) => restart_error = Some(format!("{e:#}")),
            }
        }
    }

    let fresh = read_toml(&path)?;
    let view = setup::read_settings_at(&fresh, next_index).map_err(ApiError::bad_request)?;
    let (rfq_panel, rfq_default) = rfq_surface(&fresh, &state.cfg.bots_dir);
    let message = match &restart_error {
        Some(e) => format!(
            "Removed {labelled}. The config was saved, but starting {} failed: {e}. Start it \
             yourself to apply the change.",
            bot.name
        ),
        None if restarted => format!("Removed {labelled}. Saved and restarted {}.", bot.name),
        None => format!("Removed {labelled}."),
    };
    Ok(Json(serde_json::json!({
        "settings": SettingsBody::from_view(
            &view,
            true,
            rfq_key_is_set(&path),
            rfq_panel,
            rfq_default,
        ),
        "restarted": restarted,
        "restartError": restart_error,
        "message": message,
    }))
    .into_response())
}

/// Report a failed removal, having first put a bot we stopped back up.
///
/// Removal quiesces the bot before it touches the reservation ledger, so a
/// failure after that point owns the container's state: nothing was removed,
/// and leaving it stopped would turn a rejected request into an outage. Pass
/// `stopped: None` when nothing was stopped.
async fn start_back_after_failure(
    state: &AppState,
    bot: &Bot,
    stopped: Option<&str>,
    problem: String,
) -> ApiError {
    if let Some(name) = stopped {
        if let Err(start_err) = state.docker.start(name).await {
            return ApiError::conflict(format!(
                "{problem} Also failed to start {} again ({start_err:#}).",
                bot.name
            ));
        }
    }
    ApiError::conflict(problem)
}

/// Stamp tokenless RFQ claims we can attribute to the pool about to disappear.
///
/// Unmatched live tokenless rows (upgrade-era venue slugs, stopped bot that
/// never ran `tag_books`) stay invisible to the remaining pools after the
/// slug is gone. Refuse rather than guess.
fn tag_reservations_before_pool_removal(
    toml_path: &std::path::Path,
    cfg: &Config,
    index: usize,
) -> Result<(), String> {
    let Some(removed) = cfg.pools.get(index) else {
        return Ok(());
    };
    let Some(dir) = toml_path.parent() else {
        return Ok(());
    };
    let path = dir.join(crate::rfq::reserve::RESERVATIONS_FILE);
    if !path.exists() {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut ledger = crate::rfq::reserve::Reservations::load(&path, now).map_err(|e| {
        format!(
            "couldn't read RFQ reservations ({e:#}). Not removing a corridor while claims may \
             still be live."
        )
    })?;
    let known = pool_reservation_slugs(cfg.chain_id, removed);
    let known_refs: Vec<&str> = known.iter().map(String::as_str).collect();
    ledger
        .tag_for_removed_pool(&known_refs, &removed.debt, &removed.collateral)
        .map_err(|e| {
            format!(
                "couldn't write the tagged RFQ reservations ({e:#}). Not removing a corridor while \
                 the claims on disk are still untagged — a restart would reload them without a \
                 token and hide them from the remaining pools."
            )
        })?;
    if ledger.live_tokenless_count(now) > 0 {
        return Err(
            "this bot has RFQ reservations from before token-wide accounting whose corridor we \
             cannot attribute. Start it once on the current image so those claims get tagged, or \
             wait until they expire — removing a corridor now can hide them from the remaining \
             pools."
                .to_string(),
        );
    }
    Ok(())
}

fn pool_reservation_slugs(chain_id: u64, pool: &crate::config::PoolConfig) -> Vec<String> {
    let mut slugs = Vec::new();
    if let Some(slug) = pool
        .rfq_corridor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        slugs.push(slug.to_string());
    }
    if let Some(corridor) = setup::identify_pair(chain_id, &pool.collateral, &pool.debt) {
        if !slugs.iter().any(|s| s == corridor.id) {
            slugs.push(corridor.id.to_string());
        }
    }
    slugs
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

/// Which signer backend a config selects, as a word for an error message. The
/// backend is what fixes a container's secret mount and environment at create time,
/// so a change in *this* — not the address or org within a backend — is what a
/// running container can't honour without being rebuilt.
fn signer_provider(signer: &setup::SignerView) -> &'static str {
    match signer {
        setup::SignerView::Local => "local (hot wallet)",
        setup::SignerView::Turnkey { .. } => "Turnkey",
        setup::SignerView::Mpcvault { .. } => "MPCVault",
    }
}

/// Replace a bot's config wholesale, after validating it the way the bot would.
pub async fn save_raw(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<RawUpdate>,
) -> Result<Response, ApiError> {
    // Same lock discipline as the structured save: `lock_config` locks and re-reads under
    // the lock, re-locking the moved path if a migration ran while we waited — otherwise a
    // raw whole-file write could land on a flat file migration orphaned. A raw write can
    // clobber a partial patch just as easily as the other way round, which is why it takes
    // the same per-config lock.
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    // The same parse the bot does at startup. Rejecting here is the whole point of
    // the escape hatch being server-validated rather than a blind file write.
    let incoming = Config::from_toml(&body.toml).map_err(|e| {
        ApiError::bad_request(format!(
            "that config isn't valid, and the bot would fail to start on it: {e:#}"
        ))
    })?;

    // A raw edit can't change the signer backend (`[signer].provider`). The backend's
    // secret (`turnkey-api.key`, `mpcvault-api.token`) and the Turnkey public key live
    // *outside* the TOML, and swapping it needs a container rebuilt with different mounts
    // and env — none of which a raw TOML write can supply. Reject the change and point at
    // the Change signer flow (`PUT /api/bots/{name}/signer`), which takes the credentials,
    // writes them atomically, and recreates the container.
    let current = read_toml(&path)?;
    let old = setup::try_read_signer(&current).map_err(|e| ApiError::internal(&e))?;
    let new = setup::try_read_signer(&body.toml).map_err(ApiError::bad_request)?;
    let (from, to) = (signer_provider(&old), signer_provider(&new));
    if from != to {
        return Err(ApiError::conflict(format!(
            "this changes {name}'s signer backend from {from} to {to}, which a raw config save \
             can't do: the {to} backend needs its own secret file and environment, which don't \
             live in the TOML. Use Change signer instead — it takes the credentials, writes them, \
             and rebuilds the container with the matching runtime."
        )));
    }

    // The raw editor is the documented by-hand path for editing `[[pools]]`, so
    // any change to the pool list carries the same image gate as Add and
    // Remove. Not just growth: dropping or swapping a pair leaves its live
    // claims behind under a slug that no longer names a book, and only a binary
    // that accounts per token counts those against the pools that remain.
    let pools_before = Config::from_toml(&current).map(pool_pairs).ok();
    if pools_before.as_deref() != Some(&pool_pairs(incoming)) {
        require_token_aware_image(&state, &bot).await?;
    }

    save_and_restart(&state, &bot, &path, &body.toml, 0, None).await
}

/// Write the file, then bounce the container.
///
/// The write is atomic (`write_toml_atomic`), so a bot reading its config at the
/// same moment sees either the old file or the new one, never a truncated one.
pub(super) async fn save_and_restart(
    state: &AppState,
    bot: &Bot,
    path: &std::path::Path,
    toml: &str,
    pool: usize,
    extra: Option<serde_json::Value>,
) -> Result<Response, ApiError> {
    // Re-discover the bot *under the config lock the caller holds*, before anything
    // else. `bot` was read by the handler before it took that lock, so two overlapping
    // saves for one running bot both start from the same pre-lock snapshot: if the first
    // moves it from wallet A to B and restarts, the second still thinks it is on A. The
    // live process is on B by the time the second save runs, so a claim on A guards the
    // wrong wallet. Reading here — serialized behind the lock — gets the wallet the
    // running process is actually on.
    //
    // If that read fails, abort: nothing is written and nothing is restarted. Falling
    // back to the stale pre-lock snapshot and writing anyway would claim the wallet the
    // live process already left while moving the config to a third — the live process
    // left unguarded. A save that can't first establish which wallet is live can't be
    // made safe, so it doesn't happen at all.
    let pre_save = state.bot(&bot.name).await.map_err(|e| {
        ApiError::new(
            e.status,
            format!(
                "couldn't re-read {} to save it safely ({}), so nothing was written. Try again \
                 once the Docker daemon is reachable.",
                bot.name, e.message
            ),
        )
    })?;

    // The lock is on one specific path. If a migration moved the authoritative config to
    // the per-bot layout while the caller waited for the lock, `pre_save` names the moved
    // path but `path` still points at the flat file migration orphaned — writing there
    // would succeed against a dead file and report success while the live config never
    // changed. Callers lock via `lock_config`, which re-locks the moved path, so this
    // never fires in practice; it's a hard stop against a stale path reaching the write.
    if pre_save.config_panel_path.as_deref() != Some(path) {
        return Err(ApiError::conflict(format!(
            "{}'s config moved on disk while the save was waiting — a migration to the per-bot \
             layout likely ran. Nothing was written; reload and try again.",
            pre_save.name
        )));
    }

    // Classify the change. A change to transacting-ness (taker/closer) or the wallet
    // (chain or operator address) is *safety-relevant*: the launch paths read the config
    // on disk to decide whether a restart would put a second signer on a wallet, so
    // persisting such a change without restarting leaves the on-disk config lying about
    // the live process — a later Start/Restart then trusts it and starts the second
    // signer. So a safety-relevant change to a *live* bot is all-or-nothing: applied only
    // if the restart is safe right now, else refused without writing. Everything else
    // (spreads, URLs, sizing) doesn't move what the safety checks read, so it can save
    // without a restart as before.
    //
    // `summarise` derives the would-be identity from the incoming TOML plus the key
    // beside the config on disk — which a settings save never touches — so a local bot's
    // operator address is correct without reconstructing it.
    let would_be = crate::panel::inventory::summarise(toml, path).map_err(ApiError::bad_request)?;
    let would_be_wallet = would_be.operator_address.as_ref().map(|address| WalletId {
        chain_id: would_be.chain_id,
        address: address.to_lowercase(),
    });
    let was_transacting = pre_save
        .config
        .as_ref()
        .is_some_and(|c| c.sends_transactions);
    let safety_relevant =
        would_be.sends_transactions != was_transacting || would_be_wallet != pre_save.wallet();

    if safety_relevant {
        if pre_save.state.is_running() {
            // All-or-nothing: check the claims and the fleet, then write+restart or refuse
            // without writing — nothing that can't be applied lands on disk.
            return apply_live_change(state, &pre_save, path, toml, would_be_wallet, pool).await;
        }
        if !pre_save.state.is_terminal() && pre_save.container_name.is_some() {
            // Paused or restarting: the process holds the old config in memory and can't
            // take a clean restart, so this change can't be applied — and leaving it on
            // disk would let a later start trust it and put a second signer on the wallet.
            return Err(ApiError::conflict(format!(
                "{name} is {}, so changing its taker/closer or its wallet can't be applied right \
                 now — it can't take a clean restart, and leaving the change on disk would let a \
                 later start trust it. Stop or unpause {name} first, then save.",
                pre_save.state.as_str(),
                name = pre_save.name
            )));
        }
        // Terminal or no container: not a live signer, so persisting is safe. Falls
        // through to the plain write path below.
    }

    // Claim the wallet the bot is on *before* the write. Discovery reads the config
    // file, so writing it moves the fleet's idea of which wallet this bot uses while
    // the running container is still on the old one. Holding the pre-write wallet
    // across the write and the restart stops a concurrent approval or launch grabbing
    // it, seeing no live sibling (the fleet now reports the *new* wallet), and starting
    // a second signer next to the old process. `Some(None)` — no identifiable wallet —
    // is nothing to guard; `None` — busy — is handled by `restart_after_save`.
    let old_claim = state.wallet_locks.try_claim_for(&pre_save);

    setup::write_toml_atomic(path, toml).map_err(|e| ApiError::internal(&e))?;
    tracing::info!(bot = %pre_save.name, path = %path.display(), "config saved");

    // Re-discover again from disk before deciding anything about the restart: `pre_save`
    // describes the config as it was *before* the write, and a raw save can change
    // `chain_id` or an MPC operator address. A failed re-read is a *skipped* restart, not
    // a restart from the stale identity — the daemon being unreachable is the usual
    // cause, and the restart would fail for the same reason. `restarting` is the
    // post-write bot the message block below reasons about; the `pre_save.clone()`
    // placeholder on failure is never inspected, because `restart_error` is `Some` and
    // the first message arm wins.
    let (restarting, (restarted, restart_error)) = match state.bot(&pre_save.name).await {
        Ok(rediscovered) => {
            let outcome = restart_after_save(state, &pre_save, &rediscovered, old_claim).await;
            (rediscovered, outcome)
        }
        Err(e) => {
            tracing::error!(bot = %pre_save.name, "config saved but re-reading the bot failed: {}", e.message);
            (
                pre_save.clone(),
                (
                    false,
                    Some(format!(
                        "the panel couldn't re-read {} from disk to restart it safely ({}), so it \
                         was left running the old config. Restart it yourself once the daemon is \
                         reachable.",
                        pre_save.name, e.message
                    )),
                ),
            )
        }
    };

    let fresh = read_toml(path)?;
    let view = setup::read_settings_at(&fresh, pool).map_err(ApiError::bad_request)?;
    let message = match &restart_error {
        Some(e) => format!(
            "The config was saved, but restarting {} failed: {e}. It is still running the old \
             config — restart it yourself to apply the change.",
            pre_save.name
        ),
        None if restarted => format!(
            "Saved and restarted {}. Orders it signed under the old settings stay on the book \
             until they expire.",
            pre_save.name
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
                pre_save.name,
                restarting.state.as_str()
            )
        }
        None if restarting.container_name.is_some() => format!(
            "Saved. {} isn't running, so there was nothing to restart — it picks the new config \
             up when you start it.",
            pre_save.name
        ),
        None => format!(
            "Saved. {} has no container, so there was nothing to restart.",
            pre_save.name
        ),
    };

    let (rfq_panel, rfq_default) = rfq_surface(&fresh, &state.cfg.bots_dir);
    let mut body = serde_json::json!({
        "settings": SettingsBody::from_view(
            &view,
            true,
            rfq_key_is_set(path),
            rfq_panel,
            rfq_default,
        ),
        "restarted": restarted,
        "restartError": restart_error,
        "message": message,
    });
    if let (Some(obj), Some(add)) = (
        body.as_object_mut(),
        extra.as_ref().and_then(|v| v.as_object()),
    ) {
        let note = add.get("note").and_then(|v| v.as_str()).map(str::to_string);
        for (k, v) in add {
            if k == "note" {
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
        if let Some(note) = note.filter(|n| !n.is_empty()) {
            let current = obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            obj.insert(
                "message".to_string(),
                serde_json::Value::String(format!("{current} {note}")),
            );
        }
    }

    Ok(Json(body).into_response())
}

/// Apply a safety-relevant change to a *running* bot, all-or-nothing.
///
/// A change to transacting-ness or the wallet can't be persisted without restarting, or
/// the config on disk starts lying to the launch paths about the live process. So the
/// checks happen *before* the write: claim the pre-write wallet and the wallet the change
/// selects, confirm no live sibling on the new wallet, and only then write and restart.
/// Any check that fails refuses the whole save — nothing is written — so a change that
/// can't be applied never lands on disk.
///
/// `new_wallet` is the wallet the incoming config selects (`None` when it has no
/// identifiable one). `pre_save` is the still-running bot.
async fn apply_live_change(
    state: &AppState,
    pre_save: &Bot,
    path: &std::path::Path,
    toml: &str,
    new_wallet: Option<WalletId>,
    pool: usize,
) -> Result<Response, ApiError> {
    let container = pre_save.container_name.as_deref().ok_or_else(|| {
        ApiError::conflict(format!("{} has no container to restart", pre_save.name))
    })?;

    // The pre-write wallet is held until the old process is gone; the wallet the change
    // selects is claimed for the process the restart brings up. One claim when they match.
    let old_claim = match state.wallet_locks.try_claim_for(pre_save) {
        Some(held) => held,
        None => {
            return Err(ApiError::conflict(format!(
                "{}'s operator wallet is busy — an approval or launch is running against it. \
                 Nothing was saved; wait for that to finish and try again.",
                pre_save.name
            )));
        }
    };
    let _new_claim = match &new_wallet {
        None => None,
        Some(w) if old_claim.as_ref().map(WalletClaim::wallet) == Some(w) => None,
        Some(w) => match state.wallet_locks.try_claim(w.clone()) {
            Some(claim) => Some(claim),
            None => {
                return Err(ApiError::conflict(
                    "the operator wallet this change selects is busy — an approval or launch is \
                     running against it. Nothing was saved; wait and try again.",
                ));
            }
        },
    };

    // The fleet half: refuse if a live sibling already transacts on the wallet the change
    // selects. Skip only when the overlap already exists — the running process is itself
    // transacting on that same wallet — because then the restart doesn't introduce it
    // (same rule as `restart_after_save`).
    let overlap_exists = logs::already_transacting(pre_save) && pre_save.wallet() == new_wallet;
    if !overlap_exists {
        if let Some(wallet) = &new_wallet {
            let fleet = state.fleet().await?;
            logs::no_live_sibling_on_wallet_id(&pre_save.name, wallet, &fleet)
                .map_err(ApiError::conflict)?;
        }
    }

    // Safe: commit and restart, both claims held across it.
    let old_toml = read_toml(path)?;
    setup::write_toml_atomic(path, toml).map_err(|e| ApiError::internal(&e))?;
    tracing::info!(bot = %pre_save.name, "config saved (live change), restarting");

    let response = |restarted: bool, restart_error: serde_json::Value, message: String| {
        let fresh = read_toml(path)?;
        let view = setup::read_settings_at(&fresh, pool).map_err(ApiError::bad_request)?;
        let (rfq_panel, rfq_default) = rfq_surface(&fresh, &state.cfg.bots_dir);
        Ok(Json(serde_json::json!({
            "settings": SettingsBody::from_view(
                &view,
                true,
                rfq_key_is_set(path),
                rfq_panel,
                rfq_default,
            ),
            "restarted": restarted,
            "restartError": restart_error,
            "message": message,
        }))
        .into_response())
    };

    let e = match state.docker.restart(container, STOP_GRACE_SECS).await {
        Ok(()) => {
            return response(
                true,
                serde_json::Value::Null,
                format!(
                    "Saved and restarted {}. Orders it signed under the old settings stay on the \
                     book until they expire.",
                    pre_save.name
                ),
            );
        }
        Err(e) => e,
    };

    // A failed restart is *ambiguous*: Docker may have started the replacement on the new
    // wallet before the error (a lost response), so the live process could be on either
    // config. Confirm it's gone before trusting the file — stop it first, *then* roll back.
    // Stopping first means no signer is live when the file goes back to the old wallet.
    tracing::error!(bot = %pre_save.name, "live change restart failed, stopping before rollback: {e:#}");
    let name = &pre_save.name;
    if let Err(se) = state.docker.stop(container, STOP_GRACE_SECS).await {
        // Can't confirm the process is gone, and touching the file could make it disagree
        // with a live new-wallet process. Hold both wallets until the container is stopped.
        tracing::error!(bot = %name, "couldn't stop the bot after a failed restart: {se:#}");
        crate::panel::docker::hold_until_stopped(
            state.docker.clone(),
            container.to_string(),
            (old_claim, _new_claim),
        );
        return response(
            false,
            serde_json::json!(format!("{e:#}")),
            format!(
                "{e:#}. {name} couldn't be stopped either, so its operator wallets are held \
                 blocked until it can be. Recover the Docker daemon, then stop and fix {name} by \
                 hand."
            ),
        );
    }

    // The process is gone. Roll the file back to the old config so a later Start resumes
    // the old, guarded wallet — but if that write fails, the bot is stopped safely (nothing
    // signs) with the *new* config on disk, and the message has to say so, not claim a
    // rollback that didn't happen.
    let message = match setup::write_toml_atomic(path, &old_toml) {
        Ok(()) => format!(
            "The change wasn't applied: restarting {name} failed ({e:#}), so it was stopped and \
             reverted to its old config. Start it to resume."
        ),
        Err(re) => {
            tracing::error!(bot = %name, "couldn't roll the config back after stopping: {re:#}");
            format!(
                "The change wasn't applied cleanly: restarting {name} failed ({e:#}) and its \
                 config couldn't be reverted, so {name} was stopped with the *new* config on disk. \
                 Fix or revert it, then start {name}."
            )
        }
    };
    response(false, serde_json::json!(format!("{e:#}")), message)
}

/// Bounce the container after a save, holding the wallet across the restart.
///
/// A restart is a bot launch like any other — the save may have enabled a taker or a
/// closer, turning a maker an approval was legitimately running alongside into one
/// that broadcasts — so it goes through the same claim-and-check protocol as
/// [`claim_for_launch`](super::bots::claim_for_launch), with one difference: a raw
/// save can move the bot onto a *different* wallet, so this is a transaction over two
/// of them. The pre-write wallet (`old_claim`) is held until the old container is
/// gone; the wallet the save selected is claimed for the process the restart brings
/// up. When they're the same wallet, one claim covers both.
///
/// Returns `(restarted, restart_error)`. Everything that stops the restart — a busy
/// wallet, a live sibling on the wallet the save selected, a Docker failure — is
/// reported, never asserted: a bot that saved but didn't come back is exactly the case
/// an operator must not be lied to about.
///
/// `pre_save` is the bot as it was *before* the write — the config the container is
/// still running — and `restarting` is the re-read post-write bot: the wallet and
/// target config the restart will bring up. The two differ in exactly the way that
/// matters here, so each is used for its own half (see the sibling check below).
/// `old_claim` is [`WalletLocks::try_claim_for`](super::logs::WalletLocks::try_claim_for)
/// on the pre-write bot: `None` = the pre-write wallet was busy, `Some(None)` = it had
/// no identifiable wallet, `Some(Some)` = held.
async fn restart_after_save(
    state: &AppState,
    pre_save: &Bot,
    restarting: &Bot,
    old_claim: Option<Option<WalletClaim>>,
) -> (bool, Option<String>) {
    // Only a running bot gets bounced. `docker restart` on a stopped or never-started
    // container *starts* it, which would turn "I tweaked a spread" into "I put a bot on
    // the book" — including a bot straight out of the wizard, deliberately left stopped
    // until its allowance is approved. A bot that isn't running signs nothing, so the
    // save just lands on disk and any pre-write claim drops here.
    let Some(container) = restarting.container_name.as_deref() else {
        return (false, None);
    };
    if !restarting.state.is_running() {
        return (false, None);
    }

    // Running, so the restart puts a signer back on the wallet and the pre-write wallet
    // has to be held across it (see the call site). Busy means something else is already
    // signing on it, so restarting now would put two signers on the same nonce.
    let old_claim = match old_claim {
        Some(held) => held,
        None => {
            tracing::warn!(bot = %restarting.name, "config saved but the pre-save wallet is busy");
            return (
                false,
                Some(
                    "an approval or launch is running against its operator wallet, so restarting \
                     it now would put two signers on the same nonce. It is still on the old config."
                        .to_string(),
                ),
            );
        }
    };

    // Claim the wallet the save selected too, unless it's the same one already held.
    // Held alongside `old_claim` across the restart, then both drop on return.
    let new_wallet = restarting.wallet();
    let _new_claim = match &new_wallet {
        None => None,
        Some(w) if old_claim.as_ref().map(WalletClaim::wallet) == Some(w) => None,
        Some(w) => match state.wallet_locks.try_claim(w.clone()) {
            Some(claim) => Some(claim),
            None => {
                tracing::warn!(bot = %restarting.name, "config saved but the selected wallet is busy");
                return (
                    false,
                    Some(
                        "an approval is running against the operator wallet this save selected, so \
                         restarting into it would put two signers on the same nonce. It is still on \
                         the old config."
                            .to_string(),
                    ),
                );
            }
        },
    };

    // The fleet half: a running sibling on the same wallet holds no claim, so the lock
    // reads free while its taker spends nonces. Skip it only when the overlap already
    // exists — refusing a restart that would fix it helps nobody. But "already exists"
    // is a question about the process that is *running now*, i.e. `pre_save`, not the
    // config the restart will bring up. Deciding it from `restarting` gets it backwards
    // both ways: a save that turns a taker *off* leaves `restarting` maker-only and
    // would wrongly refuse the restart that removes the overlap, and a save that turns a
    // taker *on* makes `restarting` transacting and would wrongly skip the check even
    // though the live process is maker-only and the restart introduces a second signer.
    // The overlap pre-exists only if the running process is already transacting on the
    // very wallet the restart targets — a raw save can move the wallet too, and a
    // transactor on the wallet it is *leaving* pre-exists nothing on the new one.
    let overlap_already_exists = logs::already_transacting(pre_save)
        && pre_save.wallet().as_ref() == restarting.wallet().as_ref();
    if !overlap_already_exists {
        match state.fleet().await {
            Ok(fleet) => {
                if let Err(e) = logs::no_live_sibling_on_the_wallet(restarting, &fleet) {
                    tracing::warn!(bot = %restarting.name, "config saved but a live sibling shares the wallet");
                    return (false, Some(format!("{e:#} It is still on the old config.")));
                }
            }
            Err(e) => {
                return (
                    false,
                    Some(format!(
                        "the config was saved, but the panel couldn't check the fleet before \
                         restarting ({}), so it left the bot on the old config. Restart it \
                         yourself once the daemon is reachable.",
                        e.message
                    )),
                );
            }
        }
    }

    match state.docker.restart(container, STOP_GRACE_SECS).await {
        Ok(()) => (true, None),
        Err(e) => {
            tracing::error!(bot = %restarting.name, "config saved but the restart failed: {e:#}");
            (false, Some(format!("{e:#}")))
        }
    }
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
        seed_corridor_in_state(h, name, "cngn-usdt-bsc", state);
    }

    /// Delete a pool the way the UI does: naming the pair it means, read from
    /// the `pools` list the client rendered. The index alone is not a stable
    /// name for a pool.
    async fn delete_pool(h: &Harness, bot: &str, index: usize) -> (StatusCode, String) {
        let (_, body) = h.get(&format!("/api/bots/{bot}/settings")).await;
        let v = Harness::parse(&body);
        let pool = v["pools"]
            .as_array()
            .and_then(|pools| pools.iter().find(|p| p["index"] == index))
            .unwrap_or_else(|| panic!("bot has no pool {index}: {body}"))
            .clone();
        h.delete(&format!(
            "/api/bots/{bot}/pools/{index}?collateral={}&debt={}",
            pool["collateral"].as_str().unwrap(),
            pool["debt"].as_str().unwrap(),
        ))
        .await
    }

    fn seed_corridor_in_state(h: &Harness, name: &str, corridor_id: &str, state: ContainerState) {
        let corridor = setup::find_corridor(corridor_id).unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let mut c = container(&format!("stitch-{name}"), state);
        // Panel-created bots launch from STITCH_PANEL_BOT_IMAGE. The generic
        // fixture defaults to `:latest`; the harness uses `:test`.
        c.image = h.state.cfg.bot_image.clone();
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
        // Current images declare per-token RFQ reservations; the pool-list gate
        // refuses one that doesn't. Tests for that clear this again.
        h.docker.set_image_label(
            &format!("sha256:id-stitch-{name}"),
            crate::panel::naming::LABEL_RFQ_RESERVATIONS,
            crate::panel::naming::RFQ_RESERVATIONS_TOKEN,
        );
    }

    /// A running bot whose taker leg is on, so its own process broadcasts from the
    /// operator wallet. Every seed shares `TEST_KEY`, so two of these share a wallet.
    fn seed_transacting(h: &Harness, name: &str, state: ContainerState) {
        seed_in_state(h, name, state);
        let config = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&config).unwrap() + "\nlimit_taker_enabled = true\n";
        std::fs::write(&config, toml).unwrap();
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
        // Present even when the operator hasn't touched it — templates set it.
        assert!(v["refreshThresholdBps"].as_u64().is_some());
        assert_eq!(v["pools"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(v["pools"][0]["corridorId"], "cngn-usdt-bsc");
    }

    #[tokio::test]
    async fn rfq_settings_round_trip_and_api_key_stays_off_the_wire() {
        let h = harness("settings-rfq-write");
        seed(&h, "bot-a");
        let (status, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["rfqEnabled"], false);
        assert_eq!(v["rfqPanelUnlocked"], true);
        assert_eq!(v["rfqUrl"], "");
        assert_eq!(v["rfqMakerId"], "");
        assert_eq!(v["rfqCorridor"], "");
        assert_eq!(v["rfqApiKeySet"], false);
        assert!(
            v.get("rfqApiKey").is_none(),
            "GET must never name the secret"
        );

        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({
                    "rfqEnabled": true,
                    "rfqUrl": "wss://api.textilecredit.com/v2/maker/stream",
                    "rfqMakerId": "clmaker123",
                    "rfqValidationContract": "0x00000000000000000000000000000000000000aa",
                    "rfqCorridor": "cngn-usdt-bsc",
                    "rfqApiKey": "tx_live_panel_secret",
                    "ttlSecs": 90
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], true);
        assert_eq!(
            v["settings"]["rfqUrl"],
            "wss://api.textilecredit.com/v2/maker/stream"
        );
        assert_eq!(v["settings"]["rfqMakerId"], "clmaker123");
        assert_eq!(v["settings"]["rfqCorridor"], "cngn-usdt-bsc");
        assert_eq!(v["settings"]["rfqApiKeySet"], true);
        assert_eq!(v["settings"]["ttlSecs"], 90);
        assert!(
            v["settings"].get("rfqApiKey").is_none(),
            "PATCH response must not echo the key"
        );
        assert!(
            !body.contains("tx_live_panel_secret"),
            "the raw key must never appear in the HTTP body"
        );

        let stored = std::fs::read_to_string(h.root.join("bot-a").join("rfq-api.key")).unwrap();
        assert_eq!(stored.trim(), "tx_live_panel_secret");
        let toml = std::fs::read_to_string(h.root.join("bot-a").join("stitch.toml")).unwrap();
        assert!(toml.contains("[rfq]"));
        assert!(toml.contains("clmaker123"));
        assert!(
            !toml.contains("tx_live_panel_secret"),
            "the key must not land in stitch.toml"
        );

        // A later spread-only save must not wipe RFQ or the key file.
        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "ttlSecs": 120 }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], true);
        assert_eq!(v["settings"]["rfqMakerId"], "clmaker123");
        assert_eq!(v["settings"]["rfqApiKeySet"], true);
        assert_eq!(
            std::fs::read_to_string(h.root.join("bot-a").join("rfq-api.key"))
                .unwrap()
                .trim(),
            "tx_live_panel_secret"
        );
    }

    #[tokio::test]
    async fn rfq_panel_is_always_unlocked() {
        let h = harness("settings-rfq-panel-gate");
        seed(&h, "bot-a");
        let (status, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["rfqPanelUnlocked"], true);
        assert_eq!(v["rfqDefaultUnlocked"], true);
    }

    #[tokio::test]
    async fn a_fleet_panel_toml_unlocks_rfq_default_for_every_bot() {
        let h = harness("settings-rfq-fleet-flag");
        seed(&h, "bot-a");
        std::fs::write(
            h.root.join(crate::config::PANEL_FLAGS_FILE),
            format!(
                "[experimental]\nrfq_default = \"{}\"\n",
                crate::config::RFQ_DEFAULT_GATE
            ),
        )
        .unwrap();

        let (status, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["rfqPanelUnlocked"], true);
        assert_eq!(v["rfqDefaultUnlocked"], true);
        assert_eq!(v["bookEnabled"], false);

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "bookEnabled": false }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["settings"]["bookEnabled"], false);
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(toml.contains("book_enabled = false"));
    }

    #[tokio::test]
    async fn the_legacy_card_can_put_a_migrated_bot_back_on_the_ladder() {
        // New bots are RFQ-only. Turning `book_enabled` back on has to remove
        // the key (the config default is true) and restart the bot, so the
        // ladder is genuinely live rather than just reported as on.
        let h = harness("settings-book-back-on");
        seed(&h, "bot-a");
        let config = h.root.join("bot-a/stitch.toml");
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("book_enabled = false"),
            "the catalog template ships RFQ-only"
        );

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "bookEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["settings"]["bookEnabled"], true);

        let toml = std::fs::read_to_string(&config).unwrap();
        assert!(
            !toml.contains("book_enabled"),
            "on is the absent-key default, not a written true: {toml}"
        );
        let cfg = crate::config::Config::from_toml(&toml).unwrap();
        assert!(cfg.book_enabled);
        assert!(
            cfg.pools[0].buy_enabled() && cfg.pools[0].sell_enabled(),
            "the ladder must actually be postable after the flip"
        );
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::Restart { name, .. } if name == "stitch-bot-a"
            )),
            "the running bot has to be restarted to pick the ladder up: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn turning_the_ladder_on_is_refused_when_no_side_can_post() {
        // A spread with no size rests nothing. Allowing the flip would restart
        // the bot into quoting neither RFQ nor a book.
        let h = harness("settings-book-no-size");
        seed(&h, "bot-a");
        let config = h.root.join("bot-a/stitch.toml");
        let stripped = std::fs::read_to_string(&config)
            .unwrap()
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("buy_total_liquidity_debt")
                    && !t.starts_with("buy_min_slice_debt")
                    && !t.starts_with("buy_order_size_debt")
                    && !t.starts_with("sell_total_liquidity_collateral")
                    && !t.starts_with("sell_min_slice_debt")
                    && !t.starts_with("sell_order_size_collateral")
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&config, stripped).unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "bookEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("spread and a positive size"), "{body}");
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("book_enabled = false"),
            "a refused flip must not touch the file"
        );
    }

    #[tokio::test]
    async fn turning_the_ladder_off_is_refused_when_it_was_the_only_leg() {
        // A leftover book bot with no maker credential: switching the ladder
        // off restarts it into quoting nothing. Start and Restart already
        // refuse that state; the Settings save is the third door into it.
        let h = harness("settings-book-off-last-leg");
        seed(&h, "bot-a");
        let config = h.root.join("bot-a/stitch.toml");
        super::super::testkit::keep_book_on(&config);

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "bookEnabled": false }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("nothing to do"), "{body}");
        assert!(
            !std::fs::read_to_string(&config)
                .unwrap()
                .contains("book_enabled = false"),
            "a refused switch must not touch the file"
        );
    }

    #[tokio::test]
    async fn turning_the_ladder_off_is_allowed_when_another_leg_survives() {
        // The taker leg runs independently of both the ladder and RFQ, so this
        // bot still trades after the switch and the save must go through.
        let h = harness("settings-book-off-taker");
        seed(&h, "bot-a");
        let config = h.root.join("bot-a/stitch.toml");
        super::super::testkit::keep_book_on(&config);
        let toml = std::fs::read_to_string(&config).unwrap() + "\nlimit_taker_enabled = true\n";
        std::fs::write(&config, toml).unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "bookEnabled": false }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("book_enabled = false"));
    }

    #[tokio::test]
    async fn the_ladder_can_be_switched_off_in_the_same_save_that_writes_the_key() {
        // The whole migration in one click: paste the credential and leave the
        // book together. The guard runs before `write_rfq_api_key`, so judging
        // it against the disk alone would refuse the save that fixes the bot.
        let h = harness("settings-book-off-with-key");
        seed(&h, "bot-a");
        let config = h.root.join("bot-a/stitch.toml");
        super::super::testkit::keep_book_on(&config);
        let toml = std::fs::read_to_string(&config).unwrap()
            + "\n[rfq]\nenabled = true\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\n\
               maker_id = \"mk_test\"\n\
               validation_contract = \"0x00000000000000000000000000000000000000aa\"\n";
        std::fs::write(&config, toml).unwrap();

        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "bookEnabled": false, "rfqApiKey": "tx_live_secret" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("book_enabled = false"));
        assert!(
            setup::rfq_api_key_is_set(h.root.join("bot-a")),
            "the credential from the same request must be written"
        );
    }

    #[tokio::test]
    async fn turning_rfq_off_is_allowed_even_when_it_was_the_only_leg() {
        // Deliberate: the operator flipping "Answer Swap quote requests" off is
        // saying stop, not asking to migrate. Refusing would trap them into
        // giving the bot another leg first. Start and Restart still refuse to
        // bring the result back up, which is where that belongs.
        let h = harness("settings-rfq-off-allowed");
        seed(&h, "bot-a");
        let dir = h.root.join("bot-a");
        let config = dir.join("stitch.toml");
        let toml = std::fs::read_to_string(&config).unwrap()
            + "\n[rfq]\nenabled = true\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\n\
               maker_id = \"mk_test\"\n\
               validation_contract = \"0x00000000000000000000000000000000000000aa\"\n";
        std::fs::write(&config, toml).unwrap();
        setup::write_rfq_api_key(&dir, "tx_live_secret").unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "rfqEnabled": false }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn an_already_dead_bot_can_still_be_edited() {
        // The trap this guard has to avoid: a bot with no live leg sends its
        // dead config back on every ordinary save. Settings is where an
        // operator fixes it, so those saves must go through.
        let h = harness("settings-dead-bot-editable");
        seed(&h, "bot-a"); // template: book off, no [rfq], no credential

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "ttlSecs": 90 }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["settings"]["ttlSecs"], 90);
    }

    #[tokio::test]
    async fn turning_the_ladder_on_is_refused_when_the_sizes_are_zero() {
        // A present-but-zero size passes a presence check but drafts no orders:
        // the ladder builder finds nothing above the minimum slice.
        let h = harness("settings-book-zero-size");
        seed(&h, "bot-a");
        let config = h.root.join("bot-a/stitch.toml");
        let zeroed = std::fs::read_to_string(&config)
            .unwrap()
            .replace(
                "buy_total_liquidity_debt = \"max\"",
                "buy_total_liquidity_debt = \"0\"",
            )
            .replace(
                "sell_total_liquidity_collateral = \"max\"",
                "sell_total_liquidity_collateral = \"0\"",
            );
        std::fs::write(&config, zeroed).unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "bookEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("positive size"), "{body}");
    }

    #[tokio::test]
    async fn ttl_and_refresh_threshold_round_trip_through_settings() {
        let h = harness("settings-ttl-refresh");
        seed(&h, "bot-a");
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "ttlSecs": 90, "refreshThresholdBps": 0 }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["ttlSecs"], 90);
        assert_eq!(v["settings"]["refreshThresholdBps"], 0);

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "ttlSecs": 20 }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("ttl") || body.contains("lifetime") || body.contains("deadline"),
            "{body}"
        );
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
        assert_eq!(
            v["settings"]["refreshThresholdBps"],
            before["refreshThresholdBps"]
        );
        assert_eq!(v["settings"]["buySizing"], before["buySizing"]);
        assert_eq!(v["settings"]["twapWindowSecs"], before["twapWindowSecs"]);
    }

    #[tokio::test]
    async fn experimental_twap_and_lean_round_trip_through_settings() {
        let h = harness("settings-experimental");
        seed(&h, "bot-a");
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({
                    "twapWindowSecs": "60",
                    "twapMaxDeviationBps": "50",
                    "leanShadow": true,
                    "leanFloorBps": "3.0",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "first patch: {body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["twapWindowSecs"], "60");
        assert_eq!(v["settings"]["twapMaxDeviationBps"], "50");
        assert_eq!(v["settings"]["leanShadow"], true);
        assert_eq!(v["settings"]["leanEnabled"], false);

        // Lean on without a floor is refused; nothing written for that field set.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "leanEnabled": true, "leanFloorBps": "" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("lean_floor"), "{body}");
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
            // A non-safety edit (a spread/feed change): it saves without a restart on any
            // non-running bot. (A taker/wallet change on a *paused* bot is refused — see
            // `enabling_a_taker_on_a_paused_bot_is_refused_without_writing`.)
            let (status, body) = h
                .patch_json(
                    "/api/bots/bot-a/settings",
                    json!({ "feedUrl": "https://feed.example" }),
                )
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
            assert_eq!(
                v["settings"]["feedUrl"], "https://feed.example",
                "{state:?}"
            );
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
    async fn a_failed_restart_is_reported_and_a_non_safety_save_still_stands() {
        // The dishonest version reports success and leaves the operator believing a spread
        // change took effect. A non-safety edit (feed URL) doesn't move the wallet, so a
        // failed restart leaves it saved-but-not-restarted — honestly reported.
        let h = harness("settings-restart-fail");
        seed(&h, "bot-a");
        h.docker.fail_next("daemon is unreachable");
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "feedUrl": "https://feed.example" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false);
        assert!(v["restartError"].as_str().unwrap().contains("unreachable"));
        // The file stands — a non-safety change on disk still matches the live process.
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(toml.contains("feed.example"));
    }

    #[tokio::test]
    async fn wallets_held_after_an_unrecoverable_live_change_release_once_stopped() {
        // The last-resort path: when a live change can't be applied, rolled back, or the
        // process stopped inline, the wallet claims are handed to a background task that
        // holds them blocked until the container is confirmed stopped, then releases them.
        let h = harness("hold-wallets");
        seed(&h, "bot-a");
        let wallet = h.state.bot("bot-a").await.unwrap().wallet().unwrap();
        let claim = h
            .state
            .wallet_locks
            .try_claim(wallet.clone())
            .expect("nothing else holds it");
        assert!(h.state.wallet_locks.is_claimed(&wallet));

        crate::panel::docker::hold_until_stopped(
            h.state.docker.clone(),
            "stitch-bot-a".to_string(),
            (Some(claim), None::<crate::panel::http::logs::WalletClaim>),
        );
        // The task stops the container and only then releases the wallets.
        for _ in 0..50 {
            if !h.state.wallet_locks.is_claimed(&wallet) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !h.state.wallet_locks.is_claimed(&wallet),
            "the wallet must be released once the container is stopped"
        );
        assert!(
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn held_wallets_release_when_the_container_is_already_gone() {
        // If an operator recovers by stopping or removing the container by hand, Docker
        // reports a further `stop` as an error — so the task must key off the container's
        // liveness, not a successful `stop`, or it holds the wallets forever.
        let h = harness("hold-wallets-gone");
        seed_in_state(&h, "bot-a", ContainerState::Exited); // already terminal
        let wallet = h.state.bot("bot-a").await.unwrap().wallet().unwrap();
        let claim = h
            .state
            .wallet_locks
            .try_claim(wallet.clone())
            .expect("nothing else holds it");
        h.docker.fail_next("no such container"); // a stop attempt would error

        crate::panel::docker::hold_until_stopped(
            h.state.docker.clone(),
            "stitch-bot-a".to_string(),
            (Some(claim), None::<crate::panel::http::logs::WalletClaim>),
        );
        for _ in 0..50 {
            if !h.state.wallet_locks.is_claimed(&wallet) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !h.state.wallet_locks.is_claimed(&wallet),
            "the wallets must release once the container is no longer live"
        );
        // It released via the liveness check, without a successful stop.
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn a_failed_restart_rolls_back_a_safety_relevant_change() {
        // A taker enable is safety-relevant, so it's all-or-nothing: the checks pass and it
        // commits and restarts — but a failed restart is ambiguous (Docker may have started
        // the new config before a lost response), so the bot is *stopped* to confirm no
        // signer is live, then the file is reverted to the old config.
        let h = harness("settings-restart-rollback");
        seed(&h, "bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        h.docker.fail_next("daemon is unreachable"); // only the restart fails; the stop succeeds
        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false);
        assert!(
            v["message"].as_str().unwrap().contains("reverted"),
            "{body}"
        );
        // Stopped first — the process is confirmed gone before the file goes back.
        assert!(
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "{:?}",
            h.docker.calls()
        );
        // The taker-on config was rolled back — the file matches the live process again.
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(
            before, after,
            "the change must be rolled back on a failed restart"
        );
    }

    #[tokio::test]
    async fn a_save_that_enables_a_taker_is_refused_while_an_approval_holds_the_wallet() {
        // The side door: an approval is legitimately allowed alongside a running maker-only
        // bot, and a save that switches the taker on turns that bot into one that broadcasts.
        // Applying it would put two signers on one nonce. All-or-nothing: the change is
        // refused and *not written*, so it can't be trusted by a later start either.
        let h = harness("settings-approval");
        seed(&h, "bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let bot = h.state.bot("bot-a").await.unwrap();
        let _approval = h
            .state
            .wallet_locks
            .try_claim(bot.wallet().unwrap())
            .expect("nothing else holds it");

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("busy"), "{body}");
        // Nothing written, nothing bounced.
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after, "the change must not be persisted");
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
    async fn a_wallet_changing_save_is_refused_when_the_new_wallet_is_busy() {
        // A raw save can move the bot to another chain — a wallet change. The claim on the
        // wallet it is moving *to* is the one that counts, taken before the write. When
        // something holds that wallet, the all-or-nothing path refuses without writing.
        let h = harness("settings-new-wallet");
        seed(&h, "bot-a");
        let before_toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let old_wallet = h
            .state
            .bot("bot-a")
            .await
            .unwrap()
            .wallet()
            .expect("a hot wallet has an address");

        // Something owns the wallet the *new* config selects: same key, chain 1.
        let new_wallet = crate::panel::inventory::WalletId {
            chain_id: 1,
            address: old_wallet.address.clone(),
        };
        let _busy = h
            .state
            .wallet_locks
            .try_claim(new_wallet)
            .expect("nothing else holds it");

        let toml = before_toml.replace("chain_id        = 56", "chain_id = 1");
        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": toml }))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("busy"), "{body}");
        // Nothing written, nothing bounced.
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before_toml, after, "the change must not be persisted");
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
    async fn a_save_is_refused_when_a_live_sibling_shares_the_wallet() {
        // The settings restart used to reserve the wallet without the fleet half of the
        // check the launch paths do. So a save that restarted a bot onto a wallet a live
        // transacting sibling already spends nonces from would put a second signer on
        // that nonce sequence — the sibling holds no reservation, so the set reads free.
        let h = harness("settings-sibling");
        seed(&h, "bot-a"); // maker-only, running
        seed_transacting(&h, "bot-b", ContainerState::Running); // live taker, same wallet

        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "feedUrl": "https://feed.example" }),
            )
            .await;
        // The file is the operator's intent and it's valid, so it's saved.
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false, "{body}");
        assert!(
            v["restartError"]
                .as_str()
                .unwrap()
                .contains("shares its operator wallet"),
            "{body}"
        );
        // bot-a must not have been bounced into the overlap.
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
    async fn enabling_a_taker_next_to_a_live_sibling_is_refused() {
        // The running process is maker-only, so it shares the wallet with a live taker
        // sibling harmlessly. Turning its own taker on and restarting would put a second
        // signer on that nonce sequence. The overlap-already-exists test has to read the
        // *pre-save* process (maker-only) here, not the post-save config (transacting),
        // or the check is skipped and the second signer is allowed in.
        let h = harness("settings-enable-taker");
        seed(&h, "bot-a"); // maker-only, running
        seed_transacting(&h, "bot-b", ContainerState::Running); // live taker, same wallet
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        // All-or-nothing: refused, and — the crux of Thread 7 — *not written*, so a later
        // Restart can't read a taker-on config the live process isn't running and bypass
        // the sibling check.
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("shares its operator wallet"), "{body}");
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after, "the taker-on config must not be persisted");
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
    async fn enabling_a_taker_on_a_paused_bot_is_refused_without_writing() {
        // A paused process holds the old config and can't take a clean restart, so a
        // taker change can't be applied — and leaving it on disk would let a later Start
        // trust it. All-or-nothing refuses it.
        let h = harness("settings-paused-taker");
        seed_in_state(&h, "bot-a", ContainerState::Paused);
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("paused"), "{body}");
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after, "the change must not be persisted");
    }

    #[tokio::test]
    async fn disabling_a_taker_next_to_a_live_sibling_is_allowed() {
        // The mirror case: the running process is a live taker sharing the wallet with a
        // live sibling — the overlap already exists. Turning this bot's taker off and
        // restarting *removes* its half of it, so refusing the restart helps nobody.
        // Deciding from the post-save config (now maker-only) would wrongly run the
        // sibling check and refuse; the pre-save process is the transactor, so the
        // overlap pre-exists and the check is skipped.
        let h = harness("settings-disable-taker");
        seed_transacting(&h, "bot-a", ContainerState::Running); // live taker, running
        seed_transacting(&h, "bot-b", ContainerState::Running); // live sibling, same wallet

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": false }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], true, "{body}");
        assert!(v["restartError"].is_null(), "{body}");
        assert!(
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn a_save_does_not_restart_when_the_post_write_re_read_fails() {
        // The write moves what discovery reports, so the restart has to reason about the
        // config on disk *now*. When the *post-write* re-read fails — the daemon dropped
        // out after the write — the old code fell back to the stale snapshot and restarted
        // from it, which could reserve one wallet and launch another. It must skip the
        // restart and say so instead, leaving the edit on disk.
        let h = harness("settings-reread-fail");
        seed(&h, "bot-a");
        // A non-safety edit (feed URL) goes through the write-then-restart path, which is
        // the one with a post-write re-read. `lock_config` re-reads twice (lock, then
        // verify) (#0, #1) and the pre-write re-read (#2) succeed; the post-write re-read
        // (#3) fails, standing in for the daemon dropping out after the write.
        h.docker.fail_list_after(3);

        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "feedUrl": "https://feed.example" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["restarted"], false, "{body}");
        assert!(
            v["restartError"].as_str().unwrap().contains("re-read"),
            "{body}"
        );
        // The edit still landed on disk.
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert!(toml.contains("feed.example"));
        // But nothing was restarted from the stale identity.
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
    async fn a_save_aborts_when_the_pre_write_discovery_fails() {
        // If the under-lock re-read that establishes which wallet is *live* fails, the
        // save can't be made safe: writing anyway would claim a wallet read before the
        // lock — stale under an overlapping save — while moving the config to a third,
        // leaving the live process unguarded. So the whole save aborts: nothing written,
        // nothing restarted.
        let h = harness("settings-prewrite-fail");
        seed(&h, "bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        // Handler entry (#0) succeeds; the pre-write re-read (#1) fails.
        h.docker.fail_list_after(1);

        let (status, body) = h
            .patch_json("/api/bots/bot-a/settings", json!({ "takerEnabled": true }))
            .await;
        assert_ne!(
            status,
            StatusCode::OK,
            "the save must not report success: {body}"
        );
        // Nothing was written or restarted.
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after, "the config must be left untouched");
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
    async fn a_raw_save_that_switches_the_signer_backend_is_rejected() {
        // A raw TOML edit can't switch the signer backend: the new backend's secret file
        // and env live outside the TOML, and swapping it needs a container rebuilt with
        // different mounts. So the raw editor rejects it and points at Change signer.
        let h = harness("settings-signer-switch");
        seed(&h, "bot-a"); // local hot wallet, running

        // A valid Turnkey config to paste into the raw editor. Built through the writer
        // so it parses the way the bot would.
        let tk_dir = h.root.join("turnkey-src");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config_signer(
            &tk_dir,
            corridor,
            &setup::SignerSetup::Turnkey {
                organization_id: "org-1".into(),
                sign_with: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".into(),
                operator_address: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".into(),
                api_base_url: None,
                api_public_key: "PUBKEY".into(),
                api_private_key: "PRIVKEY".into(),
            },
        )
        .unwrap();
        let turnkey_toml = std::fs::read_to_string(tk_dir.join("stitch.toml")).unwrap();
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": turnkey_toml }))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("Change signer"), "{body}");
        // Nothing written, nothing bounced.
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after, "the raw signer switch must not be persisted");
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
    async fn the_pre_save_claim_follows_the_config_on_disk_not_a_stale_read() {
        // The handler reads the bot before it takes the config lock, so two overlapping
        // saves for one running bot both start from the same pre-lock snapshot. If the
        // first moves the bot to a new wallet and restarts, the second still holds the old
        // snapshot — the live process is already on the new wallet, so a claim taken from
        // the snapshot guards the wallet it left. `save_and_restart` must re-read under the
        // lock and claim the wallet the config on disk actually names.
        let h = harness("settings-concurrent-claim");
        seed(&h, "bot-a"); // running, chain 56
        let stale = h.state.bot("bot-a").await.unwrap();
        let stale_wallet = stale.wallet().expect("a hot wallet has an address");
        assert_eq!(stale_wallet.chain_id, 56);

        // Stand in for a concurrent save that already moved the bot to chain 1 on disk.
        let path = h.root.join("bot-a/stitch.toml");
        let moved = std::fs::read_to_string(&path)
            .unwrap()
            .replace("chain_id        = 56", "chain_id = 1");
        std::fs::write(&path, &moved).unwrap();
        let live_wallet = crate::panel::inventory::WalletId {
            chain_id: 1,
            address: stale_wallet.address.clone(),
        };

        // Something already holds the wallet the live process is on now (chain 1).
        let _busy = h
            .state
            .wallet_locks
            .try_claim(live_wallet)
            .expect("nothing else holds it");

        // Save starting from the stale chain-56 snapshot. The claim must land on the
        // chain-1 wallet the file now names, find it busy, and skip the restart. Deriving
        // it from the stale snapshot would claim the unheld chain-56 wallet and restart.
        let resp = super::save_and_restart(&h.state, &stale, &path, &moved, 0, None)
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["restarted"], false, "{v}");
        assert!(
            v["restartError"].as_str().unwrap().contains("wallet"),
            "{v}"
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
    async fn a_save_whose_locked_path_no_longer_names_the_config_is_refused() {
        // The migration race: a save reads a flat path, then blocks on the config lock
        // while a migration wins it and moves the config to the per-bot layout. When the
        // save resumes, the re-read bot names the new path but the caller's `path` still
        // points at the flat file migration orphaned. Writing there would silently succeed
        // against a dead file and report success — so the save must refuse instead.
        let h = harness("settings-stale-path");
        seed(&h, "bot-a"); // config lives at bot-a/stitch.toml
        let bot = h.state.bot("bot-a").await.unwrap();
        let real = h.root.join("bot-a/stitch.toml");
        let before = std::fs::read_to_string(&real).unwrap();
        // A stale path pointing at a flat-layout file the bot is not on.
        let stale_path = h.root.join("stitch.bot-a.toml");

        let err =
            super::save_and_restart(&h.state, &bot, &stale_path, "feed_url = \"x\"\n", 0, None)
                .await
                .expect_err("a save against a path the config no longer sits on must be refused");
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert!(err.message.contains("moved on disk"), "{}", err.message);
        // Nothing was written: the real config is untouched and the orphan wasn't created.
        assert_eq!(std::fs::read_to_string(&real).unwrap(), before);
        assert!(!stale_path.exists(), "the stale path must not be written");
    }

    #[tokio::test]
    async fn a_paused_bot_is_told_the_save_is_not_applied_yet() {
        // Stitch reads its config at startup and never again, so a frozen process keeps
        // the settings it started with. Unpausing resumes those, not the file — and the
        // old message promised the opposite ("picks the new config up when you start
        // it") for a bot the UI offers Stop for, not Start.
        let h = harness("settings-paused");
        seed_in_state(&h, "bot-a", ContainerState::Paused);

        // A non-safety edit (feed URL): it saves, but a paused process keeps its old
        // config, so it's told the save isn't applied yet. (A taker/wallet change on a
        // paused bot is refused instead — see the paused-taker test above.)
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "feedUrl": "https://feed.example" }),
            )
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
        assert!(toml.contains("feed.example"));
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

    /// The by-hand path documented in ADVANCED.md goes through the raw editor,
    /// so it has to hit the same image gate as Add — a restart keeps the old
    /// per-corridor binary either way.
    #[tokio::test]
    async fn the_raw_editor_is_refused_when_a_second_pool_lands_on_a_stale_image() {
        let h = harness("raw-add-pool-stale");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let path = h.root.join("bot-a/stitch.toml");
        let one_pool = std::fs::read_to_string(&path).unwrap();
        let two_pools = setup::add_pool_from_template(
            &one_pool,
            setup::find_corridor("wbrl-usdt-celo")
                .unwrap()
                .toml_template,
        )
        .unwrap();
        h.docker.clear_image_labels("sha256:id-stitch-bot-a");

        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": two_pools }))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("Update"), "{body}");
        assert_eq!(
            one_pool,
            std::fs::read_to_string(&path).unwrap(),
            "a refused raw save must not write the second pool"
        );
    }

    /// Dropping a pool by hand leaves its live claims under a slug that no
    /// longer names a book, and only a token-aware binary counts those against
    /// the pools that remain. So a raw removal is gated too, not just growth.
    #[tokio::test]
    async fn the_raw_editor_is_refused_when_a_pool_is_dropped_on_a_stale_image() {
        let h = harness("raw-drop-pool-stale");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let path = h.root.join("bot-a/stitch.toml");
        let one_pool = std::fs::read_to_string(&path).unwrap();
        let two_pools = setup::add_pool_from_template(
            &one_pool,
            setup::find_corridor("wbrl-usdt-celo")
                .unwrap()
                .toml_template,
        )
        .unwrap();
        std::fs::write(&path, &two_pools).unwrap();
        h.docker.clear_image_labels("sha256:id-stitch-bot-a");

        // Back to one pool, by hand.
        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": one_pool }))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("Update"), "{body}");
        assert_eq!(
            two_pools,
            std::fs::read_to_string(&path).unwrap(),
            "a refused raw save must not drop the pool"
        );
    }

    /// Same raw save, current image: allowed. The gate keys on the pool list
    /// changing, so an ordinary raw edit must not start demanding a pull.
    #[tokio::test]
    async fn the_raw_editor_still_edits_one_pool_without_an_image_check() {
        let h = harness("raw-edit-no-gate");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let path = h.root.join("bot-a/stitch.toml");
        // Old binary, but the pool list isn't changing, so the gate is not this
        // save's business.
        h.docker.clear_image_labels("sha256:id-stitch-bot-a");
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("tick_interval_secs = 5", "tick_interval_secs = 45");

        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": edited }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
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

    #[tokio::test]
    async fn adding_a_same_chain_pool_rewrites_toml_and_restarts() {
        let h = harness("settings-add-pool");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["poolCount"], 2);
        assert_eq!(v["settings"]["poolIndex"], 1);
        assert_eq!(v["settings"]["pools"][1]["corridorId"], "wbrl-usdt-celo");
        assert_eq!(v["restarted"], true);
        assert!(v["message"].as_str().unwrap().contains("wBRL"), "{body}");
        assert!(v["message"].as_str().unwrap().contains("Permit2"), "{body}");

        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let cfg = crate::config::Config::from_toml(&toml).unwrap();
        assert_eq!(cfg.pools.len(), 2);
        assert!(
            cfg.pools[1]
                .feed_url
                .as_deref()
                .is_some_and(|u| u.contains("wbrl-usdt")),
            "new pool must stamp its own feed: {toml}"
        );
        assert!(
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn adding_a_pool_is_refused_when_the_image_does_not_declare_per_token_reservations() {
        let h = harness("settings-add-pool-old-binary");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        h.docker.clear_image_labels("sha256:id-stitch-bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("Update"), "{body}");
        assert_eq!(
            before,
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
            "a refused add must not write the second pool"
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "must not restart the old image: {:?}",
            h.docker.calls()
        );
    }

    /// The flip side of the pin: a container on an image that is nothing like
    /// the configured one is fine as long as it declares the feature. A
    /// locally built or side-loaded current binary is not a reason to refuse.
    #[tokio::test]
    async fn adding_a_pool_is_allowed_on_an_unrecognised_image_that_declares_the_feature() {
        let h = harness("settings-add-pool-local-build");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        h.docker
            .set_container_image("stitch-bot-a", "stitch:my-local-build");

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(
            crate::config::Config::from_toml(&toml).unwrap().pools.len(),
            2
        );
    }

    /// Production pins `STITCH_PANEL_BOT_IMAGE` to a `sha-*` tag, so the
    /// container can match the configured image exactly and still be an old
    /// per-corridor binary. Identity is not capability.
    #[tokio::test]
    async fn adding_a_pool_is_refused_on_a_pinned_pre_feature_image() {
        let h = harness("settings-add-pool-old-pin");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        // Exactly what the panel is configured to run, and locally identical.
        h.docker
            .set_container_image("stitch-bot-a", h.state.cfg.bot_image.as_str());
        h.docker.clear_image_labels("sha256:id-stitch-bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("Update"), "{body}");
        assert_eq!(
            before,
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap()
        );
    }

    /// A bot with no container would be created from the configured image, so
    /// that is the binary at stake. Its labels can only be read once it is on
    /// the host, and an unreachable registry is not permission to guess.
    #[tokio::test]
    async fn adding_a_pool_to_a_containerless_bot_is_refused_when_the_image_cannot_be_fetched() {
        let h = harness("settings-add-pool-no-registry");
        let corridor = setup::find_corridor("cngn-usdt-celo").unwrap();
        setup::write_config(h.root.join("bot-a"), corridor, TEST_KEY).unwrap();
        h.docker.fail_image("registry unreachable");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("registry"), "{body}");
        assert_eq!(
            before,
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap()
        );
    }

    #[tokio::test]
    async fn adding_a_pool_on_process_runtime_ignores_a_stale_container_tag() {
        let h = super::super::testkit::harness_process("settings-add-pool-process");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:old",
        );

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(
            crate::config::Config::from_toml(&toml).unwrap().pools.len(),
            2
        );
    }

    #[tokio::test]
    async fn adding_a_pool_on_another_chain_is_refused() {
        let h = harness("settings-add-pool-chain");
        seed(&h, "bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("chain"), "{body}");
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn adding_a_duplicate_pair_is_refused() {
        let h = harness("settings-add-pool-dup");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "cngn-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("already quotes"), "{body}");
    }

    #[tokio::test]
    async fn adding_the_same_pair_reversed_is_refused() {
        let h = harness("settings-add-pool-reversed");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Exited);
        let path = h.root.join("bot-a/stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap();
        let swapped = toml
            .replace(
                "collateral = \"0xF6829D7393dAe24509eb1E52eE8e572e2E271a4f\"",
                "collateral = \"0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e\"",
            )
            .replace(
                "debt = \"0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e\"",
                "debt = \"0xF6829D7393dAe24509eb1E52eE8e572e2E271a4f\"",
            );
        std::fs::write(&path, swapped).unwrap();
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "cngn-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("already quotes"), "{body}");
    }

    #[tokio::test]
    async fn removing_the_last_pool_is_refused() {
        let h = harness("settings-remove-last-pool");
        seed(&h, "bot-a");
        let (status, body) = delete_pool(&h, "bot-a", 0).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("at least one"), "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "a refused remove must not stop a running bot: {:?}",
            h.docker.calls()
        );
    }

    /// Remove restarts the same binary too, and an old responder ignores the
    /// `input_token` stamp the panel just wrote — so the removed pool's live
    /// claims would vanish from the corridors that remain.
    #[tokio::test]
    async fn removing_a_pool_is_refused_on_a_stale_bot_image() {
        let h = harness("settings-remove-pool-stale-image");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        h.docker.clear_image_labels("sha256:id-stitch-bot-a");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = delete_pool(&h, "bot-a", 1).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("Update"), "{body}");
        assert_eq!(
            before,
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap()
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "the image check must refuse before the stop: {:?}",
            h.docker.calls()
        );
    }

    /// The bot is already stopped by the time the config write runs, so a write
    /// failure owns putting it back — nothing was removed.
    #[tokio::test]
    async fn a_failed_write_starts_the_bot_back_up() {
        let h = harness("settings-remove-pool-write-fails");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Make the atomic write fail: `write_toml_atomic` writes a temp file
        // beside the config, so a read-only config dir stops it.
        let dir = h.root.join("bot-a");
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        let original = perms.clone();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o500);
        std::fs::set_permissions(&dir, perms).unwrap();

        let (status, body) = delete_pool(&h, "bot-a", 1).await;
        std::fs::set_permissions(&dir, original).unwrap();

        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let calls = h.docker.calls();
        let stop = calls
            .iter()
            .position(|c| matches!(c, Call::Stop { name, .. } if name == "stitch-bot-a"));
        let start = calls
            .iter()
            .position(|c| matches!(c, Call::Start(name) if name == "stitch-bot-a"));
        assert!(stop.is_some(), "it stopped the bot: {calls:?}");
        assert!(
            start.is_some_and(|s| s > stop.unwrap()),
            "a failed write must start it back: {calls:?}"
        );
        assert_eq!(
            crate::config::Config::from_toml(
                &std::fs::read_to_string(dir.join("stitch.toml")).unwrap()
            )
            .unwrap()
            .pools
            .len(),
            2,
            "nothing was removed"
        );
    }

    /// A save carries a pool index chosen against the list the client read. If
    /// someone else's remove renumbers it first, writing by index alone would
    /// put one corridor's spreads on another.
    #[tokio::test]
    async fn saving_a_pool_by_a_renumbered_index_is_refused() {
        let h = harness("settings-save-renumbered");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Exited);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let pools = crate::config::Config::from_toml(
            &std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
        )
        .unwrap()
        .pools
        .clone();

        // Naming pool 1 but sending pool 0's pair: the list moved under us.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({
                    "pool": 1,
                    "collateral": pools[0].collateral,
                    "debt": pools[0].debt,
                    "ttlSecs": 300,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("reload"), "{body}");

        // Multi-corridor writes have to say which pair they mean at all.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "pool": 1, "ttlSecs": 300 }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("which pair"), "{body}");

        // The honest write lands.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({
                    "pool": 1,
                    "collateral": pools[1].collateral,
                    "debt": pools[1].debt,
                    "ttlSecs": 300,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["settings"]["ttlSecs"], 300);
    }

    /// Two clients that both confirmed "remove pool 0" against a three-pool
    /// list must not remove two different pools. The second request's index has
    /// been renumbered under it by the first.
    #[tokio::test]
    async fn removing_a_pool_by_a_renumbered_index_is_refused() {
        let h = harness("settings-remove-pool-renumbered");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Exited);
        for corridor in ["wbrl-usdt-celo", "wars-usdt-celo"] {
            let (status, body) = h
                .post_json("/api/bots/bot-a/pools", json!({ "corridorId": corridor }))
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let path = h.root.join("bot-a/stitch.toml");
        let three = crate::config::Config::from_toml(&std::fs::read_to_string(&path).unwrap())
            .unwrap()
            .pools
            .clone();
        assert_eq!(three.len(), 3);

        // Both clients loaded the same list and picked pool 0. The first wins.
        let (status, body) = delete_pool(&h, "bot-a", 0).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The second replays its request: index 0, naming the pair it saw.
        let (status, body) = h
            .delete(&format!(
                "/api/bots/bot-a/pools/0?collateral={}&debt={}",
                three[0].collateral, three[0].debt
            ))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("reload"), "{body}");
        let left = crate::config::Config::from_toml(&std::fs::read_to_string(&path).unwrap())
            .unwrap()
            .pools
            .clone();
        assert_eq!(left.len(), 2, "only the first removal may land");
        assert!(
            left.iter().any(|p| p.collateral == three[1].collateral),
            "the pool that was renumbered to 0 must survive"
        );
    }

    #[tokio::test]
    async fn removing_a_pool_from_a_paused_bot_is_refused() {
        let h = harness("settings-remove-pool-paused");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Paused);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = delete_pool(&h, "bot-a", 1).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("paused"), "{body}");
        assert_eq!(
            before,
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
            "a paused bot's config must not change"
        );
        let calls = h.docker.calls();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Stop { .. })),
            "a paused container must not be stopped: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Start(_))),
            "and must never be started: {calls:?}"
        );
    }

    #[tokio::test]
    async fn removing_a_pool_rewrites_toml() {
        let h = harness("settings-remove-pool");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Exited);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = delete_pool(&h, "bot-a", 1).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["poolCount"], 1);
        assert_eq!(v["settings"]["pools"][0]["corridorId"], "cngn-usdt-celo");
        let toml = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(
            crate::config::Config::from_toml(&toml).unwrap().pools.len(),
            1
        );
    }

    #[tokio::test]
    async fn removing_a_pool_stops_a_running_bot_before_tagging() {
        let h = harness("settings-remove-pool-stop");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Running);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        h.docker
            .set_container_image("stitch-bot-a", h.state.cfg.bot_image.as_str());

        let path = h
            .root
            .join("bot-a")
            .join(crate::rfq::reserve::RESERVATIONS_FILE);
        let mut ledger = crate::rfq::reserve::Reservations::with_persist_path(&path);
        ledger.reserve(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            alloy_primitives::U256::from(400u64),
            4_000_000_000,
        );

        let (status, body) = delete_pool(&h, "bot-a", 0).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let calls = h.docker.calls();
        let stop = calls
            .iter()
            .position(|c| matches!(c, Call::Stop { name, .. } if name == "stitch-bot-a"));
        let start = calls
            .iter()
            .position(|c| matches!(c, Call::Start(name) if name == "stitch-bot-a"));
        assert!(stop.is_some(), "must stop before tagging: {calls:?}");
        assert!(start.is_some(), "must start after the write: {calls:?}");
        assert!(
            stop.unwrap() < start.unwrap(),
            "stop must precede start: {calls:?}"
        );
        let restored = crate::rfq::reserve::Reservations::load(&path, 0).unwrap();
        assert_eq!(
            restored.reserved_paying(
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                ["wbrl-usdt-celo"],
                true,
                0
            ),
            alloy_primitives::U256::from(400u64)
        );
    }

    #[tokio::test]
    async fn removing_a_pool_is_refused_when_a_tokenless_venue_slug_is_unmatched() {
        let h = harness("settings-remove-pool-unmatched");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Exited);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let path = h
            .root
            .join("bot-a")
            .join(crate::rfq::reserve::RESERVATIONS_FILE);
        let mut ledger = crate::rfq::reserve::Reservations::with_persist_path(&path);
        ledger.reserve(
            "rfq_venue",
            "venue-cngn-unknown",
            true,
            alloy_primitives::U256::from(400u64),
            4_000_000_000,
        );
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = delete_pool(&h, "bot-a", 0).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("tagged"), "{body}");
        assert_eq!(
            before,
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
            "an unmatched live claim must block the remove"
        );
    }

    #[tokio::test]
    async fn removing_a_pool_tags_tokenless_reservations() {
        let h = harness("settings-remove-pool-tag");
        seed_corridor_in_state(&h, "bot-a", "cngn-usdt-celo", ContainerState::Exited);
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let path = h
            .root
            .join("bot-a")
            .join(crate::rfq::reserve::RESERVATIONS_FILE);
        let mut ledger = crate::rfq::reserve::Reservations::with_persist_path(&path);
        ledger.reserve(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            alloy_primitives::U256::from(400u64),
            4_000_000_000,
        );

        let (status, body) = delete_pool(&h, "bot-a", 0).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let restored = crate::rfq::reserve::Reservations::load(&path, 0).unwrap();
        assert_eq!(
            restored.reserved_paying(
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                ["wbrl-usdt-celo"],
                true,
                0
            ),
            alloy_primitives::U256::from(400u64),
            "removing cNGN must stamp the leftover USDT claim so wBRL still sees it"
        );
    }
}
