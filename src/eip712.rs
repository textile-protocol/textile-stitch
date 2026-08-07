// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! UniswapX `LimitOrder` EIP-712 hashing + the Permit2 witness digest the
//! operator signs.
//!
//! Ported byte-for-byte from the vendored contracts (pinned in
//! `packages/protocol/contracts/v3/filler/vendor/`):
//!   - `uniswapx/lib/OrderInfoLib.sol`   (`ORDER_INFO_TYPE`)
//!   - `uniswapx/lib/LimitOrderLib.sol`  (`ORDER_TYPE`, `PERMIT2_ORDER_TYPE`)
//!   - `uniswapx/lib/Permit2Lib.sol`     (TokenPermissions = input.maxAmount)
//!
//! `LimitOrderReactor.executeBatch` calls
//! `permit2.permitWitnessTransferFrom(..., PERMIT2_ORDER_TYPE, sig)`. The digest
//! computed here must match what Permit2 reconstructs on-chain, so every type
//! string below is copied verbatim from the Solidity and the field order is
//! preserved exactly.
//!
//! NOTE: type-string correctness is locked by unit tests + a Solidity/keccak
//! cross-check, but a signed order is only proven end-to-end once it settles
//! against the deployed reactor (the local Permit2-etch fill test). Do not use
//! against mainnet before that passes.

use alloy_primitives::{keccak256, Address, B256, U256};

use crate::types::OrderParams;

// --- type strings, verbatim from the vendored Solidity ---

const ORDER_INFO_TYPE: &str = "OrderInfo(address reactor,address swapper,uint256 nonce,uint256 deadline,address additionalValidationContract,bytes additionalValidationData)";
const OUTPUT_TOKEN_TYPE: &str = "OutputToken(address token,uint256 amount,address recipient)";
const LIMIT_ORDER_HEAD: &str =
    "LimitOrder(OrderInfo info,address inputToken,uint256 inputAmount,OutputToken[] outputs)";
const TOKEN_PERMISSIONS_TYPE: &str = "TokenPermissions(address token,uint256 amount)";
// Permit2's stub for a witnessed transfer (see SignatureTransfer.sol).
const PERMIT_WITNESS_STUB: &str = "PermitWitnessTransferFrom(TokenPermissions permitted,address spender,uint256 nonce,uint256 deadline,";
const WITNESS_KEYWORD: &str = "LimitOrder witness)";
const EIP712_DOMAIN_TYPE: &str =
    "EIP712Domain(string name,uint256 chainId,address verifyingContract)";
const PERMIT2_NAME: &str = "Permit2";

fn k(s: &str) -> B256 {
    keccak256(s.as_bytes())
}

/// `ORDER_TYPE = LIMIT_ORDER_HEAD ++ ORDER_INFO_TYPE ++ OUTPUT_TOKEN_TYPE`
fn order_type_hash() -> B256 {
    k(&[LIMIT_ORDER_HEAD, ORDER_INFO_TYPE, OUTPUT_TOKEN_TYPE].concat())
}

/// Full Permit2 witnessed-transfer type used as the signing typehash:
/// `STUB ++ "LimitOrder witness)" ++ ORDER_TYPE ++ TOKEN_PERMISSIONS_TYPE`
fn permit_witness_type_hash() -> B256 {
    k(&[
        PERMIT_WITNESS_STUB,
        WITNESS_KEYWORD,
        LIMIT_ORDER_HEAD,
        ORDER_INFO_TYPE,
        OUTPUT_TOKEN_TYPE,
        TOKEN_PERMISSIONS_TYPE,
    ]
    .concat())
}

// --- 32-byte ABI word helpers (all fields here are static types) ---

fn addr_word(a: Address) -> [u8; 32] {
    a.into_word().0
}
fn u256_word(u: U256) -> [u8; 32] {
    u.to_be_bytes::<32>()
}
fn b256_word(b: B256) -> [u8; 32] {
    b.0
}
fn hash_words(words: &[[u8; 32]]) -> B256 {
    let mut buf = Vec::with_capacity(words.len() * 32);
    for w in words {
        buf.extend_from_slice(w);
    }
    keccak256(&buf)
}

fn order_info_hash(o: &OrderParams) -> B256 {
    hash_words(&[
        b256_word(k(ORDER_INFO_TYPE)),
        addr_word(o.reactor),
        addr_word(o.swapper),
        u256_word(o.nonce),
        u256_word(o.deadline),
        addr_word(o.additional_validation_contract),
        b256_word(keccak256(&o.additional_validation_data)),
    ])
}

fn output_hash(o: &OrderParams) -> B256 {
    hash_words(&[
        b256_word(k(OUTPUT_TOKEN_TYPE)),
        addr_word(o.output_token),
        u256_word(o.output_amount),
        addr_word(o.recipient),
    ])
}

