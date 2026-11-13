// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Off-hours quoting: an opt-in way to keep quoting a session-bound feed
//! while its onshore market is closed, wider and smaller.
//!
//! Some feeds price off a market that only trades in a session (COPM/USDT
//! prices off Colombia's SET-FX spot, Mon-Fri 08:00-13:00 Bogota). Outside it
//! the feed goes stale and the bot stops quoting. A pool with an
//! `[pools.off_hours]` block instead asks the feed for its fallback
//! (`offHours=1`), and the feed labels each print with a `session`:
//!
//! - `live`: an onshore print. Quoted as configured, no extra.
//! - `offshore`: an offshore reference that has moved recently. Both sides
//!   get `offshore_extra_bps` more spread.
//! - `held`: the last real market price, held. Both sides get
//!   `held_extra_bps + held_extra_bps_per_hour * hours since marketAt`,
//!   capped at `held_max_extra_bps`; past `held_max_hours` the pool quotes
//!   nothing.
//!
//! While the session isn't live every side also commits at most
//! `max_notional_debt` (debt atomic units, valued at the feed price).
//!
//! A pool without the block never sends `offHours=1`, so its requests, cache
//! entry, jump-guard anchor and quotes are exactly what they were before this
//! module existed. Everything here is pure; the consumers (ladder, taker, RFQ
//! levels and firm quotes) call [`assess`] after the usual
//! [`crate::pricing::tick::usable`] gate and apply what it returns.

use alloy_primitives::U256;
use serde::{Deserialize, Deserializer};

use crate::pricing::feed::Quote;
use crate::pricing::quote::{ask_price, bid_price, Spread};

/// Query parameter that asks a session-bound feed for its off-hours fallback.
pub const QUERY_KEY: &str = "offHours";

/// The largest extra spread the block accepts, in bps. Past 50% a bid would be
/// near zero and the quote meaningless; a typo should fail at load instead.
pub const MAX_EXTRA_BPS: u32 = 5_000;

const SECS_PER_HOUR: f64 = 3_600.0;

/// How live the feed says its price is. Absent on every feed that isn't
/// session-bound, and on every request without `offHours=1`: that reads as
/// [`Session::Live`], today's behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Session {
    #[default]
    Live,
    Offshore,
    Held,
    /// A value this build doesn't know. Never quoted: the source is saying
    /// something about the price that the bot can't price for.
    Unknown,
}

impl Session {
    pub fn label(self) -> &'static str {
        match self {
            Session::Live => "live",
            Session::Offshore => "offshore",
            Session::Held => "held",
            Session::Unknown => "unknown",
        }
    }

    pub fn is_live(self) -> bool {
        self == Session::Live
    }
}

/// `session` as the feed serves it: a missing or `null` field is live, a
/// known string is that session, anything else (an unknown string, a number,
/// an object) is [`Session::Unknown`] rather than a failed fetch, so the
/// consumers can report why the pool went dark.
pub(crate) fn lenient_session<'de, D: Deserializer<'de>>(d: D) -> Result<Session, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(match value {
        serde_json::Value::Null => Session::Live,
        serde_json::Value::String(s) => match s.as_str() {
            "live" => Session::Live,
            "offshore" => Session::Offshore,
            "held" => Session::Held,
            _ => Session::Unknown,
        },
        _ => Session::Unknown,
    })
}

/// `marketAt` as Unix seconds, or `None` when absent or malformed. A
/// non-live print without a usable `marketAt` is refused by [`assess`].
pub(crate) fn lenient_unix_secs<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(value.as_u64().filter(|t| *t > 0))
}

/// `marketSource`, informational only. Anything but a string reads as none.
pub(crate) fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(value.as_str().map(str::to_string))
}

/// A pool's `[pools.off_hours]` block. Present means the pool is off-hours
/// enabled; every field is required.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffHoursConfig {
    /// Extra bps on both sides while the session is `offshore`.
    pub offshore_extra_bps: u32,
    /// Extra bps on both sides as soon as the session is `held`.
    pub held_extra_bps: u32,
    /// Further extra bps per hour since `marketAt` while `held` (fractional
    /// hours count pro rata).
    pub held_extra_bps_per_hour: u32,
    /// Ceiling on the held extra, in bps. At least `held_extra_bps`.
    pub held_max_extra_bps: u32,
    /// Past this many hours since `marketAt` a held price is not quoted.
    pub held_max_hours: f64,
    /// Most each side commits while the session isn't live, in debt atomic
    /// units (uint256 as string, like `buy_total_liquidity_debt`). The ask
    /// side's collateral is valued at the feed price.
    pub max_notional_debt: String,
}

