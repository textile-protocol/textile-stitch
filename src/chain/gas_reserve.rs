// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Gas the bot keeps for itself when it trades the chain's own gas coin.
//!
//! On Celo the gas coin is also an ERC-20 (GoldToken): `balanceOf(bot)` on it
//! is the bot's native balance, and a transfer of it moves native coins. So a
//! CELO side quoted at `"max"` pledges the same coins the bot pays gas with,
//! and the fill that drains it leaves the wallet unable to send anything of
//! its own: no withdraw, no Permit2 re-approve, no redeem-epoch close, no
//! taker fill. [`GasReserve`] holds a fixed amount of that one token out of
//! every funded figure, so what is left after any sequence of fills is at
//! least the reserve.
//!
//! The token is matched by address per chain, never by symbol: anyone can
//! deploy an ERC-20 called "CELO", and treating it as gas would hide real
//! inventory. A token on no row of [`NATIVE_ERC20`] is never touched.

use alloy_primitives::{address, Address, U256};

use crate::chain::gas_caps::whole_tokens_to_wei;

const WEI_PER_TOKEN: u128 = 1_000_000_000_000_000_000;

/// GoldToken, the ERC-20 face of CELO. Same address on mainnet and on Celo
/// Sepolia (`packages/constants` lists it on both).
const CELO: Address = address!("471EcE3750Da237f93B8E339c536989b8978a438");

/// Default CELO reserve: 5 CELO, in wei.
///
/// Sized for the bot's own sends on Celo: a Permit2 approve is ~46k gas, a
/// withdraw 21k-50k, a limit-order taker fill a few hundred thousand. Celo's
/// base fee sits at its 200 gwei floor and `eth_gasPrice` answers ~203 gwei
/// (October 2026); wallets that bid 2x base pay ~400. At 400 gwei 5 CELO pays
/// 12.5M gas, about forty 300k-gas sends. It also clears the panel's funding
/// gate on its own: the gate asks $0.06 per outstanding approval, and 5 CELO
/// at the panel's deliberately low $0.05 fallback price is $0.25, four
/// approvals' worth. At ~$0.10 a coin it keeps about fifty cents out of the
/// book.
const CELO_RESERVE_WEI: u128 = 5 * WEI_PER_TOKEN;

/// A chain whose gas coin also answers as an ERC-20.
struct NativeErc20 {
    chain_id: u64,
    /// The ERC-20 contract whose `balanceOf` is the native balance.
    token: Address,
    /// Wei held back when `[gas].native_reserve` is unset.
    default_reserve_wei: u128,
}

/// Every chain the bot knows to have one. Celo's retired Alfajores testnet
/// (44787) used a different GoldToken address and is left out.
const NATIVE_ERC20: &[NativeErc20] = &[
    NativeErc20 {
        chain_id: 42220,
        token: CELO,
        default_reserve_wei: CELO_RESERVE_WEI,
    },
    NativeErc20 {
        chain_id: 11142220,
        token: CELO,
        default_reserve_wei: CELO_RESERVE_WEI,
    },
];

fn entry(chain_id: u64) -> Option<&'static NativeErc20> {
    NATIVE_ERC20.iter().find(|e| e.chain_id == chain_id)
}

/// The ERC-20 address of `chain_id`'s gas coin, when it has one.
pub fn native_erc20(chain_id: u64) -> Option<Address> {
    entry(chain_id).map(|e| e.token)
}

/// What one chain holds back for gas, and on which token.
///
/// `Default` holds nothing back on any token: the shape for a chain whose gas
/// coin is not an ERC-20, and for tests that don't care.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GasReserve {
    /// The gas coin's ERC-20 address; `None` and nothing is held back.
    token: Option<Address>,
    /// Wei of that token kept out of every funded figure.
    amount: U256,
}

impl GasReserve {
    /// The built-in reserve for `chain_id`: the registry's default on a chain
    /// with a native ERC-20, nothing anywhere else.
    pub fn for_chain(chain_id: u64) -> Self {
        entry(chain_id).map_or_else(Self::default, |e| Self {
            token: Some(e.token),
            amount: U256::from(e.default_reserve_wei),
        })
    }

    /// Replace the amount with `[gas].native_reserve`, in whole gas coins.
    /// Never adds a token: on a chain without a native ERC-20 the amount has
    /// nothing to apply to.
    pub fn with_override(self, native_reserve: Option<f64>) -> Self {
        Self {
            amount: native_reserve
                .map(whole_tokens_to_wei)
                .unwrap_or(self.amount),
            ..self
        }
    }

    /// Wei held back, whatever the token.
    pub fn amount(&self) -> U256 {
        self.amount
    }

