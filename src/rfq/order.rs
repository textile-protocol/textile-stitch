// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! RFQ order assembly: the taker-binding validation payload and the
//! [`OrderParams`] a firm quote signs. The EIP-712/Permit2 digest and the
//! `abi.encode(LimitOrder)` bytes come from the existing [`crate::eip712`] and
//! [`crate::taker::encode_order_bytes`] paths — RFQ adds no second signing or
//! encoding implementation to keep in sync.

use alloy_primitives::{Address, Bytes, U256};

use crate::types::OrderParams;

/// `abi.encode(address[] preferredFillers, uint256 exclusiveUntil)` with a
/// single preferred filler — the PreferredFillerValidation payload that binds
/// the signed order to the requesting taker. `exclusive_until` covers the
/// whole order lifetime (== deadline), so there is no post-exclusivity window
/// in which a lost quote becomes open-market fillable.
pub fn encode_taker_validation(taker: Address, exclusive_until: u64) -> Bytes {
    let mut out = Vec::with_capacity(4 * 32);
    // Head: [offset to address[], exclusiveUntil].
    out.extend_from_slice(&U256::from(0x40u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(exclusive_until).to_be_bytes::<32>());
    // Tail: address[] {taker}.
    out.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
    out.extend_from_slice(&taker.into_word().0);
    Bytes::from(out)
}

/// Everything the maker commits to in one firm quote.
#[derive(Debug, Clone)]
pub struct RfqOrderSpec {
    pub reactor: Address,
    /// The funding wallet: swapper, recipient, and signer are all this one
    /// address (same-address sign + fund).
    pub maker: Address,
    pub nonce: U256,
    /// Unix seconds; must be ≤ the request's maxExpiresAt.
    pub deadline_secs: u64,
    /// What the maker pays (the request's buyToken).
    pub input_token: Address,
    pub input_amount: U256,
    /// What the maker receives (the request's sellToken).
    pub output_token: Address,
    pub output_amount: U256,
    /// The chain's PreferredFillerValidation contract.
    pub validation_contract: Address,
    /// The taker the order is bound to.
    pub taker: Address,
}

/// Assemble the [`OrderParams`] for a firm quote. The venue fee is
/// deliberately NOT an output — the reactor's controller injects it at fill
/// time; signing it too would double-charge the taker.
pub fn build_order(spec: &RfqOrderSpec) -> OrderParams {
    OrderParams {
        reactor: spec.reactor,
        swapper: spec.maker,
        nonce: spec.nonce,
        deadline: U256::from(spec.deadline_secs),
        input_token: spec.input_token,
        input_amount: spec.input_amount,
        output_token: spec.output_token,
        output_amount: spec.output_amount,
        recipient: spec.maker,
        additional_validation_contract: spec.validation_contract,
        additional_validation_data: encode_taker_validation(spec.taker, spec.deadline_secs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex};

    #[test]
    fn taker_validation_encodes_like_the_venue_abi() {
        // abi.encode(address[] {taker}, uint256 exclusiveUntil): head is the
        // array offset (0x40) then the uint, tail is [length, element].
        let taker = address!("3333333333333333333333333333333333333333");
        let data = encode_taker_validation(taker, 0x0102);
        let expected = hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000102",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000003333333333333333333333333333333333333333",
        ))
        .unwrap();
        assert_eq!(data.to_vec(), expected);
    }

    #[test]
    fn a_built_order_binds_taker_recipient_and_deadline_together() {
        let spec = RfqOrderSpec {
            reactor: address!("1111111111111111111111111111111111111111"),
            maker: address!("2222222222222222222222222222222222222222"),
            nonce: U256::from(7u64),
            deadline_secs: 1_900_000_000,
            input_token: address!("4444444444444444444444444444444444444444"),
            input_amount: U256::from(1_000u64),
            output_token: address!("5555555555555555555555555555555555555555"),
            output_amount: U256::from(2_000u64),
            validation_contract: address!("6666666666666666666666666666666666666666"),
            taker: address!("3333333333333333333333333333333333333333"),
        };
        let order = build_order(&spec);
        assert_eq!(order.swapper, spec.maker);
        assert_eq!(order.recipient, spec.maker, "output returns to the funder");
        assert_eq!(
            order.additional_validation_contract,
            spec.validation_contract
        );
        assert_eq!(
            order.additional_validation_data,
            encode_taker_validation(spec.taker, spec.deadline_secs),
            "exclusiveUntil == deadline, per spec"
        );
        assert_eq!(order.deadline, U256::from(1_900_000_000u64));

        // The encoded order is accepted by the shared encoder (smoke check
        // that RFQ orders flow through the same bytes path as taker fills).
        let bytes = crate::taker::encode_order_bytes(&order);
        assert!(bytes.len() % 32 == 0 && !bytes.is_empty());
    }
}