impl OffHoursConfig {
    /// The load-time rules. `field` prefixes messages, e.g. `pools[0].off_hours`.
    pub fn validate(&self, field: &str) -> anyhow::Result<()> {
        for (name, value) in [
            ("offshore_extra_bps", self.offshore_extra_bps),
            ("held_extra_bps", self.held_extra_bps),
            ("held_extra_bps_per_hour", self.held_extra_bps_per_hour),
            ("held_max_extra_bps", self.held_max_extra_bps),
        ] {
            anyhow::ensure!(
                value <= MAX_EXTRA_BPS,
                "{field}.{name} must be at most {MAX_EXTRA_BPS}, got {value}"
            );
        }
        anyhow::ensure!(
            self.held_max_extra_bps >= self.held_extra_bps,
            "{field}.held_max_extra_bps ({}) must be at least held_extra_bps ({})",
            self.held_max_extra_bps,
            self.held_extra_bps
        );
        anyhow::ensure!(
            self.held_max_hours.is_finite() && self.held_max_hours > 0.0,
            "{field}.held_max_hours must be a positive number of hours, got {}",
            self.held_max_hours
        );
        self.max_notional()
            .map_err(|e| anyhow::anyhow!("{field}.max_notional_debt: {e}"))?;
        Ok(())
    }

    /// `max_notional_debt` parsed: a positive atomic amount that fits `u128`
    /// (the ladder's sizing unit).
    pub fn max_notional(&self) -> anyhow::Result<u128> {
        let raw = self.max_notional_debt.trim();
        let value: U256 = raw
            .parse()
            .map_err(|_| anyhow::anyhow!("must be an atomic integer, got {raw:?}"))?;
        let value = u128::try_from(value).map_err(|_| anyhow::anyhow!("is too large"))?;
        anyhow::ensure!(value > 0, "must be greater than 0");
        Ok(value)
    }
}

/// What an off-hours print changes for one pool: the extra spread on both
/// sides and the per-side size cap. Only produced for a non-live session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OffHours {
    pub session: Session,
    /// Seconds since `marketAt`, the last real market observation.
    pub market_age_secs: u64,
    pub extra_bps: u32,
    /// Debt atomic units each side may commit.
    pub max_notional_debt: u128,
}

impl OffHours {
    pub fn market_age_hours(&self) -> f64 {
        self.market_age_secs as f64 / SECS_PER_HOUR
    }
}

/// Why an otherwise usable print can't be quoted by this pool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Refusal {
    /// The feed sent a `session` this build doesn't know.
    UnknownSession,
    /// The print is offshore or held, but the pool has no off-hours block.
    NotEnabled(Session),
    /// No usable `marketAt`, or one after `now`.
    NoMarketTime(Session),
    /// Held longer than `held_max_hours`.
    HeldTooLong { age_hours: f64, max_hours: f64 },
}

impl Refusal {
    /// Operator-facing reason, for logs and the module status.
    pub fn describe(&self) -> String {
        match self {
            Refusal::UnknownSession => {
                "The price feed sent a session this Stitch doesn't know; not quoting".to_string()
            }
            Refusal::NotEnabled(s) => format!(
                "The price feed served an {} price, but this pool has no off_hours block; not quoting",
                s.label()
            ),
            Refusal::NoMarketTime(s) => format!(
                "The price feed served an {} price without a valid marketAt; not quoting",
                s.label()
            ),
            Refusal::HeldTooLong {
                age_hours,
                max_hours,
            } => format!(
                "Off-hours price held {age_hours:.1}h, past held_max_hours ({max_hours}); not quoting"
            ),
        }
    }
}

/// What this pool does with a print that already passed
/// [`crate::pricing::tick::usable`]. `Ok(None)` is a live print: quote as
/// configured. `Ok(Some(_))` widens and caps. `Err` quotes nothing.
pub fn assess(
    quote: &Quote,
    config: Option<&OffHoursConfig>,
    now: u64,
) -> Result<Option<OffHours>, Refusal> {
    let session = quote.session;
    match session {
        Session::Live => return Ok(None),
        Session::Unknown => return Err(Refusal::UnknownSession),
        Session::Offshore | Session::Held => {}
    }
    let config = config.ok_or(Refusal::NotEnabled(session))?;
    let market_age_secs = quote
        .market_at
        .and_then(|at| now.checked_sub(at))
        .ok_or(Refusal::NoMarketTime(session))?;
    let extra_bps = match session {
        Session::Offshore => config.offshore_extra_bps,
        _ => held_extra_bps(config, market_age_secs)?,
    };
    // Validated at load; a hand-built config that fails here quotes nothing.
    let max_notional_debt = config
        .max_notional()
        .map_err(|_| Refusal::NotEnabled(session))?;
    Ok(Some(OffHours {
        session,
        market_age_secs,
        extra_bps,
        max_notional_debt,
    }))
}

