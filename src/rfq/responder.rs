// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Pure RFQ quoting decisions: which side a request hits, what the maker pays
//! and receives, whether capacity allows it, and the 1s level snapshot. No
//! I/O, no clocks, no signing — the session loop in [`super`] feeds these
//! functions and owns everything async, so the price/size rules stay unit
//! testable exactly like the ladder's `quote` module.

use std::collections::HashMap;

use alloy_primitives::{Address, U256};

use crate::config::{PoolConfig, RfqCapacity};
use crate::quote::{ask_price, bid_price, Spread};
use crate::tick::is_price_usable;

use super::math::{
    collateral_for_debt, debt_for_collateral, fee_on, max_fitting_output, min_feeable_output,
    rate_ray,
};
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
    /// Debt the bid side may commit; `None` = side off for RFQ.
    pub buy_capacity_debt: Option<RfqCapacity>,
    /// Collateral the ask side may commit; `None` = side off.
    pub sell_capacity_collateral: Option<RfqCapacity>,
    /// The feed this corridor prices off (pool override or the bot default).
    pub feed_url: String,
    /// How old a mark this corridor may quote off. Per book, not per bot: one
    /// bot can carry pools on differently paced feeds, and a single scalar
    /// would apply a cron-sampled corridor's window to a live-priced one.
    pub staleness_secs: u64,
}

/// Latest funded amounts (`min(balance, Permit2 allowance)` minus the live
/// book) keyed by token. Empty / missing entries fail closed for every RFQ
/// side — Exact caps still cannot outrun a missing or smaller wallet.
#[derive(Debug, Clone, Default)]
pub struct InventoryView {
    funded: HashMap<Address, U256>,
}

impl InventoryView {
    pub fn new(funded: HashMap<Address, U256>) -> Self {
        Self { funded }
    }

    pub fn funded(&self, token: Address) -> Option<U256> {
        self.funded.get(&token).copied()
    }
}

