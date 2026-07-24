// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Pure tick-loop decisions: when the feed is too stale to quote, and when the
//! bid has moved enough to be worth re-signing.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in seconds; 0 if the clock is before the epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True if the feed hasn't updated within `staleness_secs` — never trade on it.
pub fn is_stale(feed_ts: u64, now: u64, staleness_secs: u64) -> bool {
    now.saturating_sub(feed_ts) > staleness_secs
}

/// True if a feed price can be quoted off: finite and strictly positive.
/// Zero, negative, NaN, and infinite prices are feed malfunctions — the tick
/// loop skips the pool on them (going dark, like a stale feed) rather than
/// letting them reach the maker or the TWAP.
pub fn is_price_usable(price: f64) -> bool {
    price.is_finite() && price > 0.0
}

/// True if we should sign a fresh order: no prior bid, or the bid moved at
/// least `threshold_bps` since the last one. A zero threshold re-quotes every
/// tick (any move, including none, clears it) — the default: posting is
/// off-chain and free, and a deadband only lets quotes drift stale.
pub fn should_requote(last_bid: Option<f64>, new_bid: f64, threshold_bps: u32) -> bool {
    match last_bid {
        None => true,
        Some(prev) if prev <= 0.0 => true,
        Some(prev) => {
            let moved_bps = ((new_bid - prev).abs() / prev) * 10_000.0;
            moved_bps >= f64::from(threshold_bps)
        }
    }
}

/// True when a side's order should be re-signed: no prior order, the price
/// moved at least `threshold_bps`, or the last post is old enough that a
/// replacement should land while the live order still has `repost_lead_secs` of
/// life. The age gate keeps a stable market from going dark when the price-move
/// gate alone would never fire before the TTL lapses. `last` is
/// `(price, posted_at_unix)`.
pub fn should_requote_now(
    last: Option<(f64, u64)>,
    new_bid: f64,
    threshold_bps: u32,
    now: u64,
    ttl_secs: u64,
    repost_lead_secs: u64,
) -> bool {
    match last {
        None => true,
        Some((prev, posted_at)) => {
            let aged =
                now.saturating_sub(posted_at) >= requote_age_secs(ttl_secs, repost_lead_secs);
            aged || should_requote(Some(prev), new_bid, threshold_bps)
        }
    }
}

/// The order age at which a side reposts: `ttl_secs - repost_lead_secs`, so the
/// replacement is signed while the live order still has `repost_lead_secs` of
/// life and the two overlap instead of leaving a gap.
///
/// The lead is capped at half the TTL. That cap is a safety floor against a
/// large or misconfigured lead driving the repost age to zero (which would
/// re-sign the whole ladder every tick), but it has a cost: below `ttl ≈ 2 ×
/// lead` the effective overlap is `ttl/2`, not the configured lead. So as the
/// TTL drops the gap-free margin shrinks back toward the indexer's deadline
/// margin. Keep `ttl_secs ≥ 2 × repost_lead_secs` to actually get the lead you
/// asked for.
fn requote_age_secs(ttl_secs: u64, repost_lead_secs: u64) -> u64 {
    let lead = repost_lead_secs.min(ttl_secs / 2);
    ttl_secs.saturating_sub(lead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_only_past_the_window() {
        assert!(!is_stale(100, 120, 30)); // 20s old, window 30
        assert!(is_stale(100, 140, 30)); // 40s old
        assert!(!is_stale(140, 100, 30)); // clock skew → not stale (saturating)
    }

    #[test]
    fn only_finite_positive_prices_are_usable() {
        assert!(is_price_usable(3_000.0));
        assert!(is_price_usable(f64::MIN_POSITIVE));
        // A fresh timestamp can still carry a malfunctioning price; each of
        // these must take the pool dark, not reach the maker or the TWAP.
        assert!(!is_price_usable(0.0));
        assert!(!is_price_usable(-5.0));
        assert!(!is_price_usable(f64::NAN));
        assert!(!is_price_usable(f64::INFINITY));
        assert!(!is_price_usable(f64::NEG_INFINITY));
    }

    #[test]
    fn requotes_on_first_quote_and_big_moves() {
        assert!(should_requote(None, 1.0, 10));
        // 0.05% move, threshold 0.10% → no.
        assert!(!should_requote(Some(1.0), 1.0005, 10));
        // 0.20% move, threshold 0.10% → yes.
        assert!(should_requote(Some(1.0), 1.002, 10));
    }

    #[test]
    fn a_zero_threshold_requotes_every_tick_even_flat() {
        // The no-deadband mode: the quote re-posts each tick, pinned to the
        // current center, instead of drifting stale between 10 bps moves.
        assert!(should_requote(Some(1.0), 1.0, 0));
        assert!(should_requote(Some(1.0), 1.0000001, 0));
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 0, 1, 120, 60));
    }

    #[test]
    fn requotes_near_expiry_even_when_price_is_flat() {
        // 30s TTL, 10s lead → repost at age 20 (30 - 10).
        // First quote always.
        assert!(should_requote_now(None, 1.0, 10, 0, 30, 10));
        // 5s into the TTL, flat price → not yet.
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 5, 30, 10));
        // 15s in, flat price → still before the repost age.
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 15, 30, 10));
        // 20s in (lead before expiry), flat price → re-quote to avoid a gap.
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 10, 20, 30, 10));
        // A big move still re-quotes before the age gate.
        assert!(should_requote_now(Some((1.0, 0)), 1.002, 10, 1, 30, 10));
    }

    #[test]
    fn lead_time_sets_repost_age_and_clamps_at_half_ttl() {
        // ttl 240, lead 60 → repost at age 180 (240 - 60), 60s overlap.
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 179, 240, 60));
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 10, 180, 240, 60));

        // The clamp: an oversized lead is capped at ttl/2, so the repost age
        // never collapses toward zero. ttl 120, lead 1000 → age 60, not 0.
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 59, 120, 1000));
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 10, 60, 120, 1000));

        // The cost of the clamp: at ttl = 2 × lead the overlap is exactly the
        // lead; below that it silently shrinks to ttl/2. ttl 90, lead 60 → the
        // lead is capped to 45, so the real overlap is 45s, not 60s.
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 44, 90, 60));
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 10, 45, 90, 60));

        // Zero lead → repost only at the deadline (age == ttl).
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 239, 240, 0));
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 10, 240, 240, 0));
    }
}
