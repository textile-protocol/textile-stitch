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

/// Seconds the indexer (and stitch's own slot-reuse credit) subtract from an
/// order's deadline before serving it as live. Matches
/// `FILLER_ORDER_DEADLINE_MARGIN_SECS` / `REUSABLE_DEADLINE_MARGIN_SECS`.
/// `ttl_secs` must be strictly greater than this, or posted orders are accepted
/// but immediately excluded from the live book and never fillable.
pub const LIVE_ORDER_DEADLINE_MARGIN_SECS: u64 = 30;

/// Default cap on how far a TWAP-centered quote may post through the
/// instantaneous feed, in bps (see `quote::SpotDeviationGuard`). Wide enough
/// to keep selling into ordinary transient spikes (the strategy's win), tight
/// enough that a persistent trend can't pick the lagging side off by more
/// than this per fill while the average converges.
pub const DEFAULT_TWAP_MAX_DEVIATION_BPS: u32 = 50;

/// Exclusive upper bound on `twap_max_deviation_bps`. At 10_000 bps the ask
/// floor collapses to ≤ 0 and the guard silently disables itself.
pub const MAX_TWAP_MAX_DEVIATION_BPS: u32 = 10_000;

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
    /// RFQ responder. Omitted → the responder never spawns. See [`RfqConfig`].
    #[serde(default)]
    pub rfq: Option<RfqConfig>,
    /// Post resting orders on the public ladder. Default true for files that
    /// omit the key (historical). New bots stamp `false`: they quote Swap via
    /// RFQ and do not rest orders on the book. Spreads and liquidity still
    /// size RFQ.
    #[serde(default = "default_true")]
    pub book_enabled: bool,
    /// Raw experimental gates. Values are uninterpreted strings on purpose:
    /// each consumer matches its own exact token, so a typo fails closed
    /// instead of enabling something adjacent.
    #[serde(default)]
    pub experimental: Option<ExperimentalConfig>,
    pub pools: Vec<PoolConfig>,
}

fn default_true() -> bool {
    true
}

/// The `[rfq]` block: connection details for the venue's maker quote stream.
/// The maker API key is NOT here — it comes from the environment variable
/// named by `api_key_env`, mirroring how the wallet key stays out of the file.
#[derive(Debug, Clone, Deserialize)]
pub struct RfqConfig {
    /// Master switch. `false` (the default) keeps the responder fully off even
    /// when the rest of the block is filled in — the kill switch.
    #[serde(default)]
    pub enabled: bool,
    /// The venue's maker stream, e.g. `wss://api.textilecredit.com/v2/maker/stream`.
    pub url: String,
    /// The maker id the venue issued at onboarding (sent as `X-Textile-Maker-Id`).
    pub maker_id: String,
    /// Name of the environment variable holding the maker API key.
    #[serde(default = "default_rfq_api_key_env")]
    pub api_key_env: String,
    /// The chain's PreferredFillerValidation contract. Every RFQ order binds
    /// its taker through this contract, so an off-venue observer can't fill a
    /// quote that lost.
    pub validation_contract: String,
    /// This bot's name on the venue, so one funding wallet can run several —
    /// per chain, or several on one chain, even over the same corridor. The
    /// venue only ever supersedes a session when the same id reconnects, so
    /// two bots sharing a wallet must not share this.
    ///
    /// Unset is fine and is the usual case: `rfq_instance_id` then falls back
    /// to `rfq-instance-id` next to stitch.toml, generated once on first
    /// connect. Set it when you want the venue's logs to name your bots
    /// ("bsc-cngn", "base-majors") rather than a random id.
    #[serde(default)]
    pub instance_id: Option<String>,
}

fn default_rfq_api_key_env() -> String {
    "STITCH_RFQ_API_KEY".to_string()
}

/// Venue maker stream: `wss://` anywhere, or `ws://` only on loopback.
/// A remote cleartext stream is a signing oracle for whoever can MITM it
/// (audit H-03): stitch binds the next firm quote to whatever `taker` the
/// frame names. Docker Compose sets `STITCH_ALLOW_CLEARTEXT_DOCKER=1` so
/// `ws://app:10000` (the API service name) is accepted on the local stack.
pub fn assert_rfq_stream_url(raw: &str) -> anyhow::Result<()> {
    assert_rfq_stream_url_with(raw, allow_cleartext_docker())
}

/// The rule itself, with the Docker escape hatch as an argument rather than a
/// process-global read.
///
/// Splitting it this way is what keeps the suite deterministic. `cargo test`
/// runs tests as threads in ONE process, so a test that sets
/// `STITCH_ALLOW_CLEARTEXT_DOCKER` to exercise the override was also silently
/// widening this gate for every other test running at that moment — and
/// `audit_h03_rfq_stream_url_rejects_remote_cleartext`, whose whole job is to
/// assert the gate is shut, failed roughly one run in ten. Nothing mutates the
/// environment now; the env read stays at the edge, in `allow_cleartext_docker`.
fn assert_rfq_stream_url_with(raw: &str, allow_cleartext: bool) -> anyhow::Result<()> {
    let parsed = url::Url::parse(raw.trim())
        .with_context(|| format!("[rfq].url must be a valid WebSocket URL, got {raw:?}"))?;
    anyhow::ensure!(parsed.host().is_some(), "[rfq].url must include a host");
    match parsed.scheme() {
        "wss" => Ok(()),
        "ws" => {
            anyhow::ensure!(
                host_is_loopback(parsed.host()) || allow_cleartext,
                "[rfq].url may use ws:// only on localhost, got {raw:?}"
            );
            Ok(())
        }
        other => anyhow::bail!("[rfq].url must be a ws(s):// URL, got scheme {other:?}"),
    }
}

/// Docker-internal http/ws (`http://app:8916`, `ws://app:10000`). Off unless
/// the compose file sets `STITCH_ALLOW_CLEARTEXT_DOCKER=1`.
///
/// The only place the process environment is consulted for this. Tests drive
/// the `*_with` functions directly instead of setting the variable, so no test
/// can change what a concurrently-running test sees.
fn allow_cleartext_docker() -> bool {
    matches!(
        std::env::var("STITCH_ALLOW_CLEARTEXT_DOCKER").as_deref(),
        Ok("1")
    )
}

fn host_is_loopback(host: Option<url::Host<&str>>) -> bool {
    match host {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The `[experimental]` block. Every field is a raw string read verbatim; the
/// parser attaches no meaning so an unknown/old gate token can never turn a
/// feature on by accident.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExperimentalConfig {
    /// Leftover gate tokens from the RFQ beta. Ignored: the Settings card
    /// and RFQ-as-default path are always on.
    #[serde(default)]
    pub rfq_panel: Option<String>,
    /// Leftover gate token from the RFQ-as-default rollout. Ignored.
    #[serde(default)]
    pub rfq_default: Option<String>,
}

/// Historical token that unlocked the Settings RFQ card. Kept so old files
/// still parse; the card is always visible now.
pub const RFQ_PANEL_GATE: &str = "enable-rfq-beta";

/// Historical token that turned RFQ into the default quoting mode. Kept so
/// old files still parse; RFQ-only is the only new-bot path now.
pub const RFQ_DEFAULT_GATE: &str = "enable-rfq";

/// Filename for the fleet-wide flag, dropped next to the per-bot folders
/// (`{bots_dir}/panel.toml`). Same `[experimental]` keys as a bot config.
pub const PANEL_FLAGS_FILE: &str = "panel.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    /// HTTP endpoint returning `{ price, timestamp }`.
    pub url: String,
    /// Stop quoting if the feed hasn't updated within this many seconds.
    /// The ladder uses this as written. RFQ firm quotes tighten it — see
    /// [`rfq_staleness_secs`].
    pub staleness_secs: u64,
    /// How old a mark firm RFQ quotes may price off, for THIS feed.
    ///
    /// Per feed rather than global because the right answer is a property of
    /// how often the feed republishes, and corridors differ by orders of
    /// magnitude. cNGN is a cron sample (once a minute, so a mark is routinely
    /// tens of seconds old and the window has to be wide enough to span it),
    /// while WETH or XAUT are fetched live per request and are never stale
    /// unless something has broken — at which point four minutes of drift on
    /// WETH is a very different risk from four minutes on cNGN.
    ///
    /// Absent means [`RFQ_DEFAULT_STALENESS_SECS`], and absent is the safe
    /// answer: a corridor that has not thought about this gets the tight
    /// window. Bounded above by [`RFQ_MAX_STALENESS_SECS`] so widening it
    /// stays a decision with a ceiling rather than an open dial.
    #[serde(default)]
    pub rfq_staleness_secs: Option<u64>,
}

/// How often Textile's `/price` restamps the cNGN mark: the `sample-cngn-pricing`
/// cron, once a minute. The cap below is expressed in terms of it, because the
/// only thing that number has to be right about is how many published marks it
/// can afford to miss.
pub const PRICE_FEED_CADENCE_SECS: u64 = 60;

/// What a feed gets if it does not ask: RFQ firm quotes go dark after a minute.
///
/// Deliberately the tight value, so a corridor nobody has reasoned about is
/// never quietly granted a wide window. A feed that genuinely needs more says
/// so in its own `[feed].rfq_staleness_secs`.
pub const RFQ_DEFAULT_STALENESS_SECS: u64 = 60;

/// Ceiling on what any feed may request, however wide `[feed].staleness_secs`
/// is (shipped templates use 900 for the ladder). The ladder keeps its own.
///
/// A cron-sampled feed has to clear its publication cadence with room to spare
/// or the corridor goes dark between marks and the venue reports
/// `no_makers_online` while the maker is connected and healthy. Textile's
/// `/price` stamps each cNGN quote with the pricing sampler's `observedAt`; at
/// 60s against a 3-minute sampler the corridor was quotable for one minute in
/// three.
///
/// Four intervals, not three, and the extra one is not padding. A sample's
/// `observedAt` is stamped at the top of the tick, *before* the Monierate read
/// and the Bybit probes, so by the time the row is readable the mark is already
/// a second or three old. At exactly three intervals the previous mark expires
/// at the very moment the replacement tick is scheduled, so that write latency
/// plus any cron jitter lands in a gap where the corridor publishes nothing —
/// the failure this bound exists to prevent, just narrower. A whole spare
/// interval keeps the expiry boundary away from a scheduled tick entirely.
pub const RFQ_MAX_STALENESS_SECS: u64 = PRICE_FEED_CADENCE_SECS * 4;

