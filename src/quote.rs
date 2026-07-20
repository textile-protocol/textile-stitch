// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Pricing. The operator runs a two-sided book around the feed mid: a bid (buy
//! collateral below mid — "buy low") and an ask (sell collateral above mid —
//! "sell high"). The spread on each side is the operator's own strategy,
//! expressed however they prefer — relative basis points, or an absolute amount
//! in the soft-per-stable price (e.g. cNGN/USDT, COPM/USDT). See [`Spread`].
//!
//! Atomic math is integer/U256; only the price (a small-magnitude float) is
//! scaled to an integer, so amounts stay exact for any token decimals.

use alloy_primitives::U256;

/// Fixed-point scale applied to the feed price before integer math.
const PRICE_SCALE: u128 = 1_000_000_000; // 1e9

fn ten_pow(n: u8) -> U256 {
    let mut v = U256::from(1u8);
    for _ in 0..n {
        v *= U256::from(10u8);
    }
    v
}

/// How an operator expresses the spread on one side of the book. Each side
/// (bid/ask) carries its own, so strategies can be asymmetric.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Spread {
    /// Relative, in basis points off the mid. Orientation-independent: `Bps(50)`
    /// is 0.5% either way.
    Bps(u32),
    /// Absolute, in the soft-per-stable price — collateral per debt (e.g. cNGN
    /// per USDT, COPM per USDT), the readable "local currency per dollar".
    /// Applied so the bid always buys cheaper and the ask always sells dearer,
    /// regardless of how the feed is quoted. Currency-agnostic.
    Abs(f64),
}

/// Bid price (USDT per cNGN) for buying collateral *below* the mid — buy low.
pub fn bid_price(mid: f64, spread: Spread) -> f64 {
    match spread {
        Spread::Bps(b) => mid * (1.0 - f64::from(b.min(10_000)) / 10_000.0),
        // delta is collateral per debt; cheaper collateral ⇒ more per debt ⇒ +delta.
        Spread::Abs(d) => {
            let coll_per_debt = 1.0 / mid + d;
            if coll_per_debt > 0.0 {
                1.0 / coll_per_debt
            } else {
                0.0
            }
        }
    }
}

/// Ask price (USDT per cNGN) for selling collateral *above* the mid — sell high.
pub fn ask_price(mid: f64, spread: Spread) -> f64 {
    match spread {
        Spread::Bps(b) => mid * (1.0 + f64::from(b) / 10_000.0),
        Spread::Abs(d) => {
            let coll_per_debt = 1.0 / mid - d;
            if coll_per_debt > 0.0 {
                1.0 / coll_per_debt
            } else {
                0.0
            }
        }
    }
}

/// `(input_debt_atomic, output_collateral_atomic)` for a BUY order at `bid`
/// (USDT per cNGN): the operator pays `size_debt_atomic` of debt (USDT) and
/// buys `size / bid` of collateral (cNGN).
pub fn buy_amounts_at(
    bid: f64,
    size_debt_atomic: u128,
    debt_decimals: u8,
    collateral_decimals: u8,
) -> (U256, U256) {
    let bid_scaled = (bid * PRICE_SCALE as f64).round() as u128;
    let input = U256::from(size_debt_atomic);
    if bid_scaled == 0 {
        return (input, U256::ZERO);
    }
    // output = input × 10^coll × PRICE_SCALE / (bid_scaled × 10^debt)
    let numerator = input * ten_pow(collateral_decimals) * U256::from(PRICE_SCALE);
    let denominator = U256::from(bid_scaled) * ten_pow(debt_decimals);
    (input, numerator / denominator)
}

/// Smallest collateral input whose ask output is at least `target_debt_atomic`.
///
/// Ask ladders are configured in debt units but funded in collateral. This
/// conversion rounds collateral up so the final integer-priced ask cannot land
/// one atomic debt unit below the configured ladder floor.
pub fn collateral_for_debt_ceil_at(
    ask: f64,
    target_debt_atomic: u128,
    debt_decimals: u8,
    collateral_decimals: u8,
) -> U256 {
    let ask_scaled = (ask * PRICE_SCALE as f64).round() as u128;
    if ask_scaled == 0 {
        return U256::ZERO;
    }
    // input = ceil(target × PRICE_SCALE × 10^coll / (ask × 10^debt))
    let numerator =
        U256::from(target_debt_atomic) * U256::from(PRICE_SCALE) * ten_pow(collateral_decimals);
    let denominator = U256::from(ask_scaled) * ten_pow(debt_decimals);
    let quotient = numerator / denominator;
    quotient + U256::from((numerator % denominator != U256::ZERO) as u8)
}

/// `(input_collateral_atomic, output_debt_atomic)` for an ASK order at `ask`
/// (USDT per cNGN): the operator sells `size_collateral_atomic` of collateral
/// (cNGN) for `size × ask` of debt (USDT). The mirror of [`buy_amounts_at`].
pub fn sell_amounts_at(
    ask: f64,
    size_collateral_atomic: u128,
    debt_decimals: u8,
    collateral_decimals: u8,
) -> (U256, U256) {
    let ask_scaled = (ask * PRICE_SCALE as f64).round() as u128;
    let input = U256::from(size_collateral_atomic);
    // output = input × ask × 10^debt / (PRICE_SCALE × 10^coll)
    let numerator = input * U256::from(ask_scaled) * ten_pow(debt_decimals);
    let denominator = U256::from(PRICE_SCALE) * ten_pow(collateral_decimals);
    (input, numerator / denominator)
}

