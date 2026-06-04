// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Balanced order ladders for approximating partial fills with whole orders.
//!
//! The ladder uses money-like denominations (`1, 2.5, 5` times powers of ten)
//! starting at the configured minimum slice. It allocates more inventory to the
//! lower/middle denominations and uses larger denominations to carry scale.

const MAX_PER_DENOMINATION: usize = 20;
const INITIAL_BALANCE_DIVISOR: u128 = 5;

/// Generate a deterministic, ascending list of whole-order sizes.
///
/// `total` and `min_slice` are atomic units in the same token. The returned
/// sizes sum to at most `total`; any dust below `min_slice` is intentionally
/// left unquoted.
pub fn balanced_ladder(total: u128, min_slice: u128, max_orders: usize) -> Vec<u128> {
    if total < min_slice || min_slice == 0 || max_orders == 0 {
        return Vec::new();
    }

    let denoms = denominations(total, min_slice);
    if denoms.is_empty() {
        return Vec::new();
    }

    build_with_divisor(total, max_orders, &denoms, INITIAL_BALANCE_DIVISOR)
}

fn build_with_divisor(total: u128, max_orders: usize, denoms: &[u128], divisor: u128) -> Vec<u128> {
    let mut remaining = total;
    let mut ladder = Vec::new();
    let precision_limit = max_orders.saturating_sub((max_orders / 4).max(1));

    for &denom in denoms {
        if ladder.len() >= precision_limit || denom > remaining {
            continue;
        }
        let target = (total / denom / divisor) as usize;
        let count = target.clamp(1, MAX_PER_DENOMINATION);
        for _ in 0..count {
            if ladder.len() >= precision_limit || denom > remaining {
                break;
            }
            ladder.push(denom);
            remaining -= denom;
        }
    }

    for &denom in denoms.iter().rev() {
        while ladder.len() < max_orders && denom <= remaining {
            ladder.push(denom);
            remaining -= denom;
        }
    }

    ladder.sort_unstable();
    ladder
}

fn denominations(total: u128, min_slice: u128) -> Vec<u128> {
    let mut out = Vec::new();
    let mut scale = min_slice;

    while scale <= total {
        push_denom(&mut out, scale, total);
        push_denom(&mut out, scale.saturating_mul(25) / 10, total);
        push_denom(&mut out, scale.saturating_mul(5), total);

        match scale.checked_mul(10) {
            Some(next) => scale = next,
            None => break,
        }
    }

    out.sort_unstable();
    out.dedup();
    out
}

fn push_denom(out: &mut Vec<u128>, denom: u128, total: u128) {
    if denom > 0 && denom <= total {
        out.push(denom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_balanced_50k_ladder_from_10_usdc_units() {
        let ladder = balanced_ladder(50_000, 10, 150);
        let total: u128 = ladder.iter().sum();

        assert_eq!(total, 50_000);
        assert!(ladder.len() <= 150);
        assert_eq!(ladder[0], 10);
        assert!(ladder.contains(&1_000));
        assert!(ladder.contains(&5_000));
    }

    #[test]
    fn respects_the_max_order_count() {
        let ladder = balanced_ladder(50_000, 10, 40);
        let total: u128 = ladder.iter().sum();

        assert!(ladder.len() <= 40);
        assert!(total <= 50_000);
        assert!(total >= 45_000);
    }

    #[test]
    fn never_creates_a_slice_below_the_minimum() {
        let ladder = balanced_ladder(95, 10, 20);
        let total: u128 = ladder.iter().sum();

        assert_eq!(total, 95);
        assert!(ladder.iter().all(|v| *v >= 10));
    }
}
