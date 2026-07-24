// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! One side of the maker's two-sided book, end to end: gate on the requote
//! rule, size the ladder against the funded-input budget, draft slot-keyed
//! orders, persist the nonce ledger, post, and account for what was posted.
//! Bid and ask run the same flow; [`Side`] carries the few real differences
//! (token orientation, price rule, config fields, and the ask ladder's
//! debt→collateral slice conversion).

use std::collections::HashMap;
use std::path::Path;

use alloy_primitives::{Address, U256};
use tracing::{info, warn};

use crate::config::{parse_min_slice_debt, PoolConfig, DEFAULT_MAX_LADDER_ORDERS};
use crate::funding::{
    funded_input_cap, parse_input_liquidity, record_funded_input_replacement, u256_to_u128,
    InputLiquidity, TickBudgets,
};
use crate::ladder::balanced_ladder;
use crate::poster::{drafted_input, OrderDraft, Poster};
use crate::quote::{
    ask_price, bid_price, buy_amounts_at, collateral_for_debt_ceil_at, sell_amounts_at,
    SpotDeviationGuard, Spread,
};
use crate::slots::{
    forget_spent_slot_nonces, remember_slot_inputs, reusable_slot_input, save_slot_nonce_state,
    slot_nonce,
};
use crate::tick::should_requote_now;

/// Which side of the book a quote is for. Bid buys collateral with debt below
/// mid; ask sells collateral for debt above mid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    /// Log label and ladder slot prefix ("bid" / "ask").
    pub fn label(self) -> &'static str {
        match self {
            Side::Bid => "bid",
            Side::Ask => "ask",
        }
    }

    /// Direction prefix of the side's quote key ("buy" / "sell").
    fn key_prefix(self) -> &'static str {
        match self {
            Side::Bid => "buy",
            Side::Ask => "sell",
        }
    }

    /// Requote/slot key for a pair: `buy:<pair>` / `sell:<pair>`.
    pub fn key_id(self, pair: &str) -> String {
        format!("{}:{pair}", self.key_prefix())
    }

    /// `(input_token, output_token)` for this side's orders.
    pub fn tokens(self, debt: Address, collateral: Address) -> (Address, Address) {
        match self {
            Side::Bid => (debt, collateral),
            Side::Ask => (collateral, debt),
        }
    }

    fn spread(self, pool: &PoolConfig) -> Option<Spread> {
        match self {
            Side::Bid => pool.buy_spread(),
            Side::Ask => pool.sell_spread(),
        }
    }

    fn price(self, mid: f64, spread: Spread) -> f64 {
        match self {
            Side::Bid => bid_price(mid, spread),
            Side::Ask => ask_price(mid, spread),
        }
    }

    /// Clamp this side's price to the spot-deviation guard (TWAP quoting):
    /// the bid is capped from above, the ask floored from below, so a lagging
    /// smoothed center can never post more than the configured deviation
    /// through the instantaneous feed. `None` (TWAP off) passes through.
    pub fn guarded_price(self, price: f64, guard: Option<SpotDeviationGuard>) -> f64 {
        match (self, guard) {
            (Side::Bid, Some(g)) => g.clamp_bid(price),
            (Side::Ask, Some(g)) => g.clamp_ask(price),
            (_, None) => price,
        }
    }

    fn ladder_enabled(self, pool: &PoolConfig) -> bool {
        match self {
            Side::Bid => pool.buy_ladder_enabled(),
            Side::Ask => pool.sell_ladder_enabled(),
        }
    }

    /// Configured `(total_liquidity, min_slice)` for the ladder, when both set.
    fn ladder_config(self, pool: &PoolConfig) -> Option<(&str, &str)> {
        let (total, min) = match self {
            Side::Bid => (
                pool.buy_total_liquidity_debt.as_deref(),
                pool.buy_min_slice_debt.as_deref(),
            ),
            Side::Ask => (
                pool.sell_total_liquidity_collateral.as_deref(),
                pool.sell_min_slice_debt.as_deref(),
            ),
        };
        total.zip(min)
    }

    /// Config field names, for parse-error messages.
    fn ladder_field_names(self) -> (&'static str, &'static str) {
        match self {
            Side::Bid => ("buy_total_liquidity_debt", "buy_min_slice_debt"),
            Side::Ask => ("sell_total_liquidity_collateral", "sell_min_slice_debt"),
        }
    }

    fn single_size(self, pool: &PoolConfig) -> Option<&str> {
        match self {
            Side::Bid => pool.buy_order_size_debt.as_deref(),
            Side::Ask => pool.sell_order_size_collateral.as_deref(),
        }
    }

    fn single_size_field_name(self) -> &'static str {
        match self {
            Side::Bid => "buy_order_size_debt",
            Side::Ask => "sell_order_size_collateral",
        }
    }

    fn max_orders(self, pool: &PoolConfig) -> usize {
        match self {
            Side::Bid => pool.buy_max_orders,
            Side::Ask => pool.sell_max_orders,
        }
        .unwrap_or(DEFAULT_MAX_LADDER_ORDERS) as usize
    }
}

