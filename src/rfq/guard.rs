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
use crate::pricing::off_hours::{assess, covers};
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
///
/// `priced` is the print the order was priced from, which a `feed_sides`
/// pool needs ([`feed_sides_refusal`]), and so does a book that takes
/// issuer spreads ([`issuer_spread_refusal`]). `priced_at` is when it was
/// priced (Unix seconds), so its off-hours extra is the one the order was
/// actually built with: see [`off_hours_refusal`].
pub(super) fn recheck_after_signing(
    prices: &PriceCache,
    order: &OrderParams,
    bid: bool,
    book: &CorridorBook,
    max_loss_bps: u32,
    priced: Option<&Quote>,
    priced_at: u64,
) -> Result<(), RejectReason> {
    let quote = prices.get(&book.feed_url).ok_or(RejectReason::StaleFeed)?;
    usable(&quote, crate::time::unix_now(), book.staleness_secs).map_err(|why| match why {
        Unusable::Degraded | Unusable::UnknownSession => RejectReason::DegradedFeed,
        Unusable::Stale | Unusable::BadPrice => RejectReason::StaleFeed,
    })?;
    if let Some(reason) = feed_sides_refusal(book, bid, priced, &quote) {
        return Err(reason);
    }
    if let Some(reason) =
        off_hours_refusal(book, priced, priced_at, &quote, crate::time::unix_now())
    {
        return Err(reason);
    }
    if let Some(reason) = issuer_spread_refusal(book, priced, &quote) {
        return Err(reason);
    }
    match order_refusal(order, bid, book, &quote, max_loss_bps) {
        Some(_) => Err(RejectReason::Busy),
        None => Ok(()),
    }
}

/// Whether a `feed_sides` order may still go out after signing. Such an
/// order is priced at the feed's own side, not a spread off the mid, so the
/// band (a loss limit around the mid) can't see that side move: a source
/// that sets both sides by hand can change one and keep the mid. So the
/// print the cache holds now must still carry both sides (`StaleFeed` if
/// not, as a request on it would be refused), and the order's side must not
/// have moved against the maker since pricing (`Busy` if it has): a bid may
/// only have risen, an ask only fallen. Sides are compared as printed, the
/// same JSON numbers, so no rounding enters. With no `priced` print to
/// compare to, a `feed_sides` order is withheld. `None` on any other pool.
fn feed_sides_refusal(
    book: &CorridorBook,
    bid: bool,
    priced: Option<&Quote>,
    current: &Quote,
) -> Option<RejectReason> {
    if !book.feed_sides {
        return None;
    }
    let Some((bid_now, ask_now)) = current.sides() else {
        return Some(RejectReason::StaleFeed);
    };
    let Some((bid_then, ask_then)) = priced.and_then(Quote::sides) else {
        return Some(RejectReason::Busy);
    };
    let held = if bid {
        bid_now >= bid_then
    } else {
        ask_now <= ask_then
    };
    (!held).then_some(RejectReason::Busy)
}

/// Whether an order priced off `priced` at `priced_at` may still go out on
/// the print the cache holds `now`, as far as off-hours goes. A print the
/// pool can't quote any more (held past `held_max_hours`, say) is
/// `DegradedFeed`, as a request on it would be. Otherwise the policy the
/// order was built under must cover the one in force now (see
/// [`covers`]), else `Busy`: a live-priced order has neither the extra nor
/// the cap, and a held one signed across a step of the time-grown extra is
/// too tight. The priced extra is assessed at `priced_at`, the same moment
/// `module_book` widened the book with it, not at `now`.
fn off_hours_refusal(
    book: &CorridorBook,
    priced: Option<&Quote>,
    priced_at: u64,
    current: &Quote,
    now: u64,
) -> Option<RejectReason> {
    let config = book.off_hours.as_ref();
    let current = match assess(current, config, now) {
        Err(_) => return Some(RejectReason::DegradedFeed),
        Ok(current) => current,
    };
    let then = priced.and_then(|p| assess(p, config, priced_at).ok().flatten());
    (!covers(then, current)).then_some(RejectReason::Busy)
}

