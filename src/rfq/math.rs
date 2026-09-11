// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! RFQ amount math: fee fitting, atomic-unit conversions at a priced rate, and
//! the RAY level rate. All `U256`, mirroring [`crate::pricing::quote`]'s fixed-point
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

/// Whether an amount is small enough to quote. Nothing near `U256::MAX /
/// 10_000` can settle — the reactor's fee controller multiplies amounts by
/// bps — and the math below is total without this check, so it is policy:
/// an oversized frame gets a `Size` reject instead of a wasted conversion.
pub fn is_quotable(amount: U256) -> bool {
    amount.checked_mul(U256::from(BPS_DENOMINATOR)).is_some()
}

/// `floor(x × n / d)` for `n <= d`, without ever needing `x × n` in 256 bits.
/// `U256` operators wrap silently (ruint); the plain product is used while it
/// fits, and past that the split `q·n + floor(r·n/d)` (with `x = q·d + r`)
/// is exact because `q·n` is already an integer.
fn mul_div_floor_nowrap(x: U256, n: U256, d: U256) -> U256 {
    debug_assert!(n <= d, "split form only holds for n <= d");
    x.checked_mul(n)
        .map_or_else(|| (x / d) * n + (x % d) * n / d, |product| product / d)
}

/// The venue fee the controller injects on top of an output:
/// `floor(output × fee_bps / 10000)`.
pub fn fee_on(output: U256, fee_bps: u32) -> U256 {
    mul_div_floor_nowrap(output, U256::from(fee_bps), U256::from(BPS_DENOMINATOR))
}

/// `output` plus its injected fee, or `None` when the gross does not fit in
/// 256 bits.
pub fn gross_of(output: U256, fee_bps: u32) -> Option<U256> {
    output.checked_add(fee_on(output, fee_bps))
}