/// Per-tick I/O context shared by every side of every pool. The poster already
/// carries the chain/maker/permit2/dry-run identity, so it is the single
/// source of truth here.
pub struct TickCtx<'a> {
    pub poster: &'a Poster<'a>,
    pub wallet: &'a crate::rpc::Wallet,
    pub state_path: &'a Path,
}

/// Quoting state that lives across ticks: last posted price per side and the
/// slot-nonce ledger.
pub struct QuoteState {
    pub last_quote: HashMap<String, (f64, u64)>,
    pub next_nonce: u64,
    pub slot_nonces: HashMap<String, u64>,
    pub slot_inputs: HashMap<String, String>,
    pub slot_deadlines: HashMap<String, u64>,
}

/// How a side's quote attempt ended. `Done { posted }` carries how many orders
/// this side actually posted this tick (0 when it held — nothing to requote, no
/// funded budget, or no inventory), so the tick loop can surface activity in a
/// heartbeat; `fills` counts spent nonces observed while posting — each one is
/// an order of ours that filled on-chain, the tick loop's fill signal (the
/// inventory lean re-reads balances on it). `AbortPool` means the nonce ledger
/// could not be persisted before posting — the caller must skip the rest of
/// this pool's tick (never post an order whose nonce isn't on disk).
#[must_use]
pub enum SideOutcome {
    Done { posted: usize, fills: usize },
    AbortPool,
}

impl SideOutcome {
    fn held() -> Self {
        SideOutcome::Done {
            posted: 0,
            fills: 0,
        }
    }
}

/// Quote one side of one pool for this tick. Mirrors the historical inline
/// flow exactly: requote gate → sizes → drafts → persist ledger → post →
/// rotate any spent nonce → record posted inputs → account for the replacement.
/// `price_override` (the inventory lean's live price) replaces the configured
/// spread's price when set; sizing and everything else are unchanged. `guard`
/// (TWAP quoting's spot-deviation bound) clamps whichever price is in effect,
/// so the lean and the configured spread go through the same protection.
#[allow(clippy::too_many_arguments)]
pub async fn quote_side(
    ctx: &TickCtx<'_>,
    state: &mut QuoteState,
    budgets: &mut TickBudgets,
    pool: &PoolConfig,
    pair: &str,
    debt: Address,
    collateral: Address,
    side: Side,
    mid: f64,
    price_override: Option<f64>,
    guard: Option<SpotDeviationGuard>,
    now: u64,
) -> SideOutcome {
    // A side without a configured spread is off, override or not — the lean
    // replaces the price of a running side, it never enables one.
    let Some(spread) = side.spread(pool) else {
        return SideOutcome::held();
    };
    let price = side.guarded_price(
        price_override.unwrap_or_else(|| side.price(mid, spread)),
        guard,
    );
    let key_id = side.key_id(pair);
    let label = side.label();
    if !should_requote_now(
        state.last_quote.get(&key_id).copied(),
        price,
        pool.refresh_threshold_bps,
        now,
        pool.ttl_secs,
        pool.repost_lead_secs(),
    ) {
        return SideOutcome::held();
    }

    let reusable_input =
        reusable_slot_input(&state.slot_inputs, &state.slot_deadlines, &key_id, now);
    let (input_token, output_token) = side.tokens(debt, collateral);
    let sizes = side_sizes(
        ctx,
        budgets,
        pool,
        pair,
        side,
        price,
        input_token,
        reusable_input,
    )
    .await;
    let drafts = build_drafts(
        side,
        pool,
        price,
        sizes,
        &key_id,
        &mut state.slot_nonces,
        &mut state.next_nonce,
        pair,
    );
    if drafts.is_empty() {
        // Posting an empty batch was always a no-op; skip the ledger write too.
        return SideOutcome::held();
    }

    let input_reserved = drafted_input(&drafts);
    if !persist_slot_state(
        ctx,
        state,
        pair,
        label,
        "could not persist slot nonce state; skipping post",
    ) {
        return SideOutcome::AbortPool;
    }

    let result = ctx
        .poster
        .post_many(
            pool.ttl_secs,
            input_token,
            output_token,
            &drafts,
            label,
            price,
        )
        .await;
    if !result.spent_nonces.is_empty() {
        forget_spent_slot_nonces(
            &mut state.slot_nonces,
            &mut state.slot_inputs,
            &mut state.slot_deadlines,
            &drafts,
            &result.spent_nonces,
        );
        persist_slot_state(
            ctx,
            state,
            pair,
            label,
            "could not persist spent nonce rotation",
        );
    }
    if result.posted > 0 {
        remember_slot_inputs(
            &mut state.slot_inputs,
            &mut state.slot_deadlines,
            &key_id,
            &drafts,
            result.deadline,
        );
        persist_slot_state(
            ctx,
            state,
            pair,
            label,
            "could not persist posted slot inputs",
        );
        record_funded_input_replacement(
            &mut budgets.funded_inputs,
            input_token,
            input_reserved,
            reusable_input,
        );
        info!(pair = %pair, orders = result.posted, "posted {label} ladder");
        state.last_quote.insert(key_id, (price, now));
    }
    SideOutcome::Done {
        posted: result.posted,
        fills: result.spent_nonces.len(),
    }
}

