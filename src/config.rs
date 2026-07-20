// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Operator config (a TOML file). The wallet key comes from the environment
//! (`STITCH_PRIVATE_KEY_FILE` or `STITCH_PRIVATE_KEY`), never the config file.

use alloy_primitives::U256;
use anyhow::Context;
use serde::Deserialize;

use crate::lean::{LeanMode, LeanParams, DEFAULT_BASE_BPS, DEFAULT_WIDE_BPS};
use crate::quote::Spread;

/// Default cap for generated ladder slices per side. Keep this low enough that
/// one market-maker wallet does not dominate or churn the live order book.
pub const MAX_SUPPORTED_LADDER_ORDERS: u32 = 40;
pub const DEFAULT_MAX_LADDER_ORDERS: u32 = MAX_SUPPORTED_LADDER_ORDERS;
pub const MAX_LIQUIDITY_SENTINEL: &str = "max";

/// Default seconds before a live order's deadline at which the bot reposts its
/// side, so the replacement overlaps the old order instead of leaving a gap.
/// Sized to clear the indexer's order-deadline margin (30s) plus the ~15s web
/// poll with headroom, so the live order book never blanks between reposts.
/// Effective overlap is capped at half the TTL (see `requote_age_secs`), so
/// keep `ttl_secs ≥ 2 × repost_lead_secs` to get the full lead.
pub const DEFAULT_REPOST_LEAD_SECS: u64 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub chain_id: u64,
    pub rpc_url: String,
    /// Textile indexer base URL (receives signed orders, serves the estimate).
    pub indexer_url: String,
    /// Canonical Permit2 for this chain.
    pub permit2: String,
    /// LimitOrderReactor for this chain.
    pub reactor: String,
    /// Subgraph endpoint for settlement-closing discovery (OPEN positions).
    /// Legacy: only configs that still run the closer set this.
    #[serde(default)]
    pub subgraph_url: Option<String>,
    /// Re-quote / close cadence.
    pub tick_interval_secs: u64,
    pub feed: FeedConfig,
    /// Signing backend. Omit for the local key (hotwallet) from the environment;
    /// set `provider = "turnkey" | "mpcvault"` to sign via an MPC wallet.
    #[serde(default)]
    pub signer: Option<crate::signer::SignerConfig>,
    pub pools: Vec<PoolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    /// HTTP endpoint returning `{ price, timestamp }`.
    pub url: String,
    /// Stop quoting if the feed hasn't updated within this many seconds.
    pub staleness_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidityAmount {
    Exact(U256),
    Max,
}

pub fn parse_liquidity_amount(value: &str, field: &str) -> anyhow::Result<LiquidityAmount> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case(MAX_LIQUIDITY_SENTINEL) {
        return Ok(LiquidityAmount::Max);
    }
    trimmed
        .parse::<U256>()
        .map(LiquidityAmount::Exact)
        .with_context(|| {
            format!("invalid {field}; use an atomic integer or \"{MAX_LIQUIDITY_SENTINEL}\"")
        })
}

