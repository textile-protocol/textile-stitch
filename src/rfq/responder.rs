// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Pure RFQ quoting decisions: which side a request hits, what the maker pays
//! and receives, whether capacity allows it, and the 1s level snapshot. No
//! I/O, no clocks, no signing — the session loop in [`super`] feeds these
//! functions and owns everything async, so the price/size rules stay unit
//! testable exactly like the ladder's `quote` module.

use alloy_primitives::{Address, U256};

use crate::config::PoolConfig;
use crate::quote::{ask_price, bid_price, Spread};
use crate::tick::is_price_usable;

use super::math::{collateral_for_debt, debt_for_collateral, fee_on, max_fitting_output, rate_ray};
use super::wire::{Level, LevelsFrame, QuoteRequestFrame, RejectReason};

/// One RFQ-serving pool, with everything pre-parsed so the hot path never
/// touches strings. Built once at responder start from [`PoolConfig`].
#[derive(Debug, Clone)]
pub struct CorridorBook {
    pub slug: String,
    pub collateral: Address,
    pub debt: Address,
    pub collateral_decimals: u8,
    pub debt_decimals: u8,
    pub buy_spread: Option<Spread>,
    pub sell_spread: Option<Spread>,
    /// Debt the bid side may commit (atomic); `None` = side off for RFQ.
    pub buy_capacity_debt: Option<U256>,
    /// Collateral the ask side may commit (atomic); `None` = side off.
    pub sell_capacity_collateral: Option<U256>,
    /// The feed this corridor prices off (pool override or the bot default).
    pub feed_url: String,
}

/// Build the corridor book for a pool, or `None` when the pool doesn't opt
/// into RFQ. Errors mirror config validation (bad addresses, `max` capacity) —
/// unreachable for a config that passed `Config::from_toml`, kept as errors so
/// the responder can never start on a half-parsed pool.
pub fn book_from_pool(
    pool: &PoolConfig,
    default_feed_url: &str,
) -> anyhow::Result<Option<CorridorBook>> {
    let Some(slug) = pool.rfq_corridor.clone() else {
        return Ok(None);
    };
    Ok(Some(CorridorBook {
        slug,
        collateral: pool.collateral.parse()?,
        debt: pool.debt.parse()?,
        collateral_decimals: pool.collateral_decimals,
        debt_decimals: pool.debt_decimals,
        buy_spread: pool.buy_spread(),
        sell_spread: pool.sell_spread(),
        buy_capacity_debt: pool.rfq_buy_capacity_debt()?,
        sell_capacity_collateral: pool.rfq_sell_capacity_collateral()?,
        feed_url: pool
            .feed_url
            .clone()
            .unwrap_or_else(|| default_feed_url.to_string()),
    }))
}

/// A priced firm quote, before signing. `bid` is the maker's side: true when
/// the maker buys collateral (pays debt), false when it sells collateral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotePlan {
    pub bid: bool,
    /// Signed order input — what the maker pays (the request's buyToken).
    pub input_token: Address,
    pub input: U256,
    /// Signed order output — what the maker receives (the request's sellToken).
    pub output_token: Address,
    pub output: U256,
    /// Venue fee projection on the output; never part of the signed order.
    pub fee: U256,
    /// Response sellAmount: the taker's gross (exact-input: echoed cap).
    pub sell_amount: U256,
    /// Response buyAmount == `input`.
    pub buy_amount: U256,
}