/// The part of an off-hours assessment that shapes posted orders: session,
/// extra spread and size cap. `None` is a live print. The ladder keeps the
/// policy its orders were posted under and reposts when it changes, since a
/// price that hasn't moved past the refresh threshold can still need a wider
/// spread or a smaller size.
pub type Policy = Option<(Session, u32, u128)>;

/// The [`Policy`] of an assessment.
pub fn policy(off_hours: Option<OffHours>) -> Policy {
    off_hours.map(|o| (o.session, o.extra_bps, o.max_notional_debt))
}

/// Whether orders built under `priced` still meet the policy in force `now`.
/// A live print takes anything: off-hours orders are wider and smaller than
/// it needs. An off-hours print takes only off-hours orders whose extra is
/// at least the one required now and whose cap is no larger, so neither a
/// live-priced order nor a held one whose extra has since grown can post.
pub fn covers(priced: Option<OffHours>, now: Option<OffHours>) -> bool {
    match (priced, now) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(p), Some(c)) => {
            c.extra_bps <= p.extra_bps && c.max_notional_debt >= p.max_notional_debt
        }
    }
}

/// `held_extra_bps + held_extra_bps_per_hour * hours`, rounded up to whole
/// bps and capped at `held_max_extra_bps`. Exactly `held_max_hours` still
/// quotes.
fn held_extra_bps(config: &OffHoursConfig, market_age_secs: u64) -> Result<u32, Refusal> {
    let hours = market_age_secs as f64 / SECS_PER_HOUR;
    if hours > config.held_max_hours {
        return Err(Refusal::HeldTooLong {
            age_hours: hours,
            max_hours: config.held_max_hours,
        });
    }
    let grown =
        f64::from(config.held_extra_bps) + f64::from(config.held_extra_bps_per_hour) * hours;
    let cap = config.held_max_extra_bps;
    Ok(if grown >= f64::from(cap) {
        cap
    } else {
        grown.ceil() as u32
    })
}

/// A bid lowered by `extra_bps` of itself: `bid × (1 − extra)`.
///
/// The extra is measured on the side's own price, not on the mid, so that a
/// positive bid always stays positive: `extra_bps` is validated to at most
/// [`MAX_EXTRA_BPS`] (50%), and whatever set the bid (a bps or absolute
/// spread, the inventory lean, the TWAP guard) the widened bid is at least
/// half of it. A side that widening could drive to zero would post nothing,
/// and nothing would replace its live-priced orders. `None` only for a bid
/// that is already not a positive finite price.
pub fn widen_bid_price(bid: f64, extra_bps: u32) -> Option<f64> {
    let widened = bid * (1.0 - f64::from(extra_bps) / 10_000.0);
    (bid.is_finite() && bid > 0.0 && widened > 0.0).then_some(widened)
}

/// An ask raised by `extra_bps` of itself: `ask × (1 + extra)`.
pub fn widen_ask_price(ask: f64, extra_bps: u32) -> Option<f64> {
    let widened = ask * (1.0 + f64::from(extra_bps) / 10_000.0);
    (ask.is_finite() && ask > 0.0 && widened.is_finite()).then_some(widened)
}

/// The bid spread whose price at `mid` is [`widen_bid_price`] of the old
/// one. A bps spread stays bps, rounded up so it is never tighter than that
/// price: `1 − b'/10⁴ ≤ (1 − b/10⁴)(1 − e/10⁴)`. When rounding up would reach
/// 10,000 bps (a bid of zero), the exact price is kept as an absolute spread
/// instead. `None` only when the old bid isn't a positive price.
pub fn widen_bid_spread(spread: Spread, mid: f64, extra_bps: u32) -> Option<Spread> {
    let target = widen_bid_price(bid_price(mid, spread), extra_bps)?;
    if let Spread::Bps(b) = spread {
        let keep = (10_000.0 - f64::from(b)) * (10_000.0 - f64::from(extra_bps)) / 10_000.0;
        let widened = (10_000.0 - keep).ceil();
        if widened < 10_000.0 {
            return Some(Spread::Bps(widened as u32));
        }
    }
    // bid_price(mid, Abs(d)) = 1 / (1/mid + d)
    Some(Spread::Abs(1.0 / target - 1.0 / mid))
}

