// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use alloy_primitives::U256;
use stitch_bot::closer::runner::plan_batch;
use stitch_bot::closer::strategy::{evaluate, ClosePosition, PoolParams, StrategyConfig};

fn u(s: &str) -> U256 {
    s.parse().unwrap()
}

fn pool_params() -> PoolParams {
    PoolParams {
        floor_ray: u("2000000000000000000000000"),
        buffer_ray: u("20000000000000000000000000"),
        window_secs: 432_000,
    }
}

fn strategy(now: u64) -> StrategyConfig {
    StrategyConfig {
        oracle_rate_ray: u("1500000000000000000000000000"),
        min_margin_collateral: U256::ZERO,
        skip_past_window: true,
        now,
    }
}

#[test]
fn audit_planner_does_not_select_a_later_position_when_the_fifo_front_is_rejected() {
    let pool = pool_params();
    let cfg = strategy(1_000);

    let fifo_front = ClosePosition {
        position_id: U256::from(1u64),
        c: U256::from(1_000_000_000u64),
        d: U256::from(1_000_000_000u64),
        open_time: 1_000,
    };
    let later_profitable = ClosePosition {
        position_id: U256::from(2u64),
        c: U256::from(1_550_000_000u64),
        d: U256::from(1_000_000_000u64),
        open_time: 1_000,
    };

    assert!(
        evaluate(&fifo_front, &pool, &cfg).is_none(),
        "the strategy rejects the actual FIFO front"
    );
    assert!(
        evaluate(&later_profitable, &pool, &cfg).is_some(),
        "a later position clears the margin bar"
    );

    let batch = plan_batch(&[fifo_front.clone(), later_profitable], &pool, &cfg, 1);
    assert!(
        batch.is_empty(),
        "the planner must not skip an unfillable FIFO front to select a later position"
    );
}
