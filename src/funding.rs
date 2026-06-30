// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Funded-input budgeting: how much of a token the maker can actually commit
//! to new orders this tick. The budget per token is
//! `min(balance, Permit2 allowance)` minus what the book will hold after earlier
//! replacements in this tick — with the side's own live input counted as
//! reusable (a replacement supersedes it).

use std::collections::HashMap;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Context;
use tracing::{info, warn};

use crate::approve::{buy_input_amount, sell_input_amount};
use crate::closer::executor::{encode_allowance, encode_balance_of};
use crate::config::{parse_liquidity_amount, Config, LiquidityAmount};
use crate::indexer::Indexer;
use crate::rpc::Wallet;

/// A side's configured input size: an exact amount, or "max" (whatever the
/// funded budget allows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLiquidity {
    Exact(u128),
    Max,
}

/// Per-token budget for one tick: on-chain funded amount, indexer-side live
/// commitments adjusted by earlier replacements this tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FundedInputBudget {
    pub funded: U256,
    pub committed: U256,
}

/// Mutable per-tick funding state: the per-token budgets read so far, and how
/// many "max"-sized sides share each token.
pub struct TickBudgets {
    pub funded_inputs: HashMap<Address, FundedInputBudget>,
    pub max_sides_total: HashMap<Address, u32>,
    pub max_sides_remaining: HashMap<Address, u32>,
}

impl TickBudgets {
    pub fn new(max_sides_by_token: HashMap<Address, u32>) -> Self {
        Self {
            funded_inputs: HashMap::new(),
            max_sides_total: max_sides_by_token.clone(),
            max_sides_remaining: max_sides_by_token,
        }
    }
}

/// How many enabled sides quote `"max"` liquidity per input token. Sides whose
/// size fails to parse are skipped here — the quote path warns about them.
pub fn count_max_sides(cfg: &Config) -> HashMap<Address, u32> {
    cfg.pools
        .iter()
        .flat_map(|pool| {
            [
                pool.buy_enabled()
                    .then(|| (pool.debt.as_str(), buy_input_amount(pool))),
                pool.sell_enabled()
                    .then(|| (pool.collateral.as_str(), sell_input_amount(pool))),
            ]
        })
        .flatten()
        .filter_map(|(token, amount)| match amount {
            Ok(LiquidityAmount::Max) => token.parse::<Address>().ok(),
            _ => None,
        })
        .fold(HashMap::new(), |mut counts, token| {
            *counts.entry(token).or_insert(0) += 1;
            counts
        })
}

/// One "max" side's grant this tick: no more than its equal target share of the
/// funded token balance, capped by what can be posted against the current book.
/// When prior commitments are uneven, over-allocated sides shrink toward the
/// target and release budget for under-allocated sides in the same or next tick.
/// A funding deficit can still land on the first side to re-quote, because a
/// replacement cannot exceed the post-supersede funded book.
pub fn max_input_share(
    budget: &FundedInputBudget,
    reusable_input: U256,
    total_max_sides: u32,
    remaining_max_sides: u32,
) -> U256 {
    let available = available_funded_input(budget, reusable_input);
    let total = total_max_sides.max(1);
    if total == 1 {
        return available;
    }
    let remaining = remaining_max_sides.clamp(1, total);
    let processed = total.saturating_sub(remaining);
    let total_u256 = U256::from(total);
    let mut target = budget.funded / total_u256;
    if U256::from(processed) < budget.funded % total_u256 {
        target = target.saturating_add(U256::from(1u8));
    }
    available.min(target)
}

/// Grant a "max" side its share and consume one of the token's max-side slots
/// for this tick.
pub fn take_max_share(
    max_sides_total: &HashMap<Address, u32>,
    max_sides_remaining: &mut HashMap<Address, u32>,
    token: Address,
    budget: &FundedInputBudget,
    reusable_input: U256,
) -> U256 {
    let total = max_sides_total.get(&token).copied().unwrap_or(1).max(1);
    let remaining = max_sides_remaining.get(&token).copied().unwrap_or(1).max(1);
    let grant = max_input_share(budget, reusable_input, total, remaining);
    max_sides_remaining.insert(token, remaining - 1);
    grant
}

