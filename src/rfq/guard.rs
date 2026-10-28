// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Where the RFQ responder applies `[price_guard]`.
//!
//! The jump guard runs on every feed fetch, in the price loop, before the
//! print reaches the cache: a held print is never cached, so every reader
//! (firm quotes, levels, attestation co-signing, modules) sees no price
//! and refuses the way it already does for a missing one. The hold itself
//! is kept next to the cache so the panel can say why.
//!
//! The price band runs twice: on the exact amounts of a built order just
//! before it is signed, and on each published level, so the venue is not
//! routed to a level the firm path would refuse.

use alloy_primitives::U256;

use crate::pricing::band::{level_within, order_vs_print_bps, order_within, price_wad, Order};
use crate::pricing::feed::Quote;
use crate::pricing::tick::{usable, Unusable};
use crate::protocol::attest::{check_attestation, LiveVault, NavAttestation};
use crate::protocol::types::OrderParams;

use super::responder::CorridorBook;
use super::wire::{AttestRejectReason, Level, LevelsFrame, RejectReason};
use super::PriceCache;

/// The band on a built order, before it is signed. `None` passes; `Some`
/// carries the order's distance from the print, for the log line.
pub(super) fn order_refusal(
    order: &OrderParams,
    bid: bool,
    book: &CorridorBook,
    quote: &Quote,
    max_loss_bps: u32,
) -> Option<Option<i64>> {
    let legs = Order {
        bid,
        input: order.input_amount,
        output: order.output_amount,
        debt_decimals: book.debt_decimals,
        collateral_decimals: book.collateral_decimals,
    };
    let Some(wad) = price_wad(quote.price) else {
        return Some(None);
    };
    (!order_within(&legs, wad, max_loss_bps)).then(|| order_vs_print_bps(&legs, wad))
}

/// Judge a signed order again against what the cache holds now. A slow
/// signer (MPC, custody) can take long enough for the price loop to admit a
/// new print, for the feed to flag it `degraded`, or for the jump guard to
/// pull the feed, so a signature is only sent if it still passes the gates
/// `respond` applied before signing: no print, or a stale or invalid one, is
/// `StaleFeed`; a degraded one is `DegradedFeed`; out of band is `Busy`.
pub(super) fn recheck_after_signing(
    prices: &PriceCache,
    order: &OrderParams,
    bid: bool,
    book: &CorridorBook,
    max_loss_bps: u32,
) -> Result<(), RejectReason> {
    let quote = prices.get(&book.feed_url).ok_or(RejectReason::StaleFeed)?;
    usable(&quote, crate::time::unix_now(), book.staleness_secs).map_err(|why| match why {
        Unusable::Degraded => RejectReason::DegradedFeed,
        Unusable::Stale | Unusable::BadPrice => RejectReason::StaleFeed,
    })?;
    match order_refusal(order, bid, book, &quote, max_loss_bps) {
        Some(_) => Err(RejectReason::Busy),
        None => Ok(()),
    }
}

/// The print a NAV attestation is judged against: the book's cached print,
/// through the same gate as a firm quote. No print, or a stale or invalid
/// one, is `StaleFeed`; a degraded one is `DegradedFeed`. A print the jump
/// guard holds is not cached, so it is `StaleFeed` too.
pub(super) fn attest_print(
    prices: &PriceCache,
    book: &CorridorBook,
) -> Result<Quote, AttestRejectReason> {
    let quote = prices
        .get(&book.feed_url)
        .ok_or(AttestRejectReason::StaleFeed)?;
    usable(&quote, crate::time::unix_now(), book.staleness_secs).map_err(|why| match why {
        Unusable::Degraded => AttestRejectReason::DegradedFeed,
        Unusable::Stale | Unusable::BadPrice => AttestRejectReason::StaleFeed,
    })?;
    Ok(quote)
}