/// Order sizes for the side, in the side's input-sizing unit (debt slices for
/// ladders on both sides; the configured input token amount for a single
/// order). Empty when the side is unconfigured, unfunded, or misconfigured.
#[allow(clippy::too_many_arguments)]
async fn side_sizes(
    ctx: &TickCtx<'_>,
    budgets: &mut TickBudgets,
    pool: &PoolConfig,
    pair: &str,
    side: Side,
    price: f64,
    input_token: Address,
    reusable_input: U256,
) -> Vec<u128> {
    let label = side.label();
    if side.ladder_enabled(pool) {
        let Some((total_str, min_str)) = side.ladder_config(pool) else {
            return Vec::new();
        };
        let (total_field, min_field) = side.ladder_field_names();
        let (total, min) = match (
            parse_input_liquidity(total_str, total_field),
            parse_min_slice_debt(min_str, min_field),
        ) {
            (Ok(total), Ok(min)) => (total, min),
            (Err(e), _) | (_, Err(e)) => {
                warn!(pair = %pair, error = %e, "invalid {} ladder; skipping {label}", side.key_prefix());
                return Vec::new();
            }
        };
        let Some(funded_total) =
            capped_input(ctx, budgets, pair, side, total, input_token, reusable_input).await
        else {
            return Vec::new();
        };
        // Both ladders slice in debt units. The ask is configured in
        // collateral, so convert the funded collateral total to its debt
        // equivalent at the ask price first.
        match side {
            Side::Bid => balanced_ladder(funded_total, min, side.max_orders(pool)),
            Side::Ask => match ask_ladder_sizes(
                price,
                funded_total,
                min,
                pool.debt_decimals,
                pool.collateral_decimals,
                side.max_orders(pool),
            ) {
                Ok(sizes) => sizes,
                Err(e) => {
                    warn!(pair = %pair, error = %e, "invalid sell ladder; skipping ask");
                    Vec::new()
                }
            },
        }
    } else if let Some(size_str) = side.single_size(pool) {
        match parse_input_liquidity(size_str, side.single_size_field_name()) {
            Ok(size) => capped_input(ctx, budgets, pair, side, size, input_token, reusable_input)
                .await
                .filter(|size| *size > 0)
                .map(|size| vec![size])
                .unwrap_or_default(),
            Err(e) => {
                warn!(pair = %pair, error = %e, "invalid {}; skipping {label}", side.single_size_field_name());
                Vec::new()
            }
        }
    } else {
        Vec::new()
    }
}

/// The side's configured size capped to the remaining funded budget.
#[allow(clippy::too_many_arguments)]
async fn capped_input(
    ctx: &TickCtx<'_>,
    budgets: &mut TickBudgets,
    pair: &str,
    side: Side,
    configured: InputLiquidity,
    input_token: Address,
    reusable_input: U256,
) -> Option<u128> {
    let poster = ctx.poster;
    funded_input_cap(
        poster.indexer,
        ctx.wallet,
        poster.chain_id,
        poster.maker,
        input_token,
        poster.permit2,
        configured,
        reusable_input,
        poster.dry_run,
        budgets,
        pair,
        side.label(),
    )
    .await
}

