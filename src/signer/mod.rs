// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Signing abstraction for the operator key.
//!
//! Every signature the bot makes (EIP-712 Permit2 orders and EIP-1559 txs) goes
//! through a [`Signer`]: a 32-byte digest in, a 65-byte `r ++ s ++ v` (v in
//! {27,28}) signature out. The default backend is a raw local key
//! ([`local::LocalSigner`], the "hotwallet"); MPC backends ([`turnkey`],
//! [`mpcvault`]) implement the same trait so the rest of the bot never knows
//! which one is signing. The provider is chosen by the optional `[signer]`
//! config section; absent, it is the local key from the environment (unchanged
//! behaviour).
//!
//! This module also keeps the pure crypto helpers (address derivation, the
//! local k256 sign, signature recovery) used by the local backend, the tests,
//! and [`finalize_signature`] — the shared step every remote backend runs to
//! turn a provider's `{r, s, v?}` into the canonical 65-byte form and verify it
//! recovers to the configured operator address.

use std::sync::Arc;

use alloy_primitives::{hex, keccak256, Address, B256};
use anyhow::Context as _;
use async_trait::async_trait;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde::Deserialize;
use zeroize::Zeroize;

use crate::config::Config;

pub mod local;
pub mod mpcvault;
pub mod turnkey;

pub use local::LocalSigner;

/// A backend that signs a 32-byte digest with the operator's key.
///
/// Implementations may do network I/O (MPC providers), so the method is async.
/// They are shared as `Arc<dyn Signer>` across the wallet and the poster and
/// held across `.await` on the multi-thread runtime, hence `Send + Sync`.
#[async_trait]
pub trait Signer: Send + Sync {
    /// Sign `digest`, returning `r(32) ++ s(32) ++ v(1)` with `v` in {27, 28}.
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]>;

    /// The Ethereum address this signer controls.
    fn address(&self) -> Address;

    /// How many signatures the poster may request concurrently. Remote MPC
    /// backends override this from config; local signing is instant so the
    /// default is fine.
    fn max_concurrent_signs(&self) -> usize {
        8
    }
}

/// Shared signer handle, cloned into the wallet (blue leg) and poster (green leg).
pub type DynSigner = Arc<dyn Signer>;

/// Build the configured signer once at startup. With no `[signer]` section this
/// is the local key from `STITCH_PRIVATE_KEY[_FILE]` — identical to the historic
/// hotwallet behaviour.
pub async fn build_signer(cfg: &Config) -> anyhow::Result<DynSigner> {
    match cfg.signer.clone().unwrap_or(SignerConfig::Local) {
        SignerConfig::Local => Ok(Arc::new(LocalSigner::from_env()?)),
        SignerConfig::Turnkey(c) => Ok(Arc::new(turnkey::TurnkeySigner::from_config(&c)?)),
        SignerConfig::Mpcvault(c) => Ok(Arc::new(mpcvault::MpcVaultSigner::from_config(&c).await?)),
    }
}

/// The optional `[signer]` config. Tagged by `provider`; the provider's own
/// fields sit at the same level (e.g. `provider = "turnkey"` plus the turnkey
/// fields). Secrets (API keys, tokens) are never here — they come from the env.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SignerConfig {
    /// Raw local key from the environment (the default hotwallet).
    Local,
    Turnkey(TurnkeyConfig),
    Mpcvault(MpcVaultConfig),
}