/// The ask spread whose price at `mid` is [`widen_ask_price`] of the old
/// one; see [`widen_bid_spread`]. A bps spread rounds up:
/// `1 + a'/10⁴ ≥ (1 + a/10⁴)(1 + e/10⁴)`.
pub fn widen_ask_spread(spread: Spread, mid: f64, extra_bps: u32) -> Option<Spread> {
    let target = widen_ask_price(ask_price(mid, spread), extra_bps)?;
    Some(match spread {
        Spread::Bps(a) => {
            let grown = (10_000.0 + f64::from(a)) * (10_000.0 + f64::from(extra_bps)) / 10_000.0;
            Spread::Bps((grown - 10_000.0).ceil().min(f64::from(u32::MAX)) as u32)
        }
        // ask_price(mid, Abs(d)) = 1 / (1/mid - d)
        Spread::Abs(_) => Spread::Abs(1.0 / mid - 1.0 / target),
    })
}

/// The URL a pool fetches its feed from: `base` with `offHours=1` appended
/// when the pool is off-hours enabled, and `base` untouched otherwise. The
/// price cache and the jump guard's anchor are keyed by this URL, so an
/// enabled and a plain pool on the same base never share a print.
pub fn feed_request_url(base: &str, enabled: bool) -> String {
    if !enabled || requests_off_hours(base) {
        return base.to_string();
    }
    let (head, fragment) = match base.split_once('#') {
        Some((head, frag)) => (head, Some(frag)),
        None => (base, None),
    };
    let sep = if !head.contains('?') {
        "?"
    } else if head.ends_with('?') || head.ends_with('&') {
        ""
    } else {
        "&"
    };
    let mut url = format!("{head}{sep}{QUERY_KEY}=1");
    if let Some(frag) = fragment {
        url.push('#');
        url.push_str(frag);
    }
    url
}

