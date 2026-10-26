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
use crate::pricing::curve::{self, Curve, CurveParams};
use crate::pricing::quote::{ask_price, bid_price, Spread};
use crate::pricing::tick::is_price_usable;

use super::math::{
    collateral_for_debt, debt_for_collateral, fee_on, gross_of, is_quotable, max_fitting_output,
    min_feeable_output, rate_ray,
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
    /// The inventory curve, when the module runs on this corridor (see
    /// [`crate::modules::config::CurveConfig`]). `None` takes exactly the flat
    /// path this file had before the curve existed.
    pub curve: Option<BookCurve>,
}

/// A corridor's inventory curve as the quote path reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BookCurve {
    /// Quote and publish levels off the curve (Live). False records what it
    /// would have quoted and answers flat (Preview).
    pub live: bool,
    pub params: CurveParams,
}

/// What the curve reads besides the book and the request, as the host
/// measured it: the feed's reference book and the bot's own holdings.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurveInputs {
    /// The raw reference mid (debt per collateral, as the feed serves it) and
    /// the reference half-range in bps.
    pub reference: Option<(f64, f64)>,
    /// What the share is made of. `None` when there is no fresh reading.
    pub holdings: Option<Holdings>,
}

/// The bot's holdings of the corridor's two tokens, atomic. A wallet bot's
/// raw wallet balances; a vault's free balances, valued as its NAV is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Holdings {
    pub collateral: U256,
    pub debt: U256,
}

/// The wallet as the quote path sees it, two readings per token: `funded` —
/// `min(balance, Permit2 allowance)`, gas reserve off the gas coin — is what
/// capacity may pledge, and `balances` — the raw holdings — is what the quote
/// curve's inventory share is made of. Empty / missing entries fail closed
/// for every RFQ side — Exact caps still cannot outrun a missing or smaller
/// wallet — while a missing balance only sends a curve pool to its flat
/// fallback.
#[derive(Debug, Clone, Default)]
pub struct InventoryView {
    funded: HashMap<Address, U256>,
    /// Raw wallet balances, for the quote curve's inventory share. `funded`
    /// is capped by the Permit2 allowance; the share must not be, or a leg
    /// the operator never approved reads as empty and a loaded one-sided
    /// pool quotes at the aggressive end forever.
    balances: HashMap<Address, U256>,
}

impl InventoryView {
    /// A view where every balance equals its funded amount — fixtures and
    /// tests where the whole wallet is approved, which is the common case.
    pub fn new(funded: HashMap<Address, U256>) -> Self {
        let balances = funded.clone();
        Self { funded, balances }
    }

    pub fn with_balances(funded: HashMap<Address, U256>, balances: HashMap<Address, U256>) -> Self {
        Self { funded, balances }
    }

    pub fn funded(&self, token: Address) -> Option<U256> {
        self.funded.get(&token).copied()
    }

    /// The raw wallet balance — the quote curve's input, never capacity's.
    pub fn balance(&self, token: Address) -> Option<U256> {
        self.balances.get(&token).copied()
    }
}