/// `min(balance, Permit2 allowance)` on-chain minus nothing yet — the fresh
/// budget for a token, with the indexer's live commitments attached.
async fn read_funded_budget(
    indexer: &Indexer,
    wallet: &Wallet,
    chain_id: u64,
    maker: Address,
    token: Address,
    permit2: Address,
) -> anyhow::Result<FundedInputBudget> {
    let balance = wallet
        .read_uint(token, &Bytes::from(encode_balance_of(wallet.address())))
        .await
        .context("could not read funded input")?;
    let allowance = wallet
        .read_uint(
            token,
            &Bytes::from(encode_allowance(wallet.address(), permit2)),
        )
        .await
        .context("could not read funded input")?;
    let committed = indexer
        .committed_input(chain_id, &maker.to_string(), &token.to_string())
        .await
        .context("could not read committed input")?;
    let committed = committed
        .parse::<U256>()
        .with_context(|| format!("could not parse committed input {committed}"))?;
    Ok(FundedInputBudget {
        funded: balance.min(allowance),
        committed,
    })
}

pub fn parse_u128(value: &str, field: &str) -> anyhow::Result<u128> {
    value
        .parse::<u128>()
        .with_context(|| format!("invalid {field}"))
}

pub fn parse_input_liquidity(value: &str, field: &str) -> anyhow::Result<InputLiquidity> {
    match parse_liquidity_amount(value, field)? {
        LiquidityAmount::Exact(amount) => u256_to_u128(amount, field).map(InputLiquidity::Exact),
        LiquidityAmount::Max => Ok(InputLiquidity::Max),
    }
}

pub fn u256_to_u128(value: U256, field: &str) -> anyhow::Result<u128> {
    value
        .to_string()
        .parse::<u128>()
        .with_context(|| format!("{field} does not fit in u128"))
}

fn fallback_liquidity(configured: InputLiquidity) -> U256 {
    match configured {
        InputLiquidity::Exact(configured) => U256::from(configured),
        InputLiquidity::Max => U256::from(u128::MAX),
    }
}

pub fn cap_input_liquidity(configured: InputLiquidity, available: U256) -> anyhow::Result<u128> {
    let capped = match configured {
        InputLiquidity::Exact(configured) => available.min(U256::from(configured)),
        InputLiquidity::Max => available,
    };
    u256_to_u128(capped, "funded input liquidity")
}

#[allow(clippy::too_many_arguments)]
pub async fn funded_input_cap(
    indexer: &Indexer,
    wallet: &Wallet,
    chain_id: u64,
    maker: Address,
    token: Address,
    permit2: Address,
    configured: InputLiquidity,
    reusable_input: U256,
    dry_run: bool,
    budgets: &mut TickBudgets,
    pair: &str,
    label: &str,
) -> Option<u128> {
    if let std::collections::hash_map::Entry::Vacant(e) = budgets.funded_inputs.entry(token) {
        match read_funded_budget(indexer, wallet, chain_id, maker, token, permit2).await {
            Ok(budget) => {
                e.insert(budget);
            }
            // `{:#}` prints the context chain ("could not read committed
            // input: <cause>"), preserving which read failed.
            Err(err) if dry_run => {
                warn!(pair = %pair, label, error = %format!("{err:#}"), "could not read funded budget; using configured size for dry-run");
                e.insert(FundedInputBudget {
                    funded: fallback_liquidity(configured),
                    ..FundedInputBudget::default()
                });
            }
            Err(err) => {
                warn!(pair = %pair, label, error = %format!("{err:#}"), "could not read funded budget; skipping side");
                return None;
            }
        }
    }

    let budget = budgets
        .funded_inputs
        .get(&token)
        .copied()
        .unwrap_or_default();
    let granted = match configured {
        InputLiquidity::Max => take_max_share(
            &budgets.max_sides_total,
            &mut budgets.max_sides_remaining,
            token,
            &budget,
            reusable_input,
        ),
        InputLiquidity::Exact(_) => available_funded_input(&budget, reusable_input),
    };
    let capped = match cap_input_liquidity(configured, granted) {
        Ok(capped) => capped,
        Err(e) => {
            warn!(pair = %pair, label, error = %e, "funded input liquidity is too large; skipping side");
            return None;
        }
    };
    if let InputLiquidity::Exact(configured) = configured {
        if capped < configured {
            warn!(
                pair = %pair,
                label,
                configured,
                capped,
                funded = %budget.funded,
                committed = %budget.committed,
                reusable = %reusable_input,
                "capping order liquidity to remaining funded input"
            );
        } else if is_underutilized(InputLiquidity::Exact(configured), granted) {
            info!(
                pair = %pair,
                label,
                configured,
                fundable = %granted,
                funded = %budget.funded,
                "wallet can back more than the configured size; raise it or set \"max\" to quote the full balance"
            );
        }
    }
    Some(capped)
}

