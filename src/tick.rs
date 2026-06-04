// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Pure tick-loop decisions: when the feed is too stale to quote, and when the
//! bid has moved enough to be worth re-signing.

/// True if the feed hasn't updated within `staleness_secs` — never trade on it.
pub fn is_stale(feed_ts: u64, now: u64, staleness_secs: u64) -> bool {
    now.saturating_sub(feed_ts) > staleness_secs
}

/// True if we should sign a fresh order: no prior bid, or the bid moved at
/// least `threshold_bps` since the last one.
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
/// moved at least `threshold_bps`, or the last post is past half its TTL so a
/// replacement lands before the live order expires. The age gate keeps a stable
/// market from going dark when the price-move gate alone would never fire before
/// the TTL lapses. `last` is `(price, posted_at_unix)`.
pub fn should_requote_now(
    last: Option<(f64, u64)>,
    new_bid: f64,
    threshold_bps: u32,
    now: u64,
    ttl_secs: u64,
) -> bool {
    match last {
        None => true,
        Some((prev, posted_at)) => {
            let aged = now.saturating_sub(posted_at) >= ttl_secs / 2;
            aged || should_requote(Some(prev), new_bid, threshold_bps)
        }
    }
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
    fn requotes_on_first_quote_and_big_moves() {
        assert!(should_requote(None, 1.0, 10));
        // 0.05% move, threshold 0.10% → no.
        assert!(!should_requote(Some(1.0), 1.0005, 10));
        // 0.20% move, threshold 0.10% → yes.
        assert!(should_requote(Some(1.0), 1.002, 10));
    }

    #[test]
    fn requotes_near_expiry_even_when_price_is_flat() {
        // First quote always.
        assert!(should_requote_now(None, 1.0, 10, 0, 30));
        // 5s into a 30s TTL, flat price → not yet.
        assert!(!should_requote_now(Some((1.0, 0)), 1.0, 10, 5, 30));
        // 20s in (past half the TTL), flat price → re-quote to avoid a gap.
        assert!(should_requote_now(Some((1.0, 0)), 1.0, 10, 20, 30));
        // A big move still re-quotes before the age gate.
        assert!(should_requote_now(Some((1.0, 0)), 1.002, 10, 1, 30));
    }
}