/// Smallest output whose floored fee is at least 1 atomic unit.
/// `ceil(10000 / fee_bps)`; a zero fee rate has no floor of its own.
pub fn min_feeable_output(fee_bps: u32) -> U256 {
    if fee_bps == 0 {
        return U256::from(1u8);
    }
    U256::from((BPS_DENOMINATOR + u64::from(fee_bps) - 1) / u64::from(fee_bps))
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
/// by exactly one unit, and leaving even one atomic unit on the table loses
/// price-priority ties. So: seed there, then try one unit more. Total for
/// every `cap` — the seed never forms `cap × 10000` (which wraps for caps at
/// or above `U256::MAX / 10000` and used to leave a walk-up spinning for
/// ~1e70 iterations on one core), and the fit check is overflow-checked.
pub fn max_fitting_output(cap: U256, fee_bps: u32) -> FittedOutput {
    if fee_bps == 0 {
        return FittedOutput {
            output: cap,
            fee: U256::ZERO,
        };
    }
    let bps = U256::from(BPS_DENOMINATOR);
    let seed = mul_div_floor_nowrap(cap, bps, bps + U256::from(fee_bps));
    let fitted = |output: U256| {
        let fee = fee_on(output, fee_bps);
        output
            .checked_add(fee)
            .is_some_and(|gross| gross <= cap)
            .then_some(FittedOutput { output, fee })
    };
    seed.checked_add(U256::from(1u8))
        .and_then(fitted)
        .or_else(|| fitted(seed))
        .expect("the seed always fits: seed × (10000 + fee_bps) <= cap × 10000")
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
    // Zero on overflow: the responder reads a zero leg as unquotable (Size),
    // which is the right answer for an amount the settlement cannot carry.
    collateral
        .checked_mul(scaled)
        .and_then(|v| v.checked_mul(ten_pow(debt_decimals)))
        .map_or(U256::ZERO, |v| {
            v / (U256::from(PRICE_SCALE) * ten_pow(collateral_decimals))
        })
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
    debt.checked_mul(ten_pow(collateral_decimals))
        .and_then(|v| v.checked_mul(U256::from(PRICE_SCALE)))
        .map_or(U256::ZERO, |v| v / (scaled * ten_pow(debt_decimals)))
}

/// The level rate the venue expects: human debt-per-collateral, RAY (1e27)
/// scaled — `RAY × price`. Decimal-normalized, same convention as
/// `quoteRateRay` on the venue. Atomic scaling (`× 10^debt / 10^coll`) is
/// wrong here: a 6/18 pair would publish a rate 10^12 away from the firm
/// quote and fail `level_slack`.
pub fn rate_ray(price: f64, _debt_decimals: u8, _collateral_decimals: u8) -> U256 {
    let Some(scaled) = price_scaled(price) else {
        return U256::ZERO;
    };
    ten_pow(27) * scaled / U256::from(PRICE_SCALE)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U512;

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

    fn max_quotable() -> U256 {
        U256::MAX / U256::from(BPS_DENOMINATOR)
    }

    /// Widening reference: `floor(x × f / 10000)` in 512 bits.
    fn wide_fee(x: U256, fee_bps: u32) -> U256 {
        let wide = U512::from(x) * U512::from(fee_bps) / U512::from(BPS_DENOMINATOR);
        U256::from(wide)
    }

    #[test]
    fn fee_on_never_wraps() {
        // Above U256::MAX / fee_bps the naive product wraps; the split form
        // has to agree with the 512-bit reference all the way to the top.
        for x in [
            U256::MAX,
            U256::MAX - U256::from(1u8),
            U256::MAX / U256::from(3u8),
            U256::from(1u8) << 255,
            (U256::from(1u8) << 200) + U256::from(9_999u64),
        ] {
            for fee_bps in [1u32, 5, 30, 10_000] {
                assert_eq!(
                    fee_on(x, fee_bps),
                    wide_fee(x, fee_bps),
                    "x={x} fee={fee_bps}"
                );
            }
        }
    }

    #[test]
    fn fee_fit_terminates_and_is_maximal_at_the_top_of_u256() {
        // Report S-01: `cap × 10000` wrapped for caps at or above
        // U256::MAX / 10000, the seed came out ~10000× too small, and the
        // walk-up never returned. These caps must answer immediately and
        // correctly: the gross fits (checked, so no wrapped "fit"), and one
        // more unit either overflows or breaks the cap.
        for cap in [
            U256::MAX,
            U256::MAX - U256::from(1u8),
            max_quotable(),
            max_quotable() + U256::from(1u8),
            U256::from(1u8) << 255,
        ] {
            for fee_bps in [1u32, 5, 30] {
                let fit = max_fitting_output(cap, fee_bps);
                let gross = fit.output.checked_add(fit.fee).expect("gross fits");
                assert!(gross <= cap, "cap={cap} fee={fee_bps}");
                assert_eq!(fit.fee, wide_fee(fit.output, fee_bps));
                let next = fit.output + U256::from(1u8);
                let next_gross = next.checked_add(wide_fee(next, fee_bps));
                assert!(
                    next_gross.map_or(true, |g| g > cap),
                    "not maximal at cap={cap} fee={fee_bps}"
                );
            }
        }
    }

    #[test]
    fn conversions_return_zero_instead_of_wrapping() {
        // A wrapped product would come out small and look like a fillable
        // leg; zero is what the responder rejects as Size.
        assert_eq!(debt_for_collateral(1.0, U256::MAX, 18, 6), U256::ZERO);
        assert_eq!(collateral_for_debt(1.0, U256::MAX, 6, 18), U256::ZERO);
        assert_eq!(
            debt_for_collateral(1.0, max_quotable() + U256::from(1u8), 18, 18),
            U256::ZERO
        );
        // Sane sizes are untouched by the checked path.
        assert_eq!(
            debt_for_collateral(1.0, U256::from(1_000_000u64), 6, 6),
            U256::from(1_000_000u64)
        );
    }

    #[test]
    fn is_quotable_flips_exactly_where_the_bps_product_would_wrap() {
        assert!(is_quotable(max_quotable()));
        assert!(!is_quotable(max_quotable() + U256::from(1u8)));
        assert!(!is_quotable(U256::MAX));
    }

    #[test]
    fn min_feeable_output_at_1_bps_is_10000() {
        assert_eq!(min_feeable_output(1), U256::from(10_000u64));
        assert_eq!(min_feeable_output(5), U256::from(2_000u64));
        assert_eq!(fee_on(U256::from(9_999u64), 1), U256::ZERO);
        assert_eq!(fee_on(U256::from(10_000u64), 1), U256::from(1u8));
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
    fn rate_ray_is_decimal_normalized_human_price() {
        let ray = U256::from(10u64).pow(U256::from(27u8));
        // Price 1.0 is RAY regardless of decimals — the venue compares this
        // to quoteRateRay, which divides out the decimal gap.
        assert_eq!(rate_ray(1.0, 6, 6), ray);
        assert_eq!(rate_ray(1.0, 18, 6), ray);
        assert_eq!(rate_ray(1.0, 6, 18), ray);
        assert_eq!(rate_ray(2.0, 6, 6), ray * U256::from(2u8));
        // cNGN/USDT mid (~0.000728): 728000 / 1e9 * RAY = 728e21.
        assert_eq!(
            rate_ray(0.000728, 18, 6),
            U256::from(728u64) * U256::from(10u64).pow(U256::from(21u8))
        );
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