impl SignerConfig {
    /// Concurrency the poster should use for this backend.
    pub fn max_concurrent_signs(&self) -> usize {
        match self {
            SignerConfig::Local => 8,
            SignerConfig::Turnkey(c) => c.max_concurrent_signs,
            SignerConfig::Mpcvault(c) => c.max_concurrent_signs,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnkeyConfig {
    /// Turnkey organization (or sub-organization) id.
    pub organization_id: String,
    /// Wallet account address, private key address, or private key id to sign with.
    pub sign_with: String,
    /// The EVM address `sign_with` resolves to (the operator/maker address).
    pub operator_address: String,
    #[serde(default = "default_turnkey_base_url")]
    pub api_base_url: String,
    #[serde(default = "default_max_concurrent_signs")]
    pub max_concurrent_signs: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MpcVaultConfig {
    #[serde(default = "default_mpcvault_base_url")]
    pub api_base_url: String,
    /// UUID of the MPCVault vault that holds the operator wallet.
    pub vault_uuid: String,
    /// SSH ed25519 public key of the co-located client-signer sidecar.
    pub client_signer_pubkey: String,
    /// The operator/maker EVM address the vault wallet resolves to.
    pub operator_address: String,
    /// Where the bot exposes the approval callback the sidecar calls.
    #[serde(default = "default_callback_listen_addr")]
    pub callback_listen_addr: String,
    #[serde(default = "default_poll_timeout_secs")]
    pub poll_timeout_secs: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_mpcvault_max_concurrent_signs")]
    pub max_concurrent_signs: usize,
}

fn default_turnkey_base_url() -> String {
    "https://api.turnkey.com".to_string()
}
fn default_mpcvault_base_url() -> String {
    "https://api.mpcvault.com".to_string()
}
fn default_callback_listen_addr() -> String {
    "0.0.0.0:8088".to_string()
}
fn default_poll_timeout_secs() -> u64 {
    30
}
fn default_poll_interval_ms() -> u64 {
    1000
}
fn default_max_concurrent_signs() -> usize {
    8
}
fn default_mpcvault_max_concurrent_signs() -> usize {
    4
}

/// Parse a hex private key, tolerating an optional `0x`/`0X` prefix and
/// surrounding whitespace — the documented key material form is `0x…`.
/// Stripping the prefix explicitly keeps that contract working regardless of
/// which hex backend decodes it.
pub fn parse_private_key(raw: &str) -> anyhow::Result<SigningKey> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    let mut bytes = hex::decode(body).context("private key must be hex")?;
    let key = SigningKey::from_slice(&bytes).context("invalid private key");
    bytes.zeroize();
    key
}

/// The BIP-44 account-0 path (`m/44'/60'/0'/0/0`). This is what MetaMask, Rabby,
/// and virtually every Ethereum wallet derive their first address at, so a phrase
/// imported here resolves to the same address the operator sees in their wallet.
pub const DEFAULT_DERIVATION_PATH: &str = "m/44'/60'/0'/0/0";

/// Derive the operator signing key from a BIP-39 seed phrase at
/// [`DEFAULT_DERIVATION_PATH`]. The phrase is validated (wordlist + checksum) by
/// the parse; a bad word or wrong length fails here rather than deriving a
/// garbage key. No BIP-39 passphrase (the optional "25th word") is used — that
/// keeps the derived address matching a default wallet import.
pub fn parse_mnemonic(phrase: &str) -> anyhow::Result<SigningKey> {
    use coins_bip39::{English, Mnemonic};

    // Deliberately drop the crate's source error rather than chaining it with
    // `.context(...)`: `coins_bip39`'s bad-checksum error echoes the supplied
    // phrase, and callers render failures with `{e:#}` (the whole chain), so
    // chaining would print a mistyped seed phrase unmasked on screen — one wrong
    // word is still enough material to brute-force the wallet. Return a clean,
    // phrase-free message instead.
    let mnemonic = Mnemonic::<English>::new_from_phrase(phrase.trim()).map_err(|_| {
        anyhow::anyhow!("seed phrase is not valid — check the words and their order")
    })?;
    let xpriv = mnemonic
        .derive_key(DEFAULT_DERIVATION_PATH, None)
        .map_err(|_| anyhow::anyhow!("could not derive a key from the seed phrase"))?;
    let signing_key: &SigningKey = xpriv.as_ref();
    Ok(signing_key.clone())
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

/// Parse an EVM address from config/text (tolerates surrounding whitespace).
pub fn parse_address(raw: &str) -> anyhow::Result<Address> {
    raw.trim()
        .parse::<Address>()
        .with_context(|| format!("invalid operator address {raw:?}"))
}

/// Read a secret from `<NAME>_FILE` (a file path, preferred) or `<NAME>` (the
/// raw value). Mirrors how the local key is loaded, so MPC credentials can be
/// mounted as files (the deploy default) or passed inline. Trims trailing
/// whitespace/newlines a file write tends to add.
pub(crate) fn read_env_secret(file_env: &str, raw_env: &str) -> anyhow::Result<String> {
    if let Ok(path) = std::env::var(file_env) {
        let path = path.trim();
        if !path.is_empty() {
            return std::fs::read_to_string(path)
                .map(|s| s.trim().to_string())
                .with_context(|| format!("reading {file_env} {path}"));
        }
    }
    std::env::var(raw_env)
        .map(|s| s.trim().to_string())
        .map_err(|_| anyhow::anyhow!("set {file_env} or {raw_env}"))
}

/// Decode a hex r/s component (with or without `0x`) into a left-padded 32-byte
/// big-endian array. Remote providers return r/s as hex strings.
pub(crate) fn parse_hex32(s: &str) -> anyhow::Result<[u8; 32]> {
    let t = s.trim();
    let body = t.strip_prefix("0x").unwrap_or(t);
    let bytes = hex::decode(body).context("not hex")?;
    if bytes.len() > 32 {
        anyhow::bail!("signature component longer than 32 bytes ({})", bytes.len());
    }
    let mut out = [0u8; 32];
    out[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(out)
}

/// Parse a provider's `v` (recovery id), which may be "00"/"01", "1b"/"1c", or
/// "0"/"1". Only a hint for which parity [`finalize_signature`] tries first; the
/// result is verified regardless.
pub(crate) fn parse_v(v: &str) -> Option<u8> {
    let t = v.trim();
    let t = t.strip_prefix("0x").unwrap_or(t);
    u8::from_str_radix(t, 16).ok()
}

/// Sign a 32-byte digest with a local key, returning `r(32) ++ s(32) ++ v(1)`
/// with `v` in {27,28}.
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

/// Turn a remote provider's `{r, s, v?}` into the canonical 65-byte signature.
///
/// MPC providers differ on whether they return the recovery id, and what
/// convention they use (0/1 vs 27/28). Rather than trust it, we try both
/// parities, recover the address, and keep the one that matches the configured
/// operator address — which also verifies the provider signed our exact digest
/// with the key we expect. `v_hint` (if present) just orders the attempts.
pub fn finalize_signature(
    digest: B256,
    r: &[u8; 32],
    s: &[u8; 32],
    v_hint: Option<u8>,
    expected: Address,
) -> anyhow::Result<[u8; 65]> {
    // Normalize to low-s (EIP-2) before anything else. k256 local signing is
    // already low-s, but a remote MPC provider may return a high-s value.
    // Ethereum tx validation and the OpenZeppelin ECDSA checker behind Permit2
    // both reject high-s, so a high-s signature would be rejected on broadcast
    // and on order verification even though it recovers to the right address.
    // Flipping s to the low half also flips the recovery parity, which the
    // address-matching search below resolves regardless of `v_hint`.
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(r);
    rs[32..].copy_from_slice(s);
    let sig = Signature::from_slice(&rs).context("invalid r/s signature scalars")?;
    let sig = sig.normalize_s().unwrap_or(sig);
    let norm = sig.to_bytes();

    let prefer_high = matches!(v_hint, Some(1) | Some(28));
    let order = if prefer_high {
        [28u8, 27u8]
    } else {
        [27u8, 28u8]
    };
    for v in order {
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&norm);
        out[64] = v;
        if let Ok(addr) = recover_address(digest, &out) {
            if addr == expected {
                return Ok(out);
            }
        }
    }
    anyhow::bail!("remote signature did not recover to operator address {expected}")
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

    // The Hardhat/Anvil default mnemonic. Its account 0 is exactly `KEY` above, so
    // a correct m/44'/60'/0'/0/0 derivation must reproduce that key and address.
    const MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn derives_the_hardhat_account_zero_from_its_mnemonic() {
        let from_phrase = parse_mnemonic(MNEMONIC).expect("phrase derives");
        assert_eq!(
            address_from_signing_key(&from_phrase),
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
        // Same key the raw-hex path yields — the seed phrase is just another way in.
        assert_eq!(from_phrase.to_bytes(), key().to_bytes());
    }

    #[test]
    fn mnemonic_tolerates_surrounding_whitespace() {
        let padded = format!("  {MNEMONIC}\n");
        assert_eq!(
            parse_mnemonic(&padded).unwrap().to_bytes(),
            key().to_bytes()
        );
    }

    #[test]
    fn rejects_an_invalid_seed_phrase() {
        // Empty, wrong word count, and a valid-words-but-bad-checksum phrase all
        // fail rather than deriving a garbage key.
        assert!(parse_mnemonic("").is_err());
        assert!(parse_mnemonic("zzz not real bip39 words here").is_err());
        assert!(
            parse_mnemonic("test test test test test test test test test test test test").is_err(),
            "12x 'test' has a bad checksum and must be rejected"
        );
    }

    #[test]
    fn invalid_phrase_error_never_leaks_the_phrase() {
        // 12 valid words with a bad checksum: coins_bip39's own error echoes the
        // phrase, and callers render errors with `{e:#}` (the whole chain). The
        // rendered error must not contain any of the supplied words.
        let phrase = "test test test test test test test test test test test test";
        let err = parse_mnemonic(phrase).unwrap_err();
        let shown = format!("{err:#}");
        assert!(
            !shown.contains("test"),
            "error must not surface the seed phrase, got: {shown}"
        );
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

    #[test]
    fn finalize_recovers_v_without_a_hint() {
        // Produce a real r/s with the local signer, drop v, and let finalize
        // recover it by matching the operator address.
        let digest = b256!("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20");
        let expected = address_from_signing_key(&key());
        let full = sign_digest(&key(), digest).unwrap();
        let r: [u8; 32] = full[..32].try_into().unwrap();
        let s: [u8; 32] = full[32..64].try_into().unwrap();

        for hint in [None, Some(0u8), Some(1u8), Some(27u8), Some(28u8)] {
            let got = finalize_signature(digest, &r, &s, hint, expected).unwrap();
            assert_eq!(got, full, "hint {hint:?} should still recover the right v");
        }
    }

    #[test]
    fn finalize_normalizes_high_s_remote_signatures() {
        use k256::elliptic_curve::ff::PrimeField;

        let digest = b256!("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20");
        let expected = address_from_signing_key(&key());
        let full = sign_digest(&key(), digest).unwrap(); // canonical low-s
        let r: [u8; 32] = full[..32].try_into().unwrap();

        // Build the malleable high-s twin a remote provider might return: s' = n - s.
        let sig = Signature::from_slice(&full[..64]).unwrap();
        let (_r, s) = sig.split_scalars();
        let s_high_repr = (-(*s)).to_repr();
        let mut s_high = [0u8; 32];
        s_high.copy_from_slice(&s_high_repr);

        let got = finalize_signature(digest, &r, &s_high, None, expected).unwrap();
        // Normalized back to the canonical low-s signature; recovers to the signer.
        assert_eq!(got, full);
        assert_eq!(recover_address(digest, &got).unwrap(), expected);
    }

    #[test]
    fn finalize_rejects_a_signature_for_another_address() {
        let digest = b256!("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20");
        let full = sign_digest(&key(), digest).unwrap();
        let r: [u8; 32] = full[..32].try_into().unwrap();
        let s: [u8; 32] = full[32..64].try_into().unwrap();
        let wrong = address!("000000000000000000000000000000000000dead");
        assert!(finalize_signature(digest, &r, &s, None, wrong).is_err());
    }

    #[test]
    fn parse_hex32_left_pads_and_validates() {
        let mut want = [0u8; 32];
        want[31] = 1;
        assert_eq!(parse_hex32("0x01").unwrap(), want);
        assert_eq!(parse_hex32(&"ff".repeat(32)).unwrap(), [0xffu8; 32]);
        assert!(parse_hex32(&"ab".repeat(33)).is_err());
    }

    #[test]
    fn parse_v_reads_common_encodings() {
        assert_eq!(parse_v("00"), Some(0));
        assert_eq!(parse_v("01"), Some(1));
        assert_eq!(parse_v("1b"), Some(27));
        assert_eq!(parse_v("0x1c"), Some(28));
        assert_eq!(parse_v("nope"), None);
    }
}