/// Effective RFQ staleness gate for one feed: the tightest of what the feed
/// asked for (or the safe default), the ladder's own window, and the ceiling.
///
/// Taking the feed rather than a bare number is the point. The previous global
/// `min(staleness_secs, CAP)` meant raising the cap for cNGN's cron sampler
/// silently raised it for every RFQ corridor, including live-fetched ones like
/// WETH and XAUT whose shipped templates also set `staleness_secs = 900`. Those
/// marks are fresh in normal operation, so the wider window would not show up
/// day to day — it would show up exactly when their feed broke, letting a maker
/// keep quoting minutes-old prices on a pair that moves.
pub fn rfq_staleness_secs(feed: &FeedConfig) -> u64 {
    let requested = feed
        .rfq_staleness_secs
        .unwrap_or_else(|| default_rfq_staleness_secs(&feed.url));
    feed.staleness_secs
        .min(requested)
        .min(RFQ_MAX_STALENESS_SECS)
}

/// The same rule for a pool that overrides `feed_url`.
///
/// A pool on its own feed must NOT inherit the bot-level window: that number
/// was reasoned about for a different publisher, and inheriting it is how a
/// cNGN bot carrying one WETH pool would quote that pool off a four-minute-old
/// mark. Bounded by the ladder's window for the same reason as above.
pub fn rfq_staleness_secs_for_pool(feed: &FeedConfig, pool: &PoolConfig) -> u64 {
    let Some(url) = pool.feed_url.as_deref() else {
        return rfq_staleness_secs(feed);
    };
    let requested = pool
        .rfq_staleness_secs
        .unwrap_or_else(|| default_rfq_staleness_secs(url));
    feed.staleness_secs
        .min(requested)
        .min(RFQ_MAX_STALENESS_SECS)
}

/// What a feed gets when its config says nothing, inferred from the feed URL.
///
/// Config always wins; this only decides the unset case. It exists because the
/// tight default is right for a live-fetched feed and wrong for a cron-sampled
/// one, and the bots already deployed against Textile's cNGN sampler have
/// configs written before the setting existed. Upgrading Stitch does not
/// rewrite a mounted `stitch.toml`, so without this those makers would silently
/// take the 60s default and go dark between samples — the bug this all started
/// with, reintroduced by an upgrade.
///
/// Keyed on the `pair` the feed selects rather than the host, because the pair
/// is what decides which publisher is behind the endpoint: every corridor uses
/// the same `/price` shape and differs only there. A self-hosted mirror of the
/// same endpoint therefore behaves identically, and a non-cNGN pair on the same
/// host still gets the tight window.
fn default_rfq_staleness_secs(feed_url: &str) -> u64 {
    if feed_pair(feed_url).is_some_and(|p| p.starts_with("cngn")) {
        RFQ_MAX_STALENESS_SECS
    } else {
        RFQ_DEFAULT_STALENESS_SECS
    }
}

/// The `pair` query parameter of a feed URL, lowercased.
fn feed_pair(feed_url: &str) -> Option<String> {
    let query = feed_url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.eq_ignore_ascii_case("pair")).then(|| v.to_ascii_lowercase())
    })
}

/// Price feed: `https://` anywhere, or `http://` only on loopback.
/// A remote cleartext feed is a MITM'd mid (audit M-05).
pub fn assert_feed_url(raw: &str, field: &str) -> anyhow::Result<()> {
    assert_feed_url_with(raw, field, allow_cleartext_docker())
}

/// As [`assert_rfq_stream_url_with`]: the escape hatch is an argument so tests
/// never have to touch the process environment to exercise it.
fn assert_feed_url_with(raw: &str, field: &str, allow_cleartext: bool) -> anyhow::Result<()> {
    let parsed = url::Url::parse(raw.trim())
        .with_context(|| format!("{field} must be a valid HTTP URL, got {raw:?}"))?;
    anyhow::ensure!(parsed.host().is_some(), "{field} must include a host");
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            anyhow::ensure!(
                host_is_loopback(parsed.host()) || allow_cleartext,
                "{field} may use http:// only on localhost, got {raw:?}"
            );
            Ok(())
        }
        other => anyhow::bail!("{field} must be an http(s):// URL, got scheme {other:?}"),
    }
}

/// The closer's subgraph endpoint, held to the feed URL's rules.
///
/// Same exposure: an attacker who can rewrite plaintext responses picks which
/// positions the closer sees, so cleartext is loopback-only here too.
pub fn assert_subgraph_url(raw: &str, field: &str) -> anyhow::Result<()> {
    assert_feed_url(raw, field)
}

/// Predicate form of [`assert_subgraph_url`], for the Start guards.
fn subgraph_url_usable(raw: &str) -> bool {
    !raw.trim().is_empty() && assert_subgraph_url(raw, "subgraph_url").is_ok()
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
    /// RFQ freshness window for THIS pool's feed. Only meaningful alongside
    /// `feed_url` — a pool on the bot-level feed uses the bot-level setting.
    #[serde(default)]
    pub rfq_staleness_secs: Option<u64>,
    /// Optional venue slug override. Unset is fine: when `[rfq].enabled` the
    /// pool is solicitable and the bot matches quote requests by tokens.
    /// Kept so existing configs that already set it still parse.
    #[serde(default)]
    pub rfq_corridor: Option<String>,

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
    /// Re-sign a side only when its price moves more than this since its last
    /// order (plus the TTL-driven age repost either way). 0 — the default —
    /// re-quotes every tick, keeping the book pinned to the current price:
    /// posting is off-chain and free, and a deadband lets quotes drift stale
    /// on slow moves. Set it above 0 only to cut signing/RPC churn (e.g. a
    /// rate-limited MPC signer).
    #[serde(default)]
    pub refresh_threshold_bps: u32,

    // ----- TWAP quoting. Center the spread on a short rolling time-weighted
    // average of the feed instead of the instantaneous value, so the book
    // stops chasing every tick: transient spikes get sold into above the
    // reverting mean instead of picking off a chased quote. See
    // [`crate::twap`]. -----
    /// Rolling TWAP window in seconds (~60-300 is sensible). Omit to quote
    /// off the instantaneous feed (the historical behavior). Longer filters
    /// more noise but lags real moves more.
    #[serde(default)]
    pub twap_window_secs: Option<u64>,
    /// With TWAP on: never post a side more than this many bps through the
    /// instantaneous feed (ask below spot / bid above spot), bounding what a
    /// persistent trend can extract from the lagging center. Defaults to
    /// `DEFAULT_TWAP_MAX_DEVIATION_BPS`.
    #[serde(default)]
    pub twap_max_deviation_bps: Option<u32>,

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
    ///
    /// Presence only, on purpose: `main.rs` gates on this and then warns when
    /// `build_closer_pool` rejects the values, which is how an operator finds
    /// out their closer config is broken. Use [`Self::closer_runnable`] when
    /// the answer has to mean "this leg will actually trade".
    pub fn closer_enabled(&self) -> bool {
        self.closer_pool.is_some()
            && self.floor_ray.is_some()
            && self.buffer_ray.is_some()
            && self.window_secs.is_some()
    }
    /// True when the closer is wired *and* every value parses — i.e. `main.rs`'s
    /// `build_closer_pool` would succeed. An unparseable `floor_ray` fails on
    /// every tick, which is the same dead leg as no closer at all, so anything
    /// deciding whether the bot has real work to do must ask this instead.
    pub fn closer_runnable(&self) -> bool {
        // Nonzero, same as the tokens: `close_pool_once` asks the subgraph for
        // positions in this pool, and the zero address matches none of them —
        // forever. That is a closer leg that never trades behind a Start guard
        // that says it will.
        let address = |s: &String| {
            s.parse::<alloy_primitives::Address>()
                .is_ok_and(|a| !a.is_zero())
        };
        let ray = |s: &String| s.parse::<U256>().is_ok();
        self.closer_enabled()
            && self.tokens_parse()
            && self.closer_pool.as_ref().is_some_and(address)
            && self.floor_ray.as_ref().is_some_and(ray)
            && self.buffer_ray.as_ref().is_some_and(ray)
    }
    /// True when both token addresses are usable. `main.rs` resolves `debt` and
    /// `collateral` at the top of every pool tick and `continue`s past the
    /// whole pool if either fails, so a pool that doesn't clear this bar runs
    /// no leg at all — not the ladder, not the taker, not the closer.
    ///
    /// The zero address parses but is not a token: every `balanceOf` and
    /// `allowance` against it reverts or reads zero, so the pool funds nothing
    /// and `levels_for` publishes no level. That is the same dead pool as an
    /// unparseable address, and a half-filled template is exactly how it
    /// happens — so treat it the same rather than let a Start guard call it
    /// live.
    pub fn tokens_parse(&self) -> bool {
        let usable = |s: &str| {
            s.parse::<alloy_primitives::Address>()
                .is_ok_and(|a| !a.is_zero())
        };
        usable(&self.collateral) && usable(&self.debt)
    }
    /// True when the taker leg is on and at least one side has a spread to
    /// price fills with.
    pub fn limit_taker_enabled(&self) -> bool {
        self.limit_taker_enabled.unwrap_or(false)
            && (self.buy_spread().is_some() || self.sell_spread().is_some())
            && self.tokens_parse()
    }
    /// True when the bid side would actually rest something: a spread, and a
    /// size that resolves to a positive amount. [`Self::buy_enabled`] is
    /// presence-only and is what decides approvals and funding checks; this is
    /// the stricter question — "will the ladder post" — for anything gating a
    /// restart on the book being useful.
    pub fn buy_postable(&self) -> bool {
        self.buy_spread().is_some()
            && side_drafts_orders(
                self.buy_total_liquidity_debt.as_deref(),
                self.buy_min_slice_debt.as_deref(),
                self.buy_max_orders,
                self.buy_order_size_debt.as_deref(),
                LadderUnits::SameAsSlice,
            )
    }
    /// True when `rfq::responder::book_from_pool` would succeed for this pool.
    ///
    /// Mirrors that function's four fallible steps: both token addresses, and
    /// both capacity strings. Kept in sync deliberately — `build_runtime`
    /// collects over every pool with `?`, so one pool failing here takes the
    /// whole responder down, not just its own side.
    pub fn rfq_book_buildable(&self) -> bool {
        self.tokens_parse()
            && self.rfq_buy_capacity_debt().is_ok()
            && self.rfq_sell_capacity_collateral().is_ok()
    }
    /// True when at least one RFQ side would actually publish a level.
    ///
    /// A side needs its spread *and* its capacity — `levels_for` destructures
    /// `(spread, capacity)` as a pair per side and skips the side if either is
    /// missing. That pairing is already enforced one layer down:
    /// [`Self::rfq_buy_capacity_debt`] and
    /// [`Self::rfq_sell_capacity_collateral`] both return `Ok(None)` when their
    /// side has no spread, so a spreadless side can never report capacity here.
    /// Pinned by `an_rfq_side_needs_its_spread_and_its_capacity`.
    ///
    /// A zero exact capacity is not a capacity: the responder omits that level
    /// and rejects every request against it, same as a side never configured.
    /// `Wallet` is the live balance, decided at request time.
    pub fn rfq_has_usable_capacity(&self) -> bool {
        let usable = |c: anyhow::Result<Option<RfqCapacity>>| {
            matches!(c, Ok(Some(RfqCapacity::Wallet)))
                || matches!(c, Ok(Some(RfqCapacity::Exact(v))) if !v.is_zero())
        };
        usable(self.rfq_buy_capacity_debt()) || usable(self.rfq_sell_capacity_collateral())
    }
    /// The ask-side counterpart of [`Self::buy_postable`].
    pub fn sell_postable(&self) -> bool {
        self.sell_spread().is_some()
            && side_drafts_orders(
                self.sell_total_liquidity_collateral.as_deref(),
                self.sell_min_slice_debt.as_deref(),
                self.sell_max_orders,
                self.sell_order_size_collateral.as_deref(),
                LadderUnits::ConvertedAtPrice,
            )
    }
    /// The TWAP window when TWAP quoting is on for this pool.
    pub fn twap_window(&self) -> Option<u64> {
        self.twap_window_secs.filter(|w| *w > 0)
    }
    /// The spot-deviation cap for TWAP-centered quotes, with the default.
    pub fn twap_deviation_bps(&self) -> u32 {
        self.twap_max_deviation_bps
            .unwrap_or(DEFAULT_TWAP_MAX_DEVIATION_BPS)
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

    /// Debt capacity the RFQ responder may commit on the bid side, or `None`
    /// when the side doesn't quote over RFQ (no spread configured).
    ///
    /// `"max"` is [`RfqCapacity::Wallet`]: the responder reads
    /// `min(balance, Permit2 allowance)` minus live book commitments on its
    /// own 1s loop and reserves in-flight quotes against that leftover. An
    /// explicit amount stays a hard cap. Neither path does an RPC on the
    /// quote hot path.
    pub fn rfq_buy_capacity_debt(&self) -> anyhow::Result<Option<RfqCapacity>> {
        if self.buy_spread().is_none() {
            return Ok(None);
        }
        rfq_side_capacity(
            self.buy_total_liquidity_debt.as_deref(),
            self.buy_order_size_debt.as_deref(),
            "buy",
        )
    }

    /// Collateral capacity for the RFQ ask side; see
    /// [`Self::rfq_buy_capacity_debt`].
    pub fn rfq_sell_capacity_collateral(&self) -> anyhow::Result<Option<RfqCapacity>> {
        if self.sell_spread().is_none() {
            return Ok(None);
        }
        rfq_side_capacity(
            self.sell_total_liquidity_collateral.as_deref(),
            self.sell_order_size_collateral.as_deref(),
            "sell",
        )
    }
}

/// How one RFQ side sizes itself. [`RfqCapacity::Exact`] is a configured
/// atomic cap; [`RfqCapacity::Wallet`] tracks the live funded balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfqCapacity {
    Exact(U256),
    Wallet,
}