/// Ask ladder that remains within funded collateral after each slice
/// independently rounds its collateral input up.
///
/// For `n` slices, the sum of individual ceilings can exceed the ceiling of the
/// sum by at most `n - 1` collateral atomic units. Increase the reserve until
/// it covers the rebuilt ladder's actual slice count. The reserve is monotonic
/// and bounded by `max_orders - 1`, so this reaches a fixed point quickly while
/// a one-slice ladder still reserves nothing.
fn ask_ladder_sizes(
    price: f64,
    funded_collateral: u128,
    min_slice: u128,
    debt_decimals: u8,
    collateral_decimals: u8,
    max_orders: usize,
) -> anyhow::Result<Vec<u128>> {
    let to_debt = |collateral| {
        let (_, total_debt) =
            sell_amounts_at(price, collateral, debt_decimals, collateral_decimals);
        u256_to_u128(total_debt, "sell total debt equivalent")
    };
    let mut rounding_reserve = 0u128;
    loop {
        let collateral_budget = funded_collateral.saturating_sub(rounding_reserve);
        let sizes = balanced_ladder(to_debt(collateral_budget)?, min_slice, max_orders);
        let required_reserve = sizes.len().saturating_sub(1) as u128;
        if required_reserve <= rounding_reserve {
            return Ok(sizes);
        }
        rounding_reserve = required_reserve;
    }
}

/// Turn sizes into slot-keyed drafts. Each slice gets a stable replacement
/// slot (`bid:<i>` / `ask:<i>`, or `default` for a single order) and its
/// slot's nonce; an ask slice whose collateral equivalent overflows u128 is
/// skipped, leaving its slot index gap in place.
#[allow(clippy::too_many_arguments)]
fn build_drafts(
    side: Side,
    pool: &PoolConfig,
    price: f64,
    sizes: Vec<u128>,
    key_id: &str,
    slot_nonces: &mut HashMap<String, u64>,
    next_nonce: &mut u64,
    pair: &str,
) -> Vec<OrderDraft> {
    let laddered = side.ladder_enabled(pool);
    sizes
        .into_iter()
        .enumerate()
        .filter_map(|(i, size)| {
            let (input, output) = draft_amounts(side, laddered, price, size, pool, pair)?;
            let slot_id = if laddered {
                format!("{}:{i}", side.label())
            } else {
                "default".to_string()
            };
            let slot_key = format!("{key_id}:{slot_id}");
            let nonce = slot_nonce(slot_nonces, next_nonce, slot_key.clone());
            Some(OrderDraft {
                nonce,
                slot_key,
                input_amount: input,
                output_amount: output,
                client_order_id: laddered.then_some(slot_id),
            })
        })
        .collect()
}

/// `(input, output)` amounts for one slice. Bid sizes are debt; ask ladder
/// sizes are debt and convert to a collateral slice at the ask price; an ask
/// single order is sized in collateral directly.
fn draft_amounts(
    side: Side,
    laddered: bool,
    price: f64,
    size: u128,
    pool: &PoolConfig,
    pair: &str,
) -> Option<(U256, U256)> {
    match side {
        Side::Bid => Some(buy_amounts_at(
            price,
            size,
            pool.debt_decimals,
            pool.collateral_decimals,
        )),
        Side::Ask if laddered => {
            let collateral_for_debt = collateral_for_debt_ceil_at(
                price,
                size,
                pool.debt_decimals,
                pool.collateral_decimals,
            );
            match u256_to_u128(collateral_for_debt, "sell ladder collateral slice") {
                Ok(collateral_size) => Some(sell_amounts_at(
                    price,
                    collateral_size,
                    pool.debt_decimals,
                    pool.collateral_decimals,
                )),
                Err(e) => {
                    warn!(pair = %pair, error = %e, "invalid sell ladder slice; skipping ask order");
                    None
                }
            }
        }
        Side::Ask => Some(sell_amounts_at(
            price,
            size,
            pool.debt_decimals,
            pool.collateral_decimals,
        )),
    }
}