/// Tokens a book needs live wallet reads for. Any RFQ side (Exact or Wallet)
/// needs a reading — Exact is a cap on top of the wallet, not a bypass.
pub fn wallet_tokens(books: &[CorridorBook]) -> Vec<Address> {
    let mut tokens: Vec<Address> = books
        .iter()
        .flat_map(|book| {
            // A curve corridor needs both legs read whatever its sides: the
            // share is a ratio of the two, and a one-sided pool would
            // otherwise never see its inactive leg and quote flat forever.
            let curved = book.curve.is_some();
            [
                (curved || book.buy_capacity_debt.is_some()).then_some(book.debt),
                (curved || book.sell_capacity_collateral.is_some()).then_some(book.collateral),
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

/// Remaining size this side can still sign.
///
/// `corridor_reserved` is this pool's own in-flight quotes — those eat the
/// configured Exact cap. `token_reserved` is every live claim that pays the
/// same wallet token, including siblings. Two Exact-100 pools on a 1_000
/// wallet must not let a 100 claim on A zero B's cap.
fn available_capacity(
    policy: Option<RfqCapacity>,
    token: Address,
    corridor_reserved: U256,
    token_reserved: U256,
    inv: &InventoryView,
) -> Option<U256> {
    let capacity = resolve_capacity(policy, token, inv)?;
    let funded = inv.funded(token)?;
    Some(
        capacity
            .saturating_sub(corridor_reserved)
            .min(funded.saturating_sub(token_reserved)),
    )
}

/// Build the corridor book for a pool. `rfq_corridor` is an optional label;
/// the session binds the venue slug from tokens. Errors mirror config
/// validation (bad addresses) — unreachable for a config that passed
/// `Config::from_toml`, kept as errors so the responder can never start on
/// a half-parsed pool.
/// Note: `build_runtime` collects this over every pool with `?`, so any error
/// here kills the whole responder. `PoolConfig::rfq_book_buildable` mirrors the
/// fallible steps below so the panel can refuse Start instead of letting a bot
/// come up and quote nothing — keep the two in step.
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
        curve: None,
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

/// A venue-supplied amount string as a quotable `U256`. Unparseable is a
/// malformed frame (`Busy`); anything the settlement could never carry is
/// `Size` before it reaches the fee or conversion math.
fn parse_amount(raw: &str) -> Result<U256, RejectReason> {
    let amount = raw.parse::<U256>().map_err(|_| RejectReason::Busy)?;
    is_quotable(amount)
        .then_some(amount)
        .ok_or(RejectReason::Size)
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
    token_reserved_bid: U256,
    token_reserved_ask: U256,
    inventory: &InventoryView,
) -> Result<QuotePlan, RejectReason> {
    let (bid, spread) = request_side(book, req)?;
    let price = if bid {
        bid_price(mid, spread)
    } else {
        ask_price(mid, spread)
    };
    quote_at_price(
        book,
        req,
        bid,
        price,
        reserved_bid,
        reserved_ask,
        token_reserved_bid,
        token_reserved_ask,
        inventory,
    )
}

/// What the curve did (or would have done) to one quote, for the caller's
/// log. Returned rather than logged here so the hot path stays pure and the
/// caller keeps its own tracing context.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CurveNote {
    /// The curve priced this request. In shadow (`live = false`) the answer
    /// that actually went out is `flat`; `curved` is what the curve would
    /// have said.
    Priced {
        live: bool,
        /// The flat price, debt per collateral.
        flat: f64,
        /// The curve's price for the stretch `[f0, f1]`.
        curved: f64,
        /// The wallet's naira share before the same-side advance.
        f_wallet: f64,
        /// Where this quote starts: the wallet share advanced by this side's
        /// own un-expired quotes.
        f0: f64,
        /// Where this quote would leave the share.
        f1: f64,
        /// The curve's two endpoint shades, bps off the reference mid.
        s0: f64,
        s1: f64,
        /// The quoted size and this side's already-promised size, both valued
        /// in debt units.
        size_debt: f64,
        promised_debt: f64,
        /// Share at which the stitch stops being the cheapest dollars.
        cheapest_until: f64,
    },
    /// The curve is on for this corridor but could not price this request, so
    /// the flat answer went out. The caller logs it loudly: quoting flat while
    /// loaded is the state the curve exists to defend against, and a long run
    /// on this path must be visible rather than silent.
    Flat(&'static str),
}

/// How far the reference book may sit from the jump-guarded print before the
/// curve refuses it and the side quotes flat. A sanity bound, not a tuning
/// knob. On Textile's cNGN feed the served price differs from the raw Bybit
/// mid only by the operator's centre adjustments (a few bps) and, on a `buy` or
/// `sell` alignment, by the half-range, so a reference mid or buy further than
/// 1% from the print is a bad value the jump guard never saw: it guards
/// `price` alone, and the 1,500 bps order band is the only other check.
pub const MAX_REFERENCE_GAP_BPS: f64 = 100.0;

/// Where one side's next quote starts on the curve: everything a curved
/// price needs except the quote's own size. Shared by firm quotes and the
/// levels they are judged against, so the two cannot drift apart.
#[derive(Debug, Clone, Copy)]
struct CurveStart {
    curve: Curve,
    /// The side's bps spread: the half-width of the signed order.
    order_half_bps: f64,
    half_range_bps: f64,
    /// The raw reference mid, debt per collateral.
    reference_mid: f64,
    /// The wallet's naira share before the same-side advance.
    f_wallet: f64,
    /// `f_wallet` advanced by this side's own un-expired quotes.
    f0: f64,
    /// The wallet's value in debt units: the length of the share axis.
    total: f64,
    /// This side's outstanding quoted size, in debt units.
    promised: f64,
}

impl CurveStart {
    /// The share after a quote of `size_debt` (debt units) on this side. A
    /// bid takes naira in and raises the share; an ask lowers it.
    fn end(&self, bid: bool, size_debt: f64) -> f64 {
        let step = curve::share_step(size_debt, self.total);
        if bid {
            (self.f0 + step).clamp(0.0, 1.0)
        } else {
            (self.f0 - step).clamp(0.0, 1.0)
        }
    }

    /// The signed order's price for the stretch `[from, to]`.
    fn price(&self, bid: bool, from: f64, to: f64) -> Option<f64> {
        self.curve.price(bid, from, to, self.order_half_bps)
    }
}

/// `|a / b - 1|` in bps.
fn gap_bps(a: f64, b: f64) -> f64 {
    ((a / b) - 1.0).abs() * 10_000.0
}

/// The curve for one side and where that side's next quote starts on it, or
/// the reason the side quotes flat instead. `mid` is the jump-guarded print
/// the flat price is built from; the curve anchors on `reference` and only
/// checks it against `mid`.
#[allow(clippy::too_many_arguments)]
fn curve_start(
    book: &CorridorBook,
    params: &CurveParams,
    bid: bool,
    spread: Spread,
    fee_bps: u32,
    mid: f64,
    inputs: &CurveInputs,
    reserved_bid: U256,
    reserved_ask: U256,
) -> Result<CurveStart, &'static str> {
    // The curve sizes its endpoints off the all-in cost: this side's spread
    // plus the venue fee that arrives on the request. An absolute spread has
    // no bps reading; config validation refuses that combination and this is
    // its defensive twin.
    let Spread::Bps(spread_bps) = spread else {
        return Err("the side's spread is absolute, not bps");
    };
    let all_in_bps = f64::from(spread_bps) + f64::from(fee_bps);
    let (reference_mid, half_range_bps) = inputs
        .reference
        .ok_or("the feed carries no reference book")?;
    // The feed serves the half-range its recent uncrossed books agree on and
    // omits the reference when it has none, so a width of zero or less is a
    // feed that doesn't follow that contract. Nothing to lean on either way.
    if half_range_bps <= 0.0 {
        return Err("the reference book has no width");
    }
    // The reference buy in the bot's units: dearer in NGN per USDT is
    // cheaper in USDT per cNGN.
    let reference_buy = reference_mid / (1.0 + half_range_bps / 10_000.0);
    if !(gap_bps(reference_mid, mid) <= MAX_REFERENCE_GAP_BPS
        && gap_bps(reference_buy, mid) <= MAX_REFERENCE_GAP_BPS)
    {
        return Err("the reference book is more than 1% from the guarded print");
    }
    let curve = Curve::new(reference_mid, half_range_bps, all_in_bps, params)
        .ok_or("these inputs make no curve")?;
    // The share is made of what the bot holds, never what it can promise:
    // capacity is capped by Permit2 allowances and vault policy, and a leg
    // that can't be promised (an unapproved one, a vault side at its order
    // cap) is still held, so a share read off capacity would pin the curve
    // to its most aggressive end.
    let holdings = inputs
        .holdings
        .ok_or("no fresh balance reading for the share")?;
    let (f_wallet, total) = curve::inventory_share(
        saturating_u128(holdings.collateral),
        book.collateral_decimals,
        saturating_u128(holdings.debt),
        book.debt_decimals,
        reference_mid,
    )
    .ok_or("it holds nothing to lean on")?;
    // §8 of the spec: this side's own un-expired quotes already own their
    // stretch of curve, so this one starts where they end. **This side
    // only.** A quote must never move the other side's price — otherwise
    // requesting quotes becomes a free lever on the price (measured at
    // 4.47 bps for a taker; same-side only is worth exactly zero to them).
    let promised = if bid {
        normalized(reserved_bid, book.debt_decimals)
    } else {
        normalized(reserved_ask, book.collateral_decimals) * reference_mid
    };
    let advance = curve::share_step(promised, total);
    let f0 = if bid {
        (f_wallet + advance).clamp(0.0, 1.0)
    } else {
        (f_wallet - advance).clamp(0.0, 1.0)
    };
    Ok(CurveStart {
        curve,
        // The signed order carries the SPREAD alone: the venue injects
        // fee_bps on the amounts (max_fitting_output / gross_of in
        // quote_at_price), so an order priced at the all-in would charge the
        // taker the fee twice and miss the knob endpoints by exactly fee_bps.
        // The all-in already did its job sizing s0/s1, so the realized price
        // (order + injected fee) lands on the knobs.
        order_half_bps: f64::from(spread_bps),
        half_range_bps,
        reference_mid,
        f_wallet,
        f0,
        total,
        promised,
    })
}

/// [`decide_quote`], with the corridor's inventory curve on top.
///
/// No curve on the book (the default) takes exactly the flat path with no
/// note. Preview answers flat and reports what the curve would have said.
/// Live answers off the curve.
///
/// The curve anchors on `inputs.reference`, the raw reference mid, and
/// deliberately not on `mid`, which carries the operator's manual centre
/// adjustments (the hand-rolled version of this curve; anchoring on it would
/// count the shade twice). A reference more than [`MAX_REFERENCE_GAP_BPS`]
/// from `mid` is refused and the quote goes out flat.
#[allow(clippy::too_many_arguments)]
pub fn decide_quote_curved(
    book: &CorridorBook,
    req: &QuoteRequestFrame,
    mid: f64,
    inputs: &CurveInputs,
    reserved_bid: U256,
    reserved_ask: U256,
    token_reserved_bid: U256,
    token_reserved_ask: U256,
    inventory: &InventoryView,
) -> Result<(QuotePlan, Option<CurveNote>), RejectReason> {
    let (bid, spread) = request_side(book, req)?;
    let flat = if bid {
        bid_price(mid, spread)
    } else {
        ask_price(mid, spread)
    };
    let at = |price| {
        quote_at_price(
            book,
            req,
            bid,
            price,
            reserved_bid,
            reserved_ask,
            token_reserved_bid,
            token_reserved_ask,
            inventory,
        )
    };
    let Some(book_curve) = book.curve else {
        return Ok((at(flat)?, None));
    };
    let flat_because = |reason| Ok((at(flat)?, Some(CurveNote::Flat(reason))));
    let start = match curve_start(
        book,
        &book_curve.params,
        bid,
        spread,
        req.fee_bps,
        mid,
        inputs,
        reserved_bid,
        reserved_ask,
    ) {
        Ok(start) => start,
        Err(reason) => return flat_because(reason),
    };

    // Clamp-then-price: the flat pass resolves exact-input/exact-output, the
    // fee carve-out and the capacity clamp, and the curve prices the size
    // that actually survived. Then one refresh at the curve's own price, for
    // the one case where the named side is collateral and the debt leg moves
    // with the price. That bounds the circularity far below a hundredth of a
    // bp — the debt leg's price sensitivity is itself only bps.
    let plan = at(flat)?;
    let f1 = start.end(bid, debt_leg(&plan, bid, book));
    let Some(curved) = start.price(bid, start.f0, f1) else {
        return flat_because("the curve produced no finite price");
    };
    let note = |live, curved, f1, plan: &QuotePlan| {
        let (s0, s1) = start.curve.endpoints_bps();
        CurveNote::Priced {
            live,
            flat,
            curved,
            f_wallet: start.f_wallet,
            f0: start.f0,
            f1,
            s0,
            s1,
            size_debt: debt_leg(plan, bid, book),
            promised_debt: start.promised,
            cheapest_until: start.curve.cheapest_until(start.half_range_bps),
        }
    };
    if !book_curve.live {
        let note = note(false, curved, f1, &plan);
        return Ok((plan, Some(note)));
    }
    let plan = at(curved)?;
    let f1 = start.end(bid, debt_leg(&plan, bid, book));
    let Some(curved) = start.price(bid, start.f0, f1) else {
        return flat_because("the curve produced no finite price");
    };
    let note = note(true, curved, f1, &plan);
    Ok((at(curved)?, Some(note)))
}

/// The corridor currency's share of what the bot holds and the total in
/// debt units, valued at the reference mid; `None` when the inputs make no
/// share. For the module status: pricing reads it through [`curve_start`].
pub fn holdings_share(book: &CorridorBook, inputs: &CurveInputs) -> Option<(f64, f64)> {
    let (reference_mid, _) = inputs.reference?;
    let holdings = inputs.holdings?;
    curve::inventory_share(
        saturating_u128(holdings.collateral),
        book.collateral_decimals,
        saturating_u128(holdings.debt),
        book.debt_decimals,
        reference_mid,
    )
}

/// The size a plan trades, in debt units.
pub fn plan_size(plan: &QuotePlan, book: &CorridorBook) -> f64 {
    debt_leg(plan, plan.bid, book)
}

/// The debt leg of a plan, normalized: the size that walks the share axis.
/// On the bid the maker pays debt (`input`); on the ask it receives debt
/// (`output`).
fn debt_leg(plan: &QuotePlan, bid: bool, book: &CorridorBook) -> f64 {
    normalized(
        if bid { plan.input } else { plan.output },
        book.debt_decimals,
    )
}

fn normalized(amount: U256, decimals: u8) -> f64 {
    saturating_u128(amount) as f64 / 10f64.powi(i32::from(decimals))
}

/// A reservation of `U256::MAX` (an unattributable claim) saturates; the
/// capacity check rejects the quote before a saturated share could price
/// anything.
fn saturating_u128(v: U256) -> u128 {
    u128::try_from(v).unwrap_or(u128::MAX)
}

/// Token orientation and the side's spread. sellToken = what the taker sells
/// = the maker's received output. `bid` is the maker buying collateral.
fn request_side(
    book: &CorridorBook,
    req: &QuoteRequestFrame,
) -> Result<(bool, Spread), RejectReason> {
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
    // A side with no spread was never funded; it holds no RFQ inventory.
    spread.map(|s| (bid, s)).ok_or(RejectReason::Inventory)
}

/// The amount math at one given price: exact-input/exact-output resolution,
/// the fee carve-out, the capacity clamp. Everything in today's `decide_quote`
/// below the price line, unchanged — the curve calls it a second time with its
/// own price, which is why it is split out.
#[allow(clippy::too_many_arguments)]
fn quote_at_price(
    book: &CorridorBook,
    req: &QuoteRequestFrame,
    bid: bool,
    price: f64,
    reserved_bid: U256,
    reserved_ask: U256,
    token_reserved_bid: U256,
    token_reserved_ask: U256,
    inventory: &InventoryView,
) -> Result<QuotePlan, RejectReason> {
    let (sell_token, buy_token) = if bid {
        (book.collateral, book.debt)
    } else {
        (book.debt, book.collateral)
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
            let cap = parse_amount(raw)?;
            let fit = max_fitting_output(cap, req.fee_bps);
            let input = input_for_output(fit.output);
            (input, fit.output, fit.fee, cap)
        }
        // Exact-output: the maker pays exactly buyAmount; the taker's gross is
        // the priced output plus the fee on it.
        (None, Some(raw)) => {
            let input = parse_amount(raw)?;
            let output = output_for_input(input);
            let Some(gross) = gross_of(output, req.fee_bps) else {
                return Err(RejectReason::Size);
            };
            (input, output, fee_on(output, req.fee_bps), gross)
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

    // Capacity: this pool's Exact cap minus its own claims, then the wallet
    // minus every claim that pays the same token. A `max` side with no fresh
    // reading fails closed (inventory) rather than guessing.
    let available = if bid {
        available_capacity(
            book.buy_capacity_debt,
            book.debt,
            reserved_bid,
            token_reserved_bid,
            inventory,
        )
    } else {
        available_capacity(
            book.sell_capacity_collateral,
            book.collateral,
            reserved_ask,
            token_reserved_ask,
            inventory,
        )
    };
    let Some(available) = available else {
        return Err(RejectReason::Inventory);
    };
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
        let gross = gross_of(output, req.fee_bps);
        let (Some(gross), false) = (gross, input.is_zero() || output.is_zero() || fee.is_zero())
        else {
            return Err(RejectReason::Size);
        };
        (input, output, fee, gross)
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
fn order_cap(token: Address, caps: &[(Address, U256)]) -> Option<U256> {
    caps.iter().find(|(t, _)| *t == token).map(|(_, cap)| *cap)
}

/// What one side may still publish: remaining capacity under the vault's
/// per-order cap. Debt on the bid, collateral on the ask.
fn level_capacity(
    book: &CorridorBook,
    bid: bool,
    reserved: U256,
    token_reserved: U256,
    inventory: &InventoryView,
    order_caps: &[(Address, U256)],
) -> Option<U256> {
    let (capacity, token) = if bid {
        (book.buy_capacity_debt, book.debt)
    } else {
        (book.sell_capacity_collateral, book.collateral)
    };
    available_capacity(capacity, token, reserved, token_reserved, inventory).map(|remaining| {
        match order_cap(token, order_caps) {
            Some(cap) => remaining.min(cap),
            None => remaining,
        }
    })
}

pub fn levels_for(
    book: &CorridorBook,
    mid: f64,
    reserved_bid: U256,
    reserved_ask: U256,
    token_reserved_bid: U256,
    token_reserved_ask: U256,
    as_of: String,
    inventory: &InventoryView,
    order_caps: &[(Address, U256)],
) -> LevelsFrame {
    let mut bids = Vec::new();
    if let (Some(spread), Some(remaining)) = (
        book.buy_spread,
        level_capacity(
            book,
            true,
            reserved_bid,
            token_reserved_bid,
            inventory,
            order_caps,
        ),
    ) {
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
    if let (Some(spread), Some(remaining)) = (
        book.sell_spread,
        level_capacity(
            book,
            false,
            reserved_ask,
            token_reserved_ask,
            inventory,
            order_caps,
        ),
    ) {
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
    // Only alongside a side it priced: a bookless frame has no split to explain.
    let mid_rate_ray = (!bids.is_empty() || !asks.is_empty())
        .then(|| rate_ray(mid, book.debt_decimals, book.collateral_decimals))
        .filter(|rate| !rate.is_zero())
        .map(|rate| rate.to_string());
    LevelsFrame {
        corridor_id: book.slug.clone(),
        as_of,
        bids,
        asks,
        mid_rate_ray,
    }
}

/// How many rows a Live curve publishes per side. The venue judges a firm
/// reply against the size-weighted rate of the rows the request consumes, so
/// finer rows track the curve more closely. Eight keeps the residue of a
/// part-consumed row under a bp on a whole-book stretch and stays well inside
/// the venue's 20-row cap.
pub const CURVE_LEVEL_ROWS: u64 = 8;

/// [`levels_for`], with a Live inventory curve's lean on the levels.
///
/// The venue rejects a firm reply more than `levelSlackBps` (5 by default)
/// worse for the taker than the levels it was routed on, so a Live curve that
/// published flat levels would have its loaded side refused as `level_slack`
/// exactly where it leans. A Live side publishes the curve instead (see
/// [`curve_rows`]) from the same [`curve_start`] the firm quote uses. A book
/// without the curve or in Preview, and a Live side that would answer flat,
/// publishes exactly [`levels_for`].
///
/// `fee_bps` is the venue fee the curve's endpoints are sized with. Levels
/// carry no request, so the caller passes the last one it saw; the fee is
/// per chain, and before the first request a zero only moves the shade by
/// up to the fee.
#[allow(clippy::too_many_arguments)]
pub fn levels_for_curved(
    book: &CorridorBook,
    mid: f64,
    inputs: &CurveInputs,
    fee_bps: u32,
    reserved_bid: U256,
    reserved_ask: U256,
    token_reserved_bid: U256,
    token_reserved_ask: U256,
    as_of: String,
    inventory: &InventoryView,
    order_caps: &[(Address, U256)],
) -> LevelsFrame {
    let frame = levels_for(
        book,
        mid,
        reserved_bid,
        reserved_ask,
        token_reserved_bid,
        token_reserved_ask,
        as_of,
        inventory,
        order_caps,
    );
    let Some(book_curve) = book.curve.filter(|c| c.live) else {
        return frame;
    };
    let curved = |bid: bool| -> Option<Vec<Level>> {
        let (spread, reserved, token_reserved) = if bid {
            (book.buy_spread?, reserved_bid, token_reserved_bid)
        } else {
            (book.sell_spread?, reserved_ask, token_reserved_ask)
        };
        let remaining = level_capacity(book, bid, reserved, token_reserved, inventory, order_caps)?;
        let start = curve_start(
            book,
            &book_curve.params,
            bid,
            spread,
            fee_bps,
            mid,
            inputs,
            reserved_bid,
            reserved_ask,
        )
        .ok()?;
        curve_rows(&start, book, bid, remaining)
    };
    let bids = curved(true).unwrap_or_else(|| frame.bids.clone());
    let asks = curved(false).unwrap_or_else(|| frame.asks.clone());
    LevelsFrame {
        bids,
        asks,
        ..frame
    }
}

/// The venue's default `levelSlackBps`: how much worse for the taker than its
/// levels a firm reply may be before the venue strikes it as `level_slack`.
/// Each corridor's own value arrives in the session handshake; this stands in
/// only for a venue that predates the field, which strikes at the default.
pub const DEFAULT_LEVEL_SLACK_BPS: f64 = 5.0;

/// How much worse for the taker a firm plan is than the levels it would be
/// judged against, in bps: the venue's own check (`anchorRateRay` and
/// `quoteRateRay` in api/src/v2). Positive is worse. `None` when that side
/// published no rows, where the venue skips the check.
///
/// The anchor size-weights the best-first rows the reply's collateral leg
/// consumes; past the published depth it averages every row, as the venue
/// does. A Live curve's firm quote prices from its own outstanding quotes'
/// far end, so a large same-side quote between two level publishes can move
/// the next one past the snapshot the venue routed it on; the caller declines
/// rather than reply into a strike.
pub fn worse_than_levels_bps(
    plan: &QuotePlan,
    frame: &LevelsFrame,
    debt_decimals: u8,
    collateral_decimals: u8,
) -> Option<f64> {
    let (debt, collateral, rows, size) = if plan.bid {
        (plan.input, plan.output, &frame.bids, plan.sell_amount)
    } else {
        (plan.output, plan.input, &frame.asks, plan.buy_amount)
    };
    let mut rows: Vec<(U256, U256)> = rows
        .iter()
        .filter_map(|l| Some((l.size.parse().ok()?, l.rate_ray.parse().ok()?)))
        .collect();
    if rows.is_empty() || collateral.is_zero() {
        return None;
    }
    // Best first: the dearest bid, the cheapest ask.
    rows.sort_by(|a, b| {
        if plan.bid {
            b.1.cmp(&a.1)
        } else {
            a.1.cmp(&b.1)
        }
    });
    let (mut left, mut weighted, mut consumed) = (size, U256::ZERO, U256::ZERO);
    for (row_size, rate) in &rows {
        let take = (*row_size).min(left);
        weighted = weighted.saturating_add(rate.saturating_mul(take));
        consumed += take;
        left -= take;
        if left.is_zero() {
            break;
        }
    }
    let anchor = if consumed.is_zero() {
        rows[0].1
    } else {
        weighted / consumed
    };
    // quoteRateRay: debt per collateral, decimal-normalized, RAY-scaled.
    let human = |v: U256, decimals: u8| saturating_u128(v) as f64 / 10f64.powi(i32::from(decimals));
    let firm = human(debt, debt_decimals) / human(collateral, collateral_decimals);
    let anchor = saturating_u128(anchor) as f64 / 1e27;
    let gap = (firm / anchor - 1.0) * 10_000.0;
    let worse = if plan.bid { -gap } else { gap };
    worse.is_finite().then_some(worse)
}

/// One side's levels off a Live curve: `remaining` (debt on the bid,
/// collateral on the ask, as capacity counts it) cut into
/// [`CURVE_LEVEL_ROWS`] slices, each at the curve's price for its own stretch
/// of share.
///
/// The average is additive, so the size-weighted rate of the rows a request
/// consumes is the firm quote's own average over the same stretch. The one
/// residue is a part-consumed last row: it is priced over its whole slice,
/// which is further along the curve (worse for the taker) than the part the
/// firm quote prices, so it only ever errs in the reply's favour. The shade
/// is monotone in the share, so the rows come out best-first, the order the
/// venue sorts them in. Sizes are collateral atomic on both sides, as in
/// [`levels_for`]. `None` when a slice has no usable price; the side then
/// publishes flat, as its firm quote would answer.
fn curve_rows(
    start: &CurveStart,
    book: &CorridorBook,
    bid: bool,
    remaining: U256,
) -> Option<Vec<Level>> {
    let rows = U256::from(CURVE_LEVEL_ROWS);
    // `remaining * k / rows` without the multiplication overflowing.
    let (q, r) = (remaining / rows, remaining % rows);
    let cut = |k: u64| q * U256::from(k) + r * U256::from(k) / rows;
    // The size a cut walks along the share axis, in debt units.
    let walked = |atomic: U256| {
        if bid {
            normalized(atomic, book.debt_decimals)
        } else {
            normalized(atomic, book.collateral_decimals) * start.reference_mid
        }
    };
    let priced = (0..CURVE_LEVEL_ROWS)
        .map(|k| {
            let (lo, hi) = (cut(k), cut(k + 1));
            let from = start.end(bid, walked(lo));
            let to = start.end(bid, walked(hi));
            let price = start.price(bid, from, to).filter(|p| is_price_usable(*p))?;
            let size = if bid {
                collateral_for_debt(price, hi - lo, book.debt_decimals, book.collateral_decimals)
            } else {
                hi - lo
            };
            Some((
                size,
                rate_ray(price, book.debt_decimals, book.collateral_decimals),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(
        priced
            .into_iter()
            .filter(|(size, rate)| !size.is_zero() && !rate.is_zero())
            .map(|(size, rate)| Level {
                size: size.to_string(),
                rate_ray: rate.to_string(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::Mode;

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
            curve: None,
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
        decide_quote(
            book,
            req,
            mid,
            reserved_bid,
            reserved_ask,
            reserved_bid,
            reserved_ask,
            &funded_inv(),
        )
    }

    fn levels(
        book: &CorridorBook,
        mid: f64,
        reserved_bid: U256,
        reserved_ask: U256,
        as_of: String,
    ) -> LevelsFrame {
        levels_for(
            book,
            mid,
            reserved_bid,
            reserved_ask,
            reserved_bid,
            reserved_ask,
            as_of,
            &funded_inv(),
            &[],
        )
    }

    fn decide_inv(
        book: &CorridorBook,
        req: &QuoteRequestFrame,
        mid: f64,
        reserved_bid: U256,
        reserved_ask: U256,
        inventory: &InventoryView,
    ) -> Result<QuotePlan, RejectReason> {
        decide_quote(
            book,
            req,
            mid,
            reserved_bid,
            reserved_ask,
            reserved_bid,
            reserved_ask,
            inventory,
        )
    }

    fn levels_inv(
        book: &CorridorBook,
        mid: f64,
        reserved_bid: U256,
        reserved_ask: U256,
        as_of: String,
        inventory: &InventoryView,
    ) -> LevelsFrame {
        levels_for(
            book,
            mid,
            reserved_bid,
            reserved_ask,
            reserved_bid,
            reserved_ask,
            as_of,
            inventory,
            &[],
        )
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
    fn oversized_amounts_are_rejected_before_any_math_runs() {
        // Report S-01: a sellAmount at or above U256::MAX / 10000 used to wedge
        // the fee walk-up forever. Both legs bounce it at the parse site; the
        // bound itself parses but its priced leg overflows the checked
        // conversion and reads as zero — still Size, never a wrapped small
        // input the maker would sign. Wider than U256 is a malformed frame.
        let max_quotable = U256::MAX / U256::from(10_000u64);
        let too_big = (max_quotable + U256::from(1u8)).to_string();
        let cases = [
            (Some(too_big.clone()), None, RejectReason::Size),
            (Some(U256::MAX.to_string()), None, RejectReason::Size),
            (None, Some(too_big), RejectReason::Size),
            (Some(max_quotable.to_string()), None, RejectReason::Size),
            (Some(format!("{}0", U256::MAX)), None, RejectReason::Busy),
        ];
        for (sell, buy, want) in cases {
            let mut req = request(COLLATERAL, DEBT);
            req.sell_amount = sell.clone();
            req.buy_amount = buy.clone();
            assert_eq!(
                decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO),
                Err(want),
                "sell={sell:?} buy={buy:?}"
            );
        }
        // Merely enormous (1e40, far past any supply) still clamps to capacity
        // like any other oversized order.
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some(format!("1{}", "0".repeat(40)));
        let plan = decide(&book(), &req, 1.0, U256::ZERO, U256::ZERO).unwrap();
        assert_eq!(plan.input, U256::from(5_000_000_000u64));
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
    fn levels_carry_the_mid_they_were_priced_from() {
        let b = book();
        let frame = levels(&b, 1.0, U256::ZERO, U256::ZERO, "t0".into());
        // Same units as the levels: the mid sits between the two sides.
        assert_eq!(
            frame.mid_rate_ray.as_deref(),
            Some("1000000000000000000000000000")
        );

        // Nothing published, nothing to explain.
        let full = U256::from(u64::MAX);
        let empty = levels(&b, 1.0, full, full, "t1".into());
        assert!(empty.bids.is_empty() && empty.asks.is_empty());
        assert_eq!(empty.mid_rate_ray, None);

        // Omitted on the wire when absent, so an older venue sees the same frame.
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("midRateRay"));
    }

    #[test]
    fn vault_order_caps_apply_after_reservations() {
        let b = book();
        let cap = U256::from(1_000_000_000u64);
        let reserved = U256::from(400_000_000u64);
        let caps = [(b.collateral, cap)];
        let frame = levels_for(
            &b,
            1.0,
            U256::ZERO,
            reserved,
            U256::ZERO,
            reserved,
            "t".into(),
            &funded_inv(),
            &caps,
        );
        // Remaining after reservation is 5e9 − 400e6. Cap is 1e9. Publish the
        // cap, not cap − reserved (600e6 if the wallet was clamped first).
        assert_eq!(frame.asks[0].size, "1000000000");
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
            decide_inv(
                &wallet_book,
                &req,
                1.0,
                U256::ZERO,
                U256::ZERO,
                &InventoryView::default()
            ),
            Err(RejectReason::Inventory)
        );
        let dark = levels_inv(
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
        assert!(decide_inv(&wallet_book, &req, 1.0, U256::ZERO, U256::ZERO, &inv).is_ok());

        // Reservations eat the live balance; leftover is still quoted.
        let plan = decide_inv(
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
            decide_inv(
                &wallet_book,
                &req,
                1.0,
                U256::from(2_000_000_000u64),
                U256::ZERO,
                &inv
            ),
            Err(RejectReason::Inventory)
        );

        let frame = levels_inv(&wallet_book, 1.0, U256::ZERO, U256::ZERO, "t1".into(), &inv);
        assert_eq!(frame.asks[0].size, "3000000000");

        // An exact cap used to ignore a smaller wallet and over-sign vs the
        // ladder (audit M-03). It now mins with the funded reading.
        let exact = book();
        assert_eq!(
            decide_inv(
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
        let thin_plan = decide_inv(&exact, &req, 1.0, U256::ZERO, U256::ZERO, &thin).unwrap();
        assert_eq!(
            thin_plan.input,
            U256::from(100_000u64),
            "thin wallet is quoted as a leftover slice, not size-rejected"
        );
        assert!(decide_inv(&exact, &req, 1.0, U256::ZERO, U256::ZERO, &inv).is_ok());

        // Two Exact-100 pools on a 1_000 wallet (6dp): a sibling's 100 claim
        // must not zero this pool's own cap. Amounts stay above the 1 bps
        // fee-dust floor so a leftover slice is still quotable.
        let mut capped = book();
        capped.buy_capacity_debt = Some(RfqCapacity::Exact(U256::from(100_000_000u64)));
        let fat = InventoryView::new(HashMap::from([
            (debt, U256::from(1_000_000_000u64)),
            (collateral, U256::from(1_000_000_000u64)),
        ]));
        let sibling_claim = U256::from(100_000_000u64);
        let leftover = decide_quote(
            &capped,
            &req,
            1.0,
            U256::ZERO,
            U256::ZERO,
            sibling_claim,
            U256::ZERO,
            &fat,
        )
        .unwrap();
        assert_eq!(
            leftover.input,
            U256::from(100_000_000u64),
            "this pool's Exact cap is still fully available; the sibling claim only shrinks the wallet"
        );
        let wallet_tight = InventoryView::new(HashMap::from([
            (debt, U256::from(150_000_000u64)),
            (collateral, U256::from(150_000_000u64)),
        ]));
        let squeezed = decide_quote(
            &capped,
            &req,
            1.0,
            U256::ZERO,
            U256::ZERO,
            sibling_claim,
            U256::ZERO,
            &wallet_tight,
        )
        .unwrap();
        assert_eq!(
            squeezed.input,
            U256::from(50_000_000u64),
            "wallet 150 minus the sibling's 100 leaves 50, below this pool's Exact 100"
        );
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

    // ---- the quote curve, end to end through decide_quote_curved ---------

    const MID_NGN: f64 = 1380.78;
    const HALF_RANGE: f64 = 9.40;

    fn ref_mid() -> f64 {
        1.0 / MID_NGN
    }
    fn reference() -> Option<(f64, f64)> {
        Some((ref_mid(), HALF_RANGE))
    }
    /// A curve corridor: 1 bps spread both sides (+1 bps venue fee on the
    /// request = 2 bps all-in), capacity wide open so only the wallet binds.
    fn curve_book(mode: Mode, start: f64, end: f64) -> CorridorBook {
        let mut b = book();
        b.buy_spread = Some(Spread::Bps(1));
        b.sell_spread = Some(Spread::Bps(1));
        b.buy_capacity_debt = Some(RfqCapacity::Exact(U256::from(u64::MAX)));
        b.sell_capacity_collateral = Some(RfqCapacity::Exact(U256::from(u64::MAX)));
        b.curve = (mode != Mode::Off).then_some(BookCurve {
            live: mode == Mode::Live,
            params: CurveParams {
                start_below_mid: start,
                end_above_buy: end,
                shape: 2.0,
            },
        });
        b
    }

    /// A wallet holding `cngn_value` worth of collateral (valued in debt at
    /// the reference mid) and `usdt` of debt, both 6dp.
    fn wallet(cngn_value: f64, usdt: f64) -> InventoryView {
        let coll = (cngn_value / ref_mid() * 1e6).round() as u128;
        InventoryView::new(HashMap::from([
            (COLLATERAL.parse().unwrap(), U256::from(coll)),
            (
                DEBT.parse().unwrap(),
                U256::from((usdt * 1e6).round() as u128),
            ),
        ]))
    }
    /// What a wallet bot's host hands the curve: the feed's reference book
    /// and the wallet's raw balances.
    fn inputs(b: &CorridorBook, inv: &InventoryView, reference: Option<(f64, f64)>) -> CurveInputs {
        CurveInputs {
            reference,
            holdings: inv
                .balance(b.collateral)
                .zip(inv.balance(b.debt))
                .map(|(collateral, debt)| Holdings { collateral, debt }),
        }
    }
    fn curved(
        b: &CorridorBook,
        req: &QuoteRequestFrame,
        reserved_bid: U256,
        reserved_ask: U256,
        inv: &InventoryView,
    ) -> (QuotePlan, Option<CurveNote>) {
        decide_quote_curved(
            b,
            req,
            ref_mid(),
            &inputs(b, inv, reference()),
            reserved_bid,
            reserved_ask,
            reserved_bid,
            reserved_ask,
            inv,
        )
        .unwrap()
    }
    fn priced(note: Option<CurveNote>) -> (bool, f64, f64, f64, f64) {
        match note {
            Some(CurveNote::Priced {
                live,
                flat,
                curved,
                f0,
                f1,
                ..
            }) => (live, flat, curved, f0, f1),
            other => panic!("expected a priced note, got {other:?}"),
        }
    }

    #[test]
    fn off_takes_the_flat_path_with_no_note() {
        let b = curve_book(Mode::Off, 0.0, 0.0);
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into());
        let inv = wallet(150_000.0, 150_000.0);
        let (plan, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        assert_eq!(note, None);
        let flat = decide_quote(
            &b,
            &req,
            ref_mid(),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            &inv,
        )
        .unwrap();
        assert_eq!(plan, flat, "Off must be byte-identical to today");
    }

    #[test]
    fn shadow_answers_flat_and_reports_what_the_curve_would_have_said() {
        let b = curve_book(Mode::Shadow, 0.0, 0.0);
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into());
        let inv = wallet(240_000.0, 60_000.0); // loaded: f = 0.8
        let (plan, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        let flat_plan = decide_quote(
            &b,
            &req,
            ref_mid(),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            &inv,
        )
        .unwrap();
        assert_eq!(plan, flat_plan, "shadow must not change the answer");
        let (live, flat, curve_px, _, _) = priced(note);
        assert!(!live);
        assert!(
            ((curve_px / flat - 1.0) * 10_000.0).abs() > 0.5,
            "loaded at 80%, the curve must disagree with flat by whole bps"
        );
    }

    /// The taker-REALIZED rate of a plan, in NGN per USDT: the taker's gross
    /// against what they receive, fee included — the number that must land on
    /// the knobs. Internal order prices are deliberately not used here; they
    /// carry only the spread, and judging the endpoints off them is the blind
    /// spot that let a fee double-count pass review once already.
    fn realized_ngn(plan: &QuotePlan) -> f64 {
        let f = |v: U256| u128::try_from(v).unwrap() as f64;
        if plan.bid {
            // Taker sells cNGN (gross, fee carved inside) for USDT.
            f(plan.sell_amount) / f(plan.buy_amount)
        } else {
            // Taker pays USDT (gross) for cNGN.
            f(plan.input) / f(plan.sell_amount)
        }
    }

    #[test]
    fn live_lands_both_endpoints_on_the_knobs_at_the_realized_rate() {
        // Bigger sizes than the old internal-price test, so the fee's integer
        // floor rounding stays under the tolerance: ~0.3% of the wallet, well
        // inside 0.1 bps of curve movement on a 300k book.
        for (start, end) in [(0.0, 0.0), (0.5, 1.0)] {
            let b = curve_book(Mode::Live, start, end);
            // f = 0: no naira. A bid (the stitch buys cNGN = sells dollars)
            // must land on mid - start, fee and spread included.
            let mut req = request(COLLATERAL, DEBT);
            req.buy_amount = Some("1000000000".into()); // 1,000 USDT
            let inv = wallet(0.0, 300_000.0);
            let (plan, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
            let (_, _, _, f0, _) = priced(note);
            assert!(f0 < 1e-6);
            let got = realized_ngn(&plan);
            let want = MID_NGN - start;
            assert!(
                ((got / want - 1.0) * 10_000.0).abs() < 0.1,
                "start knob {start}: taker-realized {got}, want {want}"
            );
            // f = 1: all naira. An ask (the stitch sells cNGN = buys dollars)
            // must land on buy + end.
            let mut req = request(DEBT, COLLATERAL);
            req.sell_amount = Some("1000000000".into()); // taker sells 1,000 USDT
            let inv = wallet(300_000.0, 0.0);
            let (plan, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
            let (_, _, _, f0, _) = priced(note);
            assert!(f0 > 1.0 - 1e-6);
            let got = realized_ngn(&plan);
            let want = MID_NGN * (1.0 + HALF_RANGE / 10_000.0) + end;
            assert!(
                ((got / want - 1.0) * 10_000.0).abs() < 0.1,
                "end knob {end}: taker-realized {got}, want {want}"
            );
        }
    }

    #[test]
    fn ask_exact_output_realizes_on_the_knob_too() {
        // The fourth request shape — taker names the exact cNGN to receive —
        // runs the gross_of branch no other realized test touches.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(300_000.0, 0.0); // f = 1
        let mut req = request(DEBT, COLLATERAL);
        // Taker wants exactly ~1,381,000 cNGN (about 1,000 USDT worth).
        req.buy_amount = Some("1381000000000".into());
        let (plan, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        let (_, _, _, f0, _) = priced(note);
        assert!(f0 > 1.0 - 1e-6);
        // Exact-output ask: the maker pays exactly buy_amount cNGN (input),
        // the taker's gross is sell_amount USDT. Realized NGN per USDT:
        let f = |v: U256| u128::try_from(v).unwrap() as f64;
        let got = f(plan.input) / f(plan.sell_amount);
        let want = MID_NGN * (1.0 + HALF_RANGE / 10_000.0);
        assert!(
            ((got / want - 1.0) * 10_000.0).abs() < 0.1,
            "ask exact-output realized {got}, want {want}"
        );
    }

    #[test]
    fn the_realized_endpoint_does_not_move_with_the_venue_fee() {
        // allIn is derived per request so the endpoint stays put when the fee
        // changes: the order price absorbs the difference. This is the test
        // that catches charging the fee inside the order AND injecting it.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(0.0, 300_000.0);
        let mut got = Vec::new();
        for fee in [1u32, 5] {
            let mut req = request(COLLATERAL, DEBT);
            req.buy_amount = Some("1000000000".into());
            req.fee_bps = fee;
            let (plan, _) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
            got.push(realized_ngn(&plan));
        }
        for g in &got {
            assert!(
                ((g / MID_NGN - 1.0) * 10_000.0).abs() < 0.1,
                "realized endpoint {g} drifted off the mid {MID_NGN}"
            );
        }
    }

    #[test]
    fn a_request_past_capacity_prices_the_clamped_size() {
        // Clamp-then-price: the stretch must be the size that survived the
        // capacity clamp, not the size asked for.
        let mut b = curve_book(Mode::Live, 0.0, 0.0);
        b.buy_capacity_debt = Some(RfqCapacity::Exact(U256::from(30_000_000_000u64)));
        let mut req = request(COLLATERAL, DEBT);
        req.buy_amount = Some("90000000000".into()); // asks 90k, capacity 30k
        let inv = wallet(150_000.0, 150_000.0);
        let (plan, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        assert_eq!(plan.input, U256::from(30_000_000_000u64), "clamped");
        let (_, _, _, f0, f1) = priced(note);
        assert!(
            ((f1 - f0) - 0.1).abs() < 1e-3,
            "stretch must be the clamped 30k on a 300k book (0.1), got {}",
            f1 - f0
        );
    }

    #[test]
    fn a_second_quote_on_the_same_side_starts_where_the_first_ends() {
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let mut req = request(COLLATERAL, DEBT);
        req.buy_amount = Some("30000000000".into()); // 30k USDT
        let inv = wallet(150_000.0, 150_000.0);
        let (_, fresh) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        // 30k USDT already promised on the bid: the next bid prices from the
        // far end of that stretch, which is worse for the taker.
        let held = U256::from(30_000_000_000u64);
        let (_, after) = curved(&b, &req, held, U256::ZERO, &inv);
        let (_, _, px_fresh, f0_fresh, _) = priced(fresh);
        let (_, _, px_after, f0_after, _) = priced(after);
        assert!(
            (f0_after - f0_fresh - 0.1).abs() < 1e-3,
            "30k on a 300k book advances the share by 0.1: {f0_fresh} -> {f0_after}"
        );
        assert!(
            px_after < px_fresh,
            "the maker must pay less per cNGN once loaded by its own promises"
        );
    }

    #[test]
    fn a_quote_held_on_one_side_never_moves_the_other() {
        // The 4.47 bps lever, closed: hold a huge bid quote, ask for the
        // other side — the price must not move at all.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let mut req = request(DEBT, COLLATERAL);
        req.sell_amount = Some("30000000000".into());
        let inv = wallet(150_000.0, 150_000.0);
        let (_, clean) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        let (_, loaded) = curved(
            &b,
            &req,
            U256::from(100_000_000_000u64), // 100k USDT held on the bid
            U256::ZERO,
            &inv,
        );
        let (_, _, px_clean, _, _) = priced(clean);
        let (_, _, px_loaded, _, _) = priced(loaded);
        assert_eq!(
            px_clean, px_loaded,
            "a held bid quote moved the ask price: that is the free lever"
        );
    }

    #[test]
    fn a_live_round_trip_pays_the_stitch_the_spread() {
        // Buy a fifth of the book, sell it straight back, reference unmoved.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(150_000.0, 150_000.0);
        // Leg 1: taker buys cNGN (our ask), selling 30k USDT.
        let mut leg1 = request(DEBT, COLLATERAL);
        leg1.sell_amount = Some("30000000000".into());
        let (plan1, note1) = curved(&b, &leg1, U256::ZERO, U256::ZERO, &inv);
        let (_, _, p1, f0_1, f1_1) = priced(note1);
        // The fill lands: collateral out, debt in.
        let coll0 = (150_000.0 / ref_mid() * 1e6).round() as u128;
        let after = InventoryView::new(HashMap::from([
            (COLLATERAL.parse().unwrap(), U256::from(coll0) - plan1.input),
            (
                DEBT.parse().unwrap(),
                U256::from(150_000_000_000u128) + plan1.output,
            ),
        ]));
        // Leg 2: taker sells the same cNGN back (our bid).
        let mut leg2 = request(COLLATERAL, DEBT);
        leg2.sell_amount = Some(plan1.input.to_string());
        let (_, note2) = curved(&b, &leg2, U256::ZERO, U256::ZERO, &after);
        let (_, _, p2, f0_2, f1_2) = priced(note2);
        // The return leg walks the same stretch back.
        assert!(
            (f0_2 - f1_1).abs() < 2e-3,
            "legs must chain: {f1_1} vs {f0_2}"
        );
        assert!((f1_2 - f0_1).abs() < 2e-3, "and return: {f0_1} vs {f1_2}");
        // Internal order prices carry only the spread, so out-and-back on
        // them costs 2 x spread...
        let pnl_bps = (p2 / p1 - 1.0) * 10_000.0;
        assert!(
            (-2.3..-1.6).contains(&pnl_bps),
            "order-price round trip should cost 2 x spread, got {pnl_bps}"
        );
        // ...and the REALIZED round trip — gross in, gross out, venue fee on
        // both legs — costs the full all-in twice: about -4 bps, never
        // positive. This is the number the taker actually loses.
        let paid = u128::try_from(plan1.sell_amount).unwrap() as f64
            / u128::try_from(plan1.input).unwrap() as f64;
        let (plan2, _) = curved(&b, &leg2, U256::ZERO, U256::ZERO, &after);
        let received = u128::try_from(plan2.buy_amount).unwrap() as f64
            / u128::try_from(plan2.sell_amount).unwrap() as f64;
        let realized_bps = (received / paid - 1.0) * 10_000.0;
        assert!(
            (-4.4..-3.5).contains(&realized_bps),
            "realized round trip should cost about 4 bps, got {realized_bps}"
        );
    }

    #[test]
    fn an_unapproved_leg_still_counts_toward_the_share() {
        // funded = min(balance, allowance). A buy-only pool whose cNGN is
        // never approved to Permit2 has funded(cNGN) = 0 forever — but the
        // naira is still held, and a share read off funded would keep the
        // stitch bidding at the aggressive empty end while fully loaded.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let coll: Address = COLLATERAL.parse().unwrap();
        let debt: Address = DEBT.parse().unwrap();
        let coll_atomic = U256::from((240_000.0 / ref_mid() * 1e6) as u128);
        let loaded_unapproved = InventoryView::with_balances(
            HashMap::from([(coll, U256::ZERO), (debt, U256::from(60_000_000_000u64))]),
            HashMap::from([(coll, coll_atomic), (debt, U256::from(60_000_000_000u64))]),
        );
        let mut req = request(COLLATERAL, DEBT);
        req.buy_amount = Some("1000000000".into());
        let (_, note) = curved(&b, &req, U256::ZERO, U256::ZERO, &loaded_unapproved);
        let (_, _, _, f0, _) = priced(note);
        assert!(
            (f0 - 0.8).abs() < 1e-3,
            "the share must be 0.8 off balances, not 0 off funded: {f0}"
        );
    }

    #[test]
    fn a_curved_corridor_reads_both_wallet_legs() {
        // A buy-only pool never configures sell capacity, so wallet_tokens
        // would skip its collateral — and the curve would have no share.
        let mut b = curve_book(Mode::Shadow, 0.0, 0.0);
        b.sell_capacity_collateral = None;
        let read = wallet_tokens(&[b]);
        assert!(read.contains(&COLLATERAL.parse().unwrap()));
        assert!(read.contains(&DEBT.parse().unwrap()));
        // While a curve-off one-sided pool keeps reading only its active leg.
        let mut off = book();
        off.sell_capacity_collateral = None;
        let read = wallet_tokens(&[off]);
        assert!(!read.contains(&COLLATERAL.parse().unwrap()));
        assert!(read.contains(&DEBT.parse().unwrap()));
    }

    #[test]
    fn two_reserved_quotes_price_the_same_as_one_big_one() {
        // Same-side, out of order: reservations hand out disjoint stretches,
        // so the two quotes together must charge what one quote for the sum
        // would have — the integral is exact under splitting.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(150_000.0, 150_000.0);
        let mk = |usdt: &str| {
            let mut req = request(COLLATERAL, DEBT);
            req.buy_amount = Some(usdt.into());
            req
        };
        let f = |v: U256| u128::try_from(v).unwrap() as f64;
        // First quote: 30k, nothing outstanding.
        let (p1, _) = curved(&b, &mk("30000000000"), U256::ZERO, U256::ZERO, &inv);
        // Second quote: 20k, with the first's 30k reserved on the bid.
        let (p2, _) = curved(
            &b,
            &mk("20000000000"),
            U256::from(30_000_000_000u64),
            U256::ZERO,
            &inv,
        );
        // One 50k quote, fresh book.
        let (whole, _) = curved(&b, &mk("50000000000"), U256::ZERO, U256::ZERO, &inv);
        let split_coll = f(p1.output) + f(p2.output);
        let whole_coll = f(whole.output);
        assert!(
            ((split_coll / whole_coll - 1.0) * 10_000.0).abs() < 0.01,
            "split {split_coll} vs whole {whole_coll}"
        );
    }

    #[test]
    fn an_opposite_side_trade_between_quote_and_fill_pays_the_stitch() {
        // A held quote goes stale when the other side trades first — and the
        // staleness must always favour the stitch: the intervening trade
        // moves the wallet away from the state that made the held price
        // generous.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(150_000.0, 150_000.0);
        // Held: an ask (taker buys cNGN) priced at f = 0.5.
        let mut ask = request(DEBT, COLLATERAL);
        ask.sell_amount = Some("30000000000".into());
        let (held, note) = curved(&b, &ask, U256::ZERO, U256::ZERO, &inv);
        let (_, _, held_px, _, _) = priced(note);
        let _ = held;
        // Meanwhile a bid executes: someone sells us 30k USDT of cNGN, so the
        // wallet is MORE loaded with naira when the held ask finally fills.
        let after = wallet(180_000.0, 120_000.0);
        let (_, note) = curved(&b, &ask, U256::ZERO, U256::ZERO, &after);
        let (_, _, fresh_px, _, _) = priced(note);
        // The held order's price (set at 0.5) sells cNGN dearer than the
        // fresh price at the loaded wallet — in USDT per cNGN the held ask is
        // HIGHER, so filling the stale quote pays the stitch more than a
        // fresh quote would.
        assert!(
            held_px > fresh_px,
            "stale-against-the-taker: held {held_px} must exceed fresh {fresh_px}"
        );
    }

    #[test]
    fn the_curve_falls_flat_loudly_when_it_cannot_price() {
        let b = curve_book(Mode::Shadow, 0.0, 0.0);
        let mut req = request(COLLATERAL, DEBT);
        req.sell_amount = Some("1000000000".into());
        let inv = wallet(150_000.0, 150_000.0);
        // No reference book on the feed.
        let (plan, note) = decide_quote_curved(
            &b,
            &req,
            ref_mid(),
            &inputs(&b, &inv, None),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            &inv,
        )
        .unwrap();
        assert!(matches!(note, Some(CurveNote::Flat(_))), "{note:?}");
        let flat = decide_quote(
            &b,
            &req,
            ref_mid(),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            &inv,
        )
        .unwrap();
        assert_eq!(plan, flat);
        // A reference with no width has nothing to lean on: flat too.
        let (_, note) = decide_quote_curved(
            &b,
            &req,
            ref_mid(),
            &inputs(&b, &inv, Some((ref_mid(), 0.0))),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            &inv,
        )
        .unwrap();
        assert!(matches!(note, Some(CurveNote::Flat(_))), "{note:?}");
    }

    #[test]
    fn a_reference_far_from_the_guarded_print_quotes_flat() {
        // The jump guard sees `price` only. A reference mid or buy more than
        // MAX_REFERENCE_GAP_BPS from that print is a bad value, and the live
        // curve must not anchor a signed order on it.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(240_000.0, 60_000.0);
        let flat_reason = |reference: (f64, f64)| {
            let mut req = request(COLLATERAL, DEBT);
            req.buy_amount = Some("1000000000".into());
            let (plan, note) = decide_quote_curved(
                &b,
                &req,
                ref_mid(),
                &inputs(&b, &inv, Some(reference)),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                &inv,
            )
            .unwrap();
            let flat = decide_quote(
                &b,
                &req,
                ref_mid(),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                &inv,
            )
            .unwrap();
            match note {
                Some(CurveNote::Flat(reason)) => {
                    assert_eq!(plan, flat, "a refused reference answers flat");
                    Some(reason)
                }
                _ => None,
            }
        };
        let gap = |bps: f64| ref_mid() * (1.0 + bps / 10_000.0);
        // A mid 2% off the print, either way.
        assert!(flat_reason((gap(200.0), HALF_RANGE)).is_some());
        assert!(flat_reason((gap(-200.0), HALF_RANGE)).is_some());
        // A sane mid with a half-range that puts the buy 3% away.
        assert!(flat_reason((ref_mid(), 300.0)).is_some());
        // Inside the bound — the manual adjustment, or a buy-aligned print
        // a half-range off the mid — the curve still prices.
        assert_eq!(flat_reason((gap(-8.0), HALF_RANGE)), None);
        assert_eq!(flat_reason((gap(-HALF_RANGE), HALF_RANGE)), None);
        assert_eq!(flat_reason((ref_mid(), 90.0)), None);
    }

    /// How far a firm plan sits from its levels, bps, positive = worse for
    /// the taker — the number the venue's `level_slack` check bounds.
    fn slack_used_bps(plan: &QuotePlan, frame: &LevelsFrame) -> f64 {
        worse_than_levels_bps(plan, frame, 6, 6).expect("the side has levels")
    }

    fn live_levels(b: &CorridorBook, reserved: (U256, U256), inv: &InventoryView) -> LevelsFrame {
        levels_for_curved(
            b,
            ref_mid(),
            &inputs(b, inv, reference()),
            1,
            reserved.0,
            reserved.1,
            reserved.0,
            reserved.1,
            "now".into(),
            inv,
            &[],
        )
    }

    #[test]
    fn live_firm_quotes_land_inside_the_venues_level_slack() {
        // Every request shape, from empty to full of naira, small to most of
        // the book, with and without this side's own quotes outstanding: the
        // firm reply must sit on its published levels (never more than a
        // tenth of a bp worse for the taker), where flat levels put a
        // loaded bid well past the venue's 5 bps.
        let mut checked = 0;
        for knobs in [(0.0, 0.0), (0.5, 1.0)] {
            let b = curve_book(Mode::Live, knobs.0, knobs.1);
            for naira in [0.0, 0.1, 0.5, 0.82, 0.95, 1.0] {
                let inv = wallet(300_000.0 * naira, 300_000.0 * (1.0 - naira));
                for held in [0u64, 20_000_000_000] {
                    for (bid, exact_in, usdt) in [
                        (true, false, 1_000.0),
                        (true, false, 90_000.0),
                        (true, true, 30_000.0),
                        (false, true, 1_000.0),
                        (false, true, 90_000.0),
                        (false, false, 30_000.0),
                    ] {
                        let has = if bid {
                            300_000.0 * (1.0 - naira)
                        } else {
                            300_000.0 * naira
                        };
                        if has < usdt + 25_000.0 {
                            continue; // past what this wallet can quote
                        }
                        let reserved = if bid {
                            (U256::from(held), U256::ZERO)
                        } else {
                            (U256::ZERO, U256::from((held as f64 / ref_mid()) as u128))
                        };
                        let mut req = if bid {
                            request(COLLATERAL, DEBT)
                        } else {
                            request(DEBT, COLLATERAL)
                        };
                        let atomic_usdt = format!("{}", (usdt * 1e6) as u128);
                        let atomic_cngn = format!("{}", (usdt / ref_mid() * 1e6) as u128);
                        match (bid, exact_in) {
                            (true, true) => req.sell_amount = Some(atomic_cngn),
                            (true, false) => req.buy_amount = Some(atomic_usdt),
                            (false, true) => req.sell_amount = Some(atomic_usdt),
                            (false, false) => req.buy_amount = Some(atomic_cngn),
                        }
                        let (plan, note) = curved(&b, &req, reserved.0, reserved.1, &inv);
                        assert!(matches!(note, Some(CurveNote::Priced { live: true, .. })));
                        let used = slack_used_bps(&plan, &live_levels(&b, reserved, &inv));
                        assert!(
                            used < 0.1,
                            "knobs {knobs:?} f {naira} held {held} bid {bid} in {exact_in} \
                             {usdt}: firm is {used:.3} bps worse than its levels"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked >= 80, "only {checked} cases ran");
        // The bug this closes: the same loaded bid against FLAT levels.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let inv = wallet(246_000.0, 54_000.0); // 82% naira
        let mut req = request(COLLATERAL, DEBT);
        req.buy_amount = Some("1000000000".into());
        let (plan, _) = curved(&b, &req, U256::ZERO, U256::ZERO, &inv);
        let flat = levels_for(
            &b,
            ref_mid(),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            "now".into(),
            &inv,
            &[],
        );
        assert!(
            slack_used_bps(&plan, &flat) > 5.0,
            "flat levels would strike this reply"
        );
    }

    /// A levels frame as the venue receives it.
    fn wire(frame: &LevelsFrame) -> String {
        serde_json::to_string(frame).unwrap()
    }

    #[test]
    fn only_a_live_curve_changes_the_levels() {
        let inv = wallet(240_000.0, 60_000.0);
        let flat = |b: &CorridorBook| {
            levels_for(
                b,
                ref_mid(),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                "now".into(),
                &inv,
                &[],
            )
        };
        for mode in [Mode::Off, Mode::Shadow] {
            let b = curve_book(mode, 0.0, 0.0);
            assert_eq!(
                wire(&live_levels(&b, (U256::ZERO, U256::ZERO), &inv)),
                wire(&flat(&b)),
                "{mode:?}"
            );
        }
        // Live without a reference book publishes flat, as its quotes answer.
        let b = curve_book(Mode::Live, 0.0, 0.0);
        let no_reference = levels_for_curved(
            &b,
            ref_mid(),
            &inputs(&b, &inv, None),
            1,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            "now".into(),
            &inv,
            &[],
        );
        assert_eq!(wire(&no_reference), wire(&flat(&b)));
        // Live with one: CURVE_LEVEL_ROWS rows a side, best first, and the ask
        // publishes exactly the collateral the flat level did.
        let live = live_levels(&b, (U256::ZERO, U256::ZERO), &inv);
        let rows = usize::try_from(CURVE_LEVEL_ROWS).unwrap();
        assert_eq!((live.bids.len(), live.asks.len()), (rows, rows));
        let rate = |l: &Level| l.rate_ray.parse::<U256>().unwrap();
        assert!(live.bids.windows(2).all(|w| rate(&w[0]) > rate(&w[1])));
        assert!(live.asks.windows(2).all(|w| rate(&w[0]) < rate(&w[1])));
        let depth = |rows: &[Level]| {
            rows.iter()
                .map(|l| l.size.parse::<U256>().unwrap())
                .sum::<U256>()
        };
        assert_eq!(depth(&live.asks), depth(&flat(&b).asks));
        assert_eq!(live.mid_rate_ray, flat(&b).mid_rate_ray);
    }
}
