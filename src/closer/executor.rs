// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Blue-leg on-chain calldata: closing positions via the FIFO
//! `SettlementPool.fill(maxDebtIn, maxPositions)`, plus the ERC20 `approve` the
//! filler needs first. Encoding only — the actual tx submission (provider +
//! signing + nonce/gas) wires on top of this.

use alloy_primitives::{keccak256, Address, U256};

fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes()).0;
    [h[0], h[1], h[2], h[3]]
}

/// Calldata for `SettlementPool.fill(uint256 maxDebtIn, uint256 maxPositions)`.
pub fn encode_fill(max_debt_in: U256, max_positions: U256) -> Vec<u8> {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&selector("fill(uint256,uint256)"));
    data.extend_from_slice(&max_debt_in.to_be_bytes::<32>());
    data.extend_from_slice(&max_positions.to_be_bytes::<32>());
    data
}

/// Calldata for ERC20 `approve(address spender, uint256 amount)`.
pub fn encode_approve(spender: Address, amount: U256) -> Vec<u8> {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&selector("approve(address,uint256)"));
    data.extend_from_slice(&spender.into_word().0); // address right-aligned in 32 bytes
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    data
}

/// Calldata for ERC20 `allowance(address owner, address spender)` (a read).
pub fn encode_allowance(owner: Address, spender: Address) -> Vec<u8> {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&selector("allowance(address,address)"));
    data.extend_from_slice(&owner.into_word().0);
    data.extend_from_slice(&spender.into_word().0);
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex};

    #[test]
    fn fill_has_the_right_selector_and_args() {
        let data = encode_fill(U256::from(1_000_000u64), U256::from(3u64));
        assert_eq!(&data[..4], &hex::decode("3c29fc43").unwrap()[..]);
        assert_eq!(data.len(), 68);
        assert_eq!(data[67], 3); // maxPositions in the last byte of the 2nd word
    }

    #[test]
    fn approve_has_the_right_selector_and_right_aligned_spender() {
        let spender = address!("00000000000000000000000000000000000000aa");
        let data = encode_approve(spender, U256::from(5u64));
        assert_eq!(&data[..4], &hex::decode("095ea7b3").unwrap()[..]);
        assert_eq!(&data[4 + 12..4 + 32], spender.as_slice());
        assert_eq!(data[67], 5);
    }

    #[test]
    fn allowance_has_the_right_selector_and_two_addresses() {
        let owner = address!("00000000000000000000000000000000000000bb");
        let spender = address!("00000000000000000000000000000000000000cc");
        let data = encode_allowance(owner, spender);
        assert_eq!(&data[..4], &hex::decode("dd62ed3e").unwrap()[..]);
        assert_eq!(data.len(), 68);
        assert_eq!(&data[4 + 12..4 + 32], owner.as_slice());
        assert_eq!(&data[36 + 12..36 + 32], spender.as_slice());
    }
}