/// Parse a ladder floor in atomic debt units.
///
/// Unlike total liquidity, a minimum slice cannot use `max` and must fit the
/// `u128` ladder arithmetic. Rejecting it while loading config prevents a bad
/// value from disabling one side only after the bot has started.
pub fn parse_min_slice_debt(value: &str, field: &str) -> anyhow::Result<u128> {
    let parsed = value
        .trim()
        .parse::<u128>()
        .with_context(|| format!("invalid {field}; use a positive atomic integer"))?;
    anyhow::ensure!(parsed > 0, "{field} must be greater than zero");
    Ok(parsed)
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    /// The soft/collateral asset of the pair (e.g. cNGN). The bot buys it on the
    /// bid and sells it on the ask.
    pub collateral: String,
    pub collateral_decimals: u8,
    /// The stable/debt asset of the pair (e.g. USDT).
    pub debt: String,
    pub debt_decimals: u8,
    /// Per-pool price feed (overrides the bot-level `[feed]` for this pool).
    /// Required when corridors have different prices — one shared feed can't
    /// price cNGN, COPM, and KES at once.
    #[serde(default)]
    pub feed_url: Option<String>,

    // ----- Buy side (bid below mid — "buy low"). Configure a spread (one of
    // bps / abs) and a size to enable it; omit to run sell-only. The operator
    // funds `debt` (USDT) + a Permit2 approval on it. -----
    /// Bid spread as basis points below the mid.
    #[serde(default)]
    pub buy_offset_bps: Option<u32>,
    /// Bid spread as an absolute amount in the soft-per-stable price (collateral
    /// per debt, e.g. cNGN/USDT) below the mid. Currency-agnostic.
    #[serde(default)]
    pub buy_offset_abs: Option<f64>,
    /// Debt (USDT) committed per bid, atomic units (uint256 as string).
    #[serde(default)]
    pub buy_order_size_debt: Option<String>,
    /// Total debt liquidity to quote as a balanced ladder, atomic units.
    /// When set with `buy_min_slice_debt`, this takes precedence over
    /// `buy_order_size_debt`.
    #[serde(default)]
    pub buy_total_liquidity_debt: Option<String>,
    /// Smallest bid slice, atomic debt units. For USDC/USDT this is usually
    /// 10e6 for a 10 stablecoin minimum.
    #[serde(default)]
    pub buy_min_slice_debt: Option<String>,
    /// Maximum number of bid slices to keep live for this pool.
    #[serde(default)]
    pub buy_max_orders: Option<u32>,

    // ----- Sell side (ask above mid — "sell high"). The operator funds
    // `collateral` (cNGN) + a Permit2 approval on it. -----
    /// Ask spread as basis points above the mid.
    #[serde(default)]
    pub sell_offset_bps: Option<u32>,
    /// Ask spread as an absolute amount in the soft-per-stable price (collateral
    /// per debt, e.g. cNGN/USDT) above the mid. Currency-agnostic.
    #[serde(default)]
    pub sell_offset_abs: Option<f64>,
    /// Collateral (cNGN) committed per ask, atomic units (uint256 as string).
    #[serde(default)]
    pub sell_order_size_collateral: Option<String>,
    /// Total collateral inventory to quote as a balanced ladder, atomic units.
    /// When set with `sell_min_slice_debt`, this takes precedence over
    /// `sell_order_size_collateral`.
    #[serde(default)]
    pub sell_total_liquidity_collateral: Option<String>,
    /// Smallest ask slice expressed as debt/stablecoin equivalent, atomic debt
    /// units. The bot converts each generated debt-denominated slice into
    /// collateral at the live ask price.
    #[serde(default)]
    pub sell_min_slice_debt: Option<String>,
    /// Maximum number of ask slices to keep live for this pool.
    #[serde(default)]
    pub sell_max_orders: Option<u32>,

    /// Order lifetime.
    pub ttl_secs: u64,
    /// Repost a side this many seconds before its live order expires, so the
    /// replacement overlaps the old order rather than leaving a book gap.
    /// Capped at half the TTL. Defaults to `DEFAULT_REPOST_LEAD_SECS`.
    #[serde(default)]
    pub repost_lead_secs: Option<u64>,
    /// Re-sign a side when its price moves more than this since its last order.
    pub refresh_threshold_bps: u32,

    // ----- Taker leg (user limit orders). Users rest signed limit orders in
    // the same book the bot quotes into; when one's price reaches the bot's
    // own bid/ask it can be filled on-chain via `reactor.executeBatch`. The
    // side spreads above are the pricing — a user ask fills at or below the
    // bid, a user bid at or above the ask — so a side without a spread is
    // never taken. -----
    /// Fill users' resting limit orders when they cross the bot's own quote.
    #[serde(default)]
    pub limit_taker_enabled: Option<bool>,
    /// Minimum profit per filled order, valued in debt atomic units (a
    /// gas/dust guard). Default 0 — the side spreads carry the margin.
    #[serde(default)]
    pub limit_taker_min_profit_debt: Option<String>,
    /// Most resting orders to fill in one `executeBatch` (default 10).
    #[serde(default)]
    pub limit_taker_max_orders: Option<u32>,

    // ----- Inventory-lean quoting. Leans both spreads against the wallet's
    // own inventory so the book self-rebalances and never freezes one-sided,
    // while no quote ever crosses fair (every offset is clamped to the
    // measured feed-accuracy floor). See [`crate::lean`]. -----
    /// Quote the live book off the lean prices. The pilot feature flag —
    /// revert instantly by setting it back to false and restarting.
    #[serde(default)]
    pub lean_enabled: Option<bool>,
    /// Compute and log the lean quotes next to the live ones each tick; no
    /// behavior change. The rollout's shadow step. `lean_enabled` wins if both
    /// are set.
    #[serde(default)]
    pub lean_shadow: Option<bool>,
    /// Balanced-zone half-spread in bps (default 1.0).
    #[serde(default)]
    pub lean_base_bps: Option<f64>,
    /// Extra widening of the accumulating side at the critical inventory edge,
    /// in bps (default 3.0).
    #[serde(default)]
    pub lean_wide_bps: Option<f64>,
    /// The tightest honest spread in bps: the measured p95 of the feed's error
    /// vs live Pyth. Measured, not assumed — required when lean is on.
    #[serde(default)]
    pub lean_floor_bps: Option<f64>,

    // ----- Settlement closing (auction closer). The default setup fills these;
    // omit `closer_pool` only for market-making-only configs. -----
    /// The SettlementPool to close positions in.
    #[serde(default)]
    pub closer_pool: Option<String>,
    /// Auction floor rate (RAY) — the pool's opening rate component.
    #[serde(default)]
    pub floor_ray: Option<String>,
    /// Auction buffer rate (RAY) — the decaying premium component.
    #[serde(default)]
    pub buffer_ray: Option<String>,
    /// Auction window in seconds (the decay horizon).
    #[serde(default)]
    pub window_secs: Option<u64>,
    /// Minimum net margin to close a position, collateral atomic (default 0).
    #[serde(default)]
    pub min_margin_collateral: Option<String>,
    /// Most positions to close per `fill()` (default 10).
    #[serde(default)]
    pub max_positions_per_fill: Option<u32>,
    /// Candidate positions to pull from the subgraph per tick (default 200).
    #[serde(default)]
    pub discover_first: Option<u32>,
    /// Skip positions past the auction window (default true).
    #[serde(default)]
    pub skip_past_window: Option<bool>,
}

