// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The same EIP-712 payloads [`super::eip712`] hashes, in the *other* form some
//! signing backends need: the typed-data JSON (`types` / `domain` /
//! `primaryType` / `message`).
//!
//! Most backends take the finished 32-byte digest. A provider that refuses to
//! sign opaque bytes does its own hashing instead, so it wants the structure —
//! Fireblocks' `TYPED_MESSAGE` operation is the reason this module exists (see
//! [`crate::signer::fireblocks`]). Handing over the structure rather than a
//! digest is also the point of EIP-712: the custody provider can show the
//! operator, and enforce policy on, what is actually being signed.
//!
//! **This is a second encoding of structs that already have one**, which is a
//! drift hazard: if the JSON here disagrees with the incremental hashing in
//! `eip712.rs`, the provider signs something we did not mean to sign. Two things
//! contain that:
//!
//!  1. Every payload carries both forms and the tests below hash the JSON with a
//!     *generic* EIP-712 encoder, asserting it lands on the same digest
//!     `eip712.rs` produces. Three independent paths have to agree.
//!  2. At runtime it fails closed anyway. `finalize_signature` recovers against
//!     the digest *we* computed, so a signature over different bytes recovers to
//!     a different address and the sign errors out. Wrong loudly, never wrong
//!     silently.

use alloy_primitives::{hex, Address, B256, U256};
use serde_json::{json, Value};

use crate::protocol::attest::NavAttestation;
use crate::protocol::eip712::{
    maker_enroll_digest, maker_session_digest, nav_attestation_digest, permit2_digest,
    signer_check_digest,
};
use crate::protocol::types::OrderParams;

/// An EIP-712 payload in both forms a signer might want: the digest every
/// digest-signing backend takes, and the typed-data JSON for a backend that
/// hashes the structure itself.
///
/// Built only by the constructors below, so the two halves always describe the
/// same message — there is no way to hand back a digest and an unrelated body.
///
/// The JSON is **not** materialised up front. Only Fireblocks reads it; the
/// default [`crate::signer::Signer::sign_typed`] takes `digest()` and throws the
/// rest away. Building it eagerly cost ~200 allocations per signature — and the
/// ladder signs up to 80 orders a tick — all of it wasted for the local,
/// Turnkey and MPCVault backends. So the payload keeps the *inputs* and renders
/// on demand in [`Self::typed_data`].
///
/// Keeping the inputs rather than the rendered document also tightens the
/// invariant: the JSON is derived from exactly the values the digest was
/// computed from, so the two cannot describe different messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eip712Payload {
    digest: B256,
    body: Body,
}

/// What the payload describes, kept as inputs so the JSON can be rendered only
/// if someone asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Body {
    Permit2 {
        order: OrderParams,
        permit2: Address,
        chain_id: u64,
    },
    MakerSession {
        domain_name: String,
        maker_id: String,
        signing_address: Address,
        challenge: B256,
        issued_at_ms: u64,
    },
    MakerEnroll {
        domain_name: String,
        signing_address: Address,
        funding_wallet: Address,
        chain_id: u64,
        issued_at_ms: u64,
    },
    SignerCheck {
        nonce: B256,
    },
    NavAttestation {
        att: NavAttestation,
    },
}

impl Eip712Payload {
    /// The 32-byte digest (`0x1901 ++ domainSeparator ++ structHash`).
    pub fn digest(&self) -> B256 {
        self.digest
    }

    /// The typed-data document: `{ types, primaryType, domain, message }`.
    ///
    /// Rendered on call. Owned rather than borrowed so the one caller that
    /// wants it can move it straight into its request body instead of cloning
    /// the tree back out.
    pub fn typed_data(&self) -> Value {
        match &self.body {
            Body::Permit2 {
                order,
                permit2,
                chain_id,
            } => permit2_typed_data(order, *permit2, *chain_id),
            Body::MakerSession {
                domain_name,
                maker_id,
                signing_address,
                challenge,
                issued_at_ms,
            } => maker_session_typed_data(
                domain_name,
                maker_id,
                *signing_address,
                *challenge,
                *issued_at_ms,
            ),
            Body::MakerEnroll {
                domain_name,
                signing_address,
                funding_wallet,
                chain_id,
                issued_at_ms,
            } => maker_enroll_typed_data(
                domain_name,
                *signing_address,
                *funding_wallet,
                *chain_id,
                *issued_at_ms,
            ),
            Body::SignerCheck { nonce } => signer_check_typed_data(*nonce),
            Body::NavAttestation { att } => nav_attestation_typed_data(att),
        }
    }
}

