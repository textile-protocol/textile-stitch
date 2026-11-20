// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Quoting a move-clamped price: maker-side quotes keep going out, wider.
//!
//! Textile's cNGN `/price` bounds how far its center may move per sample.
//! When the market moves further than that in one sample, the center is
//! *clamped*: a live value, computed off this sample's book, held back to the
//! move budget. On a normal morning that happens several times in a few
//! minutes. The feed flags it `degraded`, like a held or fallback center, and
//! adds `degradedReason` to say which:
//!
//! - `clamped`: quotable here. The ladder, RFQ levels and RFQ firm quotes
//!   widen both sides by `clamped_extra_bps` (per pool, default
//!   [`DEFAULT_EXTRA_BPS`]), and by at least the published `clampGapBps`, how
//!   far the clamp held the center back from the market.
//! - `held`, `fallback`, missing or anything else: not quotable, as before.
//!
//! Everything else stays strict on a clamped print: the limit-order taker,
//! the closer, NAV co-signing, spot rebalancing and manual sales all act on
//! the price itself rather than quote around it, and keep refusing any
//! degraded print through [`crate::pricing::tick::usable`].
//!
//! Widening by the gap means neither side quotes tighter than it would
//! around the market the clamp lagged: if the market really moved, the side
//! it moved toward is still at least as wide as it would be around the live
//! target; if the move was a bad print, the bot is quoting around the
//! bounded center. The configured extra covers the market moving on before
//! the next sample.
//!
//! A feed that never sends `degradedReason` (every feed but Textile's cNGN
//! ones, and the API before this field existed) is unaffected: a degraded
//! print without a reason is not quotable.

use serde::{Deserialize, Deserializer};

use crate::pricing::feed::Quote;

/// Default extra bps on both sides of a clamped print.
///
/// About one sample's whole move budget on cNGN: the API's per-sample budget
/// is 4 NGN per 3 minutes on a 1-minute cron, 1.33 NGN, which is 9.2 bps at
/// ~1,440 NGN per USDT. A clamp says the market moved further than that in
/// one sample, so this leaves room for one more full budget of movement the
/// same way before the next sample lands. The published gap takes over when
/// it's larger: the 2026-11-20 steps of 1.5-2 NGN clamped 0.2-0.7 NGN short
/// (1-5 bps), and the widest gap the API's suspect-jump guard lets through is
/// about 2 NGN per sample (14 bps).
pub const DEFAULT_EXTRA_BPS: u32 = 10;

/// The largest `clamped_extra_bps` config accepts, in bps. The same ceiling as
/// the off-hours extras: past 50% a bid is near zero.
pub const MAX_EXTRA_BPS: u32 = crate::pricing::off_hours::MAX_EXTRA_BPS;

/// The largest `clampGapBps` a clamped print may carry and still be quoted.
/// The API's own guards bound a real clamp well under this (its sanity check
/// freezes anything ~290 bps from Monierate, and the suspect-jump guard
/// freezes per-sample moves past ~23 bps). A gap past it means the feed isn't
/// describing a bounded move, so the print is refused like a held one.
pub const MAX_GAP_BPS: f64 = 500.0;

/// Why a feed says a print is degraded (`degradedReason` on `/price`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DegradedReason {
    /// No reason sent: every feed but Textile's cNGN ones, and older APIs.
    #[default]
    Missing,
    /// A live center bounded to the source's per-sample move budget.
    Clamped,
    /// A held center from an earlier sample.
    Held,
    /// The source's last-resort fallback rate.
    Fallback,
    /// A value this build doesn't know. Treated as held.
    Unknown,
}

impl DegradedReason {
    pub fn label(self) -> &'static str {
        match self {
            DegradedReason::Missing => "missing",
            DegradedReason::Clamped => "clamped",
            DegradedReason::Held => "held",
            DegradedReason::Fallback => "fallback",
            DegradedReason::Unknown => "unknown",
        }
    }
}

