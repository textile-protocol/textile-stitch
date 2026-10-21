// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The price band: the last check on a firm order before it is signed.
//!
//! The maker must not receive less than `(1 − band)` of what it gives is
//! worth at the feed print the order was priced off. One-sided: a quote in
//! the maker's favour (every spread Stitch quotes) is never bounded. With
//! `I` and `O` the order's exact input and output, `s`/`c` the debt and
//! collateral decimals, the print as `N / D` debt per collateral (`N` its
//! WAD form, `D` = 1e18), `B` = 10 000 and `L` the band in bps:
//!
//! ```text
//! bid (pay debt, get collateral):  O × N × 10^s × B  >=  I × D × 10^c × (B − L)
//! ask (pay collateral, get debt):  O × D × 10^c × B  >=  I × N × 10^s × (B − L)
//! ```
//!
//! This is Warp's rule (`packages/warp/src/policy/band.rs`) on the same
//! legs, and the print reduces to `N` the same way Warp's `price_to_wad`
//! does. So for a vault maker an order that passes here passes Warp's band
//! at the same width, provided Warp holds the same print as its reference
//! (it drops a `degraded` one) and the pool's configured decimals are the
//! vault's real ones (Warp reads them on-chain). It is checked on the final
//! signed amounts, after rounding, so it catches amount math that came out
//! wrong. It judges with the pool's configured decimals, so a wrong
//! `*_decimals` in `stitch.toml` is not something it can see.
//! Cross-multiplied in 1024 bits: equality passes, one atomic unit less
//! fails, a product that does not fit fails.
//!
//! EOA makers get no Warp, so for them this is the only bound there is.

use alloy_primitives::ruint::Uint;
use alloy_primitives::U256;

type U1024 = Uint<1024, 16>;

const BPS: u64 = 10_000;
const WAD: u128 = 1_000_000_000_000_000_000;
/// RAY / WAD: level rates are RAY-scaled, the print is WAD-scaled.
const RAY_PER_WAD: u64 = 1_000_000_000;

/// The print as WAD debt per collateral, via its 18-decimal string — the
/// same reduction Warp makes, so both sides compare against one integer.
/// `None` for anything not a positive finite price below 1e40, or one that
/// rounds to zero.
pub fn price_wad(price: f64) -> Option<U256> {
    if !price.is_finite() || price <= 0.0 || price >= 1e40 {
        return None;
    }
    let text = format!("{price:.18}");
    let (int_part, frac_part) = text.split_once('.')?;
    let digits = format!("{int_part}{frac_part}");
    let digits = digits.trim_start_matches('0');
    let wad = U256::from_str_radix(if digits.is_empty() { "0" } else { digits }, 10).ok()?;
    (!wad.is_zero()).then_some(wad)
}

/// One firm order as the band sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    /// The maker buys collateral (pays debt) when true.
    pub bid: bool,
    /// What the maker pays, atomic.
    pub input: U256,
    /// What the maker receives, atomic.
    pub output: U256,
    pub debt_decimals: u8,
    pub collateral_decimals: u8,
}

fn pow10(decimals: u8) -> Option<U1024> {
    U1024::from(10u64).checked_pow(U1024::from(decimals))
}

/// What the input is worth in output units at `wad`, as an exact fraction
/// of raw output units. `None` when it does not fit, or when it is zero
/// (a zero input or print values the input at nothing, and anything would
/// pass).
fn fair_output(order: &Order, wad: U256) -> Option<(U1024, U1024)> {
    let input = U1024::from(order.input);
    let n = U1024::from(wad);
    let d = U1024::from(WAD);
    let s = pow10(order.debt_decimals)?;
    let c = pow10(order.collateral_decimals)?;
    let (num, den) = if order.bid {
        (input.checked_mul(d)?.checked_mul(c)?, n.checked_mul(s)?)
    } else {
        (input.checked_mul(n)?.checked_mul(s)?, d.checked_mul(c)?)
    };
    (!num.is_zero() && !den.is_zero()).then_some((num, den))
}

