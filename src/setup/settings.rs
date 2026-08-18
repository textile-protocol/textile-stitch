// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Read and edit the `stitch.toml` values the Settings surfaces expose: endpoints,
//! per-pool spreads, ladder sizing, order lifetime, the tick cadence, and the
//! experimental TWAP / inventory-lean knobs (optional).
//!
//! Shared by the desktop Settings screen and Stitch, so both edit
//! configs through one implementation. Edits go through `toml_edit` so the
//! template's comments and layout survive a save, and every edit is re-validated
//! through `Config::from_toml` before it is handed back — a bad value fails here,
//! so the caller never writes a broken file. Secrets are NOT here: the operator
//! wallet lives in `stitch.key` (`writer::write_key`); the RFQ maker key lives
//! in `rfq-api.key` (`writer::write_rfq_api_key`).
//!
//! Amounts stay strings end to end. They are atomic-unit `u128`/`U256` values, so
//! parsing them into a float to render or edit would lose precision on large
//! inventories; the view carries the token decimals instead and lets the caller
//! format.

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::{assert_rfq_stream_url, parse_liquidity_amount, parse_min_slice_debt, Config};

/// How a side's spread is expressed in the config. Editing preserves whichever
/// form the operator's config already uses rather than switching representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpreadKind {
    /// Basis points below/above the mid (`buy_offset_bps` / `sell_offset_bps`).
    #[default]
    Bps,
    /// Absolute soft-per-stable offset (`buy_offset_abs` / `sell_offset_abs`).
    Abs,
}

/// One side's spread as an editable value plus the representation it uses. `value`
/// is the number rendered as text (empty when the side has no spread configured).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpreadEdit {
    pub kind: SpreadKind,
    pub value: String,
}

/// How one side's ladder is sized. Every value is an atomic-unit integer rendered
/// as text, empty when the config doesn't set it.
///
/// The bot picks the ladder (`total_liquidity` + `min_slice_debt`) over the flat
/// `order_size` when both are present, so all three are surfaced rather than
/// collapsed — an operator editing sizing needs to see which one is actually in
/// effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideSizing {
    /// Total liquidity to quote as a balanced ladder. Accepts the literal `max`,
    /// meaning "quote everything funded".
    pub total_liquidity: String,
    /// Smallest ladder slice, in atomic debt units on both sides.
    pub min_slice_debt: String,
    /// Flat size per order, used only when the ladder pair isn't set.
    pub order_size: String,
    /// Cap on live slices for this side. Empty means the bot's own default.
    pub max_orders: String,
}

/// The token pair a pool trades, so a caller can format atomic amounts and label
/// the sizing fields with the right asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolPair {
    pub collateral: String,
    pub collateral_decimals: u8,
    pub debt: String,
    pub debt_decimals: u8,
}

/// The current editable settings, read from a `stitch.toml` for form prefill.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub rpc_url: String,
    pub feed_url: String,
    pub buy: SpreadEdit,
    pub sell: SpreadEdit,
    /// Whether the taker leg is on: fill users' resting limit orders when they
    /// cross the bot's own quote. The raw configured flag, not the spread-gated
    /// effective one, so a saved-but-spreadless value round-trips.
    pub taker_enabled: bool,
    /// Which pool the pool-scoped fields above were read from.
    pub pool_index: usize,
    /// How many pools the config has. A caller editing one pool warns when there
    /// is more than one.
    pub pool_count: usize,
    pub pair: PoolPair,
    pub buy_sizing: SideSizing,
    pub sell_sizing: SideSizing,
    /// Order lifetime for this pool, in seconds. Must exceed the live-order
    /// deadline margin (`LIVE_ORDER_DEADLINE_MARGIN_SECS`).
    pub ttl_secs: u64,
    /// Re-sign a side only when its price moves more than this (bps) since its
    /// last order. 0 re-quotes every tick.
    pub refresh_threshold_bps: u32,
    /// How often the bot re-quotes, in seconds. Bot-wide, not per pool.
    pub tick_interval_secs: u64,
    // ----- Experimental (TWAP + inventory lean). Empty strings mean "unset" —
    // omit the key so the bot uses its defaults / spot quoting. -----
    /// Rolling TWAP window in seconds. Empty = quote off the instantaneous feed.
    pub twap_window_secs: String,
    /// Spot-deviation guard in bps while TWAP is on. Empty = bot default (50).
    pub twap_max_deviation_bps: String,
    /// Quote the live book off inventory-lean prices.
    pub lean_enabled: bool,
    /// Log lean quotes next to the live ones; no behavior change.
    pub lean_shadow: bool,
    /// Measured p95 feed error vs live Pyth, in bps. Required when lean is on.
    pub lean_floor_bps: String,
    /// Balanced-zone half-spread in bps. Empty = bot default (1.0).
    pub lean_base_bps: String,
    /// Extra widening at the heavy inventory edge, in bps. Empty = bot default (3.0).
    pub lean_wide_bps: String,
    // ----- RFQ (beta). Optional on the patch: a spread-only save must not
    // touch them. The maker API key is not here — it lives in `rfq-api.key`.
    /// Whether `[rfq].enabled` is set. Bot-wide.
    pub rfq_enabled: bool,
    /// The venue stream URL from `[rfq]`, empty when the block is absent.
    pub rfq_url: String,
    /// Venue maker id (`X-Textile-Maker-Id`). Empty when `[rfq]` is absent.
    pub rfq_maker_id: String,
    /// PreferredFillerValidation address. Empty when `[rfq]` is absent.
    pub rfq_validation_contract: String,
    /// This pool's `rfq_corridor` slug. Empty when the pool is not on RFQ.
    pub rfq_corridor: String,
}

impl SettingsView {
    /// A patch that would write these values back unchanged. The starting point
    /// for a form: mutate the fields the operator edited and leave the rest.
    pub fn to_patch(&self) -> SettingsPatch {
        SettingsPatch {
            pool_index: self.pool_index,
            rpc_url: self.rpc_url.clone(),
            feed_url: self.feed_url.clone(),
            buy: self.buy.clone(),
            sell: self.sell.clone(),
            taker_enabled: self.taker_enabled,
            buy_sizing: Some(self.buy_sizing.clone()),
            sell_sizing: Some(self.sell_sizing.clone()),
            ttl_secs: Some(self.ttl_secs),
            refresh_threshold_bps: Some(self.refresh_threshold_bps),
            tick_interval_secs: Some(self.tick_interval_secs),
            twap_window_secs: Some(self.twap_window_secs.clone()),
            twap_max_deviation_bps: Some(self.twap_max_deviation_bps.clone()),
            lean_enabled: Some(self.lean_enabled),
            lean_shadow: Some(self.lean_shadow),
            lean_floor_bps: Some(self.lean_floor_bps.clone()),
            lean_base_bps: Some(self.lean_base_bps.clone()),
            lean_wide_bps: Some(self.lean_wide_bps.clone()),
            // RFQ is opt-in on the patch: a form that opened before the operator
            // touched RFQ must not rewrite the [rfq] block on save.
            rfq_enabled: None,
            rfq_url: None,
            rfq_maker_id: None,
            rfq_validation_contract: None,
            rfq_corridor: None,
        }
    }
}

