// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! secp256k1 signing of an EIP-712 digest with the operator key, and address
//! derivation. Produces the 65-byte `r ++ s ++ v` signature UniswapX/Permit2
//! expect (`v` in {27, 28}).

use alloy_primitives::{hex, keccak256, Address, B256};
use anyhow::Context as _;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};

/// Parse a hex private key, tolerating an optional `0x`/`0X` prefix and
/// surrounding whitespace — the documented `STITCH_PRIVATE_KEY` form is `0x…`.
/// Stripping the prefix explicitly keeps that contract working regardless of
/// which hex backend decodes it.
pub fn parse_private_key(raw: &str) -> anyhow::Result<SigningKey> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    let bytes = hex::decode(body).context("STITCH_PRIVATE_KEY must be hex")?;
    SigningKey::from_slice(&bytes).context("invalid STITCH_PRIVATE_KEY")
}

/// Ethereum address of a verifying key (keccak of the uncompressed pubkey).
pub fn address_from_verifying_key(vk: &VerifyingKey) -> Address {
    let point = vk.to_encoded_point(false); // 0x04 ++ X(32) ++ Y(32)
    let bytes = point.as_bytes();
    let hash = keccak256(&bytes[1..]); // drop the 0x04 tag
    Address::from_slice(&hash.0[12..])
}

/// Ethereum address controlled by a signing key.
pub fn address_from_signing_key(key: &SigningKey) -> Address {
    address_from_verifying_key(key.verifying_key())
}

/// Sign a 32-byte digest, returning `r(32) ++ s(32) ++ v(1)` with `v` in {27,28}.
pub fn sign_digest(key: &SigningKey, digest: B256) -> anyhow::Result<[u8; 65]> {
    let (sig, recid): (Signature, RecoveryId) = key.sign_prehash_recoverable(digest.as_slice())?;
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = recid.to_byte() + 27;
    Ok(out)
}

/// Recover the signer address from a `sign_digest` signature (for tests/checks).
pub fn recover_address(digest: B256, signature: &[u8; 65]) -> anyhow::Result<Address> {
    let recid = RecoveryId::from_byte(signature[64].wrapping_sub(27))
        .ok_or_else(|| anyhow::anyhow!("invalid recovery id"))?;
    let sig = Signature::from_slice(&signature[..64])?;
    let vk = VerifyingKey::recover_from_prehash(digest.as_slice(), &sig, recid)?;
    Ok(address_from_verifying_key(&vk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    // Anvil/Hardhat account #0 — a well-known test vector.
    const KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn key() -> SigningKey {
        SigningKey::from_slice(&hex::decode(KEY).unwrap()).unwrap()
    }

    #[test]
    fn derives_the_known_address() {
        assert_eq!(
            address_from_signing_key(&key()),
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
    }

    #[test]
    fn parses_key_with_optional_0x_prefix_and_whitespace() {
        let want = address_from_signing_key(&key());
        for raw in [
            KEY.to_string(),
            format!("0x{KEY}"),
            format!("0X{KEY}"),
            format!("  0x{KEY}\n"),
        ] {
            let parsed = parse_private_key(&raw).expect("key parses");
            assert_eq!(address_from_signing_key(&parsed), want, "raw: {raw:?}");
        }
    }

    #[test]
    fn rejects_a_non_hex_or_empty_key() {
        assert!(parse_private_key("0xzz").is_err());
        assert!(parse_private_key("").is_err());
    }

    #[test]
    fn signature_recovers_to_the_signer() {
        let digest = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        let sig = sign_digest(&key(), digest).unwrap();
        assert!(sig[64] == 27 || sig[64] == 28);
        assert_eq!(
            recover_address(digest, &sig).unwrap(),
            address_from_signing_key(&key())
        );
    }
}
