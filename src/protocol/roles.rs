// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Zodiac Roles Modifier (v2) calldata.
//!
//! An operator-admin Safe with the Roles module enabled can let one address
//! call one function on one contract. The bot uses it to move the vault's
//! liquid floor without holding the operator-admin key: it calls
//! `execTransactionWithRole`, the modifier checks the role's scope, and the
//! Safe makes the call, so the vault sees the Safe as `msg.sender`.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::SolValue;

/// `Enum.Operation.Call`. Never `DelegateCall`: the scope is a plain call.
/// A `uint8` argument is one left-padded word, the same as a `uint256`.
const OPERATION_CALL: U256 = U256::ZERO;

fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes()).0;
    [h[0], h[1], h[2], h[3]]
}

/// `target()` — the account the modifier executes through, normally the Safe.
pub fn encode_target() -> Vec<u8> {
    selector("target()").to_vec()
}

/// `execTransactionWithRole(to, 0, data, Call, roleKey, true)`.
///
/// `shouldRevert = true` makes a refused or failed inner call revert the whole
/// transaction, so gas estimation fails before anything is broadcast instead
/// of landing a transaction that returns `false`.
pub fn encode_exec_transaction_with_role(to: Address, data: &[u8], role_key: B256) -> Vec<u8> {
    let args = (
        to,
        U256::ZERO,
        Bytes::copy_from_slice(data),
        OPERATION_CALL,
        role_key,
        true,
    );
    [
        selector("execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)").to_vec(),
        args.abi_encode_params(),
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex};

    #[test]
    fn target_selector_matches_the_modifier_abi() {
        assert_eq!(hex::encode(encode_target()), "d4b83992");
    }

    /// Pinned against viem's `encodeFunctionData` over the Roles v2 ABI.
    #[test]
    fn exec_transaction_with_role_matches_viem() {
        let inner =
            hex::decode("5ad4f0f60000000000000000000000000000000000000000000000000000000000000064")
                .unwrap();
        let encoded = encode_exec_transaction_with_role(
            address!("00000000000000000000000000000000000000aa"),
            &inner,
            B256::repeat_byte(0x11),
        );
        assert_eq!(
            hex::encode(encoded),
            "c6fe8747\
             00000000000000000000000000000000000000000000000000000000000000aa\
             0000000000000000000000000000000000000000000000000000000000000000\
             00000000000000000000000000000000000000000000000000000000000000c0\
             0000000000000000000000000000000000000000000000000000000000000000\
             1111111111111111111111111111111111111111111111111111111111111111\
             0000000000000000000000000000000000000000000000000000000000000001\
             0000000000000000000000000000000000000000000000000000000000000024\
             5ad4f0f600000000000000000000000000000000000000000000000000000000\
             0000006400000000000000000000000000000000000000000000000000000000"
        );
    }
}
