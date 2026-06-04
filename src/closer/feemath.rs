// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Rust port of `contracts/v3/libraries/logic/FeeLogic.sol` — the Settlement v3
//! fee curve and fill-time distribution. Mirrors the Solidity exactly (and its
//! TS twin `settlement-closer/src/feeMath.ts`); the tests assert against
//! vectors generated from that TS mirror, which the Foundry suite already
//! proves equivalent to the Solidity. Any change to FeeLogic.sol must be
//! mirrored here.

use alloy_primitives::U256;

const PROTOCOL_FEE_BPS: u64 = 1000; // 10%
const BPS_DIVISOR: u64 = 10_000;

/// RAY (1e27).
pub fn ray() -> U256 {
    U256::from(1_000_000_000_000_000_000_000_000_000u128)
}

fn mul_div_floor(a: U256, b: U256, c: U256) -> U256 {
    a * b / c
}

fn mul_div_ceil(a: U256, b: U256, c: U256) -> U256 {
    let p = a * b;
    let q = p / c;
    if p % c == U256::ZERO {
        q
    } else {
        q + U256::from(1u8)
    }
}

/// `min(ceil(amount*10%), floor(cap*10%))`, with the cap rounded down.
pub fn protocol_fee_from_amount(amount: U256, cap_amount: U256) -> U256 {
    if amount == U256::ZERO || cap_amount == U256::ZERO {
        return U256::ZERO;
    }
    let bps = U256::from(PROTOCOL_FEE_BPS);
    let div = U256::from(BPS_DIVISOR);
    let cap = mul_div_floor(cap_amount, bps, div);
    let raw = mul_div_ceil(amount, bps, div);
    if raw < cap {
        raw
    } else {
        cap
    }
}

