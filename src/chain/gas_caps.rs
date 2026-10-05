// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Ceilings on what one transaction can bid for gas, whatever the node says.
//!
//! The wallet fills the tip, the fee cap and the gas limit from the JSON-RPC
//! node's answers. A hostile or broken node can answer anything, and a tip is
//! paid to the block producer, so without a ceiling one send could hand most of
//! the wallet's gas balance to whoever the node colludes with. These caps sit
//! far above what normal operation bids on each chain (hundreds of times the
//! usual fee on the cheap chains) and are overridable from `[gas]` in
//! stitch.toml.

use alloy_primitives::U256;

const GWEI: u128 = 1_000_000_000;
/// 0.001 of a chain's gas token (1e15 wei), the unit the defaults below use.
const MILLI: u128 = 1_000_000_000_000_000;
const WEI_PER_TOKEN: f64 = 1e18;
const WEI_PER_GWEI: f64 = 1e9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasCaps {
    /// Highest `max_fee_per_gas` the wallet signs, in wei. The tip never
    /// exceeds the fee cap, so this bounds it too.
    pub max_fee_per_gas: U256,
    /// Highest `gas_limit * max_fee_per_gas`, in wei of the gas token: the
    /// most a single transaction can ever be charged.
    pub max_tx_fee: U256,
}

impl GasCaps {
    /// Built-in ceilings for `chain_id`. Normal fees for reference: Ethereum
    /// a few gwei (100+ in a spike), the ETH L2s and BNB Chain well under
    /// 1 gwei, Celo ~25-50 gwei, Polygon 30 to a few thousand gwei in a spike.
    pub fn for_chain(chain_id: u64) -> Self {
        let (max_fee_gwei, max_tx_fee_milli): (u128, u128) = match chain_id {
            // Ethereum mainnet: 0.1 ETH per transaction.
            1 => (500, 100),
            // ETH-gas L2s (Base, Base Sepolia, Arbitrum One, Arbitrum Sepolia,
            // Robinhood Chain, OP Mainnet): 0.01 ETH per transaction.
            8453 | 84532 | 42161 | 421614 | 4663 | 10 => (50, 10),
            // BNB Smart Chain and its testnet: 0.05 BNB.
            56 | 97 => (100, 50),
            // Celo and its testnets: 10 CELO.
            42220 | 44787 | 11142220 => (500, 10_000),
            // Polygon PoS: 50 POL.
            137 => (5_000, 50_000),
            // Anything else, local dev chains included: 0.1 of the gas token.
            _ => (1_000, 100),
        };
        Self {
            max_fee_per_gas: U256::from(max_fee_gwei * GWEI),
            max_tx_fee: U256::from(max_tx_fee_milli * MILLI),
        }
    }

    /// Replace a ceiling with the operator's own, from `[gas]`. Units are the
    /// human ones the config uses: gwei per gas, and whole gas tokens.
    pub fn with_overrides(
        self,
        max_fee_per_gas_gwei: Option<f64>,
        max_tx_fee: Option<f64>,
    ) -> Self {
        Self {
            max_fee_per_gas: max_fee_per_gas_gwei
                .map(|g| wei(g * WEI_PER_GWEI))
                .unwrap_or(self.max_fee_per_gas),
            max_tx_fee: max_tx_fee
                .map(whole_tokens_to_wei)
                .unwrap_or(self.max_tx_fee),
        }
    }

    /// The highest fee cap a transaction with `gas_limit` may carry and stay
    /// under both ceilings.
    pub fn fee_ceiling(&self, gas_limit: U256) -> U256 {
        let by_total = self
            .max_tx_fee
            .checked_div(gas_limit)
            .unwrap_or(self.max_fee_per_gas);
        self.max_fee_per_gas.min(by_total)
    }

    /// Why a plan with this fee cap and gas limit may not be signed, or
    /// `None` when it's within both ceilings.
    pub fn violation(&self, max_fee: U256, gas_limit: U256) -> Option<String> {
        if max_fee > self.max_fee_per_gas {
            return Some(format!(
                "fee cap {max_fee} wei/gas is over the {} wei/gas ceiling \
                 ([gas].max_fee_per_gas_gwei)",
                self.max_fee_per_gas
            ));
        }
        let total = max_fee.saturating_mul(gas_limit);
        (total > self.max_tx_fee).then(|| {
            format!(
                "worst-case fee {total} wei (gas limit {gas_limit} x {max_fee} wei/gas) is over \
                 the {} wei per-transaction ceiling ([gas].max_tx_fee)",
                self.max_tx_fee
            )
        })
    }
}

/// A non-negative, finite float to whole wei, rounding down. Config
/// validation has already refused anything else.
fn wei(v: f64) -> U256 {
    U256::from(v.max(0.0) as u128)
}

/// Whole gas tokens as `[gas]` writes them (`0.5` CELO) to wei, rounding down.
pub(crate) fn whole_tokens_to_wei(tokens: f64) -> U256 {
    wei(tokens * WEI_PER_TOKEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gwei(n: u64) -> U256 {
        U256::from(n) * U256::from(GWEI)
    }

    #[test]
    fn normal_fees_on_every_template_chain_sit_well_under_the_ceilings() {
        // (chain, a busy-day fee cap, a large send's gas limit)
        let busy = [
            (1, gwei(200), U256::from(400_000u64)),
            (8453, gwei(1), U256::from(2_000_000u64)),
            (4663, gwei(1), U256::from(5_000_000u64)),
            (56, gwei(5), U256::from(2_000_000u64)),
            (42220, gwei(200), U256::from(2_000_000u64)),
            (137, gwei(2_000), U256::from(2_000_000u64)),
        ];
        for (chain, max_fee, gas_limit) in busy {
            let caps = GasCaps::for_chain(chain);
            assert_eq!(caps.violation(max_fee, gas_limit), None, "chain {chain}");
        }
    }

    #[test]
    fn a_fee_cap_over_the_ceiling_is_a_violation() {
        let caps = GasCaps::for_chain(8453);
        let err = caps.violation(gwei(51), U256::from(21_000u64)).unwrap();
        assert!(err.contains("max_fee_per_gas_gwei"), "{err}");
    }

    #[test]
    fn a_total_over_the_ceiling_is_a_violation_even_at_a_legal_price() {
        // 0.01 ETH ceiling on Base: 40 gwei x 1M gas = 0.04 ETH.
        let caps = GasCaps::for_chain(8453);
        let err = caps.violation(gwei(40), U256::from(1_000_000u64)).unwrap();
        assert!(err.contains("max_tx_fee"), "{err}");
    }

    #[test]
    fn the_fee_ceiling_is_the_tighter_of_the_two() {
        let caps = GasCaps::for_chain(8453);
        assert_eq!(caps.fee_ceiling(U256::from(21_000u64)), gwei(50));
        assert_eq!(
            caps.fee_ceiling(U256::from(1_000_000u64)),
            gwei(10),
            "0.01 ETH / 1M gas"
        );
        assert_eq!(caps.fee_ceiling(U256::ZERO), gwei(50));
    }

    #[test]
    fn operator_overrides_replace_only_what_they_set() {
        let caps = GasCaps::for_chain(1).with_overrides(Some(0.5), None);
        assert_eq!(caps.max_fee_per_gas, U256::from(500_000_000u64));
        assert_eq!(caps.max_tx_fee, GasCaps::for_chain(1).max_tx_fee);
        let caps = GasCaps::for_chain(1).with_overrides(None, Some(2.5));
        assert_eq!(caps.max_tx_fee, U256::from(2_500_000_000_000_000_000u128));
    }
}