/// Pick a spread from the two optional representations. Bps wins if both are
/// set (operators shouldn't, but be deterministic).
fn spread_from(bps: Option<u32>, abs: Option<f64>) -> Option<Spread> {
    match (bps, abs) {
        (Some(b), _) => Some(Spread::Bps(b)),
        (None, Some(d)) => Some(Spread::Abs(d)),
        (None, None) => None,
    }
}

impl PoolConfig {
    /// Seconds before expiry at which this side reposts, with the default applied.
    pub fn repost_lead_secs(&self) -> u64 {
        self.repost_lead_secs.unwrap_or(DEFAULT_REPOST_LEAD_SECS)
    }
    /// The bid spread for this pool, however the operator expressed it.
    pub fn buy_spread(&self) -> Option<Spread> {
        spread_from(self.buy_offset_bps, self.buy_offset_abs)
    }
    /// The ask spread for this pool, however the operator expressed it.
    pub fn sell_spread(&self) -> Option<Spread> {
        spread_from(self.sell_offset_bps, self.sell_offset_abs)
    }
    /// True when the buy side is fully configured (a spread + a size).
    pub fn buy_enabled(&self) -> bool {
        self.buy_spread().is_some()
            && (self.buy_order_size_debt.is_some() || self.buy_ladder_enabled())
    }
    /// True when the sell side is fully configured (a spread + a size).
    pub fn sell_enabled(&self) -> bool {
        self.sell_spread().is_some()
            && (self.sell_order_size_collateral.is_some() || self.sell_ladder_enabled())
    }
    /// True when bid ladder fields are present. The max-order field is optional.
    pub fn buy_ladder_enabled(&self) -> bool {
        self.buy_total_liquidity_debt.is_some() && self.buy_min_slice_debt.is_some()
    }
    /// True when ask ladder fields are present. The max-order field is optional.
    pub fn sell_ladder_enabled(&self) -> bool {
        self.sell_total_liquidity_collateral.is_some() && self.sell_min_slice_debt.is_some()
    }
    /// True when this pool has the blue-leg close parameters wired.
    pub fn closer_enabled(&self) -> bool {
        self.closer_pool.is_some()
            && self.floor_ray.is_some()
            && self.buffer_ray.is_some()
            && self.window_secs.is_some()
    }
    /// True when the taker leg is on and at least one side has a spread to
    /// price fills with.
    pub fn limit_taker_enabled(&self) -> bool {
        self.limit_taker_enabled.unwrap_or(false)
            && (self.buy_spread().is_some() || self.sell_spread().is_some())
    }
    /// The pool's inventory-lean rollout mode. Live wins over shadow.
    pub fn lean_mode(&self) -> LeanMode {
        if self.lean_enabled.unwrap_or(false) {
            LeanMode::Live
        } else if self.lean_shadow.unwrap_or(false) {
            LeanMode::Shadow
        } else {
            LeanMode::Off
        }
    }
    /// Lean tunables with defaults applied. `None` only when the required
    /// measured floor is missing (validation rejects that for lean pools).
    pub fn lean_params(&self) -> Option<LeanParams> {
        Some(LeanParams {
            base_bps: self.lean_base_bps.unwrap_or(DEFAULT_BASE_BPS),
            wide_bps: self.lean_wide_bps.unwrap_or(DEFAULT_WIDE_BPS),
            floor_bps: self.lean_floor_bps?,
        })
    }
}

