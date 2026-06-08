// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Operator config (a TOML file). The wallet key comes from the environment
//! (`STITCH_PRIVATE_KEY_FILE` or `STITCH_PRIVATE_KEY`), never the config file.

use serde::Deserialize;

use crate::quote::Spread;

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
    /// The default setup sets this; omit only for market-making-only configs.
    #[serde(default)]
    pub subgraph_url: Option<String>,
    /// Re-quote / close cadence.
    pub tick_interval_secs: u64,
    pub feed: FeedConfig,
    pub pools: Vec<PoolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    /// HTTP endpoint returning `{ price, timestamp }`.
    pub url: String,
    /// Stop quoting if the feed hasn't updated within this many seconds.
    pub staleness_secs: u64,
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
    /// Re-sign a side when its price moves more than this since its last order.
    pub refresh_threshold_bps: u32,

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
}

impl Config {
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
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
        // The example runs both sides of the book.
        assert!(pool.buy_enabled());
        assert!(pool.sell_enabled());
        assert!(cfg.feed.staleness_secs > 0);
        // ...and the blue leg.
        assert!(cfg.subgraph_url.is_some());
        assert!(pool.closer_enabled());
        assert_eq!(pool.window_secs, Some(432_000));
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
            buy_max_orders = 150
            sell_offset_abs = 2.0
            sell_total_liquidity_collateral = "30000000000000000000000"
            sell_min_slice_debt = "10000000"
            sell_max_orders = 150
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