/// The desired new state of the editable fields, applied onto the existing TOML
/// text. The wallet key is handled separately.
///
/// The optional fields distinguish "set this to that" from "don't touch it", so a
/// caller that only edits spreads can leave sizing alone without having to know
/// its current value.
#[derive(Debug, Clone, Default)]
pub struct SettingsPatch {
    /// Which pool the pool-scoped fields apply to. Defaults to the first.
    pub pool_index: usize,
    pub rpc_url: String,
    pub feed_url: String,
    pub buy: SpreadEdit,
    pub sell: SpreadEdit,
    /// Whether the taker leg should be on for this pool.
    pub taker_enabled: bool,
    pub buy_sizing: Option<SideSizing>,
    pub sell_sizing: Option<SideSizing>,
    pub ttl_secs: Option<u64>,
    pub refresh_threshold_bps: Option<u32>,
    pub tick_interval_secs: Option<u64>,
    pub twap_window_secs: Option<String>,
    pub twap_max_deviation_bps: Option<String>,
    pub lean_enabled: Option<bool>,
    pub lean_shadow: Option<bool>,
    pub lean_floor_bps: Option<String>,
    pub lean_base_bps: Option<String>,
    pub lean_wide_bps: Option<String>,
    pub rfq_enabled: Option<bool>,
    pub rfq_url: Option<String>,
    pub rfq_maker_id: Option<String>,
    pub rfq_validation_contract: Option<String>,
    pub rfq_corridor: Option<String>,
}

/// Read the first pool's editable values from a `stitch.toml` body.
pub fn read_settings(toml_str: &str) -> Result<SettingsView> {
    read_settings_at(toml_str, 0)
}

/// Read one pool's editable values from a `stitch.toml` body. Parses through the
/// real `Config` so an unreadable file surfaces the same error the bot would hit.
pub fn read_settings_at(toml_str: &str, pool_index: usize) -> Result<SettingsView> {
    let cfg = Config::from_toml(toml_str)?;
    let pool_count = cfg.pools.len();
    let pool = cfg.pools.get(pool_index).with_context(|| {
        if pool_count == 0 {
            "config has no [[pools]] entry".to_string()
        } else {
            format!("config has {pool_count} pools, so there is no pool {pool_index}")
        }
    })?;
    Ok(SettingsView {
        rpc_url: cfg.rpc_url.clone(),
        // The bot prefers a pool's feed_url override over [feed].url (see
        // main.rs), so surface the endpoint that's actually effective.
        feed_url: pool
            .feed_url
            .clone()
            .unwrap_or_else(|| cfg.feed.url.clone()),
        buy: spread_edit(pool.buy_offset_bps, pool.buy_offset_abs),
        sell: spread_edit(pool.sell_offset_bps, pool.sell_offset_abs),
        taker_enabled: pool.limit_taker_enabled.unwrap_or(false),
        pool_index,
        pool_count,
        pair: PoolPair {
            collateral: pool.collateral.clone(),
            collateral_decimals: pool.collateral_decimals,
            debt: pool.debt.clone(),
            debt_decimals: pool.debt_decimals,
        },
        buy_sizing: SideSizing {
            total_liquidity: opt_str(&pool.buy_total_liquidity_debt),
            min_slice_debt: opt_str(&pool.buy_min_slice_debt),
            order_size: opt_str(&pool.buy_order_size_debt),
            max_orders: opt_num(pool.buy_max_orders),
        },
        sell_sizing: SideSizing {
            total_liquidity: opt_str(&pool.sell_total_liquidity_collateral),
            // The sell floor is expressed in debt units too — the bot converts
            // each generated slice into collateral at the live ask price.
            min_slice_debt: opt_str(&pool.sell_min_slice_debt),
            order_size: opt_str(&pool.sell_order_size_collateral),
            max_orders: opt_num(pool.sell_max_orders),
        },
        ttl_secs: pool.ttl_secs,
        refresh_threshold_bps: pool.refresh_threshold_bps,
        tick_interval_secs: cfg.tick_interval_secs,
        twap_window_secs: opt_num_u64(pool.twap_window_secs),
        twap_max_deviation_bps: opt_num(pool.twap_max_deviation_bps),
        lean_enabled: pool.lean_enabled.unwrap_or(false),
        lean_shadow: pool.lean_shadow.unwrap_or(false),
        lean_floor_bps: opt_f64(pool.lean_floor_bps),
        lean_base_bps: opt_f64(pool.lean_base_bps),
        lean_wide_bps: opt_f64(pool.lean_wide_bps),
        rfq_enabled: cfg.rfq.as_ref().is_some_and(|r| r.enabled),
        rfq_url: cfg.rfq.as_ref().map(|r| r.url.clone()).unwrap_or_default(),
        rfq_maker_id: cfg
            .rfq
            .as_ref()
            .map(|r| r.maker_id.clone())
            .unwrap_or_default(),
        rfq_validation_contract: cfg
            .rfq
            .as_ref()
            .map(|r| r.validation_contract.clone())
            .unwrap_or_default(),
        rfq_corridor: pool.rfq_corridor.clone().unwrap_or_default(),
    })
}