/// `degradedReason` as the feed serves it. Missing or `null` is
/// [`DegradedReason::Missing`]; a known string is that reason; anything else
/// (an unknown or differently cased string, a number, an object) is
/// [`DegradedReason::Unknown`] rather than a failed fetch, so a newer API can
/// add a reason without taking older bots' quotes off it: it just stays dark.
pub(crate) fn lenient_reason<'de, D: Deserializer<'de>>(d: D) -> Result<DegradedReason, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(match value {
        serde_json::Value::Null => DegradedReason::Missing,
        serde_json::Value::String(s) => match s.as_str() {
            "clamped" => DegradedReason::Clamped,
            "held" => DegradedReason::Held,
            "fallback" => DegradedReason::Fallback,
            _ => DegradedReason::Unknown,
        },
        _ => DegradedReason::Unknown,
    })
}

/// `clampGapBps` as the feed serves it: `None` when missing or `null`, the
/// number when it is one, and NaN for anything else, so a malformed gap is
/// refused by the same check as a negative or oversized one ([`gap_ok`])
/// instead of silently reading as no gap.
pub(crate) fn lenient_gap<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(match value {
        serde_json::Value::Null => None,
        other => Some(other.as_f64().unwrap_or(f64::NAN)),
    })
}

/// A gap a clamped print can be quoted with: finite, not negative, at most
/// [`MAX_GAP_BPS`].
fn gap_ok(gap: f64) -> bool {
    gap.is_finite() && (0.0..=MAX_GAP_BPS).contains(&gap)
}

/// A degraded print the feed says is only move-clamped, with a usable gap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clamped {
    /// `clampGapBps`, or 0 when the feed didn't send one.
    pub gap_bps: f64,
}

/// What a print's `degraded` flag means for maker-side quoting: `Ok(None)` is
/// a live print, `Ok(Some(_))` a clamped one quoted wider, `Err(())` one
/// that isn't quotable (held, fallback, no reason, an unknown reason, or a
/// clamped one with a gap [`gap_ok`] refuses). Age, price and session are
/// [`crate::pricing::tick::quotable`]'s to check.
#[allow(clippy::result_unit_err)]
pub fn classify(quote: &Quote) -> Result<Option<Clamped>, ()> {
    if !quote.degraded {
        return Ok(None);
    }
    if quote.degraded_reason != DegradedReason::Clamped {
        return Err(());
    }
    match quote.clamp_gap_bps {
        None => Ok(Some(Clamped { gap_bps: 0.0 })),
        Some(gap) if gap_ok(gap) => Ok(Some(Clamped { gap_bps: gap })),
        Some(_) => Err(()),
    }
}

/// The extra bps a pool puts on both sides of a clamped print: its
/// configured `clamped_extra_bps`, or the gap rounded up when that's larger.
pub fn extra_bps(clamped: Clamped, configured: u32) -> u32 {
    // gap_ok bounds the gap at MAX_GAP_BPS, so the cast can't truncate.
    configured.max(clamped.gap_bps.ceil() as u32)
}

/// The clamp extra one pool quotes `quote` with: `None` on a live print (or
/// one [`classify`] refuses; the caller's gate has already turned those
/// away), `Some(extra)` on a clamped one.
pub fn pool_extra_bps(quote: &Quote, configured: u32) -> Option<u32> {
    classify(quote)
        .ok()
        .flatten()
        .map(|c| extra_bps(c, configured))
}