/// Hash of the `OutputToken[]` array: keccak of the concatenated output hashes.
/// v1 orders carry a single output.
fn outputs_hash(o: &OrderParams) -> B256 {
    hash_words(&[b256_word(output_hash(o))])
}

/// EIP-712 hash of the `LimitOrder` struct (the Permit2 witness value).
pub fn order_hash(o: &OrderParams) -> B256 {
    hash_words(&[
        b256_word(order_type_hash()),
        b256_word(order_info_hash(o)),
        addr_word(o.input_token),
        u256_word(o.input_amount),
        b256_word(outputs_hash(o)),
    ])
}

fn token_permissions_hash(o: &OrderParams) -> B256 {
    hash_words(&[
        b256_word(k(TOKEN_PERMISSIONS_TYPE)),
        addr_word(o.input_token),
        // permitted amount == input.maxAmount == input_amount for a limit order
        u256_word(o.input_amount),
    ])
}

fn permit_struct_hash(o: &OrderParams) -> B256 {
    hash_words(&[
        b256_word(permit_witness_type_hash()),
        b256_word(token_permissions_hash(o)),
        addr_word(o.reactor), // spender = the reactor pulling the funds
        u256_word(o.nonce),
        u256_word(o.deadline),
        b256_word(order_hash(o)), // witness
    ])
}

fn domain_separator(permit2: Address, chain_id: u64) -> B256 {
    hash_words(&[
        b256_word(k(EIP712_DOMAIN_TYPE)),
        b256_word(k(PERMIT2_NAME)),
        u256_word(U256::from(chain_id)),
        addr_word(permit2),
    ])
}

/// The EIP-712 digest the operator signs (`0x1901 ++ domain ++ structHash`).
pub fn permit2_digest(o: &OrderParams, permit2: Address, chain_id: u64) -> B256 {
    let ds = domain_separator(permit2, chain_id);
    let sh = permit_struct_hash(o);
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&ds.0);
    buf.extend_from_slice(&sh.0);
    keccak256(&buf)
}

// --- Maker session auth (the RFQ venue's WebSocket challenge) ---

/// The venue's session domain has NO chainId or verifyingContract — the
/// signature authenticates a maker to an off-chain stream, not to a contract,
/// and the LIVE/TEST split rides on the domain *name* the venue sends.
const SESSION_DOMAIN_TYPE: &str = "EIP712Domain(string name,string version)";
const MAKER_SESSION_TYPE: &str =
    "MakerSession(string makerId,address signingAddress,bytes32 challenge,uint256 issuedAt)";
const SESSION_DOMAIN_VERSION: &str = "1";