/// Is a side's ladder total denominated in the same unit as its minimum slice?
///
/// The bid side funds in debt and slices in debt, so `balanced_ladder` sees them
/// directly. The ask side funds in *collateral* and slices in debt:
/// `maker::ask_ladder_sizes` converts the total at the live price and both token
/// decimals before laddering. Comparing those two raw numbers is a category
/// error — 1000 units of a low-priced collateral is numerically over a 1-unit
/// debt slice while being worth far less than it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LadderUnits {
    /// Total and slice share a unit: ask the builder directly.
    SameAsSlice,
    /// Total is converted at the live price first, so its size against the slice
    /// is a runtime question config cannot answer.
    ConvertedAtPrice,
}

/// True when a side would actually draft at least one order.
///
/// For a ladder this asks [`crate::ladder::balanced_ladder`] itself rather than
/// re-deriving its preconditions — it returns nothing when `total < min_slice`,
/// when `min_slice` is 0, and when `max_orders` is 0, and re-implementing that
/// here is exactly how a guard drifts from the builder.
///
/// Two cases are decided at tick time, not in config, and this can only rule out
/// what is dead whatever that tick brings: `"max"`, which the live wallet sizes,
/// and an ask total, whose comparison against the slice needs the live price.
/// For both, that leaves a positive total, a positive slice, and a positive
/// order budget — the conditions no price or balance can rescue.
fn side_drafts_orders(
    total: Option<&str>,
    min_slice: Option<&str>,
    max_orders: Option<u32>,
    order_size: Option<&str>,
    units: LadderUnits,
) -> bool {
    let orders = max_orders.unwrap_or(DEFAULT_MAX_LADDER_ORDERS) as usize;
    if let Some(raw) = total {
        let Ok(amount) = parse_liquidity_amount(raw, "total liquidity") else {
            return false;
        };
        let Some(min) = min_slice.and_then(|s| s.trim().parse::<u128>().ok()) else {
            return false;
        };
        let plausible = |t: U256| !t.is_zero() && min > 0 && orders > 0;
        return match amount {
            LiquidityAmount::Max => min > 0 && orders > 0,
            LiquidityAmount::Exact(v) => match units {
                LadderUnits::SameAsSlice => u128::try_from(v)
                    .is_ok_and(|t| !crate::ladder::balanced_ladder(t, min, orders).is_empty()),
                // The conversion is `u128`-bounded too (`u256_to_u128` on the
                // debt equivalent), but the collateral total itself is only
                // read as `u128` after conversion, so bound the input the same
                // way the funded balance is.
                LadderUnits::ConvertedAtPrice => u128::try_from(v).is_ok() && plausible(v),
            },
        };
    }
    // No ladder: a flat per-order size, which posts as long as it is positive
    // *and* fits the quote path's `u128`. `maker.rs` runs the same value through
    // `parse_input_liquidity`, which drops the side on overflow — so accepting
    // anything that merely parses as `U256` here would call a side postable that
    // draws no orders, the exact drift this predicate exists to prevent. The
    // ladder branch above already bounds the same way.
    order_size.is_some_and(|raw| raw.trim().parse::<u128>().is_ok_and(|v| v > 0))
}

/// One RFQ side's capacity policy: the ladder total (`max` → live wallet,
/// otherwise a hard cap), else the flat order size, else the side is off.
fn rfq_side_capacity(
    total: Option<&str>,
    order_size: Option<&str>,
    side: &str,
) -> anyhow::Result<Option<RfqCapacity>> {
    if let Some(raw) = total {
        return match parse_liquidity_amount(raw, &format!("{side} total liquidity"))? {
            LiquidityAmount::Exact(v) => Ok(Some(RfqCapacity::Exact(v))),
            LiquidityAmount::Max => Ok(Some(RfqCapacity::Wallet)),
        };
    }
    match order_size {
        Some(raw) => {
            let v = raw
                .trim()
                .parse::<U256>()
                .with_context(|| format!("invalid {side} order size for an rfq pool"))?;
            Ok(Some(RfqCapacity::Exact(v)))
        }
        None => Ok(None),
    }
}