/// `uint256` as a decimal string. JSON numbers are IEEE doubles, so a raw number
/// would silently lose precision on any amount past 2^53 — which every token
/// amount in wei is.
fn uint(v: U256) -> Value {
    json!(v.to_string())
}

/// An address as its lowercase `0x` form.
fn addr(a: Address) -> Value {
    json!(format!("{a:?}"))
}

fn bytes32(b: B256) -> Value {
    json!(format!("{b:?}"))
}

/// One `{ "name": …, "type": … }` field descriptor.
fn field(name: &str, ty: &str) -> Value {
    json!({ "name": name, "type": ty })
}

/// Typed data for the Permit2 witnessed transfer the operator signs for every
/// order — RFQ quotes and resting ladder orders alike.
///
/// The type strings in `eip712.rs` are concatenated in Permit2's stub order
/// (witness type last); canonical EIP-712 `encodeType` sorts referenced types
/// alphabetically. Those coincide here — `LimitOrder`, `OrderInfo`,
/// `OutputToken`, `TokenPermissions` is both the written order and the sorted
/// one — which is why the same typehash falls out of both. The test below is
/// what keeps that true rather than the comment.
pub fn permit2_payload(o: &OrderParams, permit2: Address, chain_id: u64) -> Eip712Payload {
    Eip712Payload {
        digest: permit2_digest(o, permit2, chain_id),
        body: Body::Permit2 {
            order: o.clone(),
            permit2,
            chain_id,
        },
    }
}

/// Render the Permit2 typed-data document. See [`permit2_payload`] for why the
/// type ordering below is the canonical one.
fn permit2_typed_data(o: &OrderParams, permit2: Address, chain_id: u64) -> Value {
    let types = json!({
        "EIP712Domain": [
            field("name", "string"),
            field("chainId", "uint256"),
            field("verifyingContract", "address"),
        ],
        "PermitWitnessTransferFrom": [
            field("permitted", "TokenPermissions"),
            field("spender", "address"),
            field("nonce", "uint256"),
            field("deadline", "uint256"),
            field("witness", "LimitOrder"),
        ],
        "TokenPermissions": [
            field("token", "address"),
            field("amount", "uint256"),
        ],
        "LimitOrder": [
            field("info", "OrderInfo"),
            field("inputToken", "address"),
            field("inputAmount", "uint256"),
            field("outputs", "OutputToken[]"),
        ],
        "OrderInfo": [
            field("reactor", "address"),
            field("swapper", "address"),
            field("nonce", "uint256"),
            field("deadline", "uint256"),
            field("additionalValidationContract", "address"),
            field("additionalValidationData", "bytes"),
        ],
        "OutputToken": [
            field("token", "address"),
            field("amount", "uint256"),
            field("recipient", "address"),
        ],
    });

    let message = json!({
        // Permit2's permitted amount is the order's input amount for a limit
        // order (no Dutch decay), matching `token_permissions_hash`.
        "permitted": {
            "token": addr(o.input_token),
            "amount": uint(o.input_amount),
        },
        // The reactor pulls the funds, so it is the Permit2 spender.
        "spender": addr(o.reactor),
        "nonce": uint(o.nonce),
        "deadline": uint(o.deadline),
        "witness": {
            "info": {
                "reactor": addr(o.reactor),
                "swapper": addr(o.swapper),
                "nonce": uint(o.nonce),
                "deadline": uint(o.deadline),
                "additionalValidationContract": addr(o.additional_validation_contract),
                "additionalValidationData": json!(hex::encode_prefixed(
                    &o.additional_validation_data
                )),
            },
            "inputToken": addr(o.input_token),
            "inputAmount": uint(o.input_amount),
            // v1 orders carry exactly one output, same as `outputs_hash`.
            "outputs": [{
                "token": addr(o.output_token),
                "amount": uint(o.output_amount),
                "recipient": addr(o.recipient),
            }],
        },
    });

    json!({
        "types": types,
        "primaryType": "PermitWitnessTransferFrom",
        "domain": {
            "name": "Permit2",
            "chainId": json!(chain_id),
            "verifyingContract": addr(permit2),
        },
        "message": message,
    })
}

