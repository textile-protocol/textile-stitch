// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Blue-leg orchestration: discover a pool's OPEN positions, evaluate which
//! clear the operator's margin, and close the front of the FIFO queue with one
//! `pool.fill()` — approving the debt asset first if the allowance is short.
//! The pure decisions live in [`super::strategy`]/[`super::feemath`]; this is
//! the I/O glue over [`crate::rpc::Wallet`].

use std::collections::HashMap;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use tracing::info;

use crate::rpc::Wallet;

use super::discover::Discoverer;
use super::executor::{encode_allowance, encode_approve, encode_fill};
use super::strategy::{evaluate, CloseDecision, ClosePosition, PoolParams, StrategyConfig};

/// One pool's blue-leg close target.
#[derive(Debug, Clone)]
pub struct CloserPool {
    pub pool_address: Address,
    /// Debt asset the closer pays in (e.g. USDT) — approved to the pool.
    pub debt_token: Address,
    pub params: PoolParams,
    /// Most positions to close in a single `fill()`.
    pub max_positions: usize,
    /// Candidate positions to pull from the subgraph per tick.
    pub discover_first: u32,
}

/// What one close tick decided or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutcome {
    /// No position cleared the margin bar.
    Nothing,
    /// Dry run: the batch that would have been closed.
    Planned { positions: usize, debt_in: U256 },
    /// Submitted a fill; carries the tx hash.
    Filled {
        hash: B256,
        positions: usize,
        debt_in: U256,
    },
}

/// Cooldown before a position we already submitted a `fill()` for is eligible
/// again. Covers the window where the tx is still pending or the subgraph hasn't
/// indexed the close yet — long enough to span typical subgraph lag, short
/// enough that a silently-failed fill is retried.
const RESUBMIT_COOLDOWN_SECS: u64 = 180;

/// Truncate the FIFO queue at the first position whose fill is still in flight.
///
/// `pool.fill()` always works `positions[nextFillId]` upward, so a position we
/// submitted a fill for within the cooldown blocks *everything behind it* — we
/// can't skip it and fill a later position, or the contract would spend the
/// budget on the still-pending one. Order by positionId and keep only the head
/// run of not-recently-submitted positions; entries older than the cooldown
/// fall through so a fill that never landed is retried.
fn truncate_at_pending(
    positions: Vec<ClosePosition>,
    recently: &HashMap<U256, u64>,
    now: u64,
    cooldown_secs: u64,
) -> Vec<ClosePosition> {
    let mut ordered = positions;
    ordered.sort_by_key(|p| p.position_id);
    ordered
        .into_iter()
        .take_while(|p| match recently.get(&p.position_id) {
            Some(&t) => now.saturating_sub(t) >= cooldown_secs, // expired → fillable
            None => true,
        })
        .collect()
}

/// Pure planning step: discovered positions → the batch to close.
///
/// `pool.fill()` always works `positions[nextFillId]` upward (FIFO by
/// positionId) and the bot only controls how many / the debt budget — never
/// *which* positions. So we can only close a **contiguous profitable prefix
/// from the head**: order by positionId, then stop at the first position that
/// doesn't clear. Skipping an unfillable head and planning for a later position
/// would have the contract spend the budget on the older, uneconomic one.
pub fn plan_batch(
    positions: &[ClosePosition],
    params: &PoolParams,
    strategy: &StrategyConfig,
    max_positions: usize,
) -> Vec<CloseDecision> {
    let mut ordered: Vec<&ClosePosition> = positions.iter().collect();
    ordered.sort_by_key(|p| p.position_id);
    ordered
        .into_iter()
        .map(|p| evaluate(p, params, strategy))
        .take_while(Option::is_some)
        .flatten()
        .take(max_positions)
        .collect()
}

/// Total debt the closer commits for `batch` — the `fill()` `maxDebtIn`.
pub fn total_debt_in(batch: &[CloseDecision]) -> U256 {
    batch.iter().fold(U256::ZERO, |acc, d| acc + d.debt_in)
}

/// Run one close tick for `pool`. With `dry_run`, plans but never sends.
///
/// `recently_submitted` (position id → unix submit time, owned by the caller per
/// pool) dedupes across ticks: positions we just filled are skipped until the
/// chain/subgraph catch up, so a pending tx or a lagging subgraph can't trigger
/// a duplicate `fill()`.
pub async fn close_pool_once(
    wallet: &Wallet,
    discoverer: &Discoverer,
    pool: &CloserPool,
    strategy: &StrategyConfig,
    dry_run: bool,
    recently_submitted: &mut HashMap<U256, u64>,
) -> anyhow::Result<CloseOutcome> {
    let now = strategy.now;
    // Forget entries past the cooldown so a position whose fill never landed
    // becomes eligible again.
    recently_submitted.retain(|_, t| now.saturating_sub(*t) < RESUBMIT_COOLDOWN_SECS);

    let discovered = discoverer
        .open_positions(&pool.pool_address.to_string(), pool.discover_first)
        .await?;
    let positions =
        truncate_at_pending(discovered, recently_submitted, now, RESUBMIT_COOLDOWN_SECS);
    let batch = plan_batch(&positions, &pool.params, strategy, pool.max_positions);
    if batch.is_empty() {
        return Ok(CloseOutcome::Nothing);
    }
    let debt_in = total_debt_in(&batch);
    let max_positions = U256::from(batch.len());

    if dry_run {
        return Ok(CloseOutcome::Planned {
            positions: batch.len(),
            debt_in,
        });
    }

    ensure_allowance(wallet, pool.debt_token, pool.pool_address, debt_in).await?;

    let hash = wallet
        .send(
            pool.pool_address,
            Bytes::from(encode_fill(debt_in, max_positions)),
            U256::ZERO,
        )
        .await?;
    // Record what we just submitted so the next tick doesn't re-fill these while
    // the tx is pending / the subgraph hasn't indexed the close.
    for d in &batch {
        recently_submitted.insert(d.position_id, now);
    }
    Ok(CloseOutcome::Filled {
        hash,
        positions: batch.len(),
        debt_in,
    })
}