impl Config {
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let cfg = toml::from_str::<Self>(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// True when the RFQ responder should run: the master switch is on and
    /// there is at least one pool to quote. The public ladder is a separate
    /// switch ([`Self::book_enabled`]).
    pub fn rfq_active(&self) -> bool {
        self.rfq.as_ref().is_some_and(|r| r.enabled) && !self.pools.is_empty()
    }

    /// True when the responder could actually answer something: active, and at
    /// least one pool whose tokens parse and whose buy or sell side resolves to
    /// a capacity. [`Self::rfq_active`] only asks whether the responder spawns —
    /// it will happily spawn on pools with no spread or no size, publish no
    /// usable levels, and reject every request. Anything deciding "does this bot
    /// have work to do" needs this instead.
    pub fn rfq_quotable(&self) -> bool {
        // Three conditions. The pool one is easy to miss: `build_runtime`
        // collects `book_from_pool` over *every* pool into one `Result`, so a
        // single unbuildable pool aborts the whole responder even when it has
        // no RFQ sides of its own. One good pool is not enough.
        //
        // The validation contract has to be real too — the venue rejects a
        // reply signed against the zero address as `unbound_order`, so a bot
        // with one connects, publishes levels, and never lands a quote.
        // `validate` refuses that config at load; this keeps the predicate
        // honest for one that never went through it.
        self.rfq_active()
            && self.rfq.as_ref().is_some_and(|r| {
                r.validation_contract
                    .parse::<alloy_primitives::Address>()
                    .is_ok_and(|a| !a.is_zero())
            })
            && self.pools.iter().all(|p| p.rfq_book_buildable())
            && self.pools.iter().any(|p| p.rfq_has_usable_capacity())
    }

    /// True when a leg other than the ladder and the RFQ responder has work to
    /// do. `main.rs` runs the taker and closer legs off the same feed tick but
    /// independently of both [`Self::book_enabled`] and [`Self::rfq_active`],
    /// so a bot with either one configured still trades with RFQ off.
    ///
    /// The closer needs more than its pool fields being present: `main.rs`
    /// builds the discoverer only from `subgraph_url` and puts the whole closer
    /// path behind it, and then `build_closer_pool` still has to parse the
    /// values. Either one missing is a leg that never trades. Mirror both here —
    /// this predicate decides whether Start is allowed, and claiming a dead leg
    /// is how a bot ends up "running" and silently idle.
    ///
    /// "Present" has to mean *usable*, not just non-blank. `Discoverer::new`
    /// validates nothing, so a value like `not-a-url` builds a discoverer whose
    /// every `open_positions` call fails at send time — a closer that never
    /// trades behind a panel that says running. `validate` rejects such a URL at
    /// load, and this keeps the predicate honest for configs that never got
    /// there.
    pub fn has_independent_leg(&self) -> bool {
        let closer_reachable = self
            .subgraph_url
            .as_deref()
            .is_some_and(subgraph_url_usable);
        self.pools
            .iter()
            .any(|p| p.limit_taker_enabled() || (closer_reachable && p.closer_runnable()))
    }

    /// Settings RFQ card is always visible. Tokens on disk are ignored.
    pub fn rfq_panel_unlocked(&self) -> bool {
        true
    }

    /// RFQ-as-default is always on. New bots are RFQ-only; leftover book
    /// bots see the migrate nudge. Tokens on disk are ignored.
    pub fn rfq_default_unlocked(&self) -> bool {
        true
    }

    fn validate(&self) -> anyhow::Result<()> {
        assert_feed_url(&self.feed.url, "[feed].url")?;
        // `Discoverer::new` takes this string as-is and validates nothing, so a
        // typo'd endpoint only surfaces as a failed send on every closer tick —
        // a leg that silently never trades. Catch it at load instead.
        if let Some(url) = self.subgraph_url.as_deref() {
            if !url.trim().is_empty() {
                assert_subgraph_url(url, "subgraph_url")?;
            }
        }
        for (idx, pool) in self.pools.iter().enumerate() {
            if let Some(url) = &pool.feed_url {
                assert_feed_url(url, &format!("pools[{idx}].feed_url"))?;
            }
            anyhow::ensure!(
                pool.ttl_secs > LIVE_ORDER_DEADLINE_MARGIN_SECS,
                "pools[{idx}].ttl_secs ({}) must be greater than the live-order deadline \
                 margin ({LIVE_ORDER_DEADLINE_MARGIN_SECS}s) — the indexer and stitch's own \
                 slot reuse only serve orders whose deadline is later than chain time plus \
                 that margin, so a shorter TTL posts orders that never appear as fillable depth",
                pool.ttl_secs
            );
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
            // The absolute spread's other representation. A NaN or negative
            // offset passes `spread_from` and then prices to NaN or the wrong
            // side, which `is_price_usable` drops — configured, never quotable.
            for (field, value) in [
                ("buy_offset_abs", pool.buy_offset_abs),
                ("sell_offset_abs", pool.sell_offset_abs),
            ] {
                if let Some(value) = value {
                    anyhow::ensure!(
                        value.is_finite() && value >= 0.0,
                        "pools[{idx}].{field} must be a finite, non-negative number, got {value}"
                    );
                }
            }
            // Zero batch limits are configured-but-inert: `.take(0)` yields an
            // empty batch every tick, so the leg reads as on and never acts.
            // Reject at load rather than teaching each predicate about them —
            // the bot binary has the same problem and no Start guard.
            for (field, value) in [
                ("limit_taker_max_orders", pool.limit_taker_max_orders),
                ("max_positions_per_fill", pool.max_positions_per_fill),
                ("discover_first", pool.discover_first),
            ] {
                if let Some(value) = value {
                    anyhow::ensure!(
                        value > 0,
                        "pools[{idx}].{field} must be positive; 0 makes every batch empty. \
                         Omit it for the default."
                    );
                }
            }
            // In-window the closer ramps floor→buffer via `buffer_ray -
            // floor_ray`, an unsigned subtraction. A floor above the buffer
            // underflows on the first position still inside its window, so
            // this is a crash, not a mispriced fill.
            if let (Some(floor), Some(buffer)) = (&pool.floor_ray, &pool.buffer_ray) {
                if let (Ok(floor), Ok(buffer)) =
                    (floor.trim().parse::<U256>(), buffer.trim().parse::<U256>())
                {
                    anyhow::ensure!(
                        floor <= buffer,
                        "pools[{idx}].floor_ray ({floor}) must not exceed buffer_ray ({buffer}); \
                         the closer fee ramps from the floor up to the buffer"
                    );
                }
            }
            // `fee_and_principal` divides by the closer window, so zero is a
            // panic on the first discovered position — not a misconfiguration
            // that merely quotes nothing. Same bar as `twap_window_secs` below.
            if let Some(window) = pool.window_secs {
                anyhow::ensure!(
                    window > 0,
                    "pools[{idx}].window_secs must be positive; the closer divides by it"
                );
            }
            // At 10_000 bps the bid collapses to zero, which `is_price_usable`
            // drops — the side is configured but can never produce a quote.
            // Mirrors the exclusive bound on `twap_max_deviation_bps`.
            if let Some(bps) = pool.buy_offset_bps {
                anyhow::ensure!(
                    bps < 10_000,
                    "pools[{idx}].buy_offset_bps {bps} must be below 10000; at 10000 the bid \
                     prices at zero and no quote is usable"
                );
            }
            if let Some(window) = pool.twap_window_secs {
                anyhow::ensure!(
                    window > 0,
                    "pools[{idx}].twap_window_secs must be positive; omit it to quote off the \
                     instantaneous feed"
                );
            }
            if let Some(dev) = pool.twap_max_deviation_bps {
                anyhow::ensure!(
                    pool.twap_window_secs.is_some(),
                    "pools[{idx}].twap_max_deviation_bps only applies with twap_window_secs set"
                );
                anyhow::ensure!(
                    dev > 0,
                    "pools[{idx}].twap_max_deviation_bps must be positive — 0 would pin every \
                     quote to the instantaneous feed and disable the TWAP center entirely"
                );
                anyhow::ensure!(
                    dev < MAX_TWAP_MAX_DEVIATION_BPS,
                    "pools[{idx}].twap_max_deviation_bps ({dev}) must be < {MAX_TWAP_MAX_DEVIATION_BPS} \
                     — at 10000 bps the ask floor collapses to ≤ 0 and the spot-deviation guard \
                     silently disables itself"
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
        if let Some(signer) = &self.signer {
            match signer {
                crate::signer::SignerConfig::Turnkey(c) => {
                    crate::signer::validate_signer_api_base_url("turnkey", &c.api_base_url)?;
                }
                crate::signer::SignerConfig::Mpcvault(c) => {
                    crate::signer::validate_signer_api_base_url("mpcvault", &c.api_base_url)?;
                }
                crate::signer::SignerConfig::Local => {}
            }
        }
        self.validate_rfq()?;
        Ok(())
    }

    /// RFQ cross-field rules. All of them are scoped to configs that actually
    /// turn the responder on, so a pre-RFQ config can never trip them.
    fn validate_rfq(&self) -> anyhow::Result<()> {
        let Some(rfq) = &self.rfq else { return Ok(()) };
        if !rfq.enabled {
            return Ok(());
        }
        assert_rfq_stream_url(&rfq.url)?;
        anyhow::ensure!(!rfq.maker_id.trim().is_empty(), "[rfq].maker_id is empty");
        anyhow::ensure!(
            !rfq.api_key_env.trim().is_empty(),
            "[rfq].api_key_env is empty"
        );
        let validation_contract = rfq
            .validation_contract
            .parse::<alloy_primitives::Address>()
            .context("[rfq].validation_contract is not a valid address")?;
        // Zero parses but is not the validator. The venue's `validateReply`
        // requires the deployed preferred-filler contract and rejects every
        // reply signed against anything else as `unbound_order`, so a zero here
        // is a responder that connects, publishes levels, and never lands a
        // quote. The commented `[rfq]` block in the shipped templates carries
        // the zero address as a placeholder, so uncommenting it lands exactly
        // here — with a message naming the field instead of silence.
        anyhow::ensure!(
            !validation_contract.is_zero(),
            "[rfq].validation_contract is the zero address — the venue rejects \
             every reply signed against it as `unbound_order`, so the responder \
             would run and never land a quote. Connect writes the real address; \
             don't fill this in by hand"
        );
        for (idx, pool) in self.pools.iter().enumerate() {
            if let Some(slug) = &pool.rfq_corridor {
                anyhow::ensure!(
                    !slug.trim().is_empty(),
                    "pools[{idx}].rfq_corridor is empty"
                );
            }
            // Surface unparseable capacity at load time. `"max"` is valid —
            // it means live wallet, resolved at runtime.
            pool.rfq_buy_capacity_debt()
                .with_context(|| format!("pools[{idx}] RFQ capacity"))?;
            pool.rfq_sell_capacity_collateral()
                .with_context(|| format!("pools[{idx}] RFQ capacity"))?;
        }
        Ok(())
    }
}

/// Historical parser for a fleet `panel.toml` token. Always true now —
/// RFQ-as-default does not depend on a file.
pub fn toml_has_rfq_default_gate(_toml: &str) -> bool {
    true
}

/// Fleet-wide RFQ-default is always on. `{dir}/panel.toml` is ignored.
pub fn rfq_default_flag_in_dir(_dir: &std::path::Path) -> bool {
    true
}

/// Stamp a fresh corridor template as RFQ-only. Always.
pub fn rfq_default_preset_applies(_cfg: Option<&Config>, _bots_dir: &std::path::Path) -> bool {
    true
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
            url = "https://x"
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
            ttl_secs = 60
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
            url = "https://x"
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
            ttl_secs = 60
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
        url = "https://x"
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
    fn refresh_threshold_defaults_to_requoting_every_tick() {
        let toml = LEAN_POOL_BASE.replace("refresh_threshold_bps = 10", "");
        let cfg = Config::from_toml(&toml).expect("threshold is optional");
        assert_eq!(cfg.pools[0].refresh_threshold_bps, 0);
    }

    #[test]
    fn twap_defaults_to_off_with_a_default_deviation_cap() {
        let cfg = Config::from_toml(LEAN_POOL_BASE).unwrap();
        assert_eq!(cfg.pools[0].twap_window(), None);
        assert_eq!(
            cfg.pools[0].twap_deviation_bps(),
            DEFAULT_TWAP_MAX_DEVIATION_BPS
        );
    }

    #[test]
    fn twap_window_and_deviation_parse_together() {
        let toml =
            format!("{LEAN_POOL_BASE}\ntwap_window_secs = 180\ntwap_max_deviation_bps = 80\n");
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(cfg.pools[0].twap_window(), Some(180));
        assert_eq!(cfg.pools[0].twap_deviation_bps(), 80);
    }

    #[test]
    fn a_zero_twap_window_is_rejected() {
        let toml = format!("{LEAN_POOL_BASE}\ntwap_window_secs = 0\n");
        let err = Config::from_toml(&toml).expect_err("zero window is rejected");
        assert!(err.to_string().contains("twap_window_secs"));
    }

    #[test]
    fn a_twap_deviation_without_a_window_is_rejected() {
        let toml = format!("{LEAN_POOL_BASE}\ntwap_max_deviation_bps = 50\n");
        let err = Config::from_toml(&toml).expect_err("deviation needs the window");
        assert!(err.to_string().contains("twap_window_secs"));
    }

    #[test]
    fn a_zero_twap_deviation_is_rejected() {
        let toml =
            format!("{LEAN_POOL_BASE}\ntwap_window_secs = 180\ntwap_max_deviation_bps = 0\n");
        let err = Config::from_toml(&toml).expect_err("zero deviation is rejected");
        assert!(err.to_string().contains("twap_max_deviation_bps"));
    }

    #[test]
    fn a_twap_deviation_at_or_above_10000_bps_is_rejected() {
        // 10000 bps collapses ask_floor to ≤ 0 and silently disables the guard.
        let toml =
            format!("{LEAN_POOL_BASE}\ntwap_window_secs = 60\ntwap_max_deviation_bps = 10000\n");
        let err = Config::from_toml(&toml).expect_err("10000 bps deviation is rejected");
        assert!(err.to_string().contains("twap_max_deviation_bps"));
    }

    #[test]
    fn a_ttl_at_or_below_the_live_deadline_margin_is_rejected() {
        // The indexer only serves orders with deadline > chain_time + 30s.
        // ttl == 30 posts orders that are immediately invisible; ttl == 15
        // (the short-ETH temptation) is the same footgun.
        for ttl in [0_u64, 15, LIVE_ORDER_DEADLINE_MARGIN_SECS] {
            let toml = LEAN_POOL_BASE.replace("ttl_secs = 120", &format!("ttl_secs = {ttl}"));
            let err =
                Config::from_toml(&toml).expect_err(&format!("ttl_secs = {ttl} must be rejected"));
            let msg = err.to_string();
            assert!(
                msg.contains("ttl_secs") && msg.contains("deadline margin"),
                "unexpected error for ttl={ttl}: {msg}"
            );
        }
    }

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
    fn the_example_config_is_rfq_only_with_the_responder_off() {
        // Shipped example: book off, [rfq] commented so Connect can write it.
        // The Settings card is always unlocked.
        let cfg = Config::from_toml(EXAMPLE).expect("example config parses");
        assert!(cfg.rfq.is_none());
        assert!(!cfg.rfq_active());
        assert!(!cfg.book_enabled);
        assert!(cfg.rfq_panel_unlocked());
        assert!(cfg.rfq_default_unlocked());
        assert!(cfg.pools.iter().all(|p| p.rfq_corridor.is_none()));
        let book = EXAMPLE
            .find("book_enabled = false")
            .expect("example must set book_enabled = false");
        let feed = EXAMPLE.find("\n[feed]\n").expect("example has [feed]");
        assert!(
            book < feed,
            "book_enabled must be a root key, before [feed] — after [[pools]] it is ignored"
        );
    }

    #[test]
    fn rfq_panel_and_default_are_always_on() {
        let cfg = Config::from_toml(LEAN_POOL_BASE).unwrap();
        assert!(cfg.rfq_panel_unlocked());
        assert!(cfg.rfq_default_unlocked());
        assert!(toml_has_rfq_default_gate(""));
        assert!(rfq_default_preset_applies(
            Some(&cfg),
            &std::env::temp_dir()
        ));
    }

    #[test]
    fn has_independent_leg_tracks_what_main_actually_runs() {
        // Neither leg configured.
        let bare = Config::from_toml(LEAN_POOL_BASE).unwrap();
        assert!(!bare.has_independent_leg());

        // The taker leg needs nothing beyond its own pool fields.
        let taker =
            Config::from_toml(&format!("{LEAN_POOL_BASE}\nlimit_taker_enabled = true\n")).unwrap();
        assert!(taker.has_independent_leg());

        // Closer fields with no subgraph: `main.rs` never builds the
        // discoverer, so the whole closer path is dead and Start must not
        // treat it as a live leg.
        const CLOSER_FIELDS: &str = concat!(
            "\ncloser_pool = \"0x0000000000000000000000000000000000000003\"\n",
            "floor_ray = \"1000000000000000000000000000\"\n",
            "buffer_ray = \"1000000000000000000000000000\"\n",
            "window_secs = 60\n"
        );
        let orphan_closer = Config::from_toml(&format!("{LEAN_POOL_BASE}{CLOSER_FIELDS}")).unwrap();
        assert!(orphan_closer.pools[0].closer_enabled());
        assert!(
            !orphan_closer.has_independent_leg(),
            "closer without subgraph_url is a leg that never runs"
        );

        // With the subgraph the discoverer exists and the leg is real.
        let with_subgraph = |closer: &str| {
            Config::from_toml(&format!(
                "{}{closer}",
                LEAN_POOL_BASE.replace(
                    "tick_interval_secs = 5",
                    "tick_interval_secs = 5\nsubgraph_url = \"https://subgraph\""
                )
            ))
            .unwrap()
        };
        assert!(with_subgraph(CLOSER_FIELDS).has_independent_leg());

        // A blank subgraph_url is the same as none.
        let blank = Config::from_toml(&format!(
            "{}{CLOSER_FIELDS}",
            LEAN_POOL_BASE.replace(
                "tick_interval_secs = 5",
                "tick_interval_secs = 5\nsubgraph_url = \"  \""
            )
        ))
        .unwrap();
        assert!(!blank.has_independent_leg());

        // Present but unparseable values: `build_closer_pool` rejects them on
        // every tick, so the leg is just as dead as a missing subgraph.
        // (An empty ray is not one of these — alloy parses "" as zero, so
        // `build_closer_pool` accepts it and the leg does run, just with a
        // zero floor. Only genuinely unparseable values kill it.)
        for bad in [
            CLOSER_FIELDS.replace(
                "floor_ray = \"1000000000000000000000000000\"",
                "floor_ray = \"  \"",
            ),
            CLOSER_FIELDS.replace(
                "buffer_ray = \"1000000000000000000000000000\"",
                "buffer_ray = \"not-a-number\"",
            ),
            CLOSER_FIELDS.replace(
                "closer_pool = \"0x0000000000000000000000000000000000000003\"",
                "closer_pool = \"0xnope\"",
            ),
        ] {
            let cfg = with_subgraph(&bad);
            assert!(
                cfg.pools[0].closer_enabled(),
                "presence must still hold so main.rs keeps warning: {bad}"
            );
            assert!(
                !cfg.has_independent_leg(),
                "unparseable closer must not count as a live leg: {bad}"
            );
        }
    }

    #[test]
    fn a_pool_whose_tokens_do_not_parse_runs_no_leg_at_all() {
        // `main.rs` resolves debt + collateral at the top of the pool tick and
        // `continue`s past the whole pool when either fails, so neither the
        // taker nor the closer ever reaches such a pool.
        for broken in [
            LEAN_POOL_BASE.replace(
                "collateral = \"0x0000000000000000000000000000000000000001\"",
                "collateral = \"0xnope\"",
            ),
            LEAN_POOL_BASE.replace(
                "debt = \"0x0000000000000000000000000000000000000002\"",
                "debt = \"not-an-address\"",
            ),
        ] {
            let cfg =
                Config::from_toml(&format!("{broken}\nlimit_taker_enabled = true\n")).unwrap();
            assert!(!cfg.pools[0].tokens_parse());
            assert!(
                !cfg.pools[0].limit_taker_enabled(),
                "the taker leg never runs on a pool main.rs skips"
            );
            assert!(!cfg.has_independent_leg());
        }
    }

    #[test]
    fn non_finite_absolute_offsets_are_rejected_at_load() {
        // The bps path is bounded a few lines up; this is the other spread
        // representation. NaN prices to NaN and negative prices the wrong side
        // — `is_price_usable` drops both, so the side never quotes.
        for field in ["buy_offset_abs", "sell_offset_abs"] {
            for bad in ["nan", "-1.0", "inf"] {
                let toml = LEAN_POOL_BASE.replace(
                    "refresh_threshold_bps = 10",
                    &format!("refresh_threshold_bps = 10\n{field} = {bad}"),
                );
                let err = Config::from_toml(&toml).unwrap_err().to_string();
                assert!(err.contains(field), "{field} = {bad}: {err}");
            }
            // Zero is a legitimate spread: quote at the mid.
            let ok = LEAN_POOL_BASE.replace(
                "refresh_threshold_bps = 10",
                &format!("refresh_threshold_bps = 10\n{field} = 0.0"),
            );
            assert!(Config::from_toml(&ok).is_ok(), "{field} = 0.0 must load");
        }
    }

    #[test]
    fn zero_batch_limits_are_rejected_at_load() {
        // `.take(0)` returns an empty batch forever, so these read as a live
        // leg while nothing can ever be filled or closed.
        for field in [
            "limit_taker_max_orders",
            "max_positions_per_fill",
            "discover_first",
        ] {
            let toml = LEAN_POOL_BASE.replace(
                "refresh_threshold_bps = 10",
                &format!("refresh_threshold_bps = 10\n{field} = 0"),
            );
            let err = Config::from_toml(&toml).unwrap_err().to_string();
            assert!(err.contains(field), "{field}: {err}");
            assert!(err.contains("positive"), "{field}: {err}");

            // A positive value, and omitting it entirely, both still load.
            let ok = toml.replace(&format!("{field} = 0"), &format!("{field} = 5"));
            assert!(Config::from_toml(&ok).is_ok(), "{field} = 5 must load");
        }
        assert!(
            Config::from_toml(LEAN_POOL_BASE).is_ok(),
            "defaults still load"
        );
    }

    #[test]
    fn a_closer_floor_above_its_buffer_is_rejected_at_load() {
        // `fee_and_principal` computes `buffer_ray - floor_ray` on U256 while
        // the position is still in its window — inverted, that underflows.
        let closer = |floor: &str, buffer: &str| {
            LEAN_POOL_BASE.replace(
                "refresh_threshold_bps = 10",
                &format!(
                    "refresh_threshold_bps = 10\n\
                     closer_pool = \"0x0000000000000000000000000000000000000003\"\n\
                     floor_ray = \"{floor}\"\n\
                     buffer_ray = \"{buffer}\"\n\
                     window_secs = 60"
                ),
            )
        };
        let err = Config::from_toml(&closer("2000", "1000"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("floor_ray"), "{err}");
        assert!(err.contains("buffer_ray"), "{err}");

        // Equal is fine — a flat fee with no ramp.
        assert!(Config::from_toml(&closer("1000", "1000")).is_ok());
        assert!(Config::from_toml(&closer("1000", "2000")).is_ok());
    }

    #[test]
    fn a_zero_closer_window_is_rejected_at_load() {
        // `fee_and_principal` divides by the window, so zero panics the bot on
        // the first discovered position. Reject it where twap_window_secs is.
        let toml = LEAN_POOL_BASE.replace(
            "refresh_threshold_bps = 10",
            concat!(
                "refresh_threshold_bps = 10\n",
                "closer_pool = \"0x0000000000000000000000000000000000000003\"\n",
                "floor_ray = \"1000000000000000000000000000\"\n",
                "buffer_ray = \"1000000000000000000000000000\"\n",
                "window_secs = 0\n"
            ),
        );
        let err = Config::from_toml(&toml).unwrap_err().to_string();
        assert!(err.contains("window_secs"), "{err}");
        assert!(err.contains("positive"), "{err}");

        // A positive window still loads.
        assert!(Config::from_toml(&toml.replace("window_secs = 0", "window_secs = 60")).is_ok());
    }

    #[test]
    fn a_bid_spread_that_prices_at_zero_is_rejected_at_load() {
        // 10_000 bps takes the bid to zero, which `is_price_usable` drops — the
        // side reads as configured but can never quote.
        let err = Config::from_toml(
            &LEAN_POOL_BASE.replace("buy_offset_bps = 1", "buy_offset_bps = 10000"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("buy_offset_bps"), "{err}");

        // Just under the bound is still a (very wide) usable bid.
        assert!(Config::from_toml(
            &LEAN_POOL_BASE.replace("buy_offset_bps = 1", "buy_offset_bps = 9999")
        )
        .is_ok());
    }

    #[test]
    fn a_zero_sized_side_is_not_postable() {
        // Presence is not a size: the ladder builder finds nothing above the
        // minimum slice and drafts no orders, so `"0"` must not read as a
        // ladder that would post.
        let cfg = Config::from_toml(LEAN_POOL_BASE).unwrap();
        assert!(cfg.pools[0].buy_postable() && cfg.pools[0].sell_postable());

        let zeroed = LEAN_POOL_BASE
            .replace(
                "buy_order_size_debt = \"1000000000\"",
                "buy_order_size_debt = \"0\"",
            )
            .replace(
                "sell_order_size_collateral = \"1000000\"",
                "sell_order_size_collateral = \"0\"",
            );
        let cfg = Config::from_toml(&zeroed).unwrap();
        assert!(
            cfg.pools[0].buy_enabled() && cfg.pools[0].sell_enabled(),
            "presence still holds — approvals and funding key off that"
        );
        assert!(!cfg.pools[0].buy_postable());
        assert!(!cfg.pools[0].sell_postable());

        // "max" is a real size: the live wallet supplies the total.
        let maxed = LEAN_POOL_BASE.replace(
            "buy_order_size_debt = \"1000000000\"",
            "buy_total_liquidity_debt = \"max\"\nbuy_min_slice_debt = \"1000000\"",
        );
        assert!(Config::from_toml(&maxed).unwrap().pools[0].buy_postable());
    }

    #[test]
    fn the_zero_address_is_not_a_token() {
        // It parses, but nothing lives there: every `balanceOf` and `allowance`
        // reads empty, so the pool funds nothing and publishes no level. A
        // half-filled template is exactly how a bot ends up here, and a Start
        // guard that called it live would report a bot quoting nothing.
        const ZERO: &str = "0x0000000000000000000000000000000000000000";
        assert!(Config::from_toml(LEAN_POOL_BASE).unwrap().pools[0].tokens_parse());
        for token in [
            "0x0000000000000000000000000000000000000001", // collateral
            "0x0000000000000000000000000000000000000002", // debt
        ] {
            let toml = LEAN_POOL_BASE.replace(token, ZERO);
            let cfg = Config::from_toml(&toml).unwrap();
            assert!(
                !cfg.pools[0].tokens_parse(),
                "a zero token is a pool that can never quote"
            );
            // Everything gated on the tokens follows: the RFQ book the
            // responder would build, and the taker leg Start counts as live.
            assert!(!cfg.pools[0].rfq_book_buildable());
            assert!(!cfg.pools[0].closer_runnable());
            let taker =
                Config::from_toml(&format!("{toml}\nlimit_taker_enabled = true\n")).unwrap();
            assert!(!taker.pools[0].limit_taker_enabled());
            assert!(!taker.has_independent_leg());
        }
    }

    #[test]
    fn the_ask_ladder_is_not_judged_in_the_wrong_unit() {
        // `sell_total_liquidity_collateral` is collateral atomic units;
        // `sell_min_slice_debt` is debt. `maker::ask_ladder_sizes` converts the
        // total at the live price *before* laddering, so comparing the two raw
        // numbers here is a category error in both directions — and config
        // cannot resolve it, because the price is a tick-time value.
        let ask = |total: &str, min: &str| {
            let toml = LEAN_POOL_BASE.replace(
                "sell_order_size_collateral = \"1000000\"",
                &format!(
                    "sell_total_liquidity_collateral = \"{total}\"\nsell_min_slice_debt = \"{min}\""
                ),
            );
            Config::from_toml(&toml)
                .unwrap_or_else(|e| panic!("config must parse: {e:#}"))
                .pools[0]
                .sell_postable()
        };

        // A total numerically under the slice is *not* dead: a high-priced
        // collateral converts to far more debt than its own count.
        assert!(
            ask("100", "1000"),
            "only the live price decides whether this clears the slice"
        );
        // What no price can rescue: nothing to sell, or no order budget. (A
        // zero slice never reaches the predicate — `validate` rejects it at
        // load — so the `min > 0` arm is belt and braces.)
        assert!(!ask("0", "1000"), "an empty total drafts nothing");
        let no_budget = LEAN_POOL_BASE.replace(
            "sell_order_size_collateral = \"1000000\"",
            "sell_total_liquidity_collateral = \"1000000\"\nsell_min_slice_debt = \"1000\"\nsell_max_orders = 0",
        );
        assert!(
            !Config::from_toml(&no_budget).unwrap().pools[0].sell_postable(),
            "a zero order budget drafts nothing at any price"
        );

        // The bid side shares a unit with its slice, so it still asks the
        // builder directly and a total under one slice really is dead.
        let bid = |total: &str, min: &str| {
            let toml = LEAN_POOL_BASE.replace(
                "buy_order_size_debt = \"1000000000\"",
                &format!("buy_total_liquidity_debt = \"{total}\"\nbuy_min_slice_debt = \"{min}\""),
            );
            Config::from_toml(&toml).unwrap().pools[0].buy_postable()
        };
        assert!(!bid("100", "1000"));
        assert!(bid("1000000", "1000"));
    }

    #[test]
    fn a_zero_closer_pool_is_not_a_closer_leg() {
        // Same as a zero token: it parses, but the subgraph has no positions in
        // it, so `close_pool_once` loops on an empty set forever.
        const CLOSER: &str = concat!(
            "\ncloser_pool = \"{POOL}\"\n",
            "floor_ray = \"1000000000000000000000000000\"\n",
            "buffer_ray = \"1000000000000000000000000000\"\n",
            "window_secs = 60\n"
        );
        let with_pool = |pool: &str| {
            let toml = format!(
                "{}{}",
                LEAN_POOL_BASE.replace(
                    "tick_interval_secs = 5",
                    "tick_interval_secs = 5\nsubgraph_url = \"https://subgraph\"",
                ),
                CLOSER.replace("{POOL}", pool)
            );
            Config::from_toml(&toml).unwrap()
        };
        assert!(with_pool("0x0000000000000000000000000000000000000003").has_independent_leg());
        let zero = with_pool("0x0000000000000000000000000000000000000000");
        assert!(zero.pools[0].closer_enabled(), "presence still holds");
        assert!(!zero.pools[0].closer_runnable());
        assert!(
            !zero.has_independent_leg(),
            "a closer pointed at the zero address never trades"
        );
    }

    #[test]
    fn a_flat_size_above_u128_is_not_postable() {
        // `maker.rs` runs the flat size through `parse_input_liquidity`, which
        // is `u128`-bounded and drops the side on overflow. A value that only
        // parses as `U256` is therefore a side that draws no orders — and for a
        // book-only bot, a Start guard that called it postable would report a
        // running bot posting nothing.
        let over = "340282366920938463463374607431768211456"; // u128::MAX + 1
        let toml = LEAN_POOL_BASE.replace(
            "buy_order_size_debt = \"1000000000\"",
            &format!("buy_order_size_debt = \"{over}\""),
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert!(
            !cfg.pools[0].buy_postable(),
            "a flat size the quote path cannot parse is not a live ladder"
        );
        // The boundary itself still posts.
        let at_max = LEAN_POOL_BASE.replace(
            "buy_order_size_debt = \"1000000000\"",
            &format!("buy_order_size_debt = \"{}\"", u128::MAX),
        );
        assert!(Config::from_toml(&at_max).unwrap().pools[0].buy_postable());
    }

    #[test]
    fn an_rfq_side_needs_its_spread_and_its_capacity() {
        // `levels_for` destructures `(spread, capacity)` per side and skips the
        // side when either is missing, so a config with buy capacity but no buy
        // spread must not read as quotable. The capacity accessors enforce that
        // themselves — no spread, no capacity — which is what keeps
        // `rfq_has_usable_capacity` honest without re-checking spreads.
        let base = LEAN_POOL_BASE
            .replace("buy_offset_bps = 1\n", "")
            .replace("buy_order_size_debt = \"1000000000\"\n", "")
            .replace("sell_offset_bps = 1\n", "")
            .replace("sell_order_size_collateral = \"1000000\"\n", "");
        let cfg = |extra: &str| {
            Config::from_toml(&format!("{base}{extra}"))
                .unwrap_or_else(|e| panic!("config must parse: {e:#}"))
        };

        // Size with no spread on the same side: the accessor reports no
        // capacity at all, so the side cannot claim to be live.
        let spreadless = cfg("buy_order_size_debt = \"1000000\"\n");
        assert!(matches!(
            spreadless.pools[0].rfq_buy_capacity_debt(),
            Ok(None)
        ));
        assert!(!spreadless.pools[0].rfq_has_usable_capacity());

        // Buy size without buy spread, plus a sell spread with no sell size:
        // neither side is whole, so neither publishes.
        assert!(
            !cfg("buy_order_size_debt = \"1000000\"\nsell_offset_bps = 1\n").pools[0]
                .rfq_has_usable_capacity()
        );

        // A spread with no size is not a side either.
        assert!(!cfg("buy_offset_bps = 1\n").pools[0].rfq_has_usable_capacity());

        // One complete side is enough.
        assert!(
            cfg("buy_offset_bps = 1\nbuy_order_size_debt = \"1000000\"\n").pools[0]
                .rfq_has_usable_capacity()
        );
        assert!(
            cfg("sell_offset_bps = 1\nsell_order_size_collateral = \"1000000\"\n").pools[0]
                .rfq_has_usable_capacity()
        );
    }

    #[test]
    fn a_zero_validation_contract_is_not_quotable() {
        // The venue's `validateReply` requires the deployed preferred-filler
        // validator and rejects anything else as `unbound_order`, so a zero
        // address is a responder that connects, publishes levels, and never
        // lands a quote — the panel would call it running.
        const RFQ: &str = concat!(
            "\n[rfq]\nenabled = true\n",
            "url = \"wss://api.textilecredit.com/v2/maker/stream\"\n",
            "maker_id = \"mk_test\"\n",
            "validation_contract = \"{ADDR}\"\n"
        );
        let with_contract = |addr: &str| format!("{LEAN_POOL_BASE}{}", RFQ.replace("{ADDR}", addr));

        let real = "0x00000000000000000000000000000000000000aa";
        let cfg = Config::from_toml(&with_contract(real)).expect("a real validator loads");
        assert!(cfg.rfq_quotable());

        // The shipped templates carry the zero address in their commented
        // `[rfq]` block, so uncommenting it lands here.
        let zero = "0x0000000000000000000000000000000000000000";
        let err = Config::from_toml(&with_contract(zero))
            .expect_err("a zero validator must not load")
            .to_string();
        assert!(err.contains("validation_contract"), "{err}");
        assert!(err.contains("unbound_order"), "{err}");

        // And the predicate agrees for a config that never went through
        // `validate` — the panel's Start guard reads it directly.
        let mut hand_edited = cfg;
        hand_edited.rfq.as_mut().unwrap().validation_contract = zero.to_string();
        assert!(
            !hand_edited.rfq_quotable(),
            "a bot that can never land a quote is not a live RFQ leg"
        );
    }

    #[test]
    fn an_unusable_subgraph_url_is_not_a_closer_leg() {
        const CLOSER_FIELDS: &str = concat!(
            "\ncloser_pool = \"0x0000000000000000000000000000000000000003\"\n",
            "floor_ray = \"1000000000000000000000000000\"\n",
            "buffer_ray = \"1000000000000000000000000000\"\n",
            "window_secs = 60\n"
        );
        let with_url = |url: &str| {
            format!(
                "{}{CLOSER_FIELDS}",
                LEAN_POOL_BASE.replace(
                    "tick_interval_secs = 5",
                    &format!("tick_interval_secs = 5\nsubgraph_url = \"{url}\""),
                )
            )
        };

        // `Discoverer::new` validates nothing, so a non-URL would build a
        // discoverer whose every request fails at send time. Refuse it at load
        // rather than run a closer that can never trade.
        let err = Config::from_toml(&with_url("not-a-url"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subgraph_url"),
            "load must name the bad field, got {err}"
        );
        assert!(Config::from_toml(&with_url("ftp://subgraph")).is_err());
        // Remote cleartext is the feed's MITM exposure, same answer.
        assert!(Config::from_toml(&with_url("http://subgraph")).is_err());

        // The predicate agrees for a config that never went through validate.
        let mut cfg = Config::from_toml(&with_url("https://subgraph")).unwrap();
        assert!(cfg.has_independent_leg());
        cfg.subgraph_url = Some("not-a-url".to_string());
        assert!(
            !cfg.has_independent_leg(),
            "an unreachable subgraph is not a leg Start may count"
        );
    }

    #[test]
    fn a_ladder_that_drafts_no_orders_is_not_postable() {
        // `balanced_ladder` returns nothing when the total is under one slice,
        // when the slice is zero, or when the order budget is zero. The
        // predicate asks the builder rather than re-deriving those rules.
        let ladder = |total: &str, min: &str, max_orders: &str| {
            let toml = LEAN_POOL_BASE.replace(
                "buy_order_size_debt = \"1000000000\"",
                &format!(
                    "buy_total_liquidity_debt = \"{total}\"\nbuy_min_slice_debt = \"{min}\"\n{max_orders}"
                ),
            );
            Config::from_toml(&toml)
                .unwrap_or_else(|e| panic!("config must parse: {e:#}"))
                .pools[0]
                .buy_postable()
        };

        assert!(ladder("1000000", "1000", ""), "a real ladder posts");
        assert!(
            !ladder("100", "1000", ""),
            "a total under one slice drafts nothing"
        );
        assert!(
            !ladder("1000000", "1000", "buy_max_orders = 0"),
            "a zero order budget drafts nothing"
        );
        // `balanced_ladder`'s third dead case, a zero minimum slice, can't get
        // this far — `parse_min_slice_debt` rejects it at load.
        let zero_slice = LEAN_POOL_BASE.replace(
            "buy_order_size_debt = \"1000000000\"",
            "buy_total_liquidity_debt = \"1000000\"\nbuy_min_slice_debt = \"0\"",
        );
        assert!(Config::from_toml(&zero_slice)
            .unwrap_err()
            .to_string()
            .contains("greater than zero"));
    }

    #[test]
    fn one_unbuildable_pool_makes_the_whole_bot_unquotable() {
        // `build_runtime` collects `book_from_pool` over every pool with `?`,
        // so a malformed extra pool aborts the responder even though it has no
        // RFQ sides of its own. One good pool is not enough.
        let second = LEAN_POOL_BASE
            .split("[[pools]]")
            .nth(1)
            .expect("base has a pool");
        let two_pools = format!("{LEAN_POOL_BASE}\n[[pools]]{second}\n{RFQ_BLOCK}");
        assert!(Config::from_toml(&two_pools).unwrap().rfq_quotable());

        let broken_second = format!(
            "{LEAN_POOL_BASE}\n[[pools]]{}\n{RFQ_BLOCK}",
            second.replace(
                "collateral = \"0x0000000000000000000000000000000000000001\"",
                "collateral = \"0xnope\"",
            )
        );
        let cfg = Config::from_toml(&broken_second).unwrap();
        assert!(cfg.pools[0].rfq_has_usable_capacity(), "pool 1 is fine");
        assert!(!cfg.pools[1].rfq_book_buildable(), "pool 2 is not");
        assert!(
            !cfg.rfq_quotable(),
            "a pool that aborts build_runtime takes the whole responder with it"
        );
    }

    #[test]
    fn a_zero_rfq_capacity_is_not_quotable() {
        // "0" resolves to Some(Exact(0)): present, but the responder omits the
        // level and rejects every request against it.
        let zero_buy = LEAN_POOL_BASE.replace("sell_offset_bps = 1", "").replace(
            "buy_order_size_debt = \"1000000000\"",
            "buy_total_liquidity_debt = \"0\"",
        );
        let cfg = Config::from_toml(&format!("{zero_buy}\n{RFQ_BLOCK}")).unwrap();
        assert!(matches!(
            cfg.pools[0].rfq_buy_capacity_debt(),
            Ok(Some(RfqCapacity::Exact(_)))
        ));
        assert!(cfg.rfq_active());
        assert!(
            !cfg.rfq_quotable(),
            "a zero capacity is the same as no side at all"
        );
    }

    #[test]
    fn rfq_quotable_needs_a_side_that_can_actually_answer() {
        // `rfq_active` only asks whether the responder spawns. A pool with no
        // spread yields a CorridorBook with both capacities None: it publishes
        // no usable levels and rejects every request.
        let with_rfq =
            |pool: &str| Config::from_toml(&format!("{pool}\n{RFQ_BLOCK}")).expect("config parses");
        assert!(with_rfq(LEAN_POOL_BASE).rfq_quotable());

        let no_spreads = LEAN_POOL_BASE
            .replace("buy_offset_bps = 1", "")
            .replace("sell_offset_bps = 1", "");
        let cfg = with_rfq(&no_spreads);
        assert!(cfg.rfq_active(), "the responder would still spawn");
        assert!(
            !cfg.rfq_quotable(),
            "but it can answer nothing, so Start must not count it"
        );
    }

    #[test]
    fn book_enabled_defaults_on_and_can_be_turned_off() {
        let cfg = Config::from_toml(LEAN_POOL_BASE).unwrap();
        assert!(cfg.book_enabled, "omitted book_enabled must stay on");

        // Root-level key — appending after [[pools]] would land on the pool.
        let off = LEAN_POOL_BASE.replace(
            "tick_interval_secs = 5",
            "tick_interval_secs = 5\nbook_enabled = false",
        );
        assert!(!Config::from_toml(&off).unwrap().book_enabled);
    }

    #[test]
    fn leftover_gate_tokens_still_parse() {
        let toml = format!(
            "{LEAN_POOL_BASE}\n[experimental]\nrfq_panel = \"nope\"\nrfq_default = \"nope\"\n"
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert!(cfg.rfq_panel_unlocked());
        assert!(cfg.rfq_default_unlocked());
    }

    const RFQ_BLOCK: &str = r#"
        [rfq]
        enabled = true
        url = "wss://api.textilecredit.com/v2/maker/stream"
        maker_id = "mk_test"
        validation_contract = "0x00000000000000000000000000000000000000aa"
    "#;

    #[test]
    fn rfq_is_inert_unless_enabled() {
        // Block present but disabled → inactive.
        let toml = format!(
            "{LEAN_POOL_BASE}\n{}",
            RFQ_BLOCK.replace("enabled = true", "enabled = false")
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert!(!cfg.rfq_active());

        // Enabled + a pool → active. No rfq_corridor label required.
        let toml = format!("{LEAN_POOL_BASE}\n{RFQ_BLOCK}");
        let cfg = Config::from_toml(&toml).unwrap();
        assert!(cfg.rfq_active());
        assert_eq!(
            cfg.rfq.as_ref().unwrap().api_key_env,
            "STITCH_RFQ_API_KEY",
            "the api key env var name has a default"
        );
    }

    #[test]
    fn an_enabled_rfq_block_is_validated() {
        let base = format!(
            "{}\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE
        );
        let bad_url = base.replace("wss://api.textilecredit.com/v2/maker/stream", "https://x");
        assert!(Config::from_toml(&bad_url)
            .unwrap_err()
            .to_string()
            .contains("[rfq].url"));

        let remote_ws = base.replace(
            "wss://api.textilecredit.com/v2/maker/stream",
            "ws://api.textilecredit.com/v2/maker/stream",
        );
        let err = Config::from_toml(&remote_ws).unwrap_err().to_string();
        assert!(
            err.contains("localhost"),
            "remote ws:// must be rejected, got {err}"
        );

        let local_ws = base.replace(
            "wss://api.textilecredit.com/v2/maker/stream",
            "ws://localhost:10000/v2/maker/stream",
        );
        assert!(Config::from_toml(&local_ws).is_ok());

        let bad_contract = base.replace(
            "0x00000000000000000000000000000000000000aa",
            "not-an-address",
        );
        assert!(
            format!("{:#}", Config::from_toml(&bad_contract).unwrap_err())
                .contains("validation_contract")
        );
    }

    #[test]
    fn rfq_capacity_accepts_max_as_live_wallet() {
        // The LEAN_POOL_BASE pool sizes both sides with flat order sizes, so
        // those are exact capacities.
        let toml = format!(
            "{}\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE
        );
        let cfg = Config::from_toml(&toml).unwrap();
        let pool = &cfg.pools[0];
        assert_eq!(
            pool.rfq_buy_capacity_debt().unwrap(),
            Some(RfqCapacity::Exact(U256::from(1_000_000_000u64)))
        );
        assert_eq!(
            pool.rfq_sell_capacity_collateral().unwrap(),
            Some(RfqCapacity::Exact(U256::from(1_000_000u64)))
        );

        // A ladder total wins over the flat size when both are set.
        let toml = format!(
            "{}\nbuy_total_liquidity_debt = \"5000000000\"\nbuy_min_slice_debt = \"10000000\"\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(
            cfg.pools[0].rfq_buy_capacity_debt().unwrap(),
            Some(RfqCapacity::Exact(U256::from(5_000_000_000u64)))
        );

        // `max` is the live-wallet policy — same sentinel the ladder uses.
        let toml = format!(
            "{}\nbuy_total_liquidity_debt = \"max\"\nbuy_min_slice_debt = \"10000000\"\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(
            cfg.pools[0].rfq_buy_capacity_debt().unwrap(),
            Some(RfqCapacity::Wallet)
        );

        // A side with no spread simply doesn't quote over RFQ.
        let toml = format!(
            "{}\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE.replace("sell_offset_bps = 1", "")
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(cfg.pools[0].rfq_sell_capacity_collateral().unwrap(), None);
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
            url = "https://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x0000000000000000000000000000000000000001"
            collateral_decimals = 18
            debt = "0x0000000000000000000000000000000000000002"
            debt_decimals = 6
            buy_offset_bps = 150
            buy_order_size_debt = "1000000000"
            ttl_secs = 60
            refresh_threshold_bps = 10
        "#;
        let cfg = Config::from_toml(toml).expect("buy-only config parses");
        assert!(cfg.pools[0].buy_enabled());
        assert!(!cfg.pools[0].sell_enabled());
    }

    #[test]
    fn remote_cleartext_feed_urls_are_rejected() {
        assert!(assert_feed_url("https://api.textilecredit.com/price", "[feed].url").is_ok());
        assert!(assert_feed_url("http://localhost/feed", "[feed].url").is_ok());
        assert!(assert_feed_url("http://127.0.0.1/feed", "[feed].url").is_ok());
        assert!(assert_feed_url("http://[::1]/feed", "[feed].url").is_ok());
        let err = assert_feed_url("http://8.8.8.8/feed", "[feed].url")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("localhost"),
            "remote http:// must be rejected, got {err}"
        );
        assert!(assert_feed_url("http://192.168.1.10/feed", "[feed].url").is_err());

        let toml = LEAN_POOL_BASE.replace("url = \"https://x\"", "url = \"http://8.8.8.8/feed\"");
        let err = Config::from_toml(&toml).unwrap_err().to_string();
        assert!(
            err.contains("[feed].url"),
            "config load must reject a remote cleartext feed, got {err}"
        );
    }

    fn feed(staleness_secs: u64, rfq_staleness: Option<u64>) -> FeedConfig {
        FeedConfig {
            url: "https://x".into(),
            staleness_secs,
            rfq_staleness_secs: rfq_staleness,
        }
    }

    #[test]
    fn rfq_caps_staleness_without_rejecting_a_900s_template() {
        // A 900s ladder template that says nothing about RFQ gets the tight
        // default, not the ceiling. This is the whole point of the per-feed
        // split: raising the ceiling for cNGN must not widen anything else.
        assert_eq!(
            rfq_staleness_secs(&feed(900, None)),
            RFQ_DEFAULT_STALENESS_SECS
        );
        // A feed that opts in gets what it asked for...
        assert_eq!(rfq_staleness_secs(&feed(900, Some(240))), 240);
        // ...but never past the ceiling.
        assert_eq!(
            rfq_staleness_secs(&feed(900, Some(3_600))),
            RFQ_MAX_STALENESS_SECS
        );
        // And the ladder's own window still wins when it is tighter — quoting
        // firm off a mark the ladder itself considers dead makes no sense.
        assert_eq!(rfq_staleness_secs(&feed(30, Some(240))), 30);
        assert_eq!(rfq_staleness_secs(&feed(30, None)), 30);

        let toml = format!(
            "{}\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE.replace("staleness_secs = 30", "staleness_secs = 900")
        );
        let cfg = Config::from_toml(&toml).unwrap();
        assert_eq!(cfg.feed.staleness_secs, 900);
        assert_eq!(cfg.feed.rfq_staleness_secs, None);
        assert_eq!(
            rfq_staleness_secs(&cfg.feed),
            RFQ_DEFAULT_STALENESS_SECS,
            "a template that never mentions rfq_staleness_secs must stay tight"
        );
        assert!(cfg.rfq_active());
    }

    /// Bots deployed before `rfq_staleness_secs` existed have configs that
    /// never mention it, and upgrading Stitch does not rewrite a mounted
    /// `stitch.toml`. Without an inferred default those makers would take the
    /// tight 60s on upgrade and go dark between samples — the original bug,
    /// reintroduced by the fix for it.
    #[test]
    fn an_existing_cngn_config_keeps_its_window_without_being_edited() {
        let cngn = "https://api.textilecredit.com/price?chainId=56&pair=cngn-usdt";
        assert_eq!(
            rfq_staleness_secs(&feed(900, None)),
            RFQ_DEFAULT_STALENESS_SECS,
            "a feed we know nothing about still gets the tight window"
        );
        assert_eq!(
            rfq_staleness_secs(&FeedConfig {
                url: cngn.into(),
                staleness_secs: 900,
                rfq_staleness_secs: None,
            }),
            RFQ_MAX_STALENESS_SECS
        );
    }

    /// The inference keys on the pair, not the host, because the pair is what
    /// selects the publisher — every corridor shares the same `/price` shape.
    #[test]
    fn only_cngn_pairs_infer_the_wide_window() {
        let with = |url: &str| {
            rfq_staleness_secs(&FeedConfig {
                url: url.into(),
                staleness_secs: 900,
                rfq_staleness_secs: None,
            })
        };
        let host = "https://api.textilecredit.com/price?chainId=1";
        assert_eq!(
            with(&format!("{host}&pair=cngn-usdt")),
            RFQ_MAX_STALENESS_SECS
        );
        assert_eq!(
            with(&format!("{host}&pair=CNGN-USDC")),
            RFQ_MAX_STALENESS_SECS
        );
        // Same host, live-fetched pairs: tight.
        for pair in ["weth-usdt", "xaut-usdt", "nvda-usdg", "usdc-usdt"] {
            assert_eq!(
                with(&format!("{host}&pair={pair}")),
                RFQ_DEFAULT_STALENESS_SECS,
                "{pair} is live-fetched and must not inherit the sampler window"
            );
        }
        // No pair at all, and a pair that merely contains "cngn" later on.
        assert_eq!(with(host), RFQ_DEFAULT_STALENESS_SECS);
        assert_eq!(
            with(&format!("{host}&pair=wcngn-usdt")),
            RFQ_DEFAULT_STALENESS_SECS
        );
        // Explicit config still beats the inference, in both directions.
        assert_eq!(
            rfq_staleness_secs(&FeedConfig {
                url: format!("{host}&pair=cngn-usdt"),
                staleness_secs: 900,
                rfq_staleness_secs: Some(60),
            }),
            60
        );
    }

    /// A bot can carry pools on differently paced feeds. The window has to
    /// follow the pool's feed, or one cNGN pool widens the whole bot.
    #[test]
    fn a_pool_on_its_own_feed_does_not_inherit_the_bot_window() {
        let cngn_feed = FeedConfig {
            url: "https://api.textilecredit.com/price?chainId=56&pair=cngn-usdt".into(),
            staleness_secs: 900,
            rfq_staleness_secs: Some(240),
        };
        let toml = format!(
            "{}\nrfq_corridor = \"cngn-usdt\"\n{RFQ_BLOCK}",
            LEAN_POOL_BASE.replace("staleness_secs = 30", "staleness_secs = 900")
        );
        let cfg = Config::from_toml(&toml).unwrap();
        let mut pool = cfg.pools[0].clone();

        // No override: the pool is on the bot's feed, so it takes its window.
        assert_eq!(rfq_staleness_secs_for_pool(&cngn_feed, &pool), 240);

        // Its own live-fetched feed: tight, even though the bot is on 240.
        pool.feed_url = Some("https://api.textilecredit.com/price?chainId=1&pair=weth-usdt".into());
        assert_eq!(
            rfq_staleness_secs_for_pool(&cngn_feed, &pool),
            RFQ_DEFAULT_STALENESS_SECS
        );

        // And it can still say otherwise for itself, within the ceiling.
        pool.rfq_staleness_secs = Some(3_600);
        assert_eq!(
            rfq_staleness_secs_for_pool(&cngn_feed, &pool),
            RFQ_MAX_STALENESS_SECS
        );
    }

    /// The shipped cNGN templates are the ones that need the wide window, and
    /// they are also the ones a regression would silently darken. Pin that they
    /// opt in, and that the fast live-fetched corridors do not.
    #[test]
    fn only_the_cron_sampled_templates_widen_the_rfq_window() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/setup/templates");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("templates dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let body = std::fs::read_to_string(&path).expect("template readable");
            let opts_in = body.contains("rfq_staleness_secs");
            checked += 1;
            if name.starts_with("cngn-") {
                assert!(
                    opts_in,
                    "{name} prices off the cron sampler and must widen its RFQ window"
                );
            } else {
                assert!(
                    !opts_in,
                    "{name} is live-fetched; widening its RFQ window needs its own reasoning"
                );
            }
        }
        assert!(
            checked >= 4,
            "expected the shipped templates, found {checked}"
        );
    }

    /// The cap is a freshness gate on a feed that publishes on a cron, so it
    /// has to clear that cron's period with slack. Textile's `/price` restamps
    /// cNGN once a minute; a cap at or under that period means the corridor is
    /// dark between marks and the venue answers `no_makers_online` while the
    /// maker is connected and quoting — the failure this constant was raised
    /// to fix.
    ///
    /// Strictly greater than three intervals, not `>=`. At exactly three the
    /// old mark expires the instant the third tick is scheduled, and since
    /// `observedAt` is stamped before that tick does its fetches, the write
    /// latency alone opens a gap. The margin has to survive two missed ticks
    /// *plus* the time it takes the next one to land.
    #[test]
    fn rfq_staleness_cap_clears_the_price_feed_cadence() {
        assert!(
            RFQ_MAX_STALENESS_SECS > PRICE_FEED_CADENCE_SECS * 3,
            "cap {RFQ_MAX_STALENESS_SECS}s leaves no room beyond three \
             {PRICE_FEED_CADENCE_SECS}s intervals for cron jitter and the \
             sampler's own write latency"
        );
    }

    /// Passes `false` explicitly rather than relying on the variable being
    /// unset: this asserts the gate is SHUT, and reading it from the process
    /// environment meant a sibling test setting the Docker override could open
    /// it mid-run. That is exactly what made this test fail ~1 run in 10.
    #[test]
    fn audit_h03_rfq_stream_url_rejects_remote_cleartext() {
        let check = |u: &str| assert_rfq_stream_url_with(u, false);
        assert!(check("wss://api.textilecredit.com/v2/maker/stream").is_ok());
        assert!(check("ws://localhost:10000/v2/maker/stream").is_ok());
        assert!(check("ws://127.0.0.1:10000/v2/maker/stream").is_ok());
        assert!(check("ws://[::1]:10000/v2/maker/stream").is_ok());
        assert!(check("ws://api.textilecredit.com/v2/maker/stream").is_err());
        assert!(check("ws://192.168.1.10/v2/maker/stream").is_err());
        assert!(check("https://api.textilecredit.com/v2/maker/stream").is_err());
    }

    #[test]
    fn docker_cleartext_override_allows_compose_service_hosts() {
        let stream = assert_rfq_stream_url_with("ws://app:10000/v2/maker/stream", true);
        let feed = assert_feed_url_with("http://app:8916/api/price", "[feed].url", true);
        assert!(stream.is_ok(), "{stream:?}");
        assert!(feed.is_ok(), "{feed:?}");
    }

    /// The override is opt-in and exact: only `1` opens it. Without this the
    /// argument-passing split above could drift from what the compose stack
    /// actually sets and nothing would notice.
    #[test]
    fn the_docker_override_stays_shut_for_a_compose_host() {
        assert!(assert_rfq_stream_url_with("ws://app:10000/v2/maker/stream", false).is_err());
        assert!(assert_feed_url_with("http://app:8916/api/price", "[feed].url", false).is_err());
    }
}