/// Whether `order` passes the band against the print `wad`. Pure.
pub fn order_within(order: &Order, wad: U256, max_loss_bps: u32) -> bool {
    let check = || {
        let (num, den) = fair_output(order, wad)?;
        let keep = BPS.checked_sub(u64::from(max_loss_bps))?;
        let received = U1024::from(order.output)
            .checked_mul(den)?
            .checked_mul(U1024::from(BPS))?;
        Some(received >= num.checked_mul(U1024::from(keep))?)
    };
    check().unwrap_or(false)
}

/// The order's output against the input's value at the print, in bps,
/// rounded down: `-120` is 1.2 % worse for the maker. For the log line
/// only; the decision is [`order_within`].
pub fn order_vs_print_bps(order: &Order, wad: U256) -> Option<i64> {
    let (num, den) = fair_output(order, wad)?;
    let ratio = U1024::from(order.output)
        .checked_mul(den)?
        .checked_mul(U1024::from(BPS))?
        / num;
    Some(i64::try_from(u64::try_from(ratio).ok()?).ok()? - BPS as i64)
}

/// Whether a published level rate passes the band against the print. A
/// level's rate is RAY debt per collateral, decimal-normalised (see
/// `rfq::math::rate_ray`); a bid level is where the maker buys collateral,
/// an ask where it sells it. Same rule as [`order_within`], per unit.
pub fn level_within(bid: bool, rate_ray: U256, wad: U256, max_loss_bps: u32) -> bool {
    let check = || {
        let print = U1024::from(wad).checked_mul(U1024::from(RAY_PER_WAD))?;
        let rate = U1024::from(rate_ray);
        let keep = U1024::from(BPS.checked_sub(u64::from(max_loss_bps))?);
        let b = U1024::from(BPS);
        // Buying at `rate` gets 1 / rate collateral per debt, against
        // 1 / print at the print: 1 / rate >= (1 / print) × keep / B.
        Some(if bid {
            print.checked_mul(b)? >= rate.checked_mul(keep)?
        } else {
            rate.checked_mul(b)? >= print.checked_mul(keep)?
        })
    };
    check().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U512;

    fn unit(decimals: u8) -> U256 {
        U256::from(10u64).pow(U256::from(decimals))
    }

    /// Independent oracle: the smallest passing output at `n / d` debt per
    /// collateral, the full rational in 512 bits, rounded up once.
    fn floor_oracle(bid: bool, input: U256, s: u8, c: u8, n: u64, d: u64, l: u64) -> U256 {
        let (num, den) = if bid {
            (
                U512::from(input) * U512::from(d) * U512::from(unit(c)),
                U512::from(n) * U512::from(unit(s)),
            )
        } else {
            (
                U512::from(input) * U512::from(n) * U512::from(unit(s)),
                U512::from(d) * U512::from(unit(c)),
            )
        };
        let num = num * U512::from(BPS - l);
        let den = den * U512::from(BPS);
        let q = num / den;
        let q = if q * den == num { q } else { q + U512::from(1) };
        q.to::<U256>()
    }

    fn order(bid: bool, input: U256, output: U256, s: u8, c: u8) -> Order {
        Order {
            bid,
            input,
            output,
            debt_decimals: s,
            collateral_decimals: c,
        }
    }

    #[test]
    fn the_print_reduces_to_the_same_wad_warp_uses() {
        assert_eq!(price_wad(1.0), Some(U256::from(WAD)));
        assert_eq!(
            price_wad(0.000651234567),
            Some(U256::from(651_234_567_000_000u64))
        );
        assert_eq!(
            price_wad(1550.5),
            U256::from_str_radix("1550500000000000000000", 10).ok()
        );
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e-19, 1e40] {
            assert_eq!(price_wad(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_floor_is_exact_on_both_sides_for_every_decimal_ordering() {
        // Print 1/2 debt per collateral, 100 bps. One raw unit past a round
        // input makes the floor fractional.
        for (s, c) in [(6u8, 6u8), (6, 18), (18, 6), (18, 18)] {
            for bid in [true, false] {
                let input = unit(if bid { s } else { c }) * U256::from(10u64) + U256::from(1u64);
                let floor = floor_oracle(bid, input, s, c, 1, 2, 100);
                let wad = price_wad(0.5).unwrap();
                assert!(order_within(&order(bid, input, floor, s, c), wad, 100));
                assert!(
                    !order_within(&order(bid, input, floor - U256::from(1u64), s, c), wad, 100),
                    "{s}/{c} bid={bid}: one unit under fails"
                );
            }
        }
    }

    #[test]
    fn an_inverse_pair_holds_the_same_order_to_the_same_floor() {
        // The same trade seen from a pool whose debt is the other's
        // collateral, at the reciprocal print.
        let input = unit(18) * U256::from(10u64) + U256::from(7u64);
        let a = floor_oracle(true, input, 18, 6, 2, 1, 250);
        let b = floor_oracle(false, input, 6, 18, 1, 2, 250);
        assert_eq!(a, b);
        assert!(order_within(
            &order(true, input, a, 18, 6),
            price_wad(2.0).unwrap(),
            250
        ));
        assert!(order_within(
            &order(false, input, b, 6, 18),
            price_wad(0.5).unwrap(),
            250
        ));
        assert!(!order_within(
            &order(false, input, b - U256::from(1u64), 6, 18),
            price_wad(0.5).unwrap(),
            250
        ));
    }

    #[test]
    fn a_spread_in_the_makers_favour_always_passes_and_a_10x_error_never_does() {
        let wad = price_wad(0.5).unwrap();
        // Bid 2 % under the print: more collateral per debt than fair.
        let bid = order(
            true,
            unit(6),
            unit(6) * U256::from(204u64) / U256::from(100u64),
            6,
            6,
        );
        assert!(order_within(&bid, wad, 1));
        assert_eq!(order_vs_print_bps(&bid, wad), Some(200));
        // Ask 2 % over: more debt per collateral than fair.
        let ask = order(
            false,
            unit(6),
            unit(6) * U256::from(51u64) / U256::from(100u64),
            6,
            6,
        );
        assert!(order_within(&ask, wad, 1));
        // A decimal slip of 10x either way is far outside 15 %.
        let slip = order(true, unit(6), unit(6) / U256::from(5u64), 6, 6);
        assert!(!order_within(&slip, wad, 1_500));
        assert_eq!(order_vs_print_bps(&slip, wad), Some(-9_000));
        let slip = order(false, unit(6), unit(6) / U256::from(20u64), 6, 6);
        assert!(!order_within(&slip, wad, 1_500));
    }

    #[test]
    fn a_product_that_does_not_fit_fails() {
        let o = order(false, U256::MAX, U256::MAX, 255, 255);
        assert!(!order_within(&o, U256::MAX, 100));
        assert_eq!(order_vs_print_bps(&o, U256::MAX), None);
    }

    #[test]
    fn a_zero_input_or_zero_print_fails_rather_than_passing_everything() {
        let ask = order(false, unit(6), U256::from(1u64), 6, 6);
        assert!(!order_within(&ask, U256::ZERO, 100));
        assert_eq!(order_vs_print_bps(&ask, U256::ZERO), None);
        let empty = order(true, U256::ZERO, unit(6), 6, 6);
        assert!(!order_within(&empty, price_wad(1.0).unwrap(), 100));
    }

    #[test]
    fn a_level_passes_exactly_where_an_order_at_its_rate_would() {
        let wad = price_wad(0.5).unwrap();
        let ray = |numerator: u64, denominator: u64| {
            U256::from(10u64).pow(U256::from(27u64)) * U256::from(numerator)
                / U256::from(denominator)
        };
        // A 15 % band lets the maker buy collateral at up to 0.5 / 0.85 and
        // sell at down to 0.5 × 0.85 = 0.425.
        let top_bid =
            U256::from(10u64).pow(U256::from(27u64)) * U256::from(10u64) / U256::from(17u64);
        assert!(level_within(true, top_bid, wad, 1_500));
        assert!(!level_within(true, top_bid + U256::from(1u64), wad, 1_500));
        assert!(level_within(false, ray(425, 1_000), wad, 1_500));
        assert!(!level_within(
            false,
            ray(425, 1_000) - U256::from(1u64),
            wad,
            1_500
        ));
        // Ordinary levels: bid under the print, ask over it.
        assert!(level_within(true, ray(49, 100), wad, 1));
        assert!(level_within(false, ray(51, 100), wad, 1));
    }
}