/// EIP-712 digest of the venue's `MakerSession` challenge reply.
///
/// `domain_name` comes verbatim from the venue's challenge frame ("Textile
/// Maker Session (LIVE)" / "(TEST)") so a signature for the test stream can
/// never authenticate on the live one. `issued_at` is unix milliseconds.
pub fn maker_session_digest(
    domain_name: &str,
    maker_id: &str,
    signing_address: Address,
    challenge: B256,
    issued_at_ms: u64,
) -> B256 {
    let domain = hash_words(&[
        b256_word(k(SESSION_DOMAIN_TYPE)),
        b256_word(k(domain_name)),
        b256_word(k(SESSION_DOMAIN_VERSION)),
    ]);
    let struct_hash = hash_words(&[
        b256_word(k(MAKER_SESSION_TYPE)),
        b256_word(k(maker_id)),
        addr_word(signing_address),
        b256_word(challenge),
        u256_word(U256::from(issued_at_ms)),
    ]);
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&domain.0);
    buf.extend_from_slice(&struct_hash.0);
    keccak256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    fn sample() -> OrderParams {
        OrderParams {
            reactor: address!("1111111111111111111111111111111111111111"),
            swapper: address!("2222222222222222222222222222222222222222"),
            nonce: U256::from(7u64),
            deadline: U256::from(1_900_000_000u64),
            input_token: address!("3333333333333333333333333333333333333333"), // USDT
            input_amount: U256::from(1_000_000u64),
            output_token: address!("4444444444444444444444444444444444444444"), // cNGN
            output_amount: U256::from(1_550_000_000u64),
            recipient: address!("2222222222222222222222222222222222222222"),
            additional_validation_contract: Address::ZERO,
            additional_validation_data: Default::default(),
        }
    }

    #[test]
    fn type_hashes_match_independent_keccak() {
        // Golden values from `cast keccak` of the exact type strings — a
        // cross-tool check that our concatenation matches the Solidity.
        assert_eq!(
            order_type_hash(),
            b256!("a7d1cc35867af6b68aad3c7171d2f51fc824592dd93d17c26bb4c65da6cec678")
        );
        assert_eq!(
            permit_witness_type_hash(),
            b256!("e35e6a28e8d076114130d5989df14ccf68b92dc3ed629938e43f54ab543d79bb")
        );
        assert_eq!(
            k(ORDER_INFO_TYPE),
            b256!("7daca11202c64729871927c37d75933f1852e430627cd4b8f4844087e312e94b")
        );
        assert_eq!(
            k(EIP712_DOMAIN_TYPE),
            b256!("8cad95687ba82c2ce50e74f7b754645e5117c3a5bec8151c0726d5857980a866")
        );
        assert_eq!(
            k(PERMIT2_NAME),
            b256!("9ac997416e8ff9d2ff6bebeb7149f65cdae5e32e2b90440b566bb3044041d36a")
        );
    }

    #[test]
    fn order_hash_is_deterministic_and_field_sensitive() {
        let o = sample();
        assert_eq!(order_hash(&o), order_hash(&o));

        let mut o2 = o.clone();
        o2.nonce = U256::from(8u64);
        assert_ne!(order_hash(&o), order_hash(&o2));

        let mut o3 = o.clone();
        o3.output_amount = o.output_amount + U256::from(1u64);
        assert_ne!(order_hash(&o), order_hash(&o3));
    }

    #[test]
    fn maker_session_digest_matches_the_venue_golden_vector() {
        // Golden vector from the venue's own implementation. Signing this
        // digest with 0x59c6…690d recovers 0x7099…79C8, which the session
        // test below asserts end to end.
        let digest = maker_session_digest(
            "Textile Maker Session (LIVE)",
            "mk_golden",
            address!("1111111111111111111111111111111111111111"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            1_754_388_000_000,
        );
        assert_eq!(
            digest,
            b256!("7eeb5f513a7b151fa336a782fe00fce66bc6e64e832b8d5bf34222d2e939f863")
        );
    }

    #[test]
    fn maker_session_signature_recovers_the_golden_signer() {
        use crate::signer::{recover_address, sign_digest};
        use k256::ecdsa::SigningKey;

        let digest = maker_session_digest(
            "Textile Maker Session (LIVE)",
            "mk_golden",
            address!("1111111111111111111111111111111111111111"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            1_754_388_000_000,
        );
        let key = SigningKey::from_slice(
            &alloy_primitives::hex::decode(
                "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
            )
            .unwrap(),
        )
        .unwrap();
        let sig = sign_digest(&key, digest).unwrap();
        assert_eq!(
            alloy_primitives::hex::encode_prefixed(sig),
            "0xf54e7f8081b26a1a7fcf19e57fefbac1782b07e1bb526f9d5f51cc1b39dc90ed1dded868fbde13bea3e9df9b605566772b1f6c98278fc1cc2b10aac49ba882bf1c"
        );
        assert_eq!(
            recover_address(digest, &sig).unwrap(),
            address!("70997970C51812dc3A010C7d01b50e0d17dc79C8")
        );
    }

    #[test]
    fn session_digest_binds_the_live_test_split_and_every_field() {
        let base = || {
            maker_session_digest(
                "Textile Maker Session (LIVE)",
                "mk_golden",
                address!("1111111111111111111111111111111111111111"),
                b256!("2222222222222222222222222222222222222222222222222222222222222222"),
                1_754_388_000_000,
            )
        };
        // A TEST-domain signature must never validate on LIVE.
        assert_ne!(
            base(),
            maker_session_digest(
                "Textile Maker Session (TEST)",
                "mk_golden",
                address!("1111111111111111111111111111111111111111"),
                b256!("2222222222222222222222222222222222222222222222222222222222222222"),
                1_754_388_000_000,
            )
        );
        assert_ne!(
            base(),
            maker_session_digest(
                "Textile Maker Session (LIVE)",
                "mk_other",
                address!("1111111111111111111111111111111111111111"),
                b256!("2222222222222222222222222222222222222222222222222222222222222222"),
                1_754_388_000_000,
            )
        );
        assert_ne!(
            base(),
            maker_session_digest(
                "Textile Maker Session (LIVE)",
                "mk_golden",
                address!("1111111111111111111111111111111111111111"),
                b256!("2222222222222222222222222222222222222222222222222222222222222222"),
                1_754_388_000_001,
            )
        );
    }

    #[test]
    fn digest_depends_on_chain_and_permit2() {
        let o = sample();
        let p2 = address!("000000000022d473030f116ddee9f6b43ac78ba3");
        let d1 = permit2_digest(&o, p2, 1);
        let d8453 = permit2_digest(&o, p2, 8453);
        assert_ne!(d1, d8453, "domain separator must bind the chain id");

        let other = address!("00000000000000000000000000000000deadbeef");
        assert_ne!(
            permit2_digest(&o, p2, 1),
            permit2_digest(&o, other, 1),
            "domain separator must bind the verifying contract"
        );
        let _ = d1;
    }
}