/// Judge a co-signed attestation again against what the cache holds now,
/// with the vault figures read before signing. The vault reads and a slow
/// signer give the price loop time to admit a new print, the feed time to
/// flag it `degraded`, and the jump guard time to pull it, so the signature
/// is only sent if the attestation still passes.
pub(super) fn recheck_attestation_after_signing(
    prices: &PriceCache,
    book: &CorridorBook,
    att: &NavAttestation,
    live: &LiveVault,
) -> Result<(), AttestRejectReason> {
    let quote = attest_print(prices, book)?;
    check_attestation(att, live, crate::protocol::attest::price_wad(quote.price))
}

/// Drop the levels the firm path would refuse. Pure.
pub(super) fn levels_in_band(frame: LevelsFrame, mid: f64, max_loss_bps: u32) -> LevelsFrame {
    let Some(wad) = price_wad(mid) else {
        return LevelsFrame {
            recall_asks: Vec::new(),
            bids: Vec::new(),
            asks: Vec::new(),
            mid_rate_ray: None,
            ..frame
        };
    };
    let keep = |bid: bool| {
        move |level: &Level| {
            level
                .rate_ray
                .parse::<U256>()
                .is_ok_and(|rate| level_within(bid, rate, wad, max_loss_bps))
        }
    };
    LevelsFrame {
        recall_asks: frame
            .recall_asks
            .iter()
            .filter(|l| keep(false)(l))
            .cloned()
            .collect(),
        bids: frame
            .bids
            .iter()
            .filter(|l| keep(true)(l))
            .cloned()
            .collect(),
        asks: frame
            .asks
            .iter()
            .filter(|l| keep(false)(l))
            .cloned()
            .collect(),
        ..frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::cache::FeedPolicy;
    use crate::pricing::jump::{Hold, JumpGuard, PrintHistory};

    const URL: &str = "http://feed";

    fn quote(price: f64, timestamp: u64) -> Quote {
        Quote {
            price,
            timestamp,
            degraded: false,
            reference: None,
        }
    }

    const JUMP: FeedPolicy = FeedPolicy {
        jump: JumpGuard {
            max_jump_bps: 1_000,
            settle_secs: 300,
        },
        staleness_secs: 60,
    };
    /// The tests' clock: every print below is fresh at it unless it says
    /// otherwise.
    const NOW: u64 = 110;

    fn admit(cache: &PriceCache, url: &str, quote: Quote) -> PrintHistory {
        let started = tokio::time::Instant::now()
            - std::time::Duration::from_secs(NOW.saturating_sub(quote.timestamp));
        cache.admit(started, url, quote, NOW)
    }

    fn feed(cache: &PriceCache, prints: &[(f64, u64)]) -> PrintHistory {
        prints.iter().fold(PrintHistory::default(), |_, (p, t)| {
            admit(cache, URL, quote(*p, *t))
        })
    }

    #[test]
    fn nothing_is_cached_until_a_second_read_confirms_the_first() {
        let cache = PriceCache::new(None, JUMP);
        feed(&cache, &[(0.5, 100)]);
        assert!(cache.get(URL).is_none());
        assert_eq!(cache.hold(URL), Some(Hold::WarmingUp));
        feed(&cache, &[(0.5, 100), (0.5, 101)]);
        assert_eq!(cache.get(URL), Some(quote(0.5, 101)));
        assert_eq!(cache.hold(URL), None);
    }

    #[test]
    fn a_10x_print_takes_the_price_out_of_the_cache_and_says_why() {
        let cache = PriceCache::new(None, JUMP);
        feed(&cache, &[(0.5, 100), (0.5, 101)]);
        admit(&cache, URL, quote(5.0, 102));
        assert!(cache.get(URL).is_none(), "readers see no price");
        assert!(matches!(cache.hold(URL), Some(Hold::Jumped { .. })));
        admit(&cache, URL, quote(0.5, 103));
        assert_eq!(cache.get(URL), Some(quote(0.5, 103)));
        assert_eq!(cache.hold(URL), None);
    }

    #[test]
    fn a_lagging_replica_neither_cuts_quoting_nor_moves_the_reference() {
        let cache = PriceCache::new(None, JUMP);
        feed(&cache, &[(0.5, 100), (0.5, 101)]);
        // Out of line and older: the newer print stays cached.
        let h = admit(&cache, URL, quote(5.0, 90));
        assert_eq!(cache.get(URL), Some(quote(0.5, 101)));
        assert_eq!(cache.hold(URL), None);
        // In line and older: cached, as before the guard.
        let after = admit(&cache, URL, quote(0.501, 95));
        assert_eq!(cache.get(URL), Some(quote(0.501, 95)));
        assert_eq!(after, h);
    }

    #[test]
    fn a_degraded_print_is_cached_for_the_readers_and_never_moves_the_reference() {
        let cache = PriceCache::new(None, JUMP);
        let h = feed(&cache, &[(0.5, 100), (0.5, 101)]);
        // A held or clamped centre far from the trusted print: readers see
        // it and refuse it as degraded; the guard does not learn from it.
        let held = Quote {
            degraded: true,
            ..quote(5.0, 102)
        };
        let after = admit(&cache, URL, held.clone());
        assert_eq!(after, h);
        assert_eq!(cache.get(URL), Some(held));
        // Live again at the old level: trusted at once, no jump.
        admit(&cache, URL, quote(0.5, 103));
        assert_eq!(cache.get(URL), Some(quote(0.5, 103)));
    }

    #[test]
    fn a_print_no_reader_would_quote_never_becomes_the_reference() {
        // The feed is serving an expired sample at startup: cached for the
        // readers to refuse as stale, but not a read the guard counts, so
        // two of them trust nothing.
        let cache = PriceCache::new(None, JUMP);
        let h = feed(&cache, &[(5.0, 10), (5.0, 11)]);
        assert_eq!(h, PrintHistory::default());
        assert_eq!(cache.get(URL), Some(quote(5.0, 11)));
        // The feed recovers at the real level: the startup rule applies to
        // it, not a jump from the expired one.
        admit(&cache, URL, quote(0.5, 105));
        assert_eq!(cache.hold(URL), Some(Hold::WarmingUp));
        admit(&cache, URL, quote(0.5, 106));
        assert_eq!(cache.get(URL), Some(quote(0.5, 106)));
        assert_eq!(cache.hold(URL), None);
    }

    #[test]
    fn an_unusable_print_is_left_for_the_readers_to_refuse() {
        let cache = PriceCache::new(None, JUMP);
        let h = feed(&cache, &[(0.5, 100), (0.5, 101)]);
        let after = admit(&cache, URL, quote(f64::NAN, 102));
        assert_eq!(after, h, "not an observation");
        assert!(cache.get(URL).unwrap().price.is_nan());
    }

    fn book(debt_decimals: u8, collateral_decimals: u8) -> CorridorBook {
        CorridorBook {
            slug: "c".into(),
            collateral: alloy_primitives::Address::repeat_byte(1),
            debt: alloy_primitives::Address::repeat_byte(2),
            collateral_decimals,
            debt_decimals,
            buy_spread: None,
            sell_spread: None,
            buy_capacity_debt: None,
            sell_capacity_collateral: None,
            feed_url: URL.into(),
            staleness_secs: 60,
            curve: None,
        }
    }

    /// A built order, through the same `build_order` the responder signs.
    fn built(input: u64, output: U256) -> OrderParams {
        super::super::order::build_order(&super::super::order::RfqOrderSpec {
            reactor: alloy_primitives::Address::repeat_byte(9),
            maker: alloy_primitives::Address::repeat_byte(8),
            nonce: U256::from(1u64),
            deadline_secs: 1_000,
            input_token: alloy_primitives::Address::repeat_byte(2),
            input_amount: U256::from(input),
            output_token: alloy_primitives::Address::repeat_byte(1),
            output_amount: output,
            validation_contract: alloy_primitives::Address::repeat_byte(7),
            taker: alloy_primitives::Address::repeat_byte(3),
            order_executor: None,
        })
    }

    #[test]
    fn the_hook_judges_the_built_order_s_own_amounts() {
        // USDT (6 dp) for cNGN (18 dp) at 0.00065 USDT per cNGN: 1 USDT buys
        // 1538.46… cNGN. An order whose output came out at 6-dp scale (an
        // amount-math bug) pays 12 orders of magnitude too little.
        let q = quote(0.00065, 100);
        let fair = U256::from(1_538_461_538_461_538_461_538u128);
        assert_eq!(
            order_refusal(&built(1_000_000, fair), true, &book(6, 18), &q, 1_500),
            None
        );
        let wrong_scale = built(1_000_000, U256::from(1_538_461_538u64));
        assert!(matches!(
            order_refusal(&wrong_scale, true, &book(6, 18), &q, 1_500),
            Some(Some(bps)) if bps < -9_900
        ));
        // A print the band cannot reduce refuses rather than passes.
        assert_eq!(
            order_refusal(
                &built(1_000_000, fair),
                true,
                &book(6, 18),
                &quote(f64::NAN, 100),
                1_500
            ),
            Some(None)
        );
    }

    #[test]
    fn the_re_check_refuses_every_print_respond_refuses_before_signing() {
        let fair = U256::from(1_538_461_538_461_538_461_538u128);
        let order = built(1_000_000, fair);
        let now = crate::time::unix_now();
        let recheck = |cached: Option<Quote>| {
            let cache = PriceCache::new(None, JUMP);
            if let Some(q) = cached {
                cache.set(URL.to_string(), q);
            }
            recheck_after_signing(&cache, &order, true, &book(6, 18), 1_500)
        };
        assert_eq!(recheck(Some(quote(0.00065, now))), Ok(()));
        assert_eq!(recheck(None), Err(RejectReason::StaleFeed));
        assert_eq!(
            recheck(Some(quote(0.00065, now - 61))),
            Err(RejectReason::StaleFeed)
        );
        assert_eq!(
            recheck(Some(quote(f64::NAN, now))),
            Err(RejectReason::StaleFeed)
        );
        assert_eq!(
            recheck(Some(Quote {
                degraded: true,
                ..quote(0.00065, now)
            })),
            Err(RejectReason::DegradedFeed)
        );
        // The maker pays USDT for cNGN: cNGN 20 % cheaper means the order
        // now gets 20 % too little of it.
        assert_eq!(
            recheck(Some(quote(0.00065 * 0.8, now))),
            Err(RejectReason::Busy)
        );
    }

    fn level(rate_ray: &str) -> Level {
        Level {
            size: "1".into(),
            rate_ray: rate_ray.into(),
        }
    }

    #[test]
    fn levels_outside_the_band_are_dropped_and_the_rest_kept() {
        // Mid 0.5; a 15 % band keeps a bid up to 0.5 / 0.85 = 0.588… and an
        // ask down to 0.425.
        let frame = LevelsFrame {
            recall_asks: Vec::new(),
            corridor_id: "c".into(),
            as_of: "t".into(),
            bids: vec![
                level("490000000000000000000000000"),
                level("600000000000000000000000000"),
            ],
            asks: vec![
                level("510000000000000000000000000"),
                level("420000000000000000000000000"),
            ],
            mid_rate_ray: Some("500000000000000000000000000".into()),
        };
        let rates = |levels: &[Level]| -> Vec<String> {
            levels.iter().map(|l| l.rate_ray.clone()).collect()
        };
        let kept = levels_in_band(frame.clone(), 0.5, 1_500);
        assert_eq!(rates(&kept.bids), ["490000000000000000000000000"]);
        assert_eq!(rates(&kept.asks), ["510000000000000000000000000"]);
        assert_eq!(kept.mid_rate_ray, frame.mid_rate_ray);
        let none = levels_in_band(frame, f64::NAN, 1_500);
        assert!(none.bids.is_empty() && none.asks.is_empty());
    }
}