/// Approve `spender` to pull `needed` of `token` if the current allowance is
/// short (one-time max approval — the standard closer pattern).
async fn ensure_allowance(
    wallet: &Wallet,
    token: Address,
    spender: Address,
    needed: U256,
) -> anyhow::Result<()> {
    let allowance = wallet
        .read_uint(
            token,
            &Bytes::from(encode_allowance(wallet.address(), spender)),
        )
        .await?;
    if allowance >= needed {
        return Ok(());
    }
    info!(token = %token, spender = %spender, "approving debt asset to pool");
    wallet
        .send_and_wait(
            token,
            Bytes::from(encode_approve(spender, U256::MAX)),
            U256::ZERO,
            Duration::from_secs(120),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_params() -> PoolParams {
        PoolParams {
            floor_ray: "2000000000000000000000000".parse().unwrap(),
            buffer_ray: "20000000000000000000000000".parse().unwrap(),
            window_secs: 432_000,
        }
    }

    fn strategy(now: u64) -> StrategyConfig {
        StrategyConfig {
            oracle_rate_ray: "1500000000000000000000000000".parse().unwrap(),
            min_margin_collateral: U256::ZERO,
            skip_past_window: true,
            now,
        }
    }

    fn position(id: u64, open_time: u64) -> ClosePosition {
        ClosePosition {
            position_id: U256::from(id),
            c: U256::from(1_550_000_000u64),
            d: U256::from(1_000_000_000u64),
            open_time,
        }
    }

    #[test]
    fn plans_an_empty_batch_when_nothing_clears() {
        // min_margin impossibly high → no decision survives.
        let mut s = strategy(2_000);
        s.min_margin_collateral = U256::from(u128::MAX);
        let batch = plan_batch(&[position(1, 1_000)], &pool_params(), &s, 10);
        assert!(batch.is_empty());
    }

    #[test]
    fn plans_and_sums_debt_oldest_first() {
        let positions = vec![position(2, 2_000), position(1, 1_000)];
        let batch = plan_batch(&positions, &pool_params(), &strategy(3_000), 10);
        assert_eq!(batch.len(), 2);
        // Oldest (open_time 1_000) leads.
        assert_eq!(batch[0].position_id, U256::from(1u64));
        // maxDebtIn is the sum of the per-position debt_in.
        let expected = batch[0].debt_in + batch[1].debt_in;
        assert_eq!(total_debt_in(&batch), expected);
    }

    #[test]
    fn a_pending_fifo_head_blocks_the_whole_queue() {
        // The contract fills positions[nextFillId] first, so a head whose fill is
        // still in flight blocks everything behind it — we must not skip it.
        let positions = vec![position(1, 1_000), position(2, 2_000), position(3, 3_000)];
        let mut recently = HashMap::new();
        recently.insert(U256::from(1u64), 1_000); // head submitted 100s ago — pending
        let kept = truncate_at_pending(positions, &recently, 1_100, 180);
        assert!(kept.is_empty(), "a pending head blocks the queue");
    }

    #[test]
    fn truncates_at_a_pending_middle_keeping_the_fillable_head() {
        let positions = vec![position(1, 1_000), position(2, 2_000), position(3, 3_000)];
        let mut recently = HashMap::new();
        recently.insert(U256::from(1u64), 100); // head submitted 1000s ago — expired, fillable
        recently.insert(U256::from(2u64), 1_000); // middle pending — stop here
        let kept = truncate_at_pending(positions, &recently, 1_100, 180);
        let ids: Vec<U256> = kept.iter().map(|p| p.position_id).collect();
        assert_eq!(ids, vec![U256::from(1u64)]);
    }

    #[test]
    fn caps_the_batch_at_max_positions() {
        let positions = vec![position(1, 1_000), position(2, 2_000), position(3, 3_000)];
        let batch = plan_batch(&positions, &pool_params(), &strategy(4_000), 2);
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn stops_at_the_first_unfillable_fifo_head() {
        // pool.fill() works positions[nextFillId] upward, so an unfillable head
        // blocks the batch even when a later position would clear on its own.
        let now = 1_000 + 500_000;
        // id=1 head opened long ago → past the window → not fillable (skipped).
        // id=2 recent → within window → profitable. Order in input is irrelevant.
        let batch = plan_batch(
            &[position(2, now - 1_000), position(1, 1_000)],
            &pool_params(),
            &strategy(now),
            10,
        );
        assert!(batch.is_empty(), "must not skip the unfillable FIFO head");
    }

    #[test]
    fn includes_only_the_contiguous_profitable_prefix() {
        // Head + tail profitable, middle past-window: only the head prefix plans,
        // since the contract can't reach the tail without first filling the middle.
        let now = 1_000 + 500_000;
        let batch = plan_batch(
            &[
                position(1, now - 1_000), // profitable head
                position(2, 1_000),       // past-window → blocks here
                position(3, now - 1_000), // profitable, but unreachable
            ],
            &pool_params(),
            &strategy(now),
            10,
        );
        assert_eq!(
            batch.iter().map(|d| d.position_id).collect::<Vec<_>>(),
            vec![U256::from(1u64)]
        );
    }
}