/// Whether orders built with clamp extra `priced` (`None`: priced off a live
/// print) still meet the print in force now, which needs `now`. A live print
/// takes anything: a clamped order is only wider than it needs. A clamped
/// print takes only orders at least as wide as it requires, so a live-priced
/// order, or one priced on a smaller gap, is withheld.
pub fn covers(priced: Option<u32>, now: Option<u32>) -> bool {
    match (priced, now) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(p), Some(c)) => c <= p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(body: &str) -> Quote {
        serde_json::from_str(body).unwrap()
    }

    const BASE: &str = r#""price":0.000694,"timestamp":1800000000"#;

    fn with(extra: &str) -> Quote {
        quote(&format!("{{{BASE}{extra}}}"))
    }

    #[test]
    fn the_reason_is_read_leniently() {
        let reason = |extra: &str| with(extra).degraded_reason;
        assert_eq!(reason(""), DegradedReason::Missing);
        assert_eq!(reason(r#","degradedReason":null"#), DegradedReason::Missing);
        assert_eq!(
            reason(r#","degradedReason":"clamped""#),
            DegradedReason::Clamped
        );
        assert_eq!(reason(r#","degradedReason":"held""#), DegradedReason::Held);
        assert_eq!(
            reason(r#","degradedReason":"fallback""#),
            DegradedReason::Fallback
        );
        for raw in [r#""CLAMPED""#, r#""frozen""#, "1", "true", "{}", "[]"] {
            assert_eq!(
                reason(&format!(r#","degradedReason":{raw}"#)),
                DegradedReason::Unknown,
                "{raw}"
            );
        }
    }

    #[test]
    fn the_gap_is_read_leniently_and_a_malformed_one_is_nan() {
        let gap = |extra: &str| with(extra).clamp_gap_bps;
        assert_eq!(gap(""), None);
        assert_eq!(gap(r#","clampGapBps":null"#), None);
        assert_eq!(gap(r#","clampGapBps":4.56"#), Some(4.56));
        assert_eq!(gap(r#","clampGapBps":0"#), Some(0.0));
        for raw in [r#""4.5""#, "true", "{}"] {
            assert!(
                gap(&format!(r#","clampGapBps":{raw}"#)).is_some_and(f64::is_nan),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_live_print_is_live_whatever_else_it_says() {
        assert_eq!(classify(&with("")), Ok(None));
        assert_eq!(
            classify(&with(r#","degraded":false,"degradedReason":"held""#)),
            Ok(None)
        );
    }

    #[test]
    fn only_a_clamped_reason_with_a_usable_gap_is_quotable() {
        assert_eq!(
            classify(&with(
                r#","degraded":true,"degradedReason":"clamped","clampGapBps":4.56"#
            )),
            Ok(Some(Clamped { gap_bps: 4.56 }))
        );
        // No gap: the configured extra alone.
        assert_eq!(
            classify(&with(r#","degraded":true,"degradedReason":"clamped""#)),
            Ok(Some(Clamped { gap_bps: 0.0 }))
        );
        for extra in [
            // Held, fallback, no reason (an older API), and one this build
            // doesn't know: all stay dark.
            r#","degraded":true,"degradedReason":"held""#,
            r#","degraded":true,"degradedReason":"fallback""#,
            r#","degraded":true"#,
            r#","degraded":true,"degradedReason":"wobbly""#,
            // A clamped print whose gap can't be trusted.
            r#","degraded":true,"degradedReason":"clamped","clampGapBps":-1"#,
            r#","degraded":true,"degradedReason":"clamped","clampGapBps":500.01"#,
            r#","degraded":true,"degradedReason":"clamped","clampGapBps":"4""#,
        ] {
            assert_eq!(classify(&with(extra)), Err(()), "{extra}");
        }
        assert!(classify(&with(
            r#","degraded":true,"degradedReason":"clamped","clampGapBps":500"#
        ))
        .is_ok());
    }

    #[test]
    fn the_extra_is_the_configured_one_floored_by_the_gap() {
        assert_eq!(extra_bps(Clamped { gap_bps: 0.0 }, 10), 10);
        assert_eq!(extra_bps(Clamped { gap_bps: 4.56 }, 10), 10);
        // A gap past the configured extra wins, rounded up.
        assert_eq!(extra_bps(Clamped { gap_bps: 14.01 }, 10), 15);
        assert_eq!(extra_bps(Clamped { gap_bps: 3.2 }, 0), 4);
        assert_eq!(extra_bps(Clamped { gap_bps: 500.0 }, 0), 500);
    }

    #[test]
    fn pool_extra_is_none_on_a_live_or_refused_print() {
        assert_eq!(pool_extra_bps(&with(""), 10), None);
        assert_eq!(
            pool_extra_bps(&with(r#","degraded":true,"degradedReason":"held""#), 10),
            None
        );
        assert_eq!(
            pool_extra_bps(
                &with(r#","degraded":true,"degradedReason":"clamped","clampGapBps":12.3"#),
                10
            ),
            Some(13)
        );
    }

    #[test]
    fn a_clamped_print_takes_only_orders_at_least_as_wide() {
        assert!(covers(None, None));
        assert!(covers(Some(10), None));
        assert!(!covers(None, Some(10)));
        assert!(covers(Some(10), Some(10)));
        assert!(covers(Some(15), Some(10)));
        assert!(!covers(Some(10), Some(15)));
    }
}