/// Price one request against a corridor book at mid `mid` (debt per
/// collateral, the same feed value the ladder quotes off), given what's
/// already reserved on each side. Every failure is a wire-ready reject
/// reason — the caller never has to interpret errors under the reply budget.
pub fn decide_quote(
    book: &CorridorBook,
    req: &QuoteRequestFrame,
    mid: f64,
    reserved_bid: U256,
    reserved_ask: U256,
) -> Result<QuotePlan, RejectReason> {
    // Token orientation. sellToken = what the taker sells = maker's output.
    let (Ok(sell_token), Ok(buy_token)) = (
        req.sell_token.parse::<Address>(),
        req.buy_token.parse::<Address>(),
    ) else {
        return Err(RejectReason::Busy);
    };
    let bid = if sell_token == book.collateral && buy_token == book.debt {
        true // taker sells collateral → maker buys it
    } else if sell_token == book.debt && buy_token == book.collateral {
        false // taker buys collateral → maker sells it
    } else {
        return Err(RejectReason::Busy); // not this corridor's pair
    };

    let spread = if bid {
        book.buy_spread
    } else {
        book.sell_spread
    };
    let Some(spread) = spread else {
        // The operator never funded this side; it holds no RFQ inventory.
        return Err(RejectReason::Inventory);
    };
    let price = if bid {
        bid_price(mid, spread)
    } else {
        ask_price(mid, spread)
    };
    if !is_price_usable(price) {
        return Err(RejectReason::StaleFeed);
    }

    // Maker-received output for a maker-paid input, and vice versa, at this
    // corridor's orientation.
    let input_for_output = |output: U256| {
        if bid {
            debt_for_collateral(price, output, book.debt_decimals, book.collateral_decimals)
        } else {
            collateral_for_debt(price, output, book.debt_decimals, book.collateral_decimals)
        }
    };
    let output_for_input = |input: U256| {
        if bid {
            collateral_for_debt(price, input, book.debt_decimals, book.collateral_decimals)
        } else {
            debt_for_collateral(price, input, book.debt_decimals, book.collateral_decimals)
        }
    };

    let (input, output, fee, sell_amount) = match (&req.sell_amount, &req.buy_amount) {
        // Exact-input: the taker's sellAmount is the gross cap; the order
        // output plus the injected fee must fit under it, and the response
        // echoes the cap verbatim.
        (Some(raw), None) => {
            let Ok(cap) = raw.parse::<U256>() else {
                return Err(RejectReason::Busy);
            };
            let fit = max_fitting_output(cap, req.fee_bps);
            let input = input_for_output(fit.output);
            (input, fit.output, fit.fee, cap)
        }
        // Exact-output: the maker pays exactly buyAmount; the taker's gross is
        // the priced output plus the fee on it.
        (None, Some(raw)) => {
            let Ok(input) = raw.parse::<U256>() else {
                return Err(RejectReason::Busy);
            };
            let output = output_for_input(input);
            let fee = fee_on(output, req.fee_bps);
            (input, output, fee, output + fee)
        }
        // Zero or two amounts — a malformed request.
        _ => return Err(RejectReason::Busy),
    };
    if input.is_zero() || output.is_zero() {
        return Err(RejectReason::Size);
    }

    // Capacity: the maker's committed input against configured liquidity
    // minus what in-flight quotes already claim.
    let capacity = if bid {
        book.buy_capacity_debt
    } else {
        book.sell_capacity_collateral
    };
    let Some(capacity) = capacity else {
        return Err(RejectReason::Inventory);
    };
    if input > capacity {
        return Err(RejectReason::Size);
    }
    let reserved = if bid { reserved_bid } else { reserved_ask };
    if input > capacity.saturating_sub(reserved) {
        return Err(RejectReason::Inventory);
    }

    Ok(QuotePlan {
        bid,
        input_token: buy_token,
        input,
        output_token: sell_token,
        output,
        fee,
        sell_amount,
        buy_amount: input,
    })
}

