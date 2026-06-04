// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Turn a signed [`OrderParams`] into the payload the indexer's
//! `submitFillerOrder` mutation expects.

use alloy_primitives::{hex, Address};
use k256::ecdsa::SigningKey;
use serde::Serialize;

use crate::eip712::permit2_digest;
use crate::signer::sign_digest;
use crate::types::OrderParams;

/// Wire form of a signed operator order (addresses as `0x` strings, amounts as
/// decimal strings to fit the GraphQL `BigInt` scalar).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubmitOrder {
    pub chain_id: u64,
    pub client_order_id: Option<String>,
    pub reactor: String,
    pub maker: String,
    pub input_token: String,
    pub input_amount: String,
    pub output_token: String,
    pub output_amount: String,
    pub recipient: String,
    pub nonce: String,
    pub deadline: String,
    pub signature: String,
}

/// Sign the Permit2 witness digest and package the order for the indexer.
pub fn sign_submission(
    order: &OrderParams,
    permit2: Address,
    chain_id: u64,
    key: &SigningKey,
) -> anyhow::Result<SubmitOrder> {
    let digest = permit2_digest(order, permit2, chain_id);
    let sig = sign_digest(key, digest)?;
    Ok(SubmitOrder {
        chain_id,
        client_order_id: None,
        reactor: order.reactor.to_string(),
        maker: order.swapper.to_string(),
        input_token: order.input_token.to_string(),
        input_amount: order.input_amount.to_string(),
        output_token: order.output_token.to_string(),
        output_amount: order.output_amount.to_string(),
        recipient: order.recipient.to_string(),
        nonce: order.nonce.to_string(),
        deadline: order.deadline.to_string(),
        signature: hex::encode_prefixed(sig),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{address_from_signing_key, recover_address};
    use alloy_primitives::{address, B256, U256};

    const KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const PERMIT2: Address = address!("000000000022d473030f116ddee9f6b43ac78ba3");

    #[test]
    fn produces_a_signature_that_recovers_to_the_maker() {
        let key = SigningKey::from_slice(&hex::decode(KEY).unwrap()).unwrap();
        let maker = address_from_signing_key(&key);
        let order = OrderParams {
            reactor: address!("1111111111111111111111111111111111111111"),
            swapper: maker,
            nonce: U256::from(42u64),
            deadline: U256::from(1_900_000_000u64),
            input_token: address!("3333333333333333333333333333333333333333"),
            input_amount: U256::from(1_000_000u64),
            output_token: address!("4444444444444444444444444444444444444444"),
            output_amount: U256::from(1_550_000_000u64),
            recipient: maker,
        };

        let s = sign_submission(&order, PERMIT2, 8453, &key).unwrap();

        assert_eq!(s.input_amount, "1000000");
        assert_eq!(s.output_amount, "1550000000");
        assert_eq!(s.client_order_id, None);
        assert!(s.signature.starts_with("0x"));

        // The signature recovers to the maker over the same digest.
        let sig_bytes: [u8; 65] = hex::decode(&s.signature).unwrap().try_into().unwrap();
        let digest: B256 = permit2_digest(&order, PERMIT2, 8453);
        assert_eq!(recover_address(digest, &sig_bytes).unwrap(), maker);
    }
}