fn opt_str(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn opt_num(v: Option<u32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn opt_num_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn opt_f64(v: Option<f64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

/// The current signer, read from a `stitch.toml` for form prefill. Only the
/// non-secret fields — secrets live in the env/secret file and are re-entered
/// when the operator changes the signer. No `[signer]` reads as the hot wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerView {
    Local,
    Turnkey {
        organization_id: String,
        sign_with: String,
        operator_address: String,
        api_base_url: String,
    },
    Mpcvault {
        vault_uuid: String,
        client_signer_pubkey: String,
        operator_address: String,
        api_base_url: String,
        callback_listen_addr: String,
    },
}

/// Read the current signer from a `stitch.toml` body.
///
/// Reads the `[signer]` table on its own, deliberately *not* through
/// [`Config::from_toml`]. That validates every field, so keying the signer off it
/// meant one bad `rpc_url` — or any unrelated typo anywhere in the file — reported a
/// Turnkey bot as a hot wallet. Downstream that isn't cosmetic: the compose export
/// would mount `stitch.key` and set `STITCH_PRIVATE_KEY_FILE` for a bot whose secret
/// is `turnkey-api.key`, so an operator who restored that file and then fixed the
/// TOML would still have a service that can't start.
///
/// `Ok(Local)` means the config really does select the hot wallet: no `[signer]`
/// table, or `provider = "local"`. `Err` means the body isn't TOML at all, or its
/// `[signer]` table is malformed — the signer is genuinely unknown, and a caller
/// about to bake it into a file or a container has to say so rather than guess.
pub fn try_read_signer(toml_str: &str) -> Result<SignerView> {
    use crate::signer::SignerConfig;
    let doc: toml::Value = toml::from_str(toml_str).context("parsing stitch.toml")?;
    let Some(table) = doc.get("signer") else {
        return Ok(SignerView::Local);
    };
    let signer: SignerConfig = table
        .clone()
        .try_into()
        .context("parsing the [signer] table")?;
    Ok(match signer {
        SignerConfig::Turnkey(c) => SignerView::Turnkey {
            organization_id: c.organization_id,
            sign_with: c.sign_with,
            operator_address: c.operator_address,
            api_base_url: c.api_base_url,
        },
        SignerConfig::Mpcvault(c) => SignerView::Mpcvault {
            vault_uuid: c.vault_uuid,
            client_signer_pubkey: c.client_signer_pubkey,
            operator_address: c.operator_address,
            api_base_url: c.api_base_url,
            callback_listen_addr: c.callback_listen_addr,
        },
        SignerConfig::Local => SignerView::Local,
    })
}

/// As [`try_read_signer`], reading an unparseable config as the hot wallet.
///
/// Only for showing an operator a form they are about to correct — the desktop app
/// loads whatever is on disk, however broken, so the signer picker has to render
/// something. Anything that writes the answer somewhere durable uses
/// [`try_read_signer`] and fails loudly instead.
pub fn read_signer(toml_str: &str) -> SignerView {
    try_read_signer(toml_str).unwrap_or(SignerView::Local)
}

/// Apply the patch onto `toml_str` and return the new TOML text. Preserves
/// comments/formatting, and re-validates the result before returning so a bad
/// edit fails here instead of on the next bot start.
pub fn apply_settings(toml_str: &str, patch: &SettingsPatch) -> Result<String> {
    // An empty endpoint still parses as a valid `Config` (both are plain
    // `String`s), so guard here or a cleared field would silently restart the bot
    // into a config that can't reach its RPC or feed.
    require_url(&patch.rpc_url, "RPC URL")?;
    require_url(&patch.feed_url, "price feed URL")?;

    let mut doc = toml_str
        .parse::<DocumentMut>()
        .context("parsing stitch.toml")?;

    set_value(
        doc.as_table_mut(),
        "rpc_url",
        Value::from(patch.rpc_url.trim()),
    );

    if let Some(secs) = patch.tick_interval_secs {
        anyhow::ensure!(secs > 0, "the tick interval must be at least 1 second");
        set_value(
            doc.as_table_mut(),
            "tick_interval_secs",
            Value::from(i64::try_from(secs).context("tick interval is too large")?),
        );
    }

    let index = patch.pool_index;

    write_feed_url(&mut doc, index, patch.feed_url.trim())?;

    let pool = pool_mut(&mut doc, index)?;
    apply_spread(pool, "buy", &patch.buy)?;
    apply_spread(pool, "sell", &patch.sell)?;
    apply_taker(pool, patch.taker_enabled);
    if let Some(sizing) = &patch.buy_sizing {
        apply_sizing(pool, Side::Buy, sizing)?;
    }
    if let Some(sizing) = &patch.sell_sizing {
        apply_sizing(pool, Side::Sell, sizing)?;
    }
    if let Some(secs) = patch.ttl_secs {
        // Restate the loader rule so a bad TTL fails with an operator-facing
        // message before we rewrite the file and hit Config::from_toml.
        anyhow::ensure!(
            secs > crate::config::LIVE_ORDER_DEADLINE_MARGIN_SECS,
            "order lifetime (ttl_secs) must be greater than {} seconds — shorter \
             orders are accepted on-chain but never served as fillable depth",
            crate::config::LIVE_ORDER_DEADLINE_MARGIN_SECS
        );
        set_value(
            pool,
            "ttl_secs",
            Value::from(i64::try_from(secs).context("order lifetime is too large")?),
        );
    }
    if let Some(bps) = patch.refresh_threshold_bps {
        set_value(pool, "refresh_threshold_bps", Value::from(i64::from(bps)));
    }
    if let Some(raw) = &patch.twap_window_secs {
        apply_optional_u64(pool, "twap_window_secs", raw, "TWAP window")?;
    }
    if let Some(raw) = &patch.twap_max_deviation_bps {
        apply_optional_u32(pool, "twap_max_deviation_bps", raw, "TWAP max deviation")?;
    }
    if let Some(enabled) = patch.lean_enabled {
        apply_bool_flag(pool, "lean_enabled", enabled);
    }
    if let Some(shadow) = patch.lean_shadow {
        apply_bool_flag(pool, "lean_shadow", shadow);
    }
    if let Some(raw) = &patch.lean_floor_bps {
        apply_optional_f64(pool, "lean_floor_bps", raw, "lean floor")?;
    }
    if let Some(raw) = &patch.lean_base_bps {
        apply_optional_f64(pool, "lean_base_bps", raw, "lean base")?;
    }
    if let Some(raw) = &patch.lean_wide_bps {
        apply_optional_f64(pool, "lean_wide_bps", raw, "lean wide")?;
    }
    apply_rfq(&mut doc, patch)?;

    let edited = doc.to_string();
    // Guard: never hand back something the bot can't load. This is also what
    // enforces the cross-field rules — TTL above the live-order deadline margin,
    // ladder caps, positive slices, TWAP/lean constraints — so they don't need
    // restating here.
    Config::from_toml(&edited).context("the edited config is not valid")?;
    Ok(edited)
}

/// Write the feed URL where the bot will actually read it for *this* pool, and
/// nowhere else.
///
/// The bot resolves a pool's feed as `pool.feed_url.unwrap_or(feed.url)`, so
/// there are three cases:
///
/// - The pool already overrides it: write the override. Touching `[feed].url`
///   would look effective and be ignored on restart.
/// - One pool, no override: write `[feed].url`. That's the shared value and the
///   only pool reading it, so an override would be noise.
/// - Several pools, no override: give this pool its own override. The settings
///   page is pool-scoped, and writing `[feed].url` would silently repoint every
///   other pool without an override — pools that can be quoting different pairs
///   entirely.
///
/// The last case only kicks in when the value actually changes. Every save sends
/// the whole form, so writing unconditionally would sprinkle overrides onto pools
/// the operator never edited and quietly cut them off from the shared fallback.
fn write_feed_url(doc: &mut DocumentMut, index: usize, url: &str) -> Result<()> {
    let pools = doc.get("pools").and_then(Item::as_array_of_tables);
    let overrides_feed = pools
        .and_then(|arr| arr.get(index))
        .is_some_and(|p| p.contains_key("feed_url"));
    let several_pools = pools.is_some_and(|arr| arr.len() > 1);
    let shared = doc
        .get("feed")
        .and_then(|f| f.get("url"))
        .and_then(Item::as_str)
        .unwrap_or_default();

    if overrides_feed || (several_pools && url != shared) {
        let pool = pool_mut(doc, index)?;
        set_value(pool, "feed_url", Value::from(url));
        return Ok(());
    }

    let feed = doc
        .get_mut("feed")
        .and_then(Item::as_table_mut)
        .context("config has no [feed] table")?;
    set_value(feed, "url", Value::from(url));
    Ok(())
}

/// Which side of the book a sizing edit applies to. The two sides use different
/// key names for the same concept, so this carries the mapping in one place.
#[derive(Debug, Clone, Copy)]
enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Key holding total ladder liquidity. Sized in debt on the buy side and in
    /// collateral on the sell side, hence the differing suffixes.
    fn total_liquidity_key(self) -> &'static str {
        match self {
            Side::Buy => "buy_total_liquidity_debt",
            Side::Sell => "sell_total_liquidity_collateral",
        }
    }

    /// Key holding the flat per-order size.
    fn order_size_key(self) -> &'static str {
        match self {
            Side::Buy => "buy_order_size_debt",
            Side::Sell => "sell_order_size_collateral",
        }
    }

    /// Key holding the smallest slice. Both sides express this in debt units.
    fn min_slice_key(self) -> &'static str {
        match self {
            Side::Buy => "buy_min_slice_debt",
            Side::Sell => "sell_min_slice_debt",
        }
    }

    fn max_orders_key(self) -> &'static str {
        match self {
            Side::Buy => "buy_max_orders",
            Side::Sell => "sell_max_orders",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

/// Write one side's sizing back onto the pool. Amounts stay strings in the TOML —
/// they're atomic `U256`/`u128` values that would lose precision as TOML integers
/// beyond 2^63, and the shipped templates already quote them.
///
/// Each field is validated with the same parser the bot loads it through, so an
/// operator gets the failure at save time and not on the next start.
fn apply_sizing(pool: &mut Table, side: Side, sizing: &SideSizing) -> Result<()> {
    let total = sizing.total_liquidity.trim();
    if total.is_empty() {
        pool.remove(side.total_liquidity_key());
    } else {
        parse_liquidity_amount(total, side.total_liquidity_key())?;
        set_value(pool, side.total_liquidity_key(), Value::from(total));
    }

    let min_slice = sizing.min_slice_debt.trim();
    if min_slice.is_empty() {
        pool.remove(side.min_slice_key());
    } else {
        parse_min_slice_debt(min_slice, side.min_slice_key())?;
        set_value(pool, side.min_slice_key(), Value::from(min_slice));
    }

    let order_size = sizing.order_size.trim();
    if order_size.is_empty() {
        pool.remove(side.order_size_key());
    } else {
        order_size
            .parse::<alloy_primitives::U256>()
            .with_context(|| {
                format!(
                    "{} order size must be a whole number of atomic units",
                    side.label()
                )
            })?;
        set_value(pool, side.order_size_key(), Value::from(order_size));
    }

    let max_orders = sizing.max_orders.trim();
    if max_orders.is_empty() {
        // Removing it falls back to the bot's own ladder cap rather than pinning
        // an explicit number the operator didn't choose.
        pool.remove(side.max_orders_key());
    } else {
        let n: u32 = max_orders
            .parse()
            .with_context(|| format!("{} max orders must be a whole number", side.label()))?;
        anyhow::ensure!(n > 0, "{} max orders must be at least 1", side.label());
        set_value(pool, side.max_orders_key(), Value::from(i64::from(n)));
    }
    Ok(())
}

/// One `[[pools]]` table, mutably. Errors if the index is out of range, naming the
/// count so the caller can say something useful.
fn pool_mut(doc: &mut DocumentMut, index: usize) -> Result<&mut Table> {
    let pools = doc
        .get_mut("pools")
        .and_then(Item::as_array_of_tables_mut)
        .context("config has no [[pools]] entry")?;
    let count = pools.len();
    pools
        .get_mut(index)
        .with_context(|| format!("config has {count} pools, so there is no pool {index}"))
}

/// Reject an endpoint that would leave the bot unable to reach its RPC or feed.
/// Both are used through reqwest's HTTP client, so fully parse the value and
/// require an http(s) scheme with a host — a bare `https://`, a `ws://`, or other
/// non-URL text would otherwise pass and fail every request after restart.
/// Write the `[rfq]` block and this pool's `rfq_corridor` when the patch names
/// them. A spread-only save leaves both alone — same rule as TWAP / lean.
fn apply_rfq(doc: &mut DocumentMut, patch: &SettingsPatch) -> Result<()> {
    let touching_block = patch.rfq_enabled.is_some()
        || patch.rfq_url.is_some()
        || patch.rfq_maker_id.is_some()
        || patch.rfq_validation_contract.is_some();
    if touching_block {
        let has_table = doc.get("rfq").and_then(Item::as_table).is_some();
        let only_disable = patch.rfq_enabled == Some(false)
            && patch.rfq_url.is_none()
            && patch.rfq_maker_id.is_none()
            && patch.rfq_validation_contract.is_none();
        if !(only_disable && !has_table) {
            let table = rfq_table_mut(doc);
            if let Some(enabled) = patch.rfq_enabled {
                set_value(table, "enabled", Value::from(enabled));
            }
            if let Some(url) = &patch.rfq_url {
                require_ws_url(url, "RFQ stream URL")?;
                set_value(table, "url", Value::from(url.trim()));
            }
            if let Some(maker_id) = &patch.rfq_maker_id {
                set_value(table, "maker_id", Value::from(maker_id.trim()));
            }
            if let Some(addr) = &patch.rfq_validation_contract {
                let addr = addr.trim();
                if !addr.is_empty() {
                    addr.parse::<alloy_primitives::Address>()
                        .context("RFQ validation contract is not a valid address")?;
                }
                set_value(table, "validation_contract", Value::from(addr));
            }
        }
    }
    if let Some(slug) = &patch.rfq_corridor {
        let pool = pool_mut(doc, patch.pool_index)?;
        let slug = slug.trim();
        if slug.is_empty() {
            pool.remove("rfq_corridor");
        } else {
            set_value(pool, "rfq_corridor", Value::from(slug));
        }
    }
    Ok(())
}

fn rfq_table_mut(doc: &mut DocumentMut) -> &mut Table {
    if doc.get("rfq").and_then(Item::as_table).is_none() {
        let mut table = Table::new();
        table.set_implicit(false);
        doc.insert("rfq", Item::Table(table));
    }
    doc.get_mut("rfq")
        .and_then(Item::as_table_mut)
        .expect("just inserted [rfq]")
}

/// The venue stream is a WebSocket, not HTTP — `require_url` would reject it.
fn require_ws_url(value: &str, field: &str) -> Result<()> {
    let v = value.trim();
    anyhow::ensure!(!v.is_empty(), "{field} can't be empty");
    let parsed = url::Url::parse(v)
        .with_context(|| format!("{field} must be a valid WebSocket URL (like wss://…)"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "ws" | "wss"),
        "{field} must be a ws(s):// URL (like wss://api.textilecredit.com/v2/maker/stream)"
    );
    anyhow::ensure!(
        parsed.host_str().is_some_and(|h| !h.is_empty()),
        "{field} must include a host"
    );
    assert_rfq_stream_url(v).with_context(|| format!("{field} rejected"))?;
    Ok(())
}

fn require_url(value: &str, field: &str) -> Result<()> {
    let v = value.trim();
    anyhow::ensure!(!v.is_empty(), "{field} can't be empty");
    let parsed = url::Url::parse(v)
        .with_context(|| format!("{field} must be a valid URL (like https://…)"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "{field} must be an http(s) URL (like https://…)"
    );
    anyhow::ensure!(
        parsed.host_str().is_some_and(|h| !h.is_empty()),
        "{field} must include a host (like https://api.example.com)"
    );
    Ok(())
}

/// Turn the two optional spread fields into an editable value + its kind.
fn spread_edit(bps: Option<u32>, abs: Option<f64>) -> SpreadEdit {
    match (bps, abs) {
        (Some(b), _) => SpreadEdit {
            kind: SpreadKind::Bps,
            value: b.to_string(),
        },
        (None, Some(a)) => SpreadEdit {
            kind: SpreadKind::Abs,
            value: a.to_string(),
        },
        (None, None) => SpreadEdit::default(),
    }
}

/// Write one side's spread back into the pool table, keeping the config's chosen
/// representation and removing the other form so the two can't disagree. An empty
/// value removes both offset keys, disabling that side (so the file always matches
/// what the field shows).
fn apply_spread(pool: &mut Table, side: &str, edit: &SpreadEdit) -> Result<()> {
    let raw = edit.value.trim();
    let bps_key = format!("{side}_offset_bps");
    let abs_key = format!("{side}_offset_abs");
    if raw.is_empty() {
        // Clearing a prefilled field disables the side: remove both offset forms
        // so the file matches the UI, rather than leaving the old spread in place
        // and reporting a save that didn't change anything.
        pool.remove(&bps_key);
        pool.remove(&abs_key);
        return Ok(());
    }
    match edit.kind {
        SpreadKind::Bps => {
            let n: u32 = raw
                .parse()
                .with_context(|| format!("{side} spread must be a whole number of basis points"))?;
            set_value(pool, &bps_key, Value::from(i64::from(n)));
            pool.remove(&abs_key);
        }
        SpreadKind::Abs => {
            let n: f64 = raw
                .parse()
                .with_context(|| format!("{side} spread must be a number"))?;
            // A negative (or non-finite) absolute offset crosses the book: the
            // bid would price above mid and the ask below it.
            anyhow::ensure!(
                n.is_finite() && n >= 0.0,
                "{side} spread must be a non-negative number"
            );
            set_value(pool, &abs_key, Value::from(n));
            pool.remove(&bps_key);
        }
    }
    Ok(())
}

/// Write the taker leg's on/off flag onto the pool. Mirrors the spread logic:
/// enabling sets `limit_taker_enabled = true`; disabling removes the key so the
/// file falls back to the opt-in default (off) rather than carrying an explicit
/// `false`, keeping a taker-off config byte-identical to a template that never
/// mentioned the leg.
fn apply_taker(pool: &mut Table, enabled: bool) {
    apply_bool_flag(pool, "limit_taker_enabled", enabled);
}

/// Opt-in boolean flag: write `true`, or remove the key when off so a template
/// that never mentioned the flag stays byte-identical after a round-trip.
fn apply_bool_flag(pool: &mut Table, key: &str, enabled: bool) {
    if enabled {
        set_value(pool, key, Value::from(true));
    } else {
        pool.remove(key);
    }
}

/// Optional integer key. Empty clears it (bot default / unset); non-empty must
/// parse as a positive whole number — the real loader re-checks cross-field rules.
fn apply_optional_u64(pool: &mut Table, key: &str, raw: &str, label: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        pool.remove(key);
        return Ok(());
    }
    let n: u64 = raw
        .parse()
        .with_context(|| format!("{label} must be a whole number of seconds"))?;
    anyhow::ensure!(n > 0, "{label} must be positive");
    set_value(
        pool,
        key,
        Value::from(i64::try_from(n).context(format!("{label} is too large"))?),
    );
    Ok(())
}

fn apply_optional_u32(pool: &mut Table, key: &str, raw: &str, label: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        pool.remove(key);
        return Ok(());
    }
    let n: u32 = raw
        .parse()
        .with_context(|| format!("{label} must be a whole number of basis points"))?;
    anyhow::ensure!(n > 0, "{label} must be positive");
    set_value(pool, key, Value::from(i64::from(n)));
    Ok(())
}