/// Oracle rate the blue-leg strategy values debt in: collateral-atomic per
/// debt-atomic, RAY-scaled. Derived from the same feed (`feed_price` is debt
/// per collateral, human), inverted and decimal-adjusted:
/// `RAY × 10^coll / (feed_price × 10^debt)`.
pub fn oracle_rate_ray(feed_price: f64, debt_decimals: u8, collateral_decimals: u8) -> U256 {
    let price_scaled = (feed_price * PRICE_SCALE as f64).round() as u128;
    if price_scaled == 0 {
        return U256::ZERO;
    }
    let ray = ten_pow(27);
    let numerator = ray * ten_pow(collateral_decimals) * U256::from(PRICE_SCALE);
    let denominator = U256::from(price_scaled) * ten_pow(debt_decimals);
    numerator / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_one_to_one_is_exact() {
        // bid 1.0, equal 6dp decimals: 1000 USDT buys 1000 cNGN.
        let (input, output) = buy_amounts_at(1.0, 1_000_000_000, 6, 6);
        assert_eq!(input, U256::from(1_000_000_000u64));
        assert_eq!(output, U256::from(1_000_000_000u64));
    }

    #[test]
    fn buy_below_mid_gets_more_collateral() {
        // bid 0.98 → 1000 USDT buys 1000/0.98 = 1020.408… cNGN.
        let (input, output) = buy_amounts_at(0.98, 1_000_000_000, 6, 6);
        assert_eq!(input, U256::from(1_000_000_000u64));
        assert_eq!(output, U256::from(1_020_408_163u64));
        assert!(output > input);
    }

    #[test]
    fn buy_normalizes_across_decimals() {
        // cNGN 18dp, USDT 6dp, bid 1.0: 1 USDT (1e6) buys 1 cNGN (1e18).
        let (input, output) = buy_amounts_at(1.0, 1_000_000, 6, 18);
        assert_eq!(input, U256::from(1_000_000u64));
        assert_eq!(output, U256::from(1_000_000_000_000_000_000u128));
    }

    #[test]
    fn sell_one_to_one_is_exact() {
        let (input, output) = sell_amounts_at(1.0, 1_000_000_000, 6, 6);
        assert_eq!(input, U256::from(1_000_000_000u64));
        assert_eq!(output, U256::from(1_000_000_000u64));
    }

    #[test]
    fn sell_above_mid_demands_more_debt() {
        // ask 1.02 → 1000 cNGN sells for 1020 USDT.
        let (_input, output) = sell_amounts_at(1.02, 1_000_000_000, 6, 6);
        assert_eq!(output, U256::from(1_020_000_000u64));
    }

    #[test]
    fn sell_normalizes_across_decimals() {
        // cNGN 18dp, USDT 6dp, ask 1.0: sell 1 cNGN (1e18) → 1 USDT (1e6).
        let (input, output) = sell_amounts_at(1.0, 1_000_000_000_000_000_000, 6, 18);
        assert_eq!(input, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(output, U256::from(1_000_000u64));
    }

    #[test]
    fn ask_collateral_rounds_up_to_preserve_debt_floor() {
        let target = 500_000_000u128;
        let (_, rounded_down) = buy_amounts_at(3_000.123_456, target, 6, 18);
        let rounded_down = rounded_down.to_string().parse::<u128>().unwrap();
        let (_, old_output) = sell_amounts_at(3_000.123_456, rounded_down, 6, 18);
        let collateral = collateral_for_debt_ceil_at(3_000.123_456, target, 6, 18);
        let collateral = collateral.to_string().parse::<u128>().unwrap();
        let (_, output) = sell_amounts_at(3_000.123_456, collateral, 6, 18);

        assert_eq!(old_output, U256::from(target - 1));
        assert!(output >= U256::from(target));
    }

    #[test]
    fn bps_spread_is_symmetric_off_mid() {
        let bid = bid_price(1.0, Spread::Bps(200));
        let ask = ask_price(1.0, Spread::Bps(200));
        assert!((bid - 0.98).abs() < 1e-12);
        assert!((ask - 1.02).abs() < 1e-12);
    }

    #[test]
    fn abs_spread_buys_low_and_sells_high() {
        // mid = 0.000723… USDT per cNGN (≈ 1382 soft per stable). A 2-unit
        // spread tightens to 1384/1380 → bid < mid < ask in USDT terms.
        let mid = 1.0 / 1382.0;
        let bid = bid_price(mid, Spread::Abs(2.0));
        let ask = ask_price(mid, Spread::Abs(2.0));
        assert!(bid < mid && mid < ask);
        assert!((bid - 1.0 / 1384.0).abs() < 1e-15);
        assert!((ask - 1.0 / 1380.0).abs() < 1e-15);
    }

    #[test]
    fn oracle_rate_is_ray_when_balanced() {
        // 1.0 price, equal decimals → exactly RAY (1 collateral per 1 debt).
        let ray: U256 = "1000000000000000000000000000".parse().unwrap();
        assert_eq!(oracle_rate_ray(1.0, 6, 6), ray);
    }

    #[test]
    fn oracle_rate_inverts_and_decimal_adjusts() {
        // debt USDT 6dp, collateral cNGN 18dp. 1 USDT-atomic values
        // debt_in × rate / RAY collateral-atomic. At price 1.0:
        // 1 USDT (1e6) → 1 cNGN (1e18), so rate × 1e6 / RAY == 1e18.
        let rate = oracle_rate_ray(1.0, 6, 18);
        let ray: U256 = "1000000000000000000000000000".parse().unwrap();
        let valued = U256::from(1_000_000u64) * rate / ray;
        assert_eq!(valued, U256::from(1_000_000_000_000_000_000u128));
    }
}
