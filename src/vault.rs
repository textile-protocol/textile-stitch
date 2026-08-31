// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! OperatorVault view calldata and the epoch-prefixed Permit2 nonce.
//!
//! Mirrors `packages/constants/src/operatorVaultMath.ts` (`tradingNonce`) and
//! the no-arg views on `IOperatorVault`. Encoding only — RPC lives with the
//! caller so the RFQ inventory loop can share [`crate::rpc::Wallet`].

use alloy_primitives::{keccak256, Address, U256};

fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes()).0;
    [h[0], h[1], h[2], h[3]]
}

fn encode_view(sig: &str) -> Vec<u8> {
    selector(sig).to_vec()
}

pub fn encode_trading_epoch() -> Vec<u8> {
    encode_view("tradingEpoch()")
}

pub fn encode_quotable_settlement() -> Vec<u8> {
    encode_view("quotableSettlement()")
}

pub fn encode_liquid_settlement() -> Vec<u8> {
    encode_view("liquidSettlement()")
}

pub fn encode_quotable_corridor() -> Vec<u8> {
    encode_view("quotableCorridor()")
}

pub fn encode_settlement_asset() -> Vec<u8> {
    encode_view("settlementAsset()")
}

pub fn encode_corridor_asset() -> Vec<u8> {
    encode_view("corridorAsset()")
}

pub fn encode_close_only() -> Vec<u8> {
    encode_view("closeOnly()")
}

pub fn encode_max_order_input_settlement() -> Vec<u8> {
    encode_view("maxOrderInputSettlement()")
}

pub fn encode_max_order_input_corridor() -> Vec<u8> {
    encode_view("maxOrderInputCorridor()")
}

pub fn encode_paused() -> Vec<u8> {
    encode_view("paused()")
}

pub fn encode_max_order_lifetime() -> Vec<u8> {
    encode_view("maxOrderLifetime()")
}

/// Live vault limits used on the quote path. Inventory stays quotable;
/// per-order caps and lifetime clamp each signed order separately.
#[derive(Debug, Clone, Copy)]
pub struct VaultQuotePolicy {
    pub settlement: Address,
    pub corridor: Address,
    pub max_input_settlement: U256,
    pub max_input_corridor: U256,
    pub max_lifetime_secs: u64,
}

impl VaultQuotePolicy {
    pub fn max_input_for(&self, token: Address) -> U256 {
        if token == self.settlement {
            self.max_input_settlement
        } else if token == self.corridor {
            self.max_input_corridor
        } else {
            U256::ZERO
        }
    }

    /// VaultPolicy requires the exact settlement↔corridor pair, either way.
    pub fn matches_pair(&self, input: Address, output: Address) -> bool {
        (input == self.settlement && output == self.corridor)
            || (input == self.corridor && output == self.settlement)
    }
}

/// `paused` zeros both sides. `closeOnly` zeros settlement (`VaultPolicy`
/// rejects selling settlement while closed). Per-order caps stay off this
/// path so reservations still see the full quotable balance.
pub fn apply_vault_order_policy(
    settlement_qty: U256,
    corridor_qty: U256,
    close_only: bool,
    paused: bool,
) -> (U256, U256) {
    if paused {
        return (U256::ZERO, U256::ZERO);
    }
    let settlement = if close_only {
        U256::ZERO
    } else {
        settlement_qty
    };
    (settlement, corridor_qty)
}

/// Clamp a requested deadline to `now + maxOrderLifetime`. None if that
/// leaves no usable life.
pub fn clamp_vault_deadline(
    now_secs: u64,
    deadline_secs: u64,
    max_lifetime_secs: u64,
) -> Option<u64> {
    let cap = now_secs.saturating_add(max_lifetime_secs);
    let deadline = deadline_secs.min(cap);
    (deadline > now_secs).then_some(deadline)
}

/// `(epoch << 128) | counter` — VaultPolicy requires `epochFromNonce == tradingEpoch`.
pub fn trading_nonce(epoch: u64, counter: u128) -> U256 {
    (U256::from(epoch) << 128) | U256::from(counter)
}

/// Low 128 bits of a vault nonce: per-process salt in the high 64, wall-clock
/// milliseconds ×1000 plus the process counter in the low 64. `ms * 1000`
/// stays under 2^64 until the year ~2554, so the fields never overlap. The
/// salt is what keeps two bots quoting one vault from colliding when both
/// sign in the same millisecond with equal counters — the venue serializes
/// nonce reservations per vault and would reject the second as
/// `nonce_reserved`.
pub fn vault_nonce_low(salt: u64, unix_ms: u64, counter: u64) -> u128 {
    (u128::from(salt) << 64) | (u128::from(unix_ms) * 1000 + u128::from(counter))
}