/// Tokens a book needs live wallet reads for. Any RFQ side (Exact or Wallet)
/// needs a reading — Exact is a cap on top of the wallet, not a bypass.
pub fn wallet_tokens(books: &[CorridorBook]) -> Vec<Address> {
    let mut tokens: Vec<Address> = books
        .iter()
        .flat_map(|book| {
            [
                book.buy_capacity_debt.is_some().then_some(book.debt),
                book.sell_capacity_collateral
                    .is_some()
                    .then_some(book.collateral),
            ]
        })
        .flatten()
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn resolve_capacity(
    policy: Option<RfqCapacity>,
    token: Address,
    inv: &InventoryView,
) -> Option<U256> {
    match policy? {
        RfqCapacity::Exact(v) => inv.funded(token).map(|funded| v.min(funded)),
        RfqCapacity::Wallet => inv.funded(token),
    }
}

/// Build the corridor book for a pool. `rfq_corridor` is an optional label;
/// the session binds the venue slug from tokens. Errors mirror config
/// validation (bad addresses) — unreachable for a config that passed
/// `Config::from_toml`, kept as errors so the responder can never start on
/// a half-parsed pool.
pub fn book_from_pool(
    pool: &PoolConfig,
    default_feed_url: &str,
    staleness_secs: u64,
) -> anyhow::Result<Option<CorridorBook>> {
    Ok(Some(CorridorBook {
        slug: pool.rfq_corridor.clone().unwrap_or_default(),
        collateral: pool.collateral.parse()?,
        debt: pool.debt.parse()?,
        collateral_decimals: pool.collateral_decimals,
        debt_decimals: pool.debt_decimals,
        buy_spread: pool.buy_spread(),
        sell_spread: pool.sell_spread(),
        buy_capacity_debt: pool.rfq_buy_capacity_debt()?,
        sell_capacity_collateral: pool.rfq_sell_capacity_collateral()?,
        staleness_secs,
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
    inventory: &InventoryView,
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
    if input.is_zero()
        || output.is_zero()
        || fee.is_zero()
        || output < min_feeable_output(req.fee_bps)
    {
        return Err(RejectReason::Size);
    }

    // Capacity: configured cap or the latest funded wallet reading, minus
    // what in-flight quotes already claim. A `max` side with no fresh
    // reading fails closed (inventory) rather than guessing.
    let capacity = if bid {
        resolve_capacity(book.buy_capacity_debt, book.debt, inventory)
    } else {
        resolve_capacity(book.sell_capacity_collateral, book.collateral, inventory)
    };
    let Some(capacity) = capacity else {
        return Err(RejectReason::Inventory);
    };
    let reserved = if bid { reserved_bid } else { reserved_ask };
    let available = capacity.saturating_sub(reserved);
    if available.is_zero() {
        return Err(RejectReason::Inventory);
    }
    // The venue may ask for more than we can fill (stale levels, or a slice
    // of a larger RFQ). Quote what remains instead of rejecting — the API
    // bundles several makers into one taker quote.
    let (input, output, fee, sell_amount) = if input > available {
        let input = available;
        let output = output_for_input(input);
        let fee = fee_on(output, req.fee_bps);
        if input.is_zero() || output.is_zero() || fee.is_zero() {
            return Err(RejectReason::Size);
        }
        (input, output, fee, output + fee)
    } else {
        (input, output, fee, sell_amount)
    };

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
    inventory: &InventoryView,
) -> LevelsFrame {
    let mut bids = Vec::new();
    if let (Some(spread), Some(capacity)) = (
        book.buy_spread,
        resolve_capacity(book.buy_capacity_debt, book.debt, inventory),
    ) {
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
    if let (Some(spread), Some(capacity)) = (
        book.sell_spread,
        resolve_capacity(book.sell_capacity_collateral, book.collateral, inventory),
    ) {
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
            buy_capacity_debt: Some(RfqCapacity::Exact(U256::from(5_000_000_000u64))),
            sell_capacity_collateral: Some(RfqCapacity::Exact(U256::from(5_000_000_000u64))),
            feed_url: "http://feed".into(),
            staleness_secs: 240,
        }
    }

    fn funded_inv() -> InventoryView {
        InventoryView::new(HashMap::from([
            (DEBT.parse().unwrap(), U256::from(u64::MAX)),
            (COLLATERAL.parse().unwrap(), U256::from(u64::MAX)),
        ]))
    }

    fn decide(
        book: &CorridorBook,
        req: &QuoteRequestFrame,
        mid: f64,
        reserved_bid: U256,
        reserved_ask: U256,
    ) -> Result<QuotePlan, RejectReason> {
        decide_quote(book, req, mid, reserved_bid, reserved_ask, &funded_inv())
    }

    fn levels(
        book: &CorridorBook,
        mid: f64,
        reserved_bid: U256,
        reserved_ask: U256,
        as_of: String,
    ) -> LevelsFrame {
        levels_for(book, mid, reserved_bid, reserved_ask, as_of, &funded_inv())
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
        let plan = decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();

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
        let plan = decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();

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
        let plan = decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();

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
    fn capacity_scales_down_instead_of_rejecting_size() {
        // Bigger than configured capacity: quote the cap, don't reject.
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("100000000000".into()); // maker would pay ~98e9 > 5e9
        let plan = decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();
        assert_eq!(plan.input, U256::from(5_000_000_000u64));
        assert!(plan.sell_amount < U256::from(100000000000u64));

        // Fits capacity but nothing left after in-flight quotes → inventory.
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into());
        let fully_reserved = U256::from(5_000_000_000u64);
        assert_eq!(
            decide(&book(), &req, 1.0, fully_reserved, U256::ZERO),
            Err(RejectReason::Inventory)
        );

        // Partial reservation: quote what's left rather than inventory-reject.
        let reserved_bid = U256::from(4_500_000_000u64); // 0.5e9 left
        let plan = decide(&book(), &req, 1.0, reserved_bid, U256::ZERO).unwrap();
        assert_eq!(plan.input, U256::from(500_000_000u64));

        // The ask side's reservations don't bleed into the bid check.
        assert!(decide(&book(), &req, 1.0, U256::ZERO, reserved_bid).is_ok());
    }

    #[test]
    fn an_unfunded_side_rejects_as_inventory() {
        let mut one_sided = book();
        one_sided.sell_spread = None;
        one_sided.sell_capacity_collateral = None;
        let mut req = request(DEBT, COLLATERAL); // hits the ask side
        req.sell_amount = Some("1000000".into());
        assert_eq!(
            decide(&one_sided, &req, 1.0, U256::ZERO, U256::ZERO),
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
            decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );

        // Neither amount, both amounts, or a non-numeric amount.
        let req = request(COLLATERAL, DEBT);
        assert_eq!(
            decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1".into());
        req.buy_amount = Some("1".into());
        assert_eq!(
            decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("12.5".into());
        assert_eq!(
            decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Busy)
        );
    }

    #[test]
    fn output_whose_fee_rounds_to_zero_rejects_as_size() {
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("9999".into());
        req.fee_bps = 1;
        assert_eq!(
            decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Size)
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
            decide(&wide, &req, 1.0, U256::ZERO, U256::ZERO),
            Err(RejectReason::Size)
        );
    }

    #[test]
    fn levels_shrink_with_reservations_and_drop_empty_sides() {
        let b = book();
        let frame = levels(&b, 1.0, U256::ZERO, U256::ZERO, "t0".into());
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
        let frame = levels(
            &b,
            1.0,
            U256::ZERO,
            U256::from(4_999_999_999u64),
            "t1".into(),
        );
        assert_eq!(frame.asks[0].size, "1");
        // …and a fully-claimed side disappears rather than publishing zero.
        let frame = levels(
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
    fn a_pool_is_an_rfq_book_without_a_slug() {
        let toml = r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x0000000000000000000000000000000000000000"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "https://feed"
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
        let book = book_from_pool(&cfg.pools[0], "http://feed", 240)
            .unwrap()
            .expect("every pool is a book");
        assert!(book.slug.is_empty());

        let toml = toml.replace(
            "buy_order_size_debt = \"5000000000\"",
            "buy_order_size_debt = \"5000000000\"\nrfq_corridor = \"cngn-usdc\"",
        );
        let cfg = crate::config::Config::from_toml(&toml).unwrap();
        let book = book_from_pool(&cfg.pools[0], "http://feed", 240)
            .unwrap()
            .expect("book built");
        assert_eq!(book.slug, "cngn-usdc");
        assert_eq!(
            book.buy_capacity_debt,
            Some(RfqCapacity::Exact(U256::from(5_000_000_000u64)))
        );
        assert_eq!(
            book.sell_capacity_collateral, None,
            "sell side not configured"
        );
        assert_eq!(book.feed_url, "http://feed");
    }

    #[test]
    fn max_liquidity_is_a_wallet_policy_and_needs_a_fresh_reading() {
        let mut wallet_book = book();
        wallet_book.buy_capacity_debt = Some(RfqCapacity::Wallet);
        wallet_book.sell_capacity_collateral = Some(RfqCapacity::Wallet);

        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into());

        // No reading yet — fail closed, don't guess the balance.
        assert_eq!(
            decide_quote(
                &wallet_book,
                &req,
                1.0,
                U256::ZERO,
                U256::ZERO,
                &InventoryView::default()
            ),
            Err(RejectReason::Inventory)
        );
        let dark = levels_for(
            &wallet_book,
            1.0,
            U256::ZERO,
            U256::ZERO,
            "t0".into(),
            &InventoryView::default(),
        );
        assert!(dark.bids.is_empty() && dark.asks.is_empty());

        let debt: Address = DEBT.parse().unwrap();
        let collateral: Address = COLLATERAL.parse().unwrap();
        let inv = InventoryView::new(HashMap::from([
            (debt, U256::from(2_000_000_000u64)),
            (collateral, U256::from(3_000_000_000u64)),
        ]));
        assert!(decide_quote(&wallet_book, &req, 1.0, U256::ZERO, U256::ZERO, &inv).is_ok());

        // Reservations eat the live balance; leftover is still quoted.
        let plan = decide_quote(
            &wallet_book,
            &req,
            1.0,
            U256::from(1_500_000_000u64),
            U256::ZERO,
            &inv,
        )
        .unwrap();
        assert_eq!(plan.input, U256::from(500_000_000u64));

        // Nothing left → inventory.
        assert_eq!(
            decide_quote(
                &wallet_book,
                &req,
                1.0,
                U256::from(2_000_000_000u64),
                U256::ZERO,
                &inv
            ),
            Err(RejectReason::Inventory)
        );

        let frame = levels_for(&wallet_book, 1.0, U256::ZERO, U256::ZERO, "t1".into(), &inv);
        assert_eq!(frame.asks[0].size, "3000000000");

        // An exact cap used to ignore a smaller wallet and over-sign vs the
        // ladder (audit M-03). It now mins with the funded reading.
        let exact = book();
        assert_eq!(
            decide_quote(
                &exact,
                &req,
                1.0,
                U256::ZERO,
                U256::ZERO,
                &InventoryView::default()
            ),
            Err(RejectReason::Inventory),
            "exact without a wallet reading fails closed"
        );
        let thin = InventoryView::new(HashMap::from([
            (debt, U256::from(100_000u64)),
            (collateral, U256::from(100_000u64)),
        ]));
        let thin_plan = decide_quote(&exact, &req, 1.0, U256::ZERO, U256::ZERO, &thin).unwrap();
        assert_eq!(
            thin_plan.input,
            U256::from(100_000u64),
            "thin wallet is quoted as a leftover slice, not size-rejected"
        );
        assert!(decide_quote(&exact, &req, 1.0, U256::ZERO, U256::ZERO, &inv).is_ok());
    }

    #[test]
    fn wallet_tokens_cover_every_rfq_side() {
        let exact = book();
        let tokens = wallet_tokens(&[exact.clone()]);
        assert_eq!(tokens.len(), 2);
        assert!(tokens.contains(&COLLATERAL.parse().unwrap()));
        assert!(tokens.contains(&DEBT.parse().unwrap()));

        let mut both = exact.clone();
        both.buy_capacity_debt = Some(RfqCapacity::Wallet);
        both.sell_capacity_collateral = Some(RfqCapacity::Wallet);
        let tokens = wallet_tokens(&[both]);
        assert_eq!(tokens.len(), 2);
        assert!(tokens.contains(&COLLATERAL.parse().unwrap()));
        assert!(tokens.contains(&DEBT.parse().unwrap()));
    }
}