/// True when an Exact-sized side could back strictly more than its configured
/// size — the wallet holds funded input the book never quotes. Surfaced so an
/// operator whose balance outgrew a fixed size hears about it instead of
/// silently quoting a fraction of their liquidity.
pub fn is_underutilized(configured: InputLiquidity, fundable: U256) -> bool {
    match configured {
        InputLiquidity::Exact(configured) => fundable > U256::from(configured),
        InputLiquidity::Max => false,
    }
}

pub fn available_funded_input(budget: &FundedInputBudget, reusable_input: U256) -> U256 {
    // A single side can never back more than the wallet's whole deliverable
    // balance. With a consistent read `committed >= reusable_input` (the orders
    // we're superseding are part of committed), so funded + reusable - committed
    // already lands at or below funded. But when the committed-input read is
    // stale or low — e.g. the indexer briefly timed out and returned a smaller
    // figure — the subtraction balloons toward funded + reusable, and the bot
    // drafts a ladder it can't fund. The reactor then rejects the whole batch
    // ("Order batch is not funded: requires N, maker can deliver N/2"), leaving
    // the side offline. Cap at `funded` so a bad committed read can't push a
    // side past what the wallet can actually deliver.
    budget
        .funded
        .saturating_add(reusable_input)
        .saturating_sub(budget.committed)
        .min(budget.funded)
}

