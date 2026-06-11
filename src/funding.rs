// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Funded-input budgeting: how much of a token the maker can actually commit
//! to new orders this tick. The budget per token is
//! `min(balance, Permit2 allowance)` minus what the indexer already holds as
//! live commitments, minus what earlier sides of this tick reserved — with the
//! side's own live input counted as reusable (a replacement supersedes it).

use std::collections::HashMap;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Context;
use tracing::warn;

use crate::closer::executor::{encode_allowance, encode_balance_of};
use crate::config::{parse_liquidity_amount, LiquidityAmount};
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
/// commitments, and what this tick has already reserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FundedInputBudget {
    pub funded: U256,
    pub committed: U256,
    pub reserved: U256,
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
        reserved: U256::ZERO,
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
    funded_inputs: &mut HashMap<Address, FundedInputBudget>,
    pair: &str,
    label: &str,
) -> Option<u128> {
    if !funded_inputs.contains_key(&token) {
        match read_funded_budget(indexer, wallet, chain_id, maker, token, permit2).await {
            Ok(budget) => {
                funded_inputs.insert(token, budget);
            }
            // `{:#}` prints the context chain ("could not read committed
            // input: <cause>"), preserving which read failed.
            Err(e) if dry_run => {
                warn!(pair = %pair, label, error = %format!("{e:#}"), "could not read funded budget; using configured size for dry-run");
                funded_inputs.insert(
                    token,
                    FundedInputBudget {
                        funded: fallback_liquidity(configured),
                        ..FundedInputBudget::default()
                    },
                );
            }
            Err(e) => {
                warn!(pair = %pair, label, error = %format!("{e:#}"), "could not read funded budget; skipping side");
                return None;
            }
        }
    }

    let budget = funded_inputs.get(&token).copied().unwrap_or_default();
    let available = available_funded_input(&budget, reusable_input);
    let capped = match cap_input_liquidity(configured, available) {
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
                reserved = %budget.reserved,
                reusable = %reusable_input,
                "capping order liquidity to remaining funded input"
            );
        }
    }
    Some(capped)
}

pub fn available_funded_input(budget: &FundedInputBudget, reusable_input: U256) -> U256 {
    budget
        .funded
        .saturating_add(reusable_input)
        .saturating_sub(budget.committed)
        .saturating_sub(budget.reserved)
}

pub fn reserve_funded_input(
    funded_inputs: &mut HashMap<Address, FundedInputBudget>,
    token: Address,
    amount: U256,
) {
    if amount == U256::ZERO {
        return;
    }
    let budget = funded_inputs.entry(token).or_default();
    budget.reserved = budget.reserved.saturating_add(amount);
}

/// Only the increment over the side's reusable live input needs fresh budget.
pub fn replacement_reservation(drafted_input: U256, reusable_input: U256) -> U256 {
    drafted_input.saturating_sub(reusable_input)
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
    fn input_budget_subtracts_other_corridor_commitments_before_max() {
        let budget = FundedInputBudget {
            funded: U256::from(4_000u64),
            committed: U256::from(8_000u64),
            reserved: U256::ZERO,
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
            reserved: U256::ZERO,
        };

        assert_eq!(
            available_funded_input(&budget, U256::from(4_000u64)),
            U256::from(4_000u64)
        );
    }

    #[test]
    fn reserving_funded_input_decrements_a_shared_token_budget() {
        let token: Address = "0x00000000000000000000000000000000000000bb"
            .parse()
            .unwrap();
        let mut funded_inputs = HashMap::from([(
            token,
            FundedInputBudget {
                funded: U256::from(75u64),
                committed: U256::ZERO,
                reserved: U256::ZERO,
            },
        )]);

        reserve_funded_input(&mut funded_inputs, token, U256::from(50u64));
        assert_eq!(
            available_funded_input(&funded_inputs[&token], U256::ZERO),
            U256::from(25u64)
        );

        reserve_funded_input(&mut funded_inputs, token, U256::from(50u64));
        assert_eq!(
            available_funded_input(&funded_inputs[&token], U256::ZERO),
            U256::ZERO
        );
    }

    #[test]
    fn replacement_reservation_only_charges_the_incremental_delta() {
        assert_eq!(
            replacement_reservation(U256::from(100u64), U256::from(75u64)),
            U256::from(25u64)
        );
        assert_eq!(
            replacement_reservation(U256::from(75u64), U256::from(100u64)),
            U256::ZERO
        );
    }
}