impl Config {
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let cfg = toml::from_str::<Self>(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        for (idx, pool) in self.pools.iter().enumerate() {
            if let Some(min_slice) = pool.buy_min_slice_debt.as_deref() {
                parse_min_slice_debt(min_slice, &format!("pools[{idx}].buy_min_slice_debt"))?;
            }
            if let Some(min_slice) = pool.sell_min_slice_debt.as_deref() {
                parse_min_slice_debt(min_slice, &format!("pools[{idx}].sell_min_slice_debt"))?;
            }
            if let Some(max_orders) = pool.buy_max_orders {
                anyhow::ensure!(
                    max_orders <= MAX_SUPPORTED_LADDER_ORDERS,
                    "pools[{idx}].buy_max_orders {max_orders} exceeds supported limit {MAX_SUPPORTED_LADDER_ORDERS}"
                );
            }
            if let Some(max_orders) = pool.sell_max_orders {
                anyhow::ensure!(
                    max_orders <= MAX_SUPPORTED_LADDER_ORDERS,
                    "pools[{idx}].sell_max_orders {max_orders} exceeds supported limit {MAX_SUPPORTED_LADDER_ORDERS}"
                );
            }
            if pool.lean_mode() != LeanMode::Off {
                let floor = pool.lean_floor_bps.ok_or_else(|| {
                    anyhow::anyhow!(
                        "pools[{idx}]: lean quoting needs lean_floor_bps — the measured p95 \
                         error of the price feed vs live Pyth, in bps (measure it, don't assume)"
                    )
                })?;
                anyhow::ensure!(
                    floor.is_finite() && floor > 0.0,
                    "pools[{idx}].lean_floor_bps must be a positive number of bps"
                );
                if let Some(base) = pool.lean_base_bps {
                    anyhow::ensure!(
                        base.is_finite() && base > 0.0,
                        "pools[{idx}].lean_base_bps must be a positive number of bps"
                    );
                }
                if let Some(wide) = pool.lean_wide_bps {
                    anyhow::ensure!(
                        wide.is_finite() && wide >= 0.0,
                        "pools[{idx}].lean_wide_bps must be zero or a positive number of bps"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../stitch.example.toml");

    #[test]
    fn parses_the_example_config() {
        let cfg = Config::from_toml(EXAMPLE).expect("example config parses");
        assert_eq!(cfg.chain_id, 8453);
        assert!(!cfg.pools.is_empty());
        let pool = &cfg.pools[0];
        assert_eq!(pool.collateral_decimals, 6);
        // The example runs both sides of the book...
        assert!(pool.buy_enabled());
        assert!(pool.sell_enabled());
        assert!(cfg.feed.staleness_secs > 0);
        // The taker leg is opt-in: the example documents it commented out.
        assert!(!pool.limit_taker_enabled());
    }

    #[test]
    fn bps_and_abs_spreads_both_parse_per_side() {
        let toml = r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x0000000000000000000000000000000000000000"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x0000000000000000000000000000000000000001"
            collateral_decimals = 18
            debt = "0x0000000000000000000000000000000000000002"
            debt_decimals = 6
            buy_offset_bps = 150
            buy_total_liquidity_debt = "50000000000"
            buy_min_slice_debt = "10000000"
            buy_max_orders = 40
            sell_offset_abs = 2.0
            sell_total_liquidity_collateral = "30000000000000000000000"
            sell_min_slice_debt = "10000000"
            sell_max_orders = 40
            ttl_secs = 30
            refresh_threshold_bps = 10
        "#;
        let cfg = Config::from_toml(toml).expect("config parses");
        let pool = &cfg.pools[0];
        assert_eq!(pool.buy_spread(), Some(Spread::Bps(150)));
        assert_eq!(pool.sell_spread(), Some(Spread::Abs(2.0)));
        assert!(pool.buy_enabled() && pool.sell_enabled());
        assert!(pool.buy_ladder_enabled() && pool.sell_ladder_enabled());
        assert!(!pool.closer_enabled());
    }

    #[test]
    fn max_liquidity_sentinel_parses_case_insensitively() {
        assert_eq!(
            parse_liquidity_amount("max", "buy_total_liquidity_debt").unwrap(),
            LiquidityAmount::Max
        );
        assert_eq!(
            parse_liquidity_amount(" MAX ", "sell_total_liquidity_collateral").unwrap(),
            LiquidityAmount::Max
        );
        assert_eq!(
            parse_liquidity_amount("50000000000", "buy_total_liquidity_debt").unwrap(),
            LiquidityAmount::Exact(U256::from(50_000_000_000u64))
        );
    }

    #[test]
    fn parses_500_usdt_min_slice_in_atomic_units() {
        assert_eq!(
            parse_min_slice_debt("500000000", "buy_min_slice_debt").unwrap(),
            500_000_000
        );
    }

    #[test]
    fn rejects_invalid_min_slices_while_loading_config() {
        for (field, value) in [
            ("buy_min_slice_debt", "not-an-integer"),
            ("sell_min_slice_debt", "0"),
            (
                "buy_min_slice_debt",
                "340282366920938463463374607431768211456",
            ),
        ] {
            let toml = format!("{LEAN_POOL_BASE}\n{field} = \"{value}\"\n");
            let err = Config::from_toml(&toml).expect_err("invalid floor must stop startup");
            assert!(
                err.to_string().contains(field),
                "error should name {field}: {err}"
            );
        }
    }

    #[test]
    fn rejects_ladder_order_caps_above_supported_limit() {
        let toml = r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x0000000000000000000000000000000000000000"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x0000000000000000000000000000000000000001"
            collateral_decimals = 18
            debt = "0x0000000000000000000000000000000000000002"
            debt_decimals = 6
            buy_offset_bps = 150
            buy_total_liquidity_debt = "50000000000"
            buy_min_slice_debt = "10000000"
            buy_max_orders = 41
            sell_offset_abs = 2.0
            sell_total_liquidity_collateral = "30000000000000000000000"
            sell_min_slice_debt = "10000000"
            sell_max_orders = 40
            ttl_secs = 30
            refresh_threshold_bps = 10
        "#;
        let err = Config::from_toml(toml).expect_err("oversized buy cap is rejected");
        let msg = err.to_string();
        assert!(msg.contains("buy_max_orders"));
        assert!(msg.contains("40"));

        let toml = toml
            .replace("buy_max_orders = 41", "buy_max_orders = 40")
            .replace("sell_max_orders = 40", "sell_max_orders = 41");
        let err = Config::from_toml(&toml).expect_err("oversized sell cap is rejected");
        let msg = err.to_string();
        assert!(msg.contains("sell_max_orders"));
        assert!(msg.contains("40"));
    }

    const LEAN_POOL_BASE: &str = r#"
        chain_id = 1
        rpc_url = "http://x"
        indexer_url = "http://x"
        permit2 = "0x0000000000000000000000000000000000000000"
        reactor = "0x0000000000000000000000000000000000000000"
        tick_interval_secs = 5
        [feed]
        url = "http://x"
        staleness_secs = 30
        [[pools]]
        collateral = "0x0000000000000000000000000000000000000001"
        collateral_decimals = 6
        debt = "0x0000000000000000000000000000000000000002"
        debt_decimals = 6
        buy_offset_bps = 1
        buy_order_size_debt = "1000000000"
        sell_offset_bps = 1
        sell_order_size_collateral = "1000000"
        ttl_secs = 120
        refresh_threshold_bps = 10
    "#;

    #[test]
    fn lean_defaults_to_off_and_live_wins_over_shadow() {
        let cfg = Config::from_toml(LEAN_POOL_BASE).unwrap();
        assert_eq!(cfg.pools[0].lean_mode(), LeanMode::Off);

        let toml = format!("{LEAN_POOL_BASE}\nlean_shadow = true\nlean_floor_bps = 3.0\n");
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(cfg.pools[0].lean_mode(), LeanMode::Shadow);
        let p = cfg.pools[0].lean_params().unwrap();
        assert_eq!(p.base_bps, DEFAULT_BASE_BPS);
        assert_eq!(p.wide_bps, DEFAULT_WIDE_BPS);
        assert_eq!(p.floor_bps, 3.0);

        let toml = format!(
            "{LEAN_POOL_BASE}\nlean_shadow = true\nlean_enabled = true\nlean_floor_bps = 3.0\n"
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(cfg.pools[0].lean_mode(), LeanMode::Live);
    }

    #[test]
    fn lean_without_a_measured_floor_is_rejected() {
        let toml = format!("{LEAN_POOL_BASE}\nlean_shadow = true\n");
        let err = Config::from_toml(&toml).expect_err("floor is required");
        assert!(err.to_string().contains("lean_floor_bps"));

        let toml = format!("{LEAN_POOL_BASE}\nlean_enabled = true\nlean_floor_bps = 0.0\n");
        let err = Config::from_toml(&toml).expect_err("zero floor is rejected");
        assert!(err.to_string().contains("lean_floor_bps"));
    }

    #[test]
    fn lean_tunables_must_be_sane_numbers() {
        let toml = format!(
            "{LEAN_POOL_BASE}\nlean_shadow = true\nlean_floor_bps = 3.0\nlean_base_bps = -1.0\n"
        );
        let err = Config::from_toml(&toml).expect_err("negative base is rejected");
        assert!(err.to_string().contains("lean_base_bps"));

        let toml = format!(
            "{LEAN_POOL_BASE}\nlean_shadow = true\nlean_floor_bps = 3.0\nlean_wide_bps = -0.1\n"
        );
        let err = Config::from_toml(&toml).expect_err("negative wide is rejected");
        assert!(err.to_string().contains("lean_wide_bps"));
    }

    #[test]
    fn a_side_without_a_size_or_spread_is_disabled() {
        let toml = r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x0000000000000000000000000000000000000000"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x0000000000000000000000000000000000000001"
            collateral_decimals = 18
            debt = "0x0000000000000000000000000000000000000002"
            debt_decimals = 6
            buy_offset_bps = 150
            buy_order_size_debt = "1000000000"
            ttl_secs = 30
            refresh_threshold_bps = 10
        "#;
        let cfg = Config::from_toml(toml).expect("buy-only config parses");
        assert!(cfg.pools[0].buy_enabled());
        assert!(!cfg.pools[0].sell_enabled());
    }
}