/// The 1s level snapshot for a corridor: one row per funded side, sized by
/// remaining capacity (configured minus reserved), priced off the same
/// spreads as the ladder. Sizes are collateral atomic on BOTH sides, so bid
/// capacity (debt) converts at the bid price.
pub fn levels_for(
    book: &CorridorBook,
    mid: f64,
    reserved_bid: U256,
    reserved_ask: U256,
    as_of: String,
) -> LevelsFrame {
    let mut bids = Vec::new();
    if let (Some(spread), Some(capacity)) = (book.buy_spread, book.buy_capacity_debt) {
        let remaining = capacity.saturating_sub(reserved_bid);
        let price = bid_price(mid, spread);
        if !remaining.is_zero() && is_price_usable(price) {
            let size = collateral_for_debt(
                price,
                remaining,
                book.debt_decimals,
                book.collateral_decimals,
            );
            let rate = rate_ray(price, book.debt_decimals, book.collateral_decimals);
            if !size.is_zero() && !rate.is_zero() {
                bids.push(Level {
                    size: size.to_string(),
                    rate_ray: rate.to_string(),
                });
            }
        }
    }
    let mut asks = Vec::new();
    if let (Some(spread), Some(capacity)) = (book.sell_spread, book.sell_capacity_collateral) {
        let remaining = capacity.saturating_sub(reserved_ask);
        let price = ask_price(mid, spread);
        if !remaining.is_zero() && is_price_usable(price) {
            let rate = rate_ray(price, book.debt_decimals, book.collateral_decimals);
            if !rate.is_zero() {
                asks.push(Level {
                    size: remaining.to_string(),
                    rate_ray: rate.to_string(),
                });
            }
        }
    }
    LevelsFrame {
        corridor_id: book.slug.clone(),
        as_of,
        bids,
        asks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLATERAL: &str = "0x0000000000000000000000000000000000000001";
    const DEBT: &str = "0x0000000000000000000000000000000000000002";

    fn book() -> CorridorBook {
        CorridorBook {
            slug: "cngn-usdc".into(),
            collateral: COLLATERAL.parse().unwrap(),
            debt: DEBT.parse().unwrap(),
            collateral_decimals: 6,
            debt_decimals: 6,
            buy_spread: Some(Spread::Bps(200)),
            sell_spread: Some(Spread::Bps(200)),
            buy_capacity_debt: Some(U256::from(5_000_000_000u64)),
            sell_capacity_collateral: Some(U256::from(5_000_000_000u64)),
            feed_url: "http://feed".into(),
        }
    }

    fn request(sell_token: &str, buy_token: &str) -> QuoteRequestFrame {
        QuoteRequestFrame {
            rfq_id: "rfq_1".into(),
            corridor_id: "cngn-usdc".into(),
            chain_id: 8453,
            sell_token: sell_token.into(),
            buy_token: buy_token.into(),
            sell_amount: None,
            buy_amount: None,
            taker: "0x0000000000000000000000000000000000000003".into(),
            reply_by: "2026-08-05T10:00:00.750Z".into(),
            quote_ttl_ms: 5_000,
            max_expires_at: "2026-08-05T10:02:00.000Z".into(),
            fee_bps: 1,
        }
    }

    #[test]
    fn exact_input_bid_echoes_the_cap_and_fits_output_plus_fee_under_it() {
        // Taker sells 1000 collateral (cap, 6dp) at mid 1.0, 200 bps bid, 1 bps fee.
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into());
        let plan = decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();

        assert!(plan.bid);
        // The golden fee-fit numbers ride through unchanged.
        assert_eq!(plan.output, U256::from(999_900_010u64));
        assert_eq!(plan.fee, U256::from(99_990u64));
        assert_eq!(plan.sell_amount, U256::from(1_000_000_000u64), "cap echoed");
        // Maker pays debt at the bid (0.98): 999900010 × 0.98 = 979902009.8 → floor.
        assert_eq!(plan.input, U256::from(979_902_009u64));
        assert_eq!(plan.buy_amount, plan.input);
        assert_eq!(plan.output_token, book().collateral);
        assert_eq!(plan.input_token, book().debt);
    }

    #[test]
    fn exact_input_ask_prices_collateral_at_the_ask() {
        // Taker sells 1000 debt; maker receives debt, pays collateral at 1.02.
        let mut req = request(DEBT, COLLATERAL);
        req.sell_amount = Some("1000000000".into());
        let plan = decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();

        assert!(!plan.bid);
        assert_eq!(plan.output, U256::from(999_900_010u64));
        // 999900010 / 1.02 = 980294127.45… → floor.
        assert_eq!(plan.input, U256::from(980_294_127u64));
        assert_eq!(plan.output_token, book().debt);
        assert_eq!(plan.input_token, book().collateral);
    }

    #[test]
    fn exact_output_adds_the_fee_on_top_of_the_priced_gross() {
        // Taker wants exactly 980 debt; maker pays it, receives collateral at
        // the ask, and the taker's gross is output + fee.
        let mut req = request(COLLATERAL, DEBT);
        req.buy_amount = Some("980000000".into());
        req.fee_bps = 5;
        let plan = decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();

        assert!(plan.bid);
        assert_eq!(
            plan.input,
            U256::from(980_000_000u64),
            "pays exactly buyAmount"
        );
        // 980e6 / 0.98 = 1e9 collateral received.
        assert_eq!(plan.output, U256::from(1_000_000_000u64));
        assert_eq!(plan.fee, U256::from(500_000u64));
        assert_eq!(plan.sell_amount, U256::from(1_000_500_000u64));
    }

    #[test]
    fn capacity_and_reservations_gate_with_distinct_reasons() {
        // Bigger than configured capacity outright → size.
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("100000000000".into()); // maker would pay ~98e9 > 5e9
        assert_eq!(
            decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Size)
        );

        // Fits capacity but not what's left after in-flight quotes → inventory.
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into()); // maker pays ~0.98e9
        let reserved_bid = U256::from(4_500_000_000u64); // 4.5e9 of 5e9 claimed
        assert_eq!(
            decide_quote(&book(), &req, 1.0, reserved_bid, U256::ZERO),
            Err(RejectReason::Inventory)
        );
        // The ask side's reservations don't bleed into the bid check.
        assert!(decide_quote(&book(), &req, 1.0, U256::ZERO, reserved_bid).is_ok());
    }

    #[test]
    fn an_unfunded_side_rejects_as_inventory() {
        let mut one_sided = book();
        one_sided.sell_spread = None;
        one_sided.sell_capacity_collateral = None;
        let mut req = request(DEBT, COLLATERAL); // hits the ask side
        req.sell_amount = Some("1000000".into());
        assert_eq!(
            decide_quote(&one_sided, &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Inventory)
        );
    }

    #[test]
    fn malformed_requests_reject_as_busy() {
        // Wrong pair for the corridor.
        let mut req = request(
            "0x00000000000000000000000000000000000000aa",
            "0x00000000000000000000000000000000000000bb",
        );
        req.sell_amount = Some("1000".into());
        assert_eq!(
            decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );

        // Neither amount, both amounts, or a non-numeric amount.
        let req = request(COLLATERAL, DEBT);
        assert_eq!(
            decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1".into());
        req.buy_amount = Some("1".into());
        assert_eq!(
            decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("12.5".into());
        assert_eq!(
            decide_quote(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );
    }

    #[test]
    fn dust_that_prices_to_zero_rejects_as_size() {
        // 18dp collateral sold for 6dp debt: one atomic unit of collateral is
        // worth zero atomic debt — an unfillable zero-input order.
        let mut wide = book();
        wide.collateral_decimals = 18;
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1".into());
        assert_eq!(
            decide_quote(&wide, &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Size)
        );
    }

    #[test]
    fn levels_shrink_with_reservations_and_drop_empty_sides() {
        let b = book();
        let frame = levels_for(&b, 1.0, U256::ZERO, U256::ZERO, "t0".into());
        assert_eq!(frame.corridor_id, "cngn-usdc");
        assert_eq!(frame.bids.len(), 1);
        assert_eq!(frame.asks.len(), 1);
        // Ask size is the raw collateral capacity; bid size converts debt
        // capacity at the bid (5e9 / 0.98).
        assert_eq!(frame.asks[0].size, "5000000000");
        assert_eq!(frame.bids[0].size, "5102040816");
        // Rates: RAY-scaled debt per collateral at each side's price.
        assert_eq!(frame.bids[0].rate_ray, "980000000000000000000000000");
        assert_eq!(frame.asks[0].rate_ray, "1020000000000000000000000000");

        // Reservations shrink the published size…
        let frame = levels_for(
            &b,
            1.0,
            U256::ZERO,
            U256::from(4_999_999_999u64),
            "t1".into(),
        );
        assert_eq!(frame.asks[0].size, "1");
        // …and a fully-claimed side disappears rather than publishing zero.
        let frame = levels_for(
            &b,
            1.0,
            U256::ZERO,
            U256::from(5_000_000_000u64),
            "t2".into(),
        );
        assert!(frame.asks.is_empty());
        assert_eq!(frame.bids.len(), 1);
    }

    #[test]
    fn a_ladder_only_pool_yields_no_book() {
        let toml = r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x0000000000000000000000000000000000000000"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://feed"
            staleness_secs = 30
            [[pools]]
            collateral = "0x0000000000000000000000000000000000000001"
            collateral_decimals = 6
            debt = "0x0000000000000000000000000000000000000002"
            debt_decimals = 6
            buy_offset_bps = 200
            buy_order_size_debt = "5000000000"
            ttl_secs = 60
            refresh_threshold_bps = 0
        "#;
        let cfg = crate::config::Config::from_toml(toml).unwrap();
        assert!(book_from_pool(&cfg.pools[0], "http://feed")
            .unwrap()
            .is_none());

        // Opting in picks up sides, capacity, and the default feed.
        let toml = toml.replace(
            "buy_order_size_debt = \"5000000000\"",
            "buy_order_size_debt = \"5000000000\"\nrfq_corridor = \"cngn-usdc\"",
        );
        let cfg = crate::config::Config::from_toml(&toml).unwrap();
        let book = book_from_pool(&cfg.pools[0], "http://feed")
            .unwrap()
            .expect("book built");
        assert_eq!(book.slug, "cngn-usdc");
        assert_eq!(book.buy_capacity_debt, Some(U256::from(5_000_000_000u64)));
        assert_eq!(
            book.sell_capacity_collateral, None,
            "sell side not configured"
        );
        assert_eq!(book.feed_url, "http://feed");
    }
}