    /// Whether `token` is this chain's gas coin, by address.
    pub fn covers(&self, token: Address) -> bool {
        self.token == Some(token)
    }

    /// What of a `balance` of `token` the bot may commit: the whole balance
    /// for any other token, the balance less the reserve (floored at zero)
    /// for the gas coin.
    pub fn spendable(&self, token: Address, balance: U256) -> U256 {
        if self.covers(token) {
            balance.saturating_sub(self.amount)
        } else {
            balance
        }
    }

    /// The funded amount the quoting paths size against:
    /// `min(spendable balance, Permit2 allowance)`. The reserve comes off the
    /// balance, not off the minimum, so an allowance below the balance is not
    /// cut a second time.
    pub fn funded(&self, token: Address, balance: U256, allowance: U256) -> U256 {
        self.spendable(token, balance).min(allowance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPOOF: Address = address!("000000000000000000000000000000000000CE10");
    const USDT: Address = address!("48065fbBE25f71C9282ddf5e1cD6D6A887483D5e");

    fn celo(n: u64) -> U256 {
        U256::from(n) * U256::from(WEI_PER_TOKEN)
    }

    #[test]
    fn only_celo_chains_have_a_native_erc20() {
        assert_eq!(native_erc20(42220), Some(CELO));
        assert_eq!(native_erc20(11142220), Some(CELO));
        for chain in [1, 8453, 42161, 56, 137, 4663, 31337, 44787] {
            assert_eq!(native_erc20(chain), None, "chain {chain}");
            assert_eq!(GasReserve::for_chain(chain), GasReserve::default());
        }
    }

    #[test]
    fn celo_holds_back_five_coins_by_default() {
        let reserve = GasReserve::for_chain(42220);
        assert!(reserve.covers(CELO));
        assert_eq!(reserve.amount(), celo(5));
        assert_eq!(reserve.spendable(CELO, celo(100)), celo(95));
    }

    #[test]
    fn the_match_is_by_address_on_the_right_chain() {
        let reserve = GasReserve::for_chain(42220);
        // A token named CELO at another address is just a token.
        assert!(!reserve.covers(SPOOF));
        assert_eq!(reserve.spendable(SPOOF, celo(100)), celo(100));
        // GoldToken's address on a chain where it means nothing.
        assert!(!GasReserve::for_chain(8453).covers(CELO));
        assert_eq!(
            GasReserve::for_chain(8453).spendable(CELO, celo(100)),
            celo(100)
        );
    }

    #[test]
    fn other_tokens_are_untouched() {
        let reserve = GasReserve::for_chain(42220);
        let balance = U256::from(1_000_000u64);
        assert_eq!(reserve.spendable(USDT, balance), balance);
        assert_eq!(reserve.funded(USDT, balance, U256::MAX), balance);
        assert_eq!(
            reserve.funded(USDT, balance, U256::from(7u8)),
            U256::from(7u8)
        );
    }

    #[test]
    fn a_balance_under_the_reserve_spends_nothing() {
        let reserve = GasReserve::for_chain(42220);
        assert_eq!(reserve.spendable(CELO, celo(3)), U256::ZERO);
        assert_eq!(reserve.spendable(CELO, celo(5)), U256::ZERO);
        assert_eq!(reserve.spendable(CELO, U256::ZERO), U256::ZERO);
        assert_eq!(reserve.funded(CELO, celo(3), U256::MAX), U256::ZERO);
    }

    #[test]
    fn the_reserve_comes_off_the_balance_not_the_allowance() {
        let reserve = GasReserve::for_chain(42220);
        // Allowance binds: 100 held, 5 reserved, 40 approved -> 40.
        assert_eq!(reserve.funded(CELO, celo(100), celo(40)), celo(40));
        // Balance binds: 100 held, 5 reserved, 1000 approved -> 95.
        assert_eq!(reserve.funded(CELO, celo(100), celo(1000)), celo(95));
    }

    #[test]
    fn the_operator_sets_the_amount_and_zero_turns_it_off() {
        let reserve = GasReserve::for_chain(42220).with_override(Some(0.5));
        assert_eq!(reserve.amount(), celo(1) / U256::from(2u8));
        let off = GasReserve::for_chain(42220).with_override(Some(0.0));
        assert_eq!(off.spendable(CELO, celo(100)), celo(100));
        assert_eq!(
            GasReserve::for_chain(42220).with_override(None),
            GasReserve::for_chain(42220)
        );
    }

    #[test]
    fn an_override_never_invents_a_gas_token() {
        let reserve = GasReserve::for_chain(1).with_override(Some(3.0));
        assert!(!reserve.covers(CELO));
        assert_eq!(reserve.spendable(CELO, celo(10)), celo(10));
    }
}