/// Linear Dutch fee + CP principal return. In window: fee ramps floor→buffer,
/// principal = D. Past window: fee = buffer, principal decays toward 0.
pub fn fee_and_principal(
    d: U256,
    elapsed: U256,
    floor_ray: U256,
    buffer_ray: U256,
    window: U256,
) -> (U256, U256) {
    if elapsed >= window {
        let dt = elapsed - window;
        let decay = mul_div_floor(buffer_ray, dt, window);
        let cp = if decay >= ray() {
            U256::ZERO
        } else {
            mul_div_floor(d, ray() - decay, ray())
        };
        (buffer_ray, cp)
    } else {
        let fee = floor_ray + mul_div_floor(buffer_ray - floor_ray, elapsed, window);
        (fee, d)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillQuote {
    pub fee_ray: U256,
    pub filler_debt_in: U256,
    pub filler_collateral_out: U256,
    pub cp_principal_debt: U256,
    pub trader_refund_debt: U256,
    pub cp_protocol_fee_debt: U256,
    pub filler_protocol_fee_collateral: U256,
    pub lp_principal_loss: U256,
}

pub fn quote_fill(
    c: U256,
    d: U256,
    elapsed: U256,
    floor_ray: U256,
    buffer_ray: U256,
    window: U256,
) -> FillQuote {
    let (fee_ray, cp_principal) = fee_and_principal(d, elapsed, floor_ray, buffer_ray, window);
    let two_ray = ray() * U256::from(2u8);

    let cp_yield_gross = mul_div_floor(d, fee_ray, two_ray);
    let cp_yield_cap = mul_div_floor(d, buffer_ray, two_ray);
    let cp_protocol_fee = protocol_fee_from_amount(cp_yield_gross, cp_yield_cap);

    let trader_refund = if elapsed < window && fee_ray < buffer_ray {
        mul_div_floor(d, buffer_ray - fee_ray, ray())
    } else {
        U256::ZERO
    };

    let filler_debt_in = cp_principal + cp_yield_gross + trader_refund;

    let denom = (ray() + buffer_ray) * U256::from(2u8);
    let filler_margin = mul_div_floor(c, fee_ray, denom);
    let filler_margin_cap = mul_div_floor(c, buffer_ray, denom);
    let filler_proto = protocol_fee_from_amount(filler_margin, filler_margin_cap);

    FillQuote {
        fee_ray,
        filler_debt_in,
        filler_collateral_out: c - filler_proto,
        cp_principal_debt: cp_principal,
        trader_refund_debt: trader_refund,
        cp_protocol_fee_debt: cp_protocol_fee,
        filler_protocol_fee_collateral: filler_proto,
        lp_principal_loss: d - cp_principal,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOutcome {
    pub fee_ray: U256,
    /// Debt (USDT) the filler pays.
    pub debt_in: U256,
    /// Collateral (cNGN) the filler receives, net of the protocol fee.
    pub closer_take_net: U256,
    pub lp_loss: U256,
    pub past_window: bool,
}

pub fn close_outcome(
    c: U256,
    d: U256,
    elapsed: U256,
    floor_ray: U256,
    buffer_ray: U256,
    window: U256,
) -> CloseOutcome {
    let q = quote_fill(c, d, elapsed, floor_ray, buffer_ray, window);
    CloseOutcome {
        fee_ray: q.fee_ray,
        debt_in: q.filler_debt_in,
        closer_take_net: q.filler_collateral_out,
        lp_loss: q.lp_principal_loss,
        past_window: elapsed >= window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> U256 {
        s.parse().unwrap()
    }

    // Vectors generated from settlement-closer/src/feeMath.ts (the TS mirror the
    // Foundry suite proves equivalent to FeeLogic.sol). floor 0.2%, buffer 2%,
    // window 5d; C = 1,550,000,000; D = 1,000,000,000.
    fn quote_at(elapsed: u64) -> FillQuote {
        quote_fill(
            U256::from(1_550_000_000u64),
            U256::from(1_000_000_000u64),
            U256::from(elapsed),
            u("2000000000000000000000000"),
            u("20000000000000000000000000"),
            U256::from(432_000u64),
        )
    }

    #[test]
    fn matches_solidity_at_t0() {
        let q = quote_at(0);
        assert_eq!(q.fee_ray, u("2000000000000000000000000"));
        assert_eq!(q.filler_debt_in, U256::from(1_019_000_000u64));
        assert_eq!(q.filler_collateral_out, U256::from(1_549_848_039u64));
        assert_eq!(q.cp_principal_debt, U256::from(1_000_000_000u64));
        assert_eq!(q.trader_refund_debt, U256::from(18_000_000u64));
        assert_eq!(q.cp_protocol_fee_debt, U256::from(100_000u64));
        assert_eq!(q.filler_protocol_fee_collateral, U256::from(151_961u64));
        assert_eq!(q.lp_principal_loss, U256::ZERO);
    }

    #[test]
    fn matches_solidity_at_half_window() {
        let q = quote_at(216_000);
        assert_eq!(q.fee_ray, u("11000000000000000000000000"));
        assert_eq!(q.filler_debt_in, U256::from(1_014_500_000u64));
        assert_eq!(q.filler_collateral_out, U256::from(1_549_164_215u64));
        assert_eq!(q.trader_refund_debt, U256::from(9_000_000u64));
        assert_eq!(q.cp_protocol_fee_debt, U256::from(550_000u64));
        assert_eq!(q.filler_protocol_fee_collateral, U256::from(835_785u64));
    }

    #[test]
    fn matches_solidity_at_window_edge() {
        let q = quote_at(432_000);
        assert_eq!(q.fee_ray, u("20000000000000000000000000"));
        assert_eq!(q.filler_debt_in, U256::from(1_010_000_000u64));
        assert_eq!(q.filler_collateral_out, U256::from(1_548_480_393u64));
        assert_eq!(q.trader_refund_debt, U256::ZERO);
        assert_eq!(q.lp_principal_loss, U256::ZERO);
    }

    #[test]
    fn matches_solidity_past_window_with_principal_decay() {
        let q = quote_at(864_000);
        assert_eq!(q.fee_ray, u("20000000000000000000000000"));
        assert_eq!(q.filler_debt_in, U256::from(990_000_000u64));
        assert_eq!(q.cp_principal_debt, U256::from(980_000_000u64));
        assert_eq!(q.lp_principal_loss, U256::from(20_000_000u64));
    }

    #[test]
    fn close_outcome_flags_past_window() {
        let floor = u("2000000000000000000000000");
        let buffer = u("20000000000000000000000000");
        let win = U256::from(432_000u64);
        let c = U256::from(1_550_000_000u64);
        let d = U256::from(1_000_000_000u64);
        assert!(!close_outcome(c, d, U256::ZERO, floor, buffer, win).past_window);
        assert!(close_outcome(c, d, U256::from(500_000u64), floor, buffer, win).past_window);
    }
}