/// Typed data for the venue's WebSocket session challenge.
///
/// The domain carries no `chainId` and no `verifyingContract` on purpose: this
/// authenticates a maker to an off-chain stream, not to a contract, and the
/// LIVE/TEST split rides on the domain *name*. EIP-712 allows omitting domain
/// fields as long as the `EIP712Domain` type lists exactly what is present, so
/// the type array below has to stay in lockstep with `SESSION_DOMAIN_TYPE`.
pub fn maker_session_payload(
    domain_name: &str,
    maker_id: &str,
    signing_address: Address,
    challenge: B256,
    issued_at_ms: u64,
) -> Eip712Payload {
    Eip712Payload {
        digest: maker_session_digest(
            domain_name,
            maker_id,
            signing_address,
            challenge,
            issued_at_ms,
        ),
        body: Body::MakerSession {
            domain_name: domain_name.to_string(),
            maker_id: maker_id.to_string(),
            signing_address,
            challenge,
            issued_at_ms,
        },
    }
}

/// Render the session typed data. See [`maker_session_payload`] for why the
/// domain deliberately carries no `chainId`.
fn maker_session_typed_data(
    domain_name: &str,
    maker_id: &str,
    signing_address: Address,
    challenge: B256,
    issued_at_ms: u64,
) -> Value {
    json!({
        "types": {
            "EIP712Domain": [
                field("name", "string"),
                field("version", "string"),
            ],
            "MakerSession": [
                field("makerId", "string"),
                field("signingAddress", "address"),
                field("challenge", "bytes32"),
                field("issuedAt", "uint256"),
            ],
        },
        "primaryType": "MakerSession",
        "domain": { "name": domain_name, "version": "1" },
        "message": {
            "makerId": maker_id,
            "signingAddress": addr(signing_address),
            "challenge": bytes32(challenge),
            "issuedAt": uint(U256::from(issued_at_ms)),
        },
    })
}

/// Typed data for maker enrolment. Unlike the session domain this one *does*
/// bind `chainId`, so a signature enrolling on one chain cannot enrol on
/// another, and the struct type differs from `MakerSession` so a captured
/// session challenge can never enrol.
pub fn maker_enroll_payload(
    environment: &str,
    signing_address: Address,
    funding_wallet: Address,
    chain_id: u64,
    issued_at_ms: u64,
) -> Eip712Payload {
    Eip712Payload {
        digest: maker_enroll_digest(
            environment,
            signing_address,
            funding_wallet,
            chain_id,
            issued_at_ms,
        ),
        body: Body::MakerEnroll {
            domain_name: format!("Textile Maker Enroll ({environment})"),
            signing_address,
            funding_wallet,
            chain_id,
            issued_at_ms,
        },
    }
}

/// Render the enrolment typed data. Unlike the session domain this one binds
/// `chainId`; see [`maker_enroll_payload`].
fn maker_enroll_typed_data(
    domain_name: &str,
    signing_address: Address,
    funding_wallet: Address,
    chain_id: u64,
    issued_at_ms: u64,
) -> Value {
    json!({
        "types": {
            "EIP712Domain": [
                field("name", "string"),
                field("version", "string"),
                field("chainId", "uint256"),
            ],
            "MakerEnroll": [
                field("signingAddress", "address"),
                field("fundingWallet", "address"),
                field("chainId", "uint256"),
                field("issuedAt", "uint256"),
            ],
        },
        "primaryType": "MakerEnroll",
        "domain": {
            "name": domain_name,
            "version": "1",
            "chainId": json!(chain_id),
        },
        "message": {
            "signingAddress": addr(signing_address),
            "fundingWallet": addr(funding_wallet),
            "chainId": uint(U256::from(chain_id)),
            "issuedAt": uint(U256::from(issued_at_ms)),
        },
    })
}