/// Write the nonce ledger to disk, warning with `failure` on error. A no-op
/// success in dry-run (dry-run never persists state).
fn persist_slot_state(
    ctx: &TickCtx<'_>,
    state: &QuoteState,
    pair: &str,
    label: &str,
    failure: &str,
) -> bool {
    if ctx.poster.dry_run {
        return true;
    }
    match save_slot_nonce_state(
        ctx.state_path,
        ctx.poster.chain_id,
        ctx.poster.maker,
        state.next_nonce,
        &state.slot_nonces,
        &state.slot_inputs,
        &state.slot_deadlines,
    ) {
        Ok(()) => true,
        Err(e) => {
            warn!(pair = %pair, label, error = %e, "{failure}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> PoolConfig {
        toml::from_str(
            r#"
            collateral = "0x00000000000000000000000000000000000000cc"
            collateral_decimals = 6
            debt = "0x00000000000000000000000000000000000000dd"
            debt_decimals = 6
            ttl_secs = 60
            refresh_threshold_bps = 10
            "#,
        )
        .expect("valid pool config")
    }

    const DEBT: Address = Address::new([0xdd; 20]);
    const COLLATERAL: Address = Address::new([0xcc; 20]);

    #[test]
    fn side_token_orientation_mirrors_bid_and_ask() {
        assert_eq!(Side::Bid.tokens(DEBT, COLLATERAL), (DEBT, COLLATERAL));
        assert_eq!(Side::Ask.tokens(DEBT, COLLATERAL), (COLLATERAL, DEBT));
    }

    #[test]
    fn guarded_price_clamps_each_side_toward_safety_only() {
        // Spot 3060, center lagging at 3000, 50 bps deviation budget.
        let guard = Some(SpotDeviationGuard::new(3_060.0, 50));
        // The lagging ask is lifted to trail 50 bps below spot...
        let lifted = Side::Ask.guarded_price(3_001.5, guard);
        assert!((lifted - 3_060.0 * 0.995).abs() < 1e-9);
        // ...while the bid (already deep below spot) is untouched.
        assert_eq!(Side::Bid.guarded_price(2_998.5, guard), 2_998.5);
        // No guard (TWAP off) passes prices through.
        assert_eq!(Side::Ask.guarded_price(3_001.5, None), 3_001.5);
        assert_eq!(Side::Bid.guarded_price(2_998.5, None), 2_998.5);
    }

    #[test]
    fn side_keys_and_labels_are_stable() {
        assert_eq!(Side::Bid.key_id("c/d"), "buy:c/d");
        assert_eq!(Side::Ask.key_id("c/d"), "sell:c/d");
        assert_eq!(Side::Bid.label(), "bid");
        assert_eq!(Side::Ask.label(), "ask");
    }

    #[test]
    fn bid_ladder_drafts_use_indexed_slots_and_stable_nonces() {
        let mut p = pool();
        p.buy_total_liquidity_debt = Some("100".into());
        p.buy_min_slice_debt = Some("10".into());
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 1_000u64;

        let drafts = build_drafts(
            Side::Bid,
            &p,
            1.0,
            vec![60, 40],
            "buy:pair",
            &mut slot_nonces,
            &mut next_nonce,
            "pair",
        );

        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].slot_key, "buy:pair:bid:0");
        assert_eq!(drafts[1].slot_key, "buy:pair:bid:1");
        assert_eq!(drafts[0].client_order_id.as_deref(), Some("bid:0"));
        assert_eq!(drafts[0].input_amount, U256::from(60u64));
        assert_eq!(drafts[0].output_amount, U256::from(60u64)); // price 1.0, equal decimals

        // Re-drafting the same slots reuses their nonces.
        let nonces: Vec<u64> = drafts.iter().map(|d| d.nonce).collect();
        let again = build_drafts(
            Side::Bid,
            &p,
            1.0,
            vec![60, 40],
            "buy:pair",
            &mut slot_nonces,
            &mut next_nonce,
            "pair",
        );
        assert_eq!(again.iter().map(|d| d.nonce).collect::<Vec<_>>(), nonces);
    }

    #[test]
    fn single_size_draft_uses_the_default_slot() {
        let mut p = pool();
        p.buy_order_size_debt = Some("50".into());
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 0u64;

        let drafts = build_drafts(
            Side::Bid,
            &p,
            1.0,
            vec![50],
            "buy:pair",
            &mut slot_nonces,
            &mut next_nonce,
            "pair",
        );

        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].slot_key, "buy:pair:default");
        assert_eq!(drafts[0].client_order_id, None);
    }

    #[test]
    fn ask_ladder_converts_debt_slices_to_collateral_inputs() {
        let mut p = pool();
        p.sell_total_liquidity_collateral = Some("100".into());
        p.sell_min_slice_debt = Some("10".into());
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 0u64;

        // ask 2.0 (debt per collateral): a 50-debt slice buys 25 collateral,
        // which sells back for 50 debt.
        let drafts = build_drafts(
            Side::Ask,
            &p,
            2.0,
            vec![50],
            "sell:pair",
            &mut slot_nonces,
            &mut next_nonce,
            "pair",
        );

        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].slot_key, "sell:pair:ask:0");
        assert_eq!(drafts[0].input_amount, U256::from(25u64));
        assert_eq!(drafts[0].output_amount, U256::from(50u64));
    }

    #[test]
    fn weth_ask_ladder_never_drafts_below_500_usdt_floor() {
        let mut p = pool();
        p.collateral_decimals = 18;
        p.sell_total_liquidity_collateral = Some("1000000000000000000".into());
        p.sell_min_slice_debt = Some("500000000".into());
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 0u64;

        let drafts = build_drafts(
            Side::Ask,
            &p,
            3_000.123_456,
            vec![500_000_000],
            "sell:weth/usdt",
            &mut slot_nonces,
            &mut next_nonce,
            "weth/usdt",
        );

        assert_eq!(drafts.len(), 1);
        assert!(drafts[0].output_amount >= U256::from(500_000_000u64));
    }

    #[test]
    fn rounded_weth_ask_ladder_stays_within_funded_collateral() {
        let funded = 1_000_000_000_000_000_000u128;
        let price = 3_000.123_456;
        let max_orders = 40;
        let sizes = ask_ladder_sizes(price, funded, 10_000_000, 6, 18, max_orders).unwrap();
        let mut p = pool();
        p.collateral_decimals = 18;
        p.sell_total_liquidity_collateral = Some("max".into());
        p.sell_min_slice_debt = Some("10000000".into());
        p.sell_max_orders = Some(max_orders as u32);
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 0;

        let drafts = build_drafts(
            Side::Ask,
            &p,
            price,
            sizes,
            "sell:weth/usdt",
            &mut slot_nonces,
            &mut next_nonce,
            "weth/usdt",
        );
        let drafted = drafted_input(&drafts);

        assert_eq!(drafts.len(), max_orders);
        assert!(drafted <= U256::from(funded));
        assert!(drafts
            .iter()
            .all(|draft| draft.output_amount >= U256::from(10_000_000u64)));
    }

    #[test]
    fn exactly_funded_single_ask_slice_reserves_no_rounding_overhead() {
        let funded = 500_000_000u128;
        let sizes = ask_ladder_sizes(1.0, funded, 500_000_000, 6, 6, 40).unwrap();

        assert_eq!(sizes, vec![500_000_000]);
    }

    #[test]
    fn ask_reserve_converges_when_rebuild_adds_a_slice() {
        let funded = 752u128;
        let price = 0.333_333_333;
        let sizes = ask_ladder_sizes(price, funded, 10, 6, 6, 40).unwrap();
        let mut p = pool();
        p.sell_total_liquidity_collateral = Some(funded.to_string());
        p.sell_min_slice_debt = Some("10".into());
        p.sell_max_orders = Some(40);
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 0;

        let drafts = build_drafts(
            Side::Ask,
            &p,
            price,
            sizes,
            "sell:pair",
            &mut slot_nonces,
            &mut next_nonce,
            "pair",
        );

        assert!(drafted_input(&drafts) <= U256::from(funded));
        assert!(drafts
            .iter()
            .all(|draft| draft.output_amount >= U256::from(10u64)));
    }

    #[test]
    fn ask_slice_overflowing_u128_is_skipped_and_keeps_the_index_gap() {
        let mut p = pool();
        p.sell_total_liquidity_collateral = Some("1".into());
        p.sell_min_slice_debt = Some("1".into());
        p.collateral_decimals = 18;
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 0u64;

        // At a near-zero ask, the collateral equivalent of a huge debt slice
        // overflows u128 and the slice is dropped; the next slice keeps its
        // own index (ask:1), preserving slot identity.
        let drafts = build_drafts(
            Side::Ask,
            &p,
            0.000_000_001,
            vec![u128::MAX / 2, 1_000_000],
            "sell:pair",
            &mut slot_nonces,
            &mut next_nonce,
            "pair",
        );

        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].slot_key, "sell:pair:ask:1");
    }

    #[test]
    fn ask_single_size_is_collateral_directly() {
        let p = pool();

        let (input, output) = draft_amounts(Side::Ask, false, 2.0, 100, &p, "pair").unwrap();

        assert_eq!(input, U256::from(100u64)); // collateral in
        assert_eq!(output, U256::from(200u64)); // debt out at ask 2.0
    }
}