/// Whether a feed URL asks for off-hours prices (`offHours=1`). The HTTP
/// adapter refuses a non-live session on a URL that didn't ask for one.
pub fn requests_off_hours(url: &str) -> bool {
    url::Url::parse(url.trim())
        .is_ok_and(|u| u.query_pairs().any(|(k, v)| k == QUERY_KEY && v == "1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const HOUR: u64 = 3_600;

    fn config() -> OffHoursConfig {
        OffHoursConfig {
            offshore_extra_bps: 40,
            held_extra_bps: 60,
            held_extra_bps_per_hour: 10,
            held_max_extra_bps: 200,
            held_max_hours: 72.0,
            max_notional_debt: "5000000000".to_string(),
        }
    }

    fn print(session: Session, market_at: Option<u64>) -> Quote {
        Quote {
            price: 0.00025,
            timestamp: NOW,
            degraded: false,
            reference: None,
            bid: None,
            ask: None,
            session,
            market_at,
            market_source: None,
        }
    }

    #[test]
    fn a_live_print_quotes_as_configured_with_or_without_the_block() {
        let live = print(Session::Live, Some(NOW - 30));
        assert_eq!(assess(&live, None, NOW), Ok(None));
        assert_eq!(assess(&live, Some(&config()), NOW), Ok(None));
    }

    #[test]
    fn an_offshore_print_adds_the_flat_offshore_extra() {
        let q = print(Session::Offshore, Some(NOW - 2 * HOUR));
        let off = assess(&q, Some(&config()), NOW).unwrap().unwrap();
        assert_eq!(off.session, Session::Offshore);
        assert_eq!(off.extra_bps, 40);
        assert_eq!(off.market_age_secs, 2 * HOUR);
        assert_eq!(off.max_notional_debt, 5_000_000_000);
        // Offshore doesn't grow with age.
        let older = print(Session::Offshore, Some(NOW - 100 * HOUR));
        assert_eq!(
            assess(&older, Some(&config()), NOW)
                .unwrap()
                .unwrap()
                .extra_bps,
            40
        );
    }

    #[test]
    fn a_held_print_grows_per_hour_and_stops_at_the_cap() {
        let held = |age| {
            assess(&print(Session::Held, Some(NOW - age)), Some(&config()), NOW)
                .unwrap()
                .unwrap()
                .extra_bps
        };
        assert_eq!(held(0), 60);
        assert_eq!(held(HOUR), 70);
        // Fractional hours count, rounded up to whole bps.
        assert_eq!(held(HOUR / 2), 65);
        assert_eq!(held(HOUR / 3), 64);
        // 60 + 10 * 14 = 200, the cap; past it stays at the cap.
        assert_eq!(held(14 * HOUR), 200);
        assert_eq!(held(30 * HOUR), 200);
    }

    #[test]
    fn a_held_print_past_max_hours_is_not_quoted() {
        let at_limit = print(Session::Held, Some(NOW - 72 * HOUR));
        assert!(assess(&at_limit, Some(&config()), NOW).is_ok());
        let past = print(Session::Held, Some(NOW - 72 * HOUR - 1));
        assert!(matches!(
            assess(&past, Some(&config()), NOW),
            Err(Refusal::HeldTooLong { .. })
        ));
    }

    #[test]
    fn non_live_prints_need_the_block_a_known_session_and_a_market_time() {
        for session in [Session::Offshore, Session::Held] {
            let q = print(session, Some(NOW - 60));
            assert_eq!(assess(&q, None, NOW), Err(Refusal::NotEnabled(session)));
            let no_time = print(session, None);
            assert_eq!(
                assess(&no_time, Some(&config()), NOW),
                Err(Refusal::NoMarketTime(session))
            );
            let future = print(session, Some(NOW + 1));
            assert_eq!(
                assess(&future, Some(&config()), NOW),
                Err(Refusal::NoMarketTime(session))
            );
        }
        let unknown = print(Session::Unknown, Some(NOW));
        assert_eq!(
            assess(&unknown, Some(&config()), NOW),
            Err(Refusal::UnknownSession)
        );
        assert_eq!(assess(&unknown, None, NOW), Err(Refusal::UnknownSession));
    }

    #[test]
    fn widening_only_moves_prices_away_from_mid() {
        // COPM/USDT: about 4,000 COP per dollar, so an absolute spread of 5
        // is 5 COP per USDT.
        let mid = 0.00025;
        for spread in [Spread::Bps(30), Spread::Abs(5.0)] {
            let bid = bid_price(mid, spread);
            let ask = ask_price(mid, spread);
            let wide_bid = bid_price(mid, widen_bid_spread(spread, mid, 50).unwrap());
            let wide_ask = ask_price(mid, widen_ask_spread(spread, mid, 50).unwrap());
            assert!(wide_bid < bid, "{spread:?}");
            assert!(wide_ask > ask, "{spread:?}");
            // At least 50 bps of the side's own price further out, and no
            // more than the 1 bps a bps spread rounds up by.
            assert!(wide_bid <= bid * 0.995 + 1e-15, "{spread:?}");
            assert!(wide_bid >= bid * 0.995 - mid * 1e-4, "{spread:?}");
            assert!(wide_ask >= ask * 1.005 - 1e-15, "{spread:?}");
            assert!(wide_ask <= ask * 1.005 + mid * 1e-4, "{spread:?}");
        }
        // (1 − 0.003)(1 − 0.005) = 1 − 0.0079850: 79.85 bps, rounded up.
        assert_eq!(
            widen_bid_spread(Spread::Bps(30), mid, 50),
            Some(Spread::Bps(80))
        );
        // (1 + 0.003)(1 + 0.005) = 1 + 0.0080150.
        assert_eq!(
            widen_ask_spread(Spread::Bps(30), mid, 50),
            Some(Spread::Bps(81))
        );
        assert_eq!(
            widen_bid_spread(Spread::Bps(30), mid, 0),
            Some(Spread::Bps(30))
        );
        assert_eq!(widen_bid_price(10.0, 5_000), Some(5.0));
        assert_eq!(widen_bid_price(0.0, 50), None);
        // An ask already at the degenerate end has nothing to widen from.
        assert_eq!(widen_ask_spread(Spread::Abs(1.0 / mid), mid, 10), None);
    }

    #[test]
    fn no_extra_can_widen_a_positive_bid_to_nothing() {
        let mid = 0.00025;
        // 6,000 bps of spread plus a 4,000 bps held extra: additively that
        // was a bid of zero and the side posted nothing.
        assert_eq!(
            widen_bid_spread(Spread::Bps(6_000), mid, 4_000),
            Some(Spread::Bps(7_600))
        );
        // Rounding up would reach 10,000 bps: kept exact, as absolute.
        let edge = widen_bid_spread(Spread::Bps(9_999), mid, MAX_EXTRA_BPS).unwrap();
        assert!(matches!(edge, Spread::Abs(_)), "{edge:?}");
        let edge_bid = bid_price(mid, edge);
        assert!(edge_bid > 0.0);
        assert!((edge_bid - mid * 1e-4 * 0.5).abs() < 1e-15);
        for spread in [Spread::Bps(9_999), Spread::Abs(1e9)] {
            for extra in [0, 1, 4_000, MAX_EXTRA_BPS] {
                let wide = widen_bid_spread(spread, mid, extra).unwrap();
                assert!(bid_price(mid, wide) > 0.0, "{spread:?} + {extra}");
            }
        }
    }

    #[test]
    fn the_request_url_gets_off_hours_only_when_enabled() {
        let base = "https://textile.example/api/price?pair=copm-usdt&chainId=42220&corridorId=abc";
        assert_eq!(feed_request_url(base, false), base);
        assert_eq!(feed_request_url(base, true), format!("{base}&offHours=1"));
        assert_eq!(
            feed_request_url("https://textile.example/api/price", true),
            "https://textile.example/api/price?offHours=1"
        );
        assert_eq!(
            feed_request_url("https://textile.example/api/price?", true),
            "https://textile.example/api/price?offHours=1"
        );
        assert_eq!(
            feed_request_url("https://textile.example/price?pair=a#x", true),
            "https://textile.example/price?pair=a&offHours=1#x"
        );
        // Already asking: left as written, never doubled.
        let asked = format!("{base}&offHours=1");
        assert_eq!(feed_request_url(&asked, true), asked);
        assert!(requests_off_hours(&feed_request_url(base, true)));
        assert!(!requests_off_hours(base));
    }

    #[test]
    fn the_block_is_validated() {
        assert!(config().validate("pools[0].off_hours").is_ok());
        let bad = [
            OffHoursConfig {
                held_max_extra_bps: 50,
                ..config()
            },
            OffHoursConfig {
                held_max_hours: 0.0,
                ..config()
            },
            OffHoursConfig {
                held_max_hours: f64::NAN,
                ..config()
            },
            OffHoursConfig {
                offshore_extra_bps: MAX_EXTRA_BPS + 1,
                ..config()
            },
            OffHoursConfig {
                max_notional_debt: "0".into(),
                ..config()
            },
            OffHoursConfig {
                max_notional_debt: "max".into(),
                ..config()
            },
            OffHoursConfig {
                max_notional_debt: U256::MAX.to_string(),
                ..config()
            },
        ];
        for cfg in bad {
            assert!(cfg.validate("pools[0].off_hours").is_err(), "{cfg:?}");
        }
    }

    fn off(session: Session, extra_bps: u32, max_notional_debt: u128) -> Option<OffHours> {
        Some(OffHours {
            session,
            market_age_secs: 0,
            extra_bps,
            max_notional_debt,
        })
    }

    #[test]
    fn covers_needs_the_extra_in_force_now() {
        // Live now: anything priced goes, off-hours orders are wider.
        assert!(covers(None, None));
        assert!(covers(off(Session::Held, 100, 5), None));
        // Off-hours now: never a live-priced order.
        assert!(!covers(None, off(Session::Offshore, 0, 5)));
        // Held extra grew while signing: the order is too tight.
        assert!(!covers(
            off(Session::Held, 100, 5),
            off(Session::Held, 101, 5)
        ));
        assert!(covers(
            off(Session::Held, 101, 5),
            off(Session::Held, 100, 5)
        ));
        assert!(covers(
            off(Session::Held, 120, 5),
            off(Session::Offshore, 60, 5)
        ));
        assert!(!covers(
            off(Session::Held, 120, 5),
            off(Session::Held, 120, 4)
        ));
    }

    #[test]
    fn the_policy_tells_live_from_each_off_hours_state() {
        assert_eq!(policy(None), None);
        assert_ne!(policy(off(Session::Held, 0, 5)), policy(None));
        assert_ne!(
            policy(off(Session::Held, 100, 5)),
            policy(off(Session::Held, 105, 5))
        );
        assert_eq!(
            policy(Some(OffHours {
                market_age_secs: 99,
                ..off(Session::Held, 100, 5).unwrap()
            })),
            policy(off(Session::Held, 100, 5))
        );
    }
}
