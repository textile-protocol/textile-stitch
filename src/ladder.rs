// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Balanced order ladders for approximating partial fills with whole orders.
//!
//! The ladder uses money-like denominations (`1, 2.5, 5` times powers of ten)
//! starting at the configured minimum slice. It allocates more inventory to the
//! lower/middle denominations and uses larger denominations to carry scale.

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

    build_balanced(total, min_slice, max_orders, &denoms)
}

fn build_balanced(total: u128, min_slice: u128, max_orders: usize, denoms: &[u128]) -> Vec<u128> {
    let mut remaining = total;
    let mut ladder = Vec::new();

    // Reserve enough slots for carry depth, then add one precision rung at each
    // small denomination. Avoid repeating the minimum slice until we know the
    // larger orders can carry the target liquidity.
    let precision_limit = ((max_orders * 3) / 5).max(1);
    let precision_ceiling = (total / 10).max(min_slice);
    for &denom in denoms.iter().filter(|&&d| d <= precision_ceiling) {
        if ladder.len() >= precision_limit || ladder.len() + 1 >= max_orders {
            break;
        }
        if denom <= remaining {
            ladder.push(denom);
            remaining -= denom;
        }
    }

    while remaining >= min_slice && ladder.len() < max_orders {
        let slots_left = max_orders - ladder.len();
        if slots_left == 1 {
            ladder.push(remaining);
            remaining = 0;
            break;
        }

        let next = carry_slice(remaining, min_slice, slots_left, denoms);
        ladder.push(next);
        remaining -= next;
    }

    if remaining > 0 {
        if let Some(last) = ladder.last_mut() {
            *last += remaining;
        }
    }

    ladder.sort_unstable();
    ladder
}

fn carry_slice(remaining: u128, min_slice: u128, slots_left: usize, denoms: &[u128]) -> u128 {
    let reserved_tail = min_slice.saturating_mul((slots_left - 1) as u128);
    let max_now = remaining.saturating_sub(reserved_tail);
    if max_now <= min_slice {
        return min_slice.min(remaining);
    }

    let average = remaining / slots_left as u128;
    let target = average.saturating_mul(2).max(min_slice);
    let cap = max_now.min(target);

    denoms
        .iter()
        .rev()
        .copied()
        .find(|&denom| denom <= cap)
        .unwrap_or_else(|| min_slice.min(max_now))
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
        assert_eq!(total, 50_000);
    }

    #[test]
    fn low_order_cap_still_carries_meaningful_depth() {
        let ladder = balanced_ladder(50_000, 10, 15);
        let total: u128 = ladder.iter().sum();

        assert_eq!(
            ladder,
            vec![
                10, 25, 50, 100, 250, 500, 565, 1_000, 2_500, 5_000, 5_000, 5_000, 10_000, 10_000,
                10_000
            ]
        );
        assert_eq!(total, 50_000);
    }

    #[test]
    fn never_creates_a_slice_below_the_minimum() {
        let ladder = balanced_ladder(95, 10, 20);
        let total: u128 = ladder.iter().sum();

        assert_eq!(total, 95);
        assert!(ladder.iter().all(|v| *v >= 10));
    }
}