/// Update the tick budget after a side posts: the old live input is superseded
/// by the drafted input, so later sides see the post-replacement book.
pub fn record_funded_input_replacement(
    funded_inputs: &mut HashMap<Address, FundedInputBudget>,
    token: Address,
    drafted_input: U256,
    reusable_input: U256,
) {
    if drafted_input == reusable_input {
        return;
    }
    let budget = funded_inputs.entry(token).or_default();
    budget.committed = budget
        .committed
        .saturating_sub(reusable_input)
        .saturating_add(drafted_input);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_liquidity_keeps_configured_size_when_fully_funded() {
        assert_eq!(
            cap_input_liquidity(InputLiquidity::Exact(1_000_000), U256::from(2_000_000u64))
                .unwrap(),
            1_000_000
        );
    }

    #[test]
    fn input_liquidity_caps_to_available_funded_input() {
        assert_eq!(
            cap_input_liquidity(InputLiquidity::Exact(1_000_000), U256::from(500_000u64)).unwrap(),
            500_000
        );
    }

    #[test]
    fn available_input_subtracts_committed_on_a_consistent_read() {
        // Healthy read: committed (80) includes the 30 we're reusing, so
        // 100 + 30 - 80 = 50 is free for this side.
        let budget = FundedInputBudget {
            funded: U256::from(100u64),
            committed: U256::from(80u64),
        };
        assert_eq!(
            available_funded_input(&budget, U256::from(30u64)),
            U256::from(50u64)
        );
    }

    #[test]
    fn available_input_never_exceeds_funded_on_a_stale_committed_read() {
        // The indexer read timed out and under-reported committed as 0, so
        // funded (100) + reusable (100) - committed (0) would be 200 — twice the
        // wallet's deliverable balance. Capped at funded, the side drafts at most
        // 100, so the reactor can't reject the batch as "not funded".
        let budget = FundedInputBudget {
            funded: U256::from(100u64),
            committed: U256::ZERO,
        };
        assert_eq!(
            available_funded_input(&budget, U256::from(100u64)),
            U256::from(100u64)
        );
    }

    #[test]
    fn max_input_liquidity_uses_all_available_funded_input() {
        assert_eq!(
            parse_input_liquidity("max", "buy_total_liquidity_debt").unwrap(),
            InputLiquidity::Max
        );
        assert_eq!(
            parse_input_liquidity("MAX", "sell_total_liquidity_collateral").unwrap(),
            InputLiquidity::Max
        );
        assert_eq!(
            cap_input_liquidity(InputLiquidity::Max, U256::from(750_000u64)).unwrap(),
            750_000
        );
    }

    #[test]
    fn underutilization_flags_exact_sizes_the_wallet_outgrew() {
        // Wallet can back 10_000 but the side only quotes 4_300.
        assert!(is_underutilized(
            InputLiquidity::Exact(4_300),
            U256::from(10_000u64)
        ));
        // Fully used or underfunded exact sizes are not underutilized.
        assert!(!is_underutilized(
            InputLiquidity::Exact(4_300),
            U256::from(4_300u64)
        ));
        assert!(!is_underutilized(
            InputLiquidity::Exact(4_300),
            U256::from(1_000u64)
        ));
        // "max" always quotes the full fundable budget.
        assert!(!is_underutilized(
            InputLiquidity::Max,
            U256::from(10_000u64)
        ));
    }

    #[test]
    fn input_budget_subtracts_other_corridor_commitments_before_max() {
        let budget = FundedInputBudget {
            funded: U256::from(4_000u64),
            committed: U256::from(8_000u64),
        };

        assert_eq!(
            available_funded_input(&budget, U256::from(4_000u64)),
            U256::ZERO
        );
    }

    #[test]
    fn input_budget_reuses_current_corridor_commitment_for_replacement() {
        let budget = FundedInputBudget {
            funded: U256::from(4_000u64),
            committed: U256::from(4_000u64),
        };

        assert_eq!(
            available_funded_input(&budget, U256::from(4_000u64)),
            U256::from(4_000u64)
        );
    }

    #[test]
    fn recording_replacement_updates_the_shared_token_budget() {
        let token: Address = "0x00000000000000000000000000000000000000bb"
            .parse()
            .unwrap();
        let mut funded_inputs = HashMap::from([(
            token,
            FundedInputBudget {
                funded: U256::from(75u64),
                committed: U256::ZERO,
            },
        )]);

        record_funded_input_replacement(&mut funded_inputs, token, U256::from(50u64), U256::ZERO);
        assert_eq!(
            available_funded_input(&funded_inputs[&token], U256::ZERO),
            U256::from(25u64)
        );

        record_funded_input_replacement(
            &mut funded_inputs,
            token,
            U256::from(25u64),
            U256::from(50u64),
        );
        assert_eq!(
            available_funded_input(&funded_inputs[&token], U256::ZERO),
            U256::from(50u64)
        );
    }

    #[test]
    fn max_share_splits_the_funded_balance_evenly_across_sides() {
        // Cold start: two max sides on one token, nothing committed yet.
        let budget = FundedInputBudget {
            funded: U256::from(10_000u64),
            committed: U256::ZERO,
        };
        assert_eq!(
            max_input_share(&budget, U256::ZERO, 2, 2),
            U256::from(5_000u64)
        );

        // The second side draws after the first posted its half.
        let budget = FundedInputBudget {
            funded: U256::from(10_000u64),
            committed: U256::from(5_000u64),
        };
        assert_eq!(
            max_input_share(&budget, U256::ZERO, 2, 1),
            U256::from(5_000u64)
        );
    }

    #[test]
    fn max_share_keeps_a_sides_live_input_when_nothing_is_free() {
        // Steady state: both sides hold half, the whole balance is committed.
        let budget = FundedInputBudget {
            funded: U256::from(10_000u64),
            committed: U256::from(10_000u64),
        };
        assert_eq!(
            max_input_share(&budget, U256::from(5_000u64), 2, 2),
            U256::from(5_000u64)
        );
    }

    #[test]
    fn max_share_splits_a_topped_up_balance_without_shrinking_either_side() {
        // Both sides hold 5k, then the wallet receives 4k more.
        let budget = FundedInputBudget {
            funded: U256::from(14_000u64),
            committed: U256::from(10_000u64),
        };
        assert_eq!(
            max_input_share(&budget, U256::from(5_000u64), 2, 2),
            U256::from(7_000u64)
        );
    }

    #[test]
    fn max_share_rebalances_when_one_side_holds_the_full_balance() {
        let token: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let mut funded_inputs = HashMap::from([(
            token,
            FundedInputBudget {
                funded: U256::from(10_000u64),
                committed: U256::from(10_000u64),
            },
        )]);

        let first_grant = max_input_share(&funded_inputs[&token], U256::from(10_000u64), 2, 2);
        assert_eq!(first_grant, U256::from(5_000u64));
        record_funded_input_replacement(
            &mut funded_inputs,
            token,
            first_grant,
            U256::from(10_000u64),
        );

        assert_eq!(
            max_input_share(&funded_inputs[&token], U256::ZERO, 2, 1),
            U256::from(5_000u64)
        );
    }

    #[test]
    fn max_share_with_one_side_takes_the_full_available_budget() {
        let budget = FundedInputBudget {
            funded: U256::from(10_000u64),
            committed: U256::from(4_000u64),
        };
        assert_eq!(
            max_input_share(&budget, U256::from(4_000u64), 1, 1),
            available_funded_input(&budget, U256::from(4_000u64))
        );
    }

    #[test]
    fn max_share_deficit_lands_on_the_first_side_to_requote() {
        // Balance fell below the live commitments: no immediately postable budget, and the
        // side shrinks to what the wallet can still back.
        let budget = FundedInputBudget {
            funded: U256::from(8_000u64),
            committed: U256::from(10_000u64),
        };
        assert_eq!(
            max_input_share(&budget, U256::from(5_000u64), 2, 2),
            U256::from(3_000u64)
        );
    }

    #[test]
    fn taking_a_max_share_consumes_one_side_slot_per_tick() {
        let token: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let mut remaining = HashMap::from([(token, 2u32)]);
        let budget = FundedInputBudget {
            funded: U256::from(10_000u64),
            committed: U256::ZERO,
        };

        let total = remaining.clone();
        assert_eq!(
            take_max_share(&total, &mut remaining, token, &budget, U256::ZERO),
            U256::from(5_000u64)
        );
        assert_eq!(remaining[&token], 1);

        // An unknown or exhausted counter behaves like a single max side.
        let other: Address = "0x00000000000000000000000000000000000000ab"
            .parse()
            .unwrap();
        assert_eq!(
            take_max_share(&HashMap::new(), &mut remaining, other, &budget, U256::ZERO),
            U256::from(10_000u64)
        );
    }

    #[test]
    fn count_max_sides_groups_enabled_max_sides_by_input_token() {
        let toml = r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x00000000000000000000000000000000000000a1"
            collateral_decimals = 6
            debt = "0x00000000000000000000000000000000000000dd"
            debt_decimals = 6
            ttl_secs = 30
            refresh_threshold_bps = 10
            buy_offset_bps = 150
            buy_total_liquidity_debt = "max"
            buy_min_slice_debt = "10000000"
            sell_offset_bps = 150
            sell_total_liquidity_collateral = "max"
            sell_min_slice_debt = "10000000"
            [[pools]]
            collateral = "0x00000000000000000000000000000000000000a2"
            collateral_decimals = 6
            debt = "0x00000000000000000000000000000000000000dd"
            debt_decimals = 6
            ttl_secs = 30
            refresh_threshold_bps = 10
            buy_offset_bps = 150
            buy_total_liquidity_debt = "max"
            buy_min_slice_debt = "10000000"
            sell_offset_bps = 150
            sell_order_size_collateral = "2000000000"
        "#;
        let cfg = crate::config::Config::from_toml(toml).unwrap();

        let counts = count_max_sides(&cfg);

        let debt: Address = "0x00000000000000000000000000000000000000dd"
            .parse()
            .unwrap();
        let coll_a1: Address = "0x00000000000000000000000000000000000000a1"
            .parse()
            .unwrap();
        let coll_a2: Address = "0x00000000000000000000000000000000000000a2"
            .parse()
            .unwrap();
        // Two pools bid "max" with the same debt token; only pool A1 asks "max".
        assert_eq!(counts.get(&debt), Some(&2));
        assert_eq!(counts.get(&coll_a1), Some(&1));
        assert_eq!(counts.get(&coll_a2), None);
    }
}