pub fn address_from_word(word: U256) -> Address {
    let bytes = word.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    #[test]
    fn view_selectors_match_cast_keccak() {
        assert_eq!(&encode_trading_epoch(), &hex::decode("e3c85e7e").unwrap());
        assert_eq!(
            &encode_quotable_settlement(),
            &hex::decode("0f8769a0").unwrap()
        );
        assert_eq!(
            &encode_liquid_settlement(),
            &hex::decode("84d4fc64").unwrap()
        );
        assert_eq!(
            &encode_quotable_corridor(),
            &hex::decode("2e1741f9").unwrap()
        );
        assert_eq!(
            &encode_settlement_asset(),
            &hex::decode("d3781d58").unwrap()
        );
        assert_eq!(&encode_corridor_asset(), &hex::decode("f0f85843").unwrap());
        assert_eq!(&encode_close_only(), &hex::decode("c7dc844d").unwrap());
        assert_eq!(
            &encode_max_order_input_settlement(),
            &hex::decode("cb06c682").unwrap()
        );
        assert_eq!(
            &encode_max_order_input_corridor(),
            &hex::decode("65e36cd1").unwrap()
        );
        assert_eq!(&encode_paused(), &hex::decode("5c975abb").unwrap());
        assert_eq!(
            &encode_max_order_lifetime(),
            &hex::decode("9c454e9d").unwrap()
        );
    }

    #[test]
    fn close_only_zeros_settlement_and_pause_zeros_both() {
        let (open_s, open_c) =
            apply_vault_order_policy(U256::from(1_000u64), U256::from(2_000u64), false, false);
        assert_eq!(open_s, U256::from(1_000u64));
        assert_eq!(open_c, U256::from(2_000u64));
        let (closed, still) =
            apply_vault_order_policy(U256::from(1_000u64), U256::from(2_000u64), true, false);
        assert_eq!(closed, U256::ZERO);
        assert_eq!(still, U256::from(2_000u64));
        let (paused_s, paused_c) =
            apply_vault_order_policy(U256::from(1_000u64), U256::from(2_000u64), false, true);
        assert_eq!(paused_s, U256::ZERO);
        assert_eq!(paused_c, U256::ZERO);
    }

    #[test]
    fn matches_pair_is_the_vault_assets_in_either_direction() {
        let policy = VaultQuotePolicy {
            settlement: Address::from([1u8; 20]),
            corridor: Address::from([2u8; 20]),
            max_input_settlement: U256::ZERO,
            max_input_corridor: U256::ZERO,
            max_lifetime_secs: 0,
        };
        let other = Address::from([3u8; 20]);
        assert!(policy.matches_pair(policy.settlement, policy.corridor));
        assert!(policy.matches_pair(policy.corridor, policy.settlement));
        assert!(!policy.matches_pair(policy.settlement, other));
        assert!(!policy.matches_pair(other, policy.corridor));
    }

    #[test]
    fn vault_deadline_clamps_to_max_lifetime() {
        assert_eq!(clamp_vault_deadline(1_000, 2_000, 30), Some(1_030));
        assert_eq!(clamp_vault_deadline(1_000, 1_010, 30), Some(1_010));
        assert_eq!(clamp_vault_deadline(1_000, 1_000, 30), None);
    }

    #[test]
    fn vault_nonce_low_namespaces_by_process_salt() {
        let a = vault_nonce_low(1, 1_754_388_000_000, 0);
        let b = vault_nonce_low(2, 1_754_388_000_000, 0);
        // Same millisecond, same counter, different process — distinct nonces.
        assert_ne!(a, b);
        // Salt sits above the ms field; ms*1000 + counter survives below it.
        assert_eq!(a >> 64, 1u128);
        assert_eq!(a & u128::from(u64::MAX), 1_754_388_000_000u128 * 1000);
        // Epoch still lands in the high half of the full nonce.
        let n = trading_nonce(7, a);
        assert_eq!(n >> 128, U256::from(7u64));
    }

    #[test]
    fn trading_nonce_embeds_the_epoch_in_the_high_half() {
        let n = trading_nonce(7, 42);
        assert_eq!(n >> 128, U256::from(7u64));
        assert_eq!(
            n & ((U256::from(1u8) << 128) - U256::from(1u8)),
            U256::from(42u64)
        );
    }
}
