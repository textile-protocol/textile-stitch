// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! RFQ amount math: fee fitting, atomic-unit conversions at a priced rate, and
//! the RAY level rate. All `U256`, mirroring [`crate::quote`]'s fixed-point
//! approach (price scaled to 1e9) so RFQ and ladder pricing can never drift on
//! rounding conventions.

use alloy_primitives::U256;

/// Same fixed-point scale as `quote::PRICE_SCALE` — one convention crate-wide.
const PRICE_SCALE: u128 = 1_000_000_000; // 1e9
const BPS_DENOMINATOR: u64 = 10_000;

fn ten_pow(n: u8) -> U256 {
    (0..n).fold(U256::from(1u8), |v, _| v * U256::from(10u8))
}

fn price_scaled(price: f64) -> Option<U256> {
    let scaled = (price * PRICE_SCALE as f64).round();
    (scaled.is_finite() && scaled > 0.0).then(|| U256::from(scaled as u128))
}

/// The venue fee the controller injects on top of an output:
/// `floor(output × fee_bps / 10000)`.
pub fn fee_on(output: U256, fee_bps: u32) -> U256 {
    output * U256::from(fee_bps) / U256::from(BPS_DENOMINATOR)
}

/// An output that fits a gross cap together with its injected fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FittedOutput {
    /// What the maker's signed order receives.
    pub output: U256,
    /// `fee_on(output, fee_bps)` — the venue's projection, never in the order.
    pub fee: U256,
}

/// Largest `output` with `output + floor(output × fee_bps / 10000) <= cap`.
///
/// Exact-input requests cap the taker's gross spend at their sellAmount; the
/// fee floors, so the naive `cap × 10000 / (10000 + fee_bps)` can undershoot
/// by a unit or two. Start there and walk up — the floor bounds the walk to a
/// couple of steps, and leaving even one atomic unit on the table loses
/// price-priority ties.
pub fn max_fitting_output(cap: U256, fee_bps: u32) -> FittedOutput {
    if fee_bps == 0 {
        return FittedOutput {
            output: cap,
            fee: U256::ZERO,
        };
    }
    let denominator = U256::from(BPS_DENOMINATOR + u64::from(fee_bps));
    let mut output = cap * U256::from(BPS_DENOMINATOR) / denominator;
    let fits = |o: U256| o + fee_on(o, fee_bps) <= cap;
    while fits(output + U256::from(1u8)) {
        output += U256::from(1u8);
    }
    FittedOutput {
        output,
        fee: fee_on(output, fee_bps),
    }
}

/// Debt atomic for `collateral` atomic at `price` (debt per collateral,
/// human): `collateral × price × 10^debt / 10^coll`, floored.
pub fn debt_for_collateral(
    price: f64,
    collateral: U256,
    debt_decimals: u8,
    collateral_decimals: u8,
) -> U256 {
    let Some(scaled) = price_scaled(price) else {
        return U256::ZERO;
    };
    collateral * scaled * ten_pow(debt_decimals)
        / (U256::from(PRICE_SCALE) * ten_pow(collateral_decimals))
}

/// Collateral atomic for `debt` atomic at `price` (debt per collateral,
/// human): `debt × 10^coll / (price × 10^debt)`, floored.
pub fn collateral_for_debt(
    price: f64,
    debt: U256,
    debt_decimals: u8,
    collateral_decimals: u8,
) -> U256 {
    let Some(scaled) = price_scaled(price) else {
        return U256::ZERO;
    };
    debt * ten_pow(collateral_decimals) * U256::from(PRICE_SCALE)
        / (scaled * ten_pow(debt_decimals))
}