/// A throwaway payload for proving a signer works, used by the panel's Verify
/// button and nothing else.
///
/// It signs under its own domain (`Stitch Signer Check`) with its own struct, so
/// the resulting signature authorises nothing: no contract verifies it and the
/// venue does not know the domain. That matters because Verify is offered before
/// the operator has committed to anything — it must not be a way to extract a
/// signature that means something elsewhere.
/// Typed data for countersigning a vault NAV attestation: the exact struct
/// the risk signer signed, under the vault's own domain. See
/// `protocol::attest` for what the bot checks before it signs this.
pub fn nav_attestation_payload(att: &NavAttestation) -> Eip712Payload {
    Eip712Payload {
        digest: nav_attestation_digest(att),
        body: Body::NavAttestation { att: att.clone() },
    }
}

fn nav_attestation_typed_data(a: &NavAttestation) -> Value {
    json!({
        "types": {
            "EIP712Domain": [
                field("name", "string"),
                field("version", "string"),
                field("chainId", "uint256"),
                field("verifyingContract", "address"),
            ],
            "NavAttestation": [
                field("vault", "address"),
                field("chainId", "uint256"),
                field("epochId", "uint256"),
                field("corridorAssetPrice", "uint256"),
                field("nav", "uint256"),
                field("lastSettledNav", "uint256"),
                field("freeSettlement", "uint256"),
                field("freeCorridor", "uint256"),
                field("validAfter", "uint256"),
                field("validUntil", "uint256"),
            ],
        },
        "primaryType": "NavAttestation",
        "domain": {
            "name": "OperatorVault",
            "version": "1",
            "chainId": uint(a.chain_id),
            "verifyingContract": addr(a.vault),
        },
        "message": {
            "vault": addr(a.vault),
            "chainId": uint(a.chain_id),
            "epochId": uint(a.epoch_id),
            "corridorAssetPrice": uint(a.corridor_asset_price),
            "nav": uint(a.nav),
            "lastSettledNav": uint(a.last_settled_nav),
            "freeSettlement": uint(a.free_settlement),
            "freeCorridor": uint(a.free_corridor),
            "validAfter": uint(a.valid_after),
            "validUntil": uint(a.valid_until),
        },
    })
}

pub fn signer_check_payload(nonce: B256) -> Eip712Payload {
    Eip712Payload {
        digest: signer_check_digest(nonce),
        body: Body::SignerCheck { nonce },
    }
}

/// Render the connectivity-check typed data. See [`signer_check_payload`] for
/// why it has its own domain.
fn signer_check_typed_data(nonce: B256) -> Value {
    json!({
            "types": {
                "EIP712Domain": [
                    field("name", "string"),
                    field("version", "string"),
                ],
                "SignerCheck": [field("nonce", "bytes32")],
            },
            "primaryType": "SignerCheck",
            "domain": { "name": "Stitch Signer Check", "version": "1" },
            "message": { "nonce": bytes32(nonce) },
    })
}

/// A generic EIP-712 encoder, used only to check the payloads above.
///
/// Deliberately written from the spec rather than from `eip712.rs`: it walks the
/// `types` map the way a wallet (or Fireblocks) does, so agreeing with the
/// hand-rolled incremental hashing is evidence, not tautology. If this and
/// `eip712.rs` ever disagree, one of them is wrong and a test says so before an
/// operator finds out.
#[cfg(test)]
pub(crate) mod verify {
    use super::*;
    use alloy_primitives::keccak256;
    use std::collections::BTreeSet;