/// Whether an order priced at an issuer's spread may still go out after
/// signing. An issuer can set a spread, or Textile withdraw one, while a slow
/// signer works, and the band can't see it: it bounds the loss around the
/// mid, and a spread change moves the order less than that. So the spread
/// that applies to the print the cache holds now must be the one the order
/// was priced at, none included (`Busy` if not). `None` on a book that takes
/// no issuer spread, and when there's no pricing print to compare to: only
/// the firm-quote path prices from the book's spreads, and it passes one.
fn issuer_spread_refusal(
    book: &CorridorBook,
    priced: Option<&Quote>,
    current: &Quote,
) -> Option<RejectReason> {
    let bounds = book.issuer_spread?;
    let priced = priced?;
    let applied = |quote: &Quote| {
        crate::pricing::issuer_spread::applicable(
            Some(bounds),
            book.curve.is_some(),
            quote.spread.verified(),
        )
        .map(|s| (s.buy_bps, s.sell_bps))
    };
    (applied(priced) != applied(current)).then_some(RejectReason::Busy)
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
        Unusable::Degraded | Unusable::UnknownSession => AttestRejectReason::DegradedFeed,
        Unusable::Stale | Unusable::BadPrice => AttestRejectReason::StaleFeed,
    })?;
    // NAV is only co-signed on a live market price. Off-hours a vault's NAV
    // waits for the session, as it did when the feed simply went stale.
    if !quote.session.is_live() {
        return Err(AttestRejectReason::StaleFeed);
    }
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
            spread: Default::default(),
            reference: None,
            bid: None,
            ask: None,
            session: Default::default(),
            market_at: None,
            market_source: None,
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
            feed_sides: false,
            off_hours: None,
            issuer_spread: None,
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
            recheck_after_signing(
                &cache,
                &order,
                true,
                &book(6, 18),
                1_500,
                None,
                crate::time::unix_now(),
            )
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

    #[test]
    fn an_order_priced_at_an_issuer_spread_is_withheld_when_it_changed_while_signing() {
        use crate::pricing::issuer_spread::{IssuerSpread, SpreadBounds, SpreadSlot};
        let now = crate::time::unix_now();
        let with = |spread: Option<(u32, u32, u64)>| Quote {
            spread: spread.map_or(SpreadSlot::None, |(buy_bps, sell_bps, revision)| {
                SpreadSlot::Verified(IssuerSpread {
                    buy_bps,
                    sell_bps,
                    revision,
                })
            }),
            ..quote(0.00065, now)
        };
        let takes = CorridorBook {
            issuer_spread: Some(SpreadBounds {
                min_bps: 10,
                max_bps: 80,
            }),
            ..book(6, 18)
        };
        let order = built(1_000_000, U256::from(1_538_461_538_461_538_461_538u128));
        let recheck = |book: &CorridorBook, now: Quote, priced: Option<&Quote>| {
            let cache = PriceCache::new(None, JUMP);
            cache.set(URL.to_string(), now);
            recheck_after_signing(
                &cache,
                &order,
                true,
                book,
                1_500,
                priced,
                crate::time::unix_now(),
            )
        };
        let priced = with(Some((25, 30, 12)));

        assert_eq!(recheck(&takes, priced.clone(), Some(&priced)), Ok(()));
        // Set again at a later revision with the same offsets: still the price.
        assert_eq!(
            recheck(&takes, with(Some((25, 30, 14))), Some(&priced)),
            Ok(())
        );
        // Changed, or withdrawn, while the signer worked: withheld.
        assert_eq!(
            recheck(&takes, with(Some((40, 30, 13))), Some(&priced)),
            Err(RejectReason::Busy)
        );
        assert_eq!(
            recheck(&takes, with(None), Some(&priced)),
            Err(RejectReason::Busy)
        );
        // A spread outside the book's bounds never applied either way.
        assert_eq!(
            recheck(
                &takes,
                with(Some((90, 90, 13))),
                Some(&with(Some((95, 95, 12))))
            ),
            Ok(())
        );
        // A book that takes no issuer spread doesn't care.
        assert_eq!(recheck(&book(6, 18), with(None), Some(&priced)), Ok(()));
    }

    #[test]
    fn a_feed_sides_order_is_withheld_when_its_side_moved_while_signing() {
        let sided = |bid: f64, ask: f64| Quote {
            bid: Some(bid),
            ask: Some(ask),
            ..quote((bid + ask) / 2.0, crate::time::unix_now())
        };
        let sided_book = CorridorBook {
            feed_sides: true,
            ..book(6, 18)
        };
        // Priced at a 0.00064 bid and a 0.00066 ask; the order itself sits
        // well inside the band, so only the side check can refuse it.
        let priced = sided(0.00064, 0.00066);
        let order = built(1_000_000, U256::from(1_538_461_538_461_538_461_538u128));
        let recheck = |bid: bool, now: Option<Quote>, priced: Option<&Quote>| {
            let cache = PriceCache::new(None, JUMP);
            if let Some(q) = now {
                cache.set(URL.to_string(), q);
            }
            recheck_after_signing(
                &cache,
                &order,
                bid,
                &sided_book,
                1_500,
                priced,
                crate::time::unix_now(),
            )
        };

        assert_eq!(recheck(true, Some(priced.clone()), Some(&priced)), Ok(()));
        assert_eq!(recheck(false, Some(priced.clone()), Some(&priced)), Ok(()));
        // The desk lowered its bid (same mid): the signed bid now overpays.
        assert_eq!(
            recheck(true, Some(sided(0.00063, 0.00067)), Some(&priced)),
            Err(RejectReason::Busy)
        );
        // Raised its ask: the signed ask now sells too cheap.
        assert_eq!(
            recheck(false, Some(sided(0.00063, 0.00067)), Some(&priced)),
            Err(RejectReason::Busy)
        );
        // A move in the maker's favour on its own side lets the order go.
        assert_eq!(
            recheck(true, Some(sided(0.00065, 0.00066)), Some(&priced)),
            Ok(())
        );
        assert_eq!(
            recheck(false, Some(sided(0.00064, 0.00065)), Some(&priced)),
            Ok(())
        );
        // The print lost its sides: no side to quote, as a request would see.
        assert_eq!(
            recheck(
                true,
                Some(quote(0.00065, crate::time::unix_now())),
                Some(&priced)
            ),
            Err(RejectReason::StaleFeed)
        );
        // Nothing to compare against: withheld rather than guessed.
        assert_eq!(
            recheck(true, Some(priced.clone()), None),
            Err(RejectReason::Busy)
        );
        // Another pool never consults the sides.
        let cache = PriceCache::new(None, JUMP);
        cache.set(URL.to_string(), quote(0.00065, crate::time::unix_now()));
        assert_eq!(
            recheck_after_signing(
                &cache,
                &order,
                true,
                &book(6, 18),
                1_500,
                None,
                crate::time::unix_now(),
            ),
            Ok(())
        );
    }

    #[test]
    fn a_held_order_signed_across_a_step_of_the_held_extra_is_withheld() {
        let now = crate::time::unix_now();
        let held_book = CorridorBook {
            off_hours: Some(crate::pricing::off_hours::OffHoursConfig {
                offshore_extra_bps: 50,
                held_extra_bps: 100,
                held_extra_bps_per_hour: 36,
                held_max_extra_bps: 1_000,
                held_max_hours: 24.0,
                max_notional_debt: "1000000000".into(),
            }),
            ..book(6, 18)
        };
        // Held for an hour: 136 bps now. 36 bps an hour is one bp every 100 s.
        let held = Quote {
            session: crate::pricing::off_hours::Session::Held,
            market_at: Some(now - 3_600),
            ..quote(0.00065, now)
        };
        let order = built(1_000_000, U256::from(1_538_461_538_461_538_461_538u128));
        let cache = PriceCache::new(None, JUMP);
        cache.set(URL.to_string(), held.clone());
        let recheck = |priced_at| {
            recheck_after_signing(
                &cache,
                &order,
                true,
                &held_book,
                1_500,
                Some(&held),
                priced_at,
            )
        };
        // Priced this second: the extra it was built with is the one in force.
        assert_eq!(recheck(now), Ok(()));
        // Priced 200 s ago, at 134 bps: two steps too tight to go out now.
        assert_eq!(recheck(now - 200), Err(RejectReason::Busy));
    }

    #[test]
    fn an_order_is_withheld_when_the_session_tightened_while_signing() {
        let now = crate::time::unix_now();
        let off_hours_book = CorridorBook {
            off_hours: Some(crate::pricing::off_hours::OffHoursConfig {
                offshore_extra_bps: 50,
                held_extra_bps: 100,
                held_extra_bps_per_hour: 0,
                held_max_extra_bps: 100,
                held_max_hours: 24.0,
                max_notional_debt: "1000000000".into(),
            }),
            ..book(6, 18)
        };
        let print = |session| Quote {
            session,
            market_at: Some(now - 60),
            ..quote(0.00065, now)
        };
        let order = built(1_000_000, U256::from(1_538_461_538_461_538_461_538u128));
        let recheck = |b: &CorridorBook, priced: Quote, current: Quote| {
            let cache = PriceCache::new(None, JUMP);
            cache.set(URL.to_string(), current);
            recheck_after_signing(&cache, &order, true, b, 1_500, Some(&priced), now)
        };
        use crate::pricing::off_hours::Session::{Held, Live, Offshore, Unknown};
        // Same session, or one with a smaller extra: the order is at least
        // as wide as the pool quotes now.
        assert_eq!(recheck(&off_hours_book, print(Held), print(Held)), Ok(()));
        assert_eq!(
            recheck(&off_hours_book, print(Held), print(Offshore)),
            Ok(())
        );
        assert_eq!(
            recheck(&off_hours_book, print(Offshore), print(Live)),
            Ok(())
        );
        // Priced live (no extra, no cap) and the market closed meanwhile.
        assert_eq!(
            recheck(&off_hours_book, print(Live), print(Held)),
            Err(RejectReason::Busy)
        );
        assert_eq!(
            recheck(&off_hours_book, print(Offshore), print(Held)),
            Err(RejectReason::Busy)
        );
        // Even with no extra configured, a live-priced order lacks the cap.
        let no_extra = CorridorBook {
            off_hours: off_hours_book.off_hours.clone().map(|o| {
                crate::pricing::off_hours::OffHoursConfig {
                    offshore_extra_bps: 0,
                    ..o
                }
            }),
            ..book(6, 18)
        };
        assert_eq!(
            recheck(&no_extra, print(Live), print(Offshore)),
            Err(RejectReason::Busy)
        );
        // A print the pool can't quote at all.
        assert_eq!(
            recheck(&off_hours_book, print(Held), print(Unknown)),
            Err(RejectReason::DegradedFeed)
        );
        assert_eq!(
            recheck(&book(6, 18), print(Live), print(Offshore)),
            Err(RejectReason::DegradedFeed)
        );
        // Plain pools on live prints are untouched.
        assert_eq!(recheck(&book(6, 18), print(Live), print(Live)), Ok(()));
    }

    #[test]
    fn nav_is_only_attested_on_a_live_price() {
        let now = crate::time::unix_now();
        let cache = PriceCache::new(None, JUMP);
        cache.set(URL.to_string(), quote(0.00065, now));
        assert!(attest_print(&cache, &book(6, 18)).is_ok());
        for session in [
            crate::pricing::off_hours::Session::Offshore,
            crate::pricing::off_hours::Session::Held,
        ] {
            cache.set(
                URL.to_string(),
                Quote {
                    session,
                    market_at: Some(now),
                    ..quote(0.00065, now)
                },
            );
            assert!(matches!(
                attest_print(&cache, &book(6, 18)),
                Err(AttestRejectReason::StaleFeed)
            ));
        }
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
