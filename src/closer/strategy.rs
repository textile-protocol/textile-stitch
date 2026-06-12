// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Close strategy: for each candidate position compute the filler's net margin
//! (collateral received minus debt paid, valued via the oracle) and decide
//! whether it clears the target. `pool.fill()` is FIFO and the caller can only
//! close a contiguous prefix from the head, so the batching (in
//! [`super::runner::plan_batch`]) stops at the first position that doesn't clear
//! rather than cherry-picking later profitable ones.
//!
//! Mirrors the strategy behavior used by the TypeScript app layer.

use alloy_primitives::U256;

use super::feemath::{close_outcome, ray};

#[derive(Debug, Clone)]
pub struct ClosePosition {
    pub position_id: U256,
    pub c: U256,
    pub d: U256,
    pub open_time: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct PoolParams {
    pub floor_ray: U256,
    pub buffer_ray: U256,
    pub window_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct StrategyConfig {
    /// Oracle rate: collateral per debt, RAY-scaled.
    pub oracle_rate_ray: U256,
    /// Minimum acceptable net margin, collateral atomic units.
    pub min_margin_collateral: U256,
    pub skip_past_window: bool,
    pub now: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseDecision {
    pub position_id: U256,
    pub fee_ray: U256,
    pub debt_in: U256,
    pub closer_take_net: U256,
    pub margin_collateral: U256,
    pub past_window: bool,
    pub open_time: u64,
}

pub fn evaluate(
    pos: &ClosePosition,
    pool: &PoolParams,
    cfg: &StrategyConfig,
) -> Option<CloseDecision> {
    if cfg.now < pos.open_time {
        return None;
    }
    let elapsed = U256::from(cfg.now - pos.open_time);
    let out = close_outcome(
        pos.c,
        pos.d,
        elapsed,
        pool.floor_ray,
        pool.buffer_ray,
        U256::from(pool.window_secs),
    );
    if out.past_window && cfg.skip_past_window {
        return None;
    }
    if out.debt_in == U256::ZERO {
        return None;
    }
    // Value the debt outlay in collateral so margin is on one axis.
    let debt_as_collateral = out.debt_in * cfg.oracle_rate_ray / ray();
    if out.closer_take_net <= debt_as_collateral {
        return None;
    }
    let margin = out.closer_take_net - debt_as_collateral;
    if margin <= cfg.min_margin_collateral {
        return None;
    }
    Some(CloseDecision {
        position_id: pos.position_id,
        fee_ray: out.fee_ray,
        debt_in: out.debt_in,
        closer_take_net: out.closer_take_net,
        margin_collateral: margin,
        past_window: out.past_window,
        open_time: pos.open_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> U256 {
        s.parse().unwrap()
    }

    fn pool() -> PoolParams {
        PoolParams {
            floor_ray: u("2000000000000000000000000"),
            buffer_ray: u("20000000000000000000000000"),
            window_secs: 432_000,
        }
    }

    fn position() -> ClosePosition {
        ClosePosition {
            position_id: U256::from(1u64),
            c: U256::from(1_550_000_000u64),
            d: U256::from(1_000_000_000u64),
            open_time: 1_000,
        }
    }

    fn cfg(min_margin: &str, now: u64, skip_past_window: bool) -> StrategyConfig {
        StrategyConfig {
            oracle_rate_ray: u("1500000000000000000000000000"), // 1.5 collateral/debt
            min_margin_collateral: u(min_margin),
            skip_past_window,
            now,
        }
    }

    #[test]
    fn accepts_a_profitable_close_with_exact_margin() {
        // At t=0: closer_take_net 1,549,848,039 − debt_in 1,019,000,000 × 1.5.
        let d = evaluate(&position(), &pool(), &cfg("0", 1_000, true)).unwrap();
        assert_eq!(d.margin_collateral, U256::from(21_348_039u64));
        assert!(!d.past_window);
    }

    #[test]
    fn rejects_below_the_minimum_margin() {
        assert!(evaluate(&position(), &pool(), &cfg("30000000", 1_000, true)).is_none());
    }

    #[test]
    fn skips_past_window_when_configured() {
        let now = 1_000 + 500_000; // past the 432,000s window
        assert!(evaluate(&position(), &pool(), &cfg("0", now, true)).is_none());
        // ...but allows it when skip_past_window = false.
        assert!(evaluate(&position(), &pool(), &cfg("0", now, false)).is_some());
    }

    #[test]
    fn ignores_a_position_from_the_future() {
        assert!(evaluate(&position(), &pool(), &cfg("0", 500, true)).is_none());
    }
}