    /// `Name(type1 field1,type2 field2)` for one struct.
    fn encode_one(name: &str, types: &Value) -> String {
        let fields = types[name]
            .as_array()
            .unwrap_or_else(|| panic!("type {name} is not an array"))
            .iter()
            .map(|f| {
                format!(
                    "{} {}",
                    f["type"].as_str().expect("field type"),
                    f["name"].as_str().expect("field name")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{name}({fields})")
    }

    /// Every struct type `name` refers to, transitively, excluding itself.
    fn deps(name: &str, types: &Value, out: &mut BTreeSet<String>) {
        let Some(fields) = types[name].as_array() else {
            return;
        };
        for f in fields {
            let ty = f["type"].as_str().expect("field type");
            let base = ty.trim_end_matches("[]");
            // A struct is exactly a type that appears in the `types` map.
            if types.get(base).is_some() && !out.contains(base) {
                out.insert(base.to_string());
                deps(base, types, out);
            }
        }
    }

    /// `encodeType`: the primary struct first, then its dependencies sorted by
    /// name (`BTreeSet` gives the ordering for free).
    fn encode_type(primary: &str, types: &Value) -> String {
        let mut set = BTreeSet::new();
        deps(primary, types, &mut set);
        set.remove(primary);
        let mut out = encode_one(primary, types);
        for dep in &set {
            out.push_str(&encode_one(dep, types));
        }
        out
    }

    fn type_hash(primary: &str, types: &Value) -> B256 {
        keccak256(encode_type(primary, types).as_bytes())
    }

    /// One 32-byte `encodeData` word for a value of declared type `ty`.
    fn encode_value(ty: &str, v: &Value, types: &Value) -> [u8; 32] {
        if let Some(inner) = ty.strip_suffix("[]") {
            let items = v.as_array().expect("array value");
            let mut buf = Vec::with_capacity(items.len() * 32);
            for item in items {
                buf.extend_from_slice(&encode_value(inner, item, types));
            }
            return keccak256(&buf).0;
        }
        match ty {
            "address" => {
                v.as_str()
                    .expect("address string")
                    .parse::<Address>()
                    .expect("valid address")
                    .into_word()
                    .0
            }
            "uint256" => {
                // Accept both the decimal-string form the payloads use and a
                // bare JSON number, so a hand-written fixture can use either.
                let u = match v {
                    Value::String(s) => s.parse::<U256>().expect("decimal uint256"),
                    Value::Number(n) => U256::from(n.as_u64().expect("uint256 fits u64")),
                    other => panic!("uint256 must be a string or number, got {other}"),
                };
                u.to_be_bytes::<32>()
            }
            "bytes32" => {
                v.as_str()
                    .expect("bytes32 string")
                    .parse::<B256>()
                    .expect("valid bytes32")
                    .0
            }
            "string" => keccak256(v.as_str().expect("string value").as_bytes()).0,
            "bytes" => {
                let s = v.as_str().expect("bytes string");
                let raw = hex::decode(s.strip_prefix("0x").unwrap_or(s)).expect("hex bytes");
                keccak256(&raw).0
            }
            struct_name => hash_struct(struct_name, v, types).0,
        }
    }

    /// `hashStruct(s) = keccak256(typeHash ‖ encodeData(s))`.
    fn hash_struct(primary: &str, value: &Value, types: &Value) -> B256 {
        let mut buf = Vec::new();
        buf.extend_from_slice(&type_hash(primary, types).0);
        for f in types[primary].as_array().expect("struct fields") {
            let name = f["name"].as_str().expect("field name");
            let ty = f["type"].as_str().expect("field type");
            buf.extend_from_slice(&encode_value(ty, &value[name], types));
        }
        keccak256(&buf)
    }

    /// The full digest of a typed-data document, per EIP-712.
    pub(crate) fn digest_of(td: &Value) -> B256 {
        let types = &td["types"];
        let primary = td["primaryType"].as_str().expect("primaryType");
        let mut buf = Vec::with_capacity(66);
        buf.extend_from_slice(&[0x19, 0x01]);
        buf.extend_from_slice(&hash_struct("EIP712Domain", &td["domain"], types).0);
        buf.extend_from_slice(&hash_struct(primary, &td["message"], types).0);
        keccak256(&buf)
    }
}

#[cfg(test)]
mod tests {
    use super::verify::digest_of;
    use super::*;
    use alloy_primitives::{address, b256, Bytes};

    const PERMIT2: Address = address!("000000000022d473030f116ddee9f6b43ac78ba3");

    fn sample() -> OrderParams {
        OrderParams {
            reactor: address!("1111111111111111111111111111111111111111"),
            swapper: address!("2222222222222222222222222222222222222222"),
            nonce: U256::from(7u64),
            deadline: U256::from(1_900_000_000u64),
            input_token: address!("3333333333333333333333333333333333333333"),
            input_amount: U256::from(1_000_000u64),
            output_token: address!("4444444444444444444444444444444444444444"),
            output_amount: U256::from(1_550_000_000u64),
            recipient: address!("2222222222222222222222222222222222222222"),
            additional_validation_contract: Address::ZERO,
            additional_validation_data: Default::default(),
        }
    }

    /// The whole reason this module can be trusted: the typed-data JSON, hashed
    /// by a generic EIP-712 encoder, lands on exactly the digest the bot's own
    /// incremental hashing produces. Break either encoding and this fails.
    /// Pinned to `navAttestationDigest` in `@textile/constants` (viem
    /// `hashTypedData`), which the Hardhat parity test pins to `VaultLib`.
    #[test]
    fn nav_attestation_typed_data_hashes_to_the_vault_digest() {
        let att = NavAttestation {
            vault: address!("2222222222222222222222222222222222222222"),
            chain_id: U256::from(8453u64),
            epoch_id: U256::from(7u64),
            corridor_asset_price: U256::from(1_500_000_000_000_000_000u128),
            nav: U256::from(16_000_000u64),
            last_settled_nav: U256::from(12_345u64),
            free_settlement: U256::from(10_000_000u64),
            free_corridor: U256::from(4_000_000_000_000_000_000u128),
            valid_after: U256::from(1_700_000_000u64),
            valid_until: U256::from(1_700_003_600u64),
        };
        let expected = b256!("38120dd57a44efe427559d8f5f94a600ff44b7df87ca0a832a81ef0954e01985");
        let payload = nav_attestation_payload(&att);
        assert_eq!(payload.digest(), expected);
        assert_eq!(digest_of(&payload.typed_data()), expected);
    }

    #[test]
    fn permit2_typed_data_hashes_to_the_same_digest() {
        let order = sample();
        for chain_id in [1u64, 56, 8453, 42220] {
            let payload = permit2_payload(&order, PERMIT2, chain_id);
            assert_eq!(
                payload.digest(),
                permit2_digest(&order, PERMIT2, chain_id),
                "payload digest must match the canonical one (chain {chain_id})"
            );
            assert_eq!(
                digest_of(&payload.typed_data()),
                permit2_digest(&order, PERMIT2, chain_id),
                "typed-data JSON must hash to the canonical digest (chain {chain_id})"
            );
        }
    }

    /// Non-empty `additionalValidationData` exercises the `bytes` branch, which
    /// an all-zero order never does.
    #[test]
    fn permit2_typed_data_handles_validation_callback_data() {
        let order = OrderParams {
            additional_validation_contract: address!("5555555555555555555555555555555555555555"),
            additional_validation_data: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
            ..sample()
        };
        let payload = permit2_payload(&order, PERMIT2, 8453);
        assert_eq!(
            digest_of(&payload.typed_data()),
            permit2_digest(&order, PERMIT2, 8453)
        );
    }

    /// Large amounts are where a JSON number would quietly lose precision, so
    /// pin the decimal-string encoding with a value past 2^53.
    #[test]
    fn permit2_typed_data_keeps_full_uint256_precision() {
        let big = U256::from(2u64).pow(U256::from(200u64)) + U256::from(12345u64);
        let order = OrderParams {
            input_amount: big,
            output_amount: big - U256::from(1u64),
            nonce: big,
            ..sample()
        };
        let payload = permit2_payload(&order, PERMIT2, 8453);
        assert_eq!(
            payload.typed_data()["message"]["permitted"]["amount"],
            serde_json::json!(big.to_string()),
            "amounts must be decimal strings, not JSON numbers"
        );
        assert_eq!(
            digest_of(&payload.typed_data()),
            permit2_digest(&order, PERMIT2, 8453)
        );
    }

    #[test]
    fn maker_session_typed_data_hashes_to_the_same_digest() {
        let signer = address!("2222222222222222222222222222222222222222");
        let challenge = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        for domain in [
            "Textile Maker Session (LIVE)",
            "Textile Maker Session (TEST)",
        ] {
            let payload =
                maker_session_payload(domain, "maker-abc", signer, challenge, 1_754_388_000_000);
            assert_eq!(
                payload.digest(),
                maker_session_digest(domain, "maker-abc", signer, challenge, 1_754_388_000_000)
            );
            assert_eq!(
                digest_of(&payload.typed_data()),
                payload.digest(),
                "session typed data must hash to the canonical digest ({domain})"
            );
        }
    }

    /// The LIVE/TEST split rides on the domain name alone, so the two payloads
    /// must not collide — the same property `eip712.rs` asserts for the digests.
    #[test]
    fn maker_session_typed_data_separates_live_from_test() {
        let signer = address!("2222222222222222222222222222222222222222");
        let challenge = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        let live = maker_session_payload(
            "Textile Maker Session (LIVE)",
            "m",
            signer,
            challenge,
            1_754_388_000_000,
        );
        let test = maker_session_payload(
            "Textile Maker Session (TEST)",
            "m",
            signer,
            challenge,
            1_754_388_000_000,
        );
        assert_ne!(live.digest(), test.digest());
    }

    #[test]
    fn maker_enroll_typed_data_hashes_to_the_same_digest() {
        let signer = address!("2222222222222222222222222222222222222222");
        let funding = address!("6666666666666666666666666666666666666666");
        for (env, chain_id) in [("LIVE", 56u64), ("TEST", 97u64), ("LIVE", 8453)] {
            let payload = maker_enroll_payload(env, signer, funding, chain_id, 1_754_388_000_000);
            assert_eq!(
                payload.digest(),
                maker_enroll_digest(env, signer, funding, chain_id, 1_754_388_000_000)
            );
            assert_eq!(
                digest_of(&payload.typed_data()),
                payload.digest(),
                "enroll typed data must hash to the canonical digest ({env}/{chain_id})"
            );
        }
    }

    /// Permit2's stub concatenates the witness type last; canonical EIP-712
    /// sorts referenced types alphabetically. They coincide for this struct —
    /// which is exactly why the typed-data path works at all. Pin it, because
    /// renaming any struct could silently break the coincidence.
    #[test]
    fn permit2_encode_type_matches_the_canonical_ordering() {
        let payload = permit2_payload(&sample(), PERMIT2, 8453);
        let td = payload.typed_data();
        let types = &td["types"];
        let names: Vec<&str> = types["PermitWitnessTransferFrom"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "TokenPermissions",
                "address",
                "uint256",
                "uint256",
                "LimitOrder"
            ],
            "field order is part of the typehash and must match Permit2's stub"
        );
    }

    #[test]
    fn signer_check_typed_data_hashes_to_the_same_digest() {
        let nonce = b256!("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20");
        let payload = signer_check_payload(nonce);
        assert_eq!(digest_of(&payload.typed_data()), payload.digest());
    }

    /// The check payload must not collide with anything the bot really signs,
    /// or a "just testing" signature would authorise something.
    #[test]
    fn signer_check_cannot_collide_with_a_real_payload() {
        let nonce = b256!("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20");
        let check = signer_check_payload(nonce);
        let session =
            maker_session_payload("Textile Maker Session (LIVE)", "m", Address::ZERO, nonce, 1);
        assert_ne!(check.digest(), session.digest());
        assert_ne!(
            check.typed_data()["domain"]["name"],
            session.typed_data()["domain"]["name"]
        );
    }

    /// The domain type array has to list exactly the fields the domain carries;
    /// an extra or missing entry changes the separator.
    #[test]
    fn domain_types_match_the_domains_they_describe() {
        let session = maker_session_payload(
            "Textile Maker Session (LIVE)",
            "m",
            Address::ZERO,
            B256::ZERO,
            1,
        );
        let td = session.typed_data();
        let declared: Vec<&str> = td["types"]["EIP712Domain"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        let present: Vec<&str> = td["domain"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(declared, present, "session domain: declared vs present");
        assert!(
            !present.contains(&"chainId"),
            "the session domain must not bind a chain"
        );
    }
}