/// The level rate the venue expects: debt-atomic per collateral-atomic,
/// RAY (1e27) scaled — `RAY × price × 10^debt / 10^coll`.
pub fn rate_ray(price: f64, debt_decimals: u8, collateral_decimals: u8) -> U256 {
    let Some(scaled) = price_scaled(price) else {
        return U256::ZERO;
    };
    ten_pow(27) * scaled * ten_pow(debt_decimals)
        / (U256::from(PRICE_SCALE) * ten_pow(collateral_decimals))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_fit_matches_the_venue_golden_case_at_1_bps() {
        // cap 1000000000 at 1 bps → output 999900010, fee 99990 (the spec's
        // worked example; the naive division alone lands one unit short).
        let fit = max_fitting_output(U256::from(1_000_000_000u64), 1);
        assert_eq!(fit.output, U256::from(999_900_010u64));
        assert_eq!(fit.fee, U256::from(99_990u64));
        assert!(fit.output + fit.fee <= U256::from(1_000_000_000u64));
    }

    #[test]
    fn fee_fit_at_5_bps_is_maximal_and_within_cap() {
        let cap = U256::from(1_000_000_000u64);
        let fit = max_fitting_output(cap, 5);
        // Naive floor(1e9 × 10000 / 10005) = 999500249, but the fee floor
        // leaves room for one more unit: 999500250 + 499750 == cap exactly.
        assert_eq!(fit.output, U256::from(999_500_250u64));
        assert_eq!(fit.fee, U256::from(499_750u64));
        assert!(fit.output + fit.fee <= cap);
        // Maximality: one more unit would blow the cap.
        let next = fit.output + U256::from(1u8);
        assert!(next + fee_on(next, 5) > cap);
    }

    #[test]
    fn zero_fee_passes_the_cap_through() {
        let cap = U256::from(123_456_789u64);
        let fit = max_fitting_output(cap, 0);
        assert_eq!(fit.output, cap);
        assert_eq!(fit.fee, U256::ZERO);
    }

    #[test]
    fn fee_fit_is_maximal_across_a_sweep() {
        // Property check on small caps where floor effects bite hardest.
        for cap in (0u64..2_000).step_by(7) {
            for fee_bps in [1u32, 5, 30, 100] {
                let cap = U256::from(cap);
                let fit = max_fitting_output(cap, fee_bps);
                assert!(fit.output + fit.fee <= cap);
                let next = fit.output + U256::from(1u8);
                assert!(
                    next + fee_on(next, fee_bps) > cap,
                    "not maximal at cap={cap} fee={fee_bps}"
                );
            }
        }
    }

    #[test]
    fn conversions_agree_with_the_ladder_math() {
        // quote::sell_amounts_at(1.02, 1000e6, 6, 6) yields 1020e6 debt out;
        // the RFQ conversion must match the ladder's integer convention.
        assert_eq!(
            debt_for_collateral(1.02, U256::from(1_000_000_000u64), 6, 6),
            U256::from(1_020_000_000u64)
        );
        // quote::buy_amounts_at(0.98, 1000e6, 6, 6) → 1020408163 collateral.
        assert_eq!(
            collateral_for_debt(0.98, U256::from(1_000_000_000u64), 6, 6),
            U256::from(1_020_408_163u64)
        );
        // Decimal normalization: 1 cNGN (18dp) at price 1.0 → 1 USDT (6dp).
        assert_eq!(
            debt_for_collateral(1.0, U256::from(10u64).pow(U256::from(18u8)), 6, 18),
            U256::from(1_000_000u64)
        );
        assert_eq!(
            collateral_for_debt(1.0, U256::from(1_000_000u64), 6, 18),
            U256::from(10u64).pow(U256::from(18u8))
        );
    }

    #[test]
    fn rate_ray_is_the_atomic_debt_per_collateral_in_ray() {
        let ray = U256::from(10u64).pow(U256::from(27u8));
        // Equal decimals at price 1.0 → exactly RAY.
        assert_eq!(rate_ray(1.0, 6, 6), ray);
        // 18dp collateral, 6dp debt at 1.0 → RAY / 1e12 (one atomic collateral
        // unit is worth far less than one atomic debt unit).
        assert_eq!(
            rate_ray(1.0, 6, 18),
            ray / U256::from(10u64).pow(U256::from(12u8))
        );
        // Price scales linearly.
        assert_eq!(rate_ray(2.0, 6, 6), ray * U256::from(2u8));
    }

    #[test]
    fn garbage_prices_collapse_to_zero_not_a_panic() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                debt_for_collateral(bad, U256::from(1u8), 6, 6),
                U256::ZERO,
                "{bad}"
            );
            assert_eq!(collateral_for_debt(bad, U256::from(1u8), 6, 6), U256::ZERO);
            assert_eq!(rate_ray(bad, 6, 6), U256::ZERO);
        }
    }
}