/// Optional floating bps value. Empty clears the key; non-empty must be finite
/// and non-negative. Cross-field rules (lean needs a positive floor, etc.) stay
/// with `Config::from_toml`.
fn apply_optional_f64(pool: &mut Table, key: &str, raw: &str, label: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        pool.remove(key);
        return Ok(());
    }
    let n: f64 = raw
        .parse()
        .with_context(|| format!("{label} must be a number of basis points"))?;
    anyhow::ensure!(
        n.is_finite() && n >= 0.0,
        "{label} must be a non-negative number of basis points"
    );
    set_value(pool, key, Value::from(n));
    Ok(())
}

/// Set a key's value while preserving its existing decor (the surrounding
/// whitespace and any inline `# comment`). Inserts a fresh key when it's absent.
fn set_value(table: &mut Table, key: &str, new: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        let mut next = new;
        *next.decor_mut() = existing.decor().clone();
        *existing = next;
    } else {
        table.insert(key, Item::Value(new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!("templates/cngn-usdt-bsc.toml");

    fn patch_from(view: &SettingsView) -> SettingsPatch {
        view.to_patch()
    }

    const TURNKEY_SIGNER: &str = "\n[signer]\nprovider = \"turnkey\"\n\
         organization_id = \"org-1\"\n\
         sign_with = \"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"\n\
         operator_address = \"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"\n";

    #[test]
    fn the_signer_survives_an_invalid_field_elsewhere_in_the_config() {
        // Reading it through `Config::from_toml` meant any unrelated typo reported a
        // Turnkey bot as a hot wallet, and callers then mounted `stitch.key` and set
        // STITCH_PRIVATE_KEY_FILE for a bot whose secret is `turnkey-api.key`. The
        // signer table stands alone, so it's read alone.
        let broken = format!(
            "{}{TURNKEY_SIGNER}",
            TEMPLATE.replace("tick_interval_secs = 5", "tick_interval_secs = \"soon\"")
        );
        assert!(
            Config::from_toml(&broken).is_err(),
            "the premise: the loader rejects this config"
        );
        assert!(matches!(
            try_read_signer(&broken).unwrap(),
            SignerView::Turnkey { .. }
        ));
    }

    #[test]
    fn a_config_with_no_signer_table_really_is_the_hot_wallet() {
        // The legitimate Local case, which has to stay distinguishable from "couldn't
        // tell" now that the latter is an error.
        assert!(matches!(
            try_read_signer(TEMPLATE).unwrap(),
            SignerView::Local
        ));
        assert!(matches!(
            try_read_signer("[signer]\nprovider = \"local\"\n").unwrap(),
            SignerView::Local
        ));
    }

    #[test]
    fn an_unreadable_signer_is_an_error_not_a_guess() {
        // Unparseable TOML, and a signer table that names a provider without its
        // required fields. Both mean "unknown", and a caller about to write mounts and
        // env into a file has to be told rather than handed the hot wallet.
        assert!(try_read_signer("this is not [ toml").is_err());
        assert!(try_read_signer("[signer]\nprovider = \"turnkey\"\n").is_err());
        // The lenient wrapper is still lenient, for the desktop form.
        assert!(matches!(
            read_signer("this is not [ toml"),
            SignerView::Local
        ));
    }

    /// A config with two pools, for the pool-indexing tests.
    fn two_pool_config() -> String {
        let pool_start = TEMPLATE.find("[[pools]]").unwrap();
        // The second pool differs so the tests can tell them apart.
        let second = TEMPLATE[pool_start..].replace("buy_offset_bps = 1", "buy_offset_bps = 25");
        format!("{TEMPLATE}\n{second}")
    }

    #[test]
    fn reads_current_values_from_a_template() {
        let v = read_settings(TEMPLATE).unwrap();
        assert!(v.rpc_url.starts_with("https://bsc-dataseed.binance.org"));
        assert_eq!(
            v.feed_url,
            "https://api.textilecredit.com/price?chainId=56&pair=cngn-usdt"
        );
        assert_eq!(
            v.buy,
            SpreadEdit {
                kind: SpreadKind::Bps,
                value: "1".into()
            }
        );
        assert_eq!(
            v.sell,
            SpreadEdit {
                kind: SpreadKind::Bps,
                value: "1".into()
            }
        );
        assert_eq!(v.pool_count, 1);
    }

    #[test]
    fn an_unset_optional_field_leaves_the_file_alone() {
        // The contract the desktop Settings screen relies on: a caller that only edits
        // spreads leaves sizing, lifetime and cadence unset, and those keep whatever is
        // in the file. Building the patch from a view captured when a screen *opened*
        // instead writes stale values back over anything that changed since — which,
        // now that Stitch edits the same stitch.toml, is a live concern.
        let before = read_settings(TEMPLATE).unwrap();
        // Something else edits the file: a longer order lifetime and a slower tick.
        let externally_changed = TEMPLATE
            .replace("ttl_secs = 120", "ttl_secs = 600")
            .replace("tick_interval_secs = 5", "tick_interval_secs = 30");

        // A spread-only save, with the optional fields left unset.
        let patch = SettingsPatch {
            pool_index: 0,
            rpc_url: before.rpc_url.clone(),
            feed_url: before.feed_url.clone(),
            buy: before.buy.clone(),
            sell: before.sell.clone(),
            taker_enabled: true,
            ..SettingsPatch::default()
        };
        let after = read_settings(&apply_settings(&externally_changed, &patch).unwrap()).unwrap();
        assert_eq!(after.ttl_secs, 600, "the external lifetime must survive");
        assert_eq!(
            after.tick_interval_secs, 30,
            "the external cadence must survive"
        );
        assert!(after.taker_enabled, "and the edit still lands");

        // For contrast: the same save built from the stale view reverts both, which is
        // the bug this guards.
        let stale = before.to_patch();
        let reverted =
            read_settings(&apply_settings(&externally_changed, &stale).unwrap()).unwrap();
        assert_eq!(reverted.ttl_secs, 120);
        assert_eq!(reverted.tick_interval_secs, 5);
    }

    #[test]
    fn a_noop_patch_keeps_the_file_byte_identical() {
        let view = read_settings(TEMPLATE).unwrap();
        let out = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();
        assert_eq!(
            out, TEMPLATE,
            "re-writing current values must not perturb the file"
        );
    }

    #[test]
    fn edits_all_four_fields_and_preserves_comments() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "https://rpc.example.com/key".into();
        view.feed_url = "https://feed.example.com/price".into();
        view.buy.value = "7".into();
        view.sell.value = "9".into();
        let out = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();

        let back = read_settings(&out).unwrap();
        assert_eq!(back.rpc_url, "https://rpc.example.com/key");
        assert_eq!(back.feed_url, "https://feed.example.com/price");
        assert_eq!(back.buy.value, "7");
        assert_eq!(back.sell.value, "9");

        // A block comment far from any edited line survives.
        assert!(out.contains("# Textile's own price endpoint."));
        // The inline comment on the edited spread line survives too.
        assert!(out.contains("# 1 bps below mid"));
        // Untouched keys are still there.
        assert!(out.contains("permit2"));
        assert!(out.contains("refresh_threshold_bps"));
    }

    #[test]
    fn writes_a_side_back_as_abs_when_the_source_used_abs() {
        // Start from a config whose buy side uses the absolute form.
        let src = TEMPLATE.replace(
            "buy_offset_bps = 1                              # 1 bps below mid",
            "buy_offset_abs = 0.0000015",
        );
        let mut view = read_settings(&src).unwrap();
        assert_eq!(view.buy.kind, SpreadKind::Abs);
        view.buy.value = "0.0000025".into();
        let out = apply_settings(&src, &patch_from(&view)).unwrap();
        assert!(out.contains("buy_offset_abs = 0.0000025"));
        assert!(!out.contains("buy_offset_bps"));
    }

    #[test]
    fn a_blank_or_non_http_endpoint_is_rejected() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "   ".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("RPC URL"));

        // A non-http scheme parses but is rejected by the scheme check.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "ws://node.example.com".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("http(s)"));

        // A scheme with no host (a common typo) is rejected too.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "https://".into();
        assert!(apply_settings(TEMPLATE, &patch_from(&view)).is_err());

        let mut view = read_settings(TEMPLATE).unwrap();
        view.feed_url = "not-a-url".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("feed"));
    }

    #[test]
    fn feed_edit_follows_the_first_pools_override_when_present() {
        // A custom config where the first pool overrides the feed; the bot reads
        // this, not [feed].url.
        let src = TEMPLATE.replace(
            "collateral_decimals = 6",
            "collateral_decimals = 6\nfeed_url = \"https://pool-feed.example.com/old\"",
        );
        // read_settings surfaces the effective (override) endpoint.
        let mut view = read_settings(&src).unwrap();
        assert_eq!(view.feed_url, "https://pool-feed.example.com/old");

        // Saving writes back to the override, leaving [feed].url untouched.
        view.feed_url = "https://pool-feed.example.com/new".into();
        let out = apply_settings(&src, &patch_from(&view)).unwrap();
        assert!(out.contains("feed_url = \"https://pool-feed.example.com/new\""));
        assert!(out.contains(
            "url            = \"https://api.textilecredit.com/price?chainId=56&pair=cngn-usdt\""
        ));
    }

    #[test]
    fn a_feed_edit_on_one_pool_leaves_the_others_on_the_old_feed() {
        // The settings page is pool-scoped. Writing the shared [feed].url would
        // repoint every other pool without an override — and a second pool can be
        // quoting a different pair entirely, so it would start pricing off the
        // wrong feed without anyone touching it.
        let src = two_pool_config();
        let mut view = read_settings_at(&src, 1).unwrap();
        view.feed_url = "https://feed.example.com/second".into();
        let out = apply_settings(&src, &patch_from(&view)).unwrap();

        assert_eq!(
            read_settings_at(&out, 1).unwrap().feed_url,
            "https://feed.example.com/second"
        );
        assert_eq!(
            read_settings_at(&out, 0).unwrap().feed_url,
            read_settings_at(&src, 0).unwrap().feed_url,
            "the untouched pool must keep the feed it had"
        );
    }

    #[test]
    fn saving_a_pool_without_touching_the_feed_adds_no_override() {
        // Every save sends the whole form. Writing an override each time would
        // sprinkle them onto pools nobody edited and cut them off from the shared
        // [feed].url for good.
        let src = two_pool_config();
        let mut view = read_settings_at(&src, 1).unwrap();
        view.buy.value = "7".into();
        let out = apply_settings(&src, &patch_from(&view)).unwrap();
        assert!(!out.contains("feed_url"), "{out}");

        // A single-pool config keeps writing the shared value: it's the only
        // reader, so an override would be noise.
        let mut only = read_settings(TEMPLATE).unwrap();
        only.feed_url = "https://feed.example.com/solo".into();
        let out = apply_settings(TEMPLATE, &patch_from(&only)).unwrap();
        assert!(!out.contains("feed_url"), "{out}");
        assert_eq!(
            read_settings(&out).unwrap().feed_url,
            "https://feed.example.com/solo"
        );
    }

    #[test]
    fn clearing_a_prefilled_spread_removes_it_rather_than_leaving_it_stale() {
        let mut view = read_settings(TEMPLATE).unwrap();
        assert_eq!(view.buy.value, "1"); // template preloads a buy spread
        view.buy.value = "   ".into(); // operator clears it
        let out = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();
        assert!(!out.contains("buy_offset_bps"));
        assert!(!out.contains("buy_offset_abs"));
        // The sell side and the rest of the config are untouched and still valid.
        assert!(out.contains("sell_offset_bps"));
        let back = read_settings(&out).unwrap();
        assert_eq!(back.buy, SpreadEdit::default());
    }

    #[test]
    fn a_negative_absolute_spread_is_rejected() {
        // Start from a config whose sell side uses the absolute form.
        let src = TEMPLATE.replace(
            "sell_offset_bps = 1                             # 1 bps above mid",
            "sell_offset_abs = 0.0000015",
        );
        let mut view = read_settings(&src).unwrap();
        assert_eq!(view.sell.kind, SpreadKind::Abs);
        view.sell.value = "-0.0000015".into();
        let err = apply_settings(&src, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("non-negative"));
    }

    #[test]
    fn taker_defaults_off_and_toggling_it_round_trips() {
        let mut view = read_settings(TEMPLATE).unwrap();
        // The shipped template doesn't opt into the taker leg.
        assert!(!view.taker_enabled);

        // Enabling writes the flag onto the first pool.
        view.taker_enabled = true;
        let on = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();
        assert!(on.contains("limit_taker_enabled = true"));
        assert!(read_settings(&on).unwrap().taker_enabled);

        // Disabling again removes the key rather than writing `false`, so the file
        // returns byte-for-byte to the taker-off template.
        let mut back = read_settings(&on).unwrap();
        back.taker_enabled = false;
        let off = apply_settings(&on, &patch_from(&back)).unwrap();
        assert!(!off.contains("limit_taker_enabled"));
        assert_eq!(off, TEMPLATE, "toggling off restores the original file");
    }

    #[test]
    fn a_non_numeric_spread_is_rejected_before_returning() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy.value = "wide".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("basis points"));
    }

    #[test]
    fn an_edit_that_would_break_the_config_errors_and_returns_nothing_usable() {
        // An empty RPC URL still parses as a string, so force an invalid value a
        // different way: a spread that overflows u32 fails to parse as bps.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy.value = "99999999999".into();
        assert!(apply_settings(TEMPLATE, &patch_from(&view)).is_err());
    }

    #[test]
    fn a_view_round_trips_the_pair_lifetime_and_cadence() {
        let v = read_settings(TEMPLATE).unwrap();
        assert_eq!(v.pool_index, 0);
        assert_eq!(v.pair.debt_decimals, 18, "USDT on BSC is 18 decimals");
        assert!(v.ttl_secs > 0);
        assert!(v.tick_interval_secs > 0);
        // A no-op save of everything, sizing included, still leaves the file alone.
        let out = apply_settings(TEMPLATE, &v.to_patch()).unwrap();
        assert_eq!(
            out, TEMPLATE,
            "a full no-op patch must not perturb the file"
        );
    }

    #[test]
    fn ladder_sizing_round_trips_as_quoted_atomic_strings() {
        let mut view = read_settings(TEMPLATE).unwrap();
        let mut sizing = view.buy_sizing.clone();
        // A value well past 2^63, which would be corrupted if written as a TOML
        // integer instead of a string.
        sizing.total_liquidity = "123456789012345678901234567890".into();
        sizing.min_slice_debt = "10000000".into();
        sizing.max_orders = "12".into();
        view.buy_sizing = sizing;

        let out = apply_settings(TEMPLATE, &view.to_patch()).unwrap();
        let back = read_settings(&out).unwrap();
        assert_eq!(
            back.buy_sizing.total_liquidity, "123456789012345678901234567890",
            "a large atomic amount must survive verbatim"
        );
        assert_eq!(back.buy_sizing.min_slice_debt, "10000000");
        assert_eq!(back.buy_sizing.max_orders, "12");
    }

    #[test]
    fn the_max_liquidity_sentinel_is_accepted() {
        // "max" means quote everything funded; it must not be rejected as
        // non-numeric.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy_sizing.total_liquidity = "max".into();
        view.buy_sizing.min_slice_debt = "10000000".into();
        let out = apply_settings(TEMPLATE, &view.to_patch()).unwrap();
        assert_eq!(
            read_settings(&out).unwrap().buy_sizing.total_liquidity,
            "max"
        );
    }

    #[test]
    fn bad_sizing_values_are_rejected_with_the_field_named() {
        let base = read_settings(TEMPLATE).unwrap();

        let mut view = base.clone();
        view.buy_sizing.total_liquidity = "lots".into();
        let err = apply_settings(TEMPLATE, &view.to_patch()).unwrap_err();
        assert!(err.to_string().contains("buy_total_liquidity_debt"));

        // A zero floor would silently disable the side mid-flight.
        let mut view = base.clone();
        view.buy_sizing.min_slice_debt = "0".into();
        let err = apply_settings(TEMPLATE, &view.to_patch()).unwrap_err();
        assert!(err.to_string().contains("greater than zero"));

        let mut view = base.clone();
        view.sell_sizing.max_orders = "0".into();
        let err = apply_settings(TEMPLATE, &view.to_patch()).unwrap_err();
        assert!(err.to_string().contains("at least 1"));

        let mut view = base;
        view.buy_sizing.order_size = "1.5".into();
        let err = apply_settings(TEMPLATE, &view.to_patch()).unwrap_err();
        assert!(err.to_string().contains("atomic units"));
    }

    #[test]
    fn clearing_a_sizing_field_removes_it_rather_than_zeroing_it() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy_sizing.max_orders = "9".into();
        let with = apply_settings(TEMPLATE, &view.to_patch()).unwrap();
        assert!(with.contains("buy_max_orders"));

        let mut back = read_settings(&with).unwrap();
        back.buy_sizing.max_orders = "  ".into();
        let without = apply_settings(&with, &back.to_patch()).unwrap();
        assert!(
            !without.contains("buy_max_orders"),
            "clearing must fall back to the bot default, not pin an explicit value"
        );
    }

    #[test]
    fn a_ttl_below_the_deadline_margin_is_rejected() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.ttl_secs = 1;
        let err = apply_settings(TEMPLATE, &view.to_patch()).unwrap_err();
        assert!(
            err.to_string().contains("ttl_secs") || err.to_string().contains("order lifetime"),
            "{err:#}"
        );
    }

    #[test]
    fn refresh_threshold_bps_round_trips() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.refresh_threshold_bps = 25;
        let out = apply_settings(TEMPLATE, &view.to_patch()).unwrap();
        let back = read_settings(&out).unwrap();
        assert_eq!(back.refresh_threshold_bps, 25);

        // 0 is valid: re-quote every tick (and what TWAP corridors usually want).
        view.refresh_threshold_bps = 0;
        let out = apply_settings(&out, &view.to_patch()).unwrap();
        assert_eq!(read_settings(&out).unwrap().refresh_threshold_bps, 0);
    }

    #[test]
    fn a_zero_tick_interval_is_rejected() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.tick_interval_secs = 0;
        let err = apply_settings(TEMPLATE, &view.to_patch()).unwrap_err();
        assert!(err.to_string().contains("at least 1 second"));
    }

    #[test]
    fn a_patch_with_no_sizing_leaves_the_existing_sizing_alone() {
        // This is what lets the desktop screen save spreads without knowing the
        // sizing values, and the panel send a partial edit.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy_sizing.min_slice_debt = "5000000".into();
        let seeded = apply_settings(TEMPLATE, &view.to_patch()).unwrap();

        let mut patch = read_settings(&seeded).unwrap().to_patch();
        patch.buy_sizing = None;
        patch.sell_sizing = None;
        patch.ttl_secs = None;
        patch.refresh_threshold_bps = None;
        patch.tick_interval_secs = None;
        patch.buy.value = "44".into();

        let out = apply_settings(&seeded, &patch).unwrap();
        let back = read_settings(&out).unwrap();
        assert_eq!(back.buy.value, "44", "the edited field changed");
        assert_eq!(
            back.buy_sizing.min_slice_debt, "5000000",
            "an unsent field must be left as it was"
        );
    }

    #[test]
    fn pool_indexing_reads_and_writes_the_right_pool() {
        let src = two_pool_config();
        let first = read_settings_at(&src, 0).unwrap();
        let second = read_settings_at(&src, 1).unwrap();
        assert_eq!(first.pool_count, 2);
        assert_eq!(first.buy.value, "1");
        assert_eq!(second.buy.value, "25");

        // Editing pool 1 must not touch pool 0.
        let mut patch = second.to_patch();
        patch.buy.value = "77".into();
        let out = apply_settings(&src, &patch).unwrap();
        assert_eq!(read_settings_at(&out, 0).unwrap().buy.value, "1");
        assert_eq!(read_settings_at(&out, 1).unwrap().buy.value, "77");
    }

    #[test]
    fn an_out_of_range_pool_says_how_many_there_are() {
        let src = two_pool_config();
        let err = read_settings_at(&src, 5).unwrap_err();
        assert!(err.to_string().contains("2 pools"));

        let mut patch = read_settings(&src).unwrap().to_patch();
        patch.pool_index = 5;
        let err = apply_settings(&src, &patch).unwrap_err();
        assert!(err.to_string().contains("2 pools"));
    }

    #[test]
    fn twap_and_lean_round_trip_through_the_settings_view() {
        let mut view = read_settings(TEMPLATE).unwrap();
        assert!(view.twap_window_secs.is_empty());
        assert!(!view.lean_enabled);

        view.twap_window_secs = "60".into();
        view.twap_max_deviation_bps = "50".into();
        view.lean_shadow = true;
        view.lean_floor_bps = "3.0".into();
        view.lean_base_bps = "1.0".into();
        view.lean_wide_bps = "3.0".into();

        let out = apply_settings(TEMPLATE, &view.to_patch()).unwrap();
        let back = read_settings(&out).unwrap();
        assert_eq!(back.twap_window_secs, "60");
        assert_eq!(back.twap_max_deviation_bps, "50");
        assert!(back.lean_shadow);
        assert!(!back.lean_enabled);
        assert_eq!(back.lean_floor_bps, "3");
        assert_eq!(back.lean_base_bps, "1");
        assert_eq!(back.lean_wide_bps, "3");

        // Enabling lean without a floor is refused by the real loader.
        let mut bad = back.clone();
        bad.lean_enabled = true;
        bad.lean_floor_bps.clear();
        assert!(apply_settings(&out, &bad.to_patch()).is_err());

        // Clearing TWAP while leaving the deviation set is refused — the guard
        // only applies with a window. Also clear the deviation to actually drop
        // TWAP; a partial clear must not leave a half-configured pool.
        let mut cleared = back;
        cleared.twap_window_secs.clear();
        let err = apply_settings(&out, &cleared.to_patch()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("twap_max_deviation_bps"),
            "expected deviation-without-window refusal, got: {msg}"
        );
        cleared.twap_max_deviation_bps.clear();
        let dropped = apply_settings(&out, &cleared.to_patch()).unwrap();
        assert!(!dropped.contains("twap_window_secs"));
        assert!(!dropped.contains("twap_max_deviation_bps"));
    }

    #[test]
    fn clearing_experimental_fields_removes_them() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.twap_window_secs = "60".into();
        view.lean_shadow = true;
        view.lean_floor_bps = "3".into();
        let with = apply_settings(TEMPLATE, &view.to_patch()).unwrap();
        assert!(with.contains("twap_window_secs"));
        assert!(with.contains("lean_shadow"));

        let mut back = read_settings(&with).unwrap();
        back.twap_window_secs.clear();
        back.lean_shadow = false;
        back.lean_floor_bps.clear();
        let without = apply_settings(&with, &back.to_patch()).unwrap();
        assert!(!without.contains("twap_window_secs"), "{without}");
        assert!(!without.contains("lean_shadow"), "{without}");
        assert!(!without.contains("lean_floor_bps"), "{without}");
    }

    #[test]
    fn a_spread_save_leaves_an_existing_rfq_block_untouched() {
        let src = format!(
            "{}\nrfq_corridor = \"cngn-usdt\"\n\n[rfq]\nenabled = true\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\nmaker_id = \"mk_x\"\nvalidation_contract = \"0x00000000000000000000000000000000000000aa\"\n",
            TEMPLATE.trim_end()
        );
        let v = read_settings(&src).unwrap();
        assert!(v.rfq_enabled);
        assert_eq!(v.rfq_url, "wss://api.textilecredit.com/v2/maker/stream");
        assert_eq!(v.rfq_maker_id, "mk_x");
        assert_eq!(
            v.rfq_validation_contract,
            "0x00000000000000000000000000000000000000aa"
        );
        assert_eq!(v.rfq_corridor, "cngn-usdt");

        // to_patch() is what a form sends when RFQ wasn't edited.
        let out = apply_settings(&src, &v.to_patch()).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn rfq_fields_round_trip_through_the_patch() {
        let mut patch = read_settings(TEMPLATE).unwrap().to_patch();
        patch.rfq_enabled = Some(true);
        patch.rfq_url = Some("wss://api.textilecredit.com/v2/maker/stream".into());
        patch.rfq_maker_id = Some("clmaker123".into());
        patch.rfq_validation_contract = Some("0x00000000000000000000000000000000000000aa".into());
        patch.rfq_corridor = Some("cngn-usdt-bsc".into());

        let out = apply_settings(TEMPLATE, &patch).unwrap();
        let back = read_settings(&out).unwrap();
        assert!(back.rfq_enabled);
        assert_eq!(back.rfq_url, "wss://api.textilecredit.com/v2/maker/stream");
        assert_eq!(back.rfq_maker_id, "clmaker123");
        assert_eq!(
            back.rfq_validation_contract,
            "0x00000000000000000000000000000000000000aa"
        );
        assert_eq!(back.rfq_corridor, "cngn-usdt-bsc");

        // Clearing the corridor removes the key; disabling keeps the block.
        patch.rfq_enabled = Some(false);
        patch.rfq_corridor = Some(String::new());
        let off = apply_settings(&out, &patch).unwrap();
        let back = read_settings(&off).unwrap();
        assert!(!back.rfq_enabled);
        assert!(back.rfq_corridor.is_empty());
        assert!(off.contains("[rfq]"), "disable must not delete the block");
    }

    #[test]
    fn rfq_rejects_a_non_websocket_url() {
        let mut patch = read_settings(TEMPLATE).unwrap().to_patch();
        patch.rfq_url = Some("https://api.textilecredit.com/v2/maker/stream".into());
        let err = apply_settings(TEMPLATE, &patch).unwrap_err();
        assert!(err.to_string().contains("ws"), "got {err:#}");
    }

    #[test]
    fn rfq_rejects_a_remote_cleartext_websocket() {
        let mut patch = read_settings(TEMPLATE).unwrap().to_patch();
        patch.rfq_url = Some("ws://api.textilecredit.com/v2/maker/stream".into());
        let err = apply_settings(TEMPLATE, &patch).unwrap_err();
        let full = format!("{err:#}");
        assert!(
            full.contains("localhost"),
            "remote ws:// must be rejected, got {full}"
        );
    }

    #[test]
    fn a_spread_only_patch_leaves_experimental_fields_alone() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.twap_window_secs = "120".into();
        let seeded = apply_settings(TEMPLATE, &view.to_patch()).unwrap();

        let mut patch = read_settings(&seeded).unwrap().to_patch();
        patch.twap_window_secs = None;
        patch.twap_max_deviation_bps = None;
        patch.lean_enabled = None;
        patch.lean_shadow = None;
        patch.lean_floor_bps = None;
        patch.lean_base_bps = None;
        patch.lean_wide_bps = None;
        patch.buy.value = "11".into();

        let out = apply_settings(&seeded, &patch).unwrap();
        let back = read_settings(&out).unwrap();
        assert_eq!(back.buy.value, "11");
        assert_eq!(back.twap_window_secs, "120");
    }
}
