// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The local-key signer (the "hotwallet"): a raw secp256k1 key held in process,
//! loaded from `STITCH_PRIVATE_KEY_FILE` (preferred) or `STITCH_PRIVATE_KEY`.
//! This is the default backend and is unchanged from the original key handling.

use std::env::VarError;

use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use k256::ecdsa::SigningKey;

use super::{address_from_signing_key, parse_private_key, sign_digest, Signer};

pub const PRIVATE_KEY_ENV: &str = "STITCH_PRIVATE_KEY";
pub const PRIVATE_KEY_FILE_ENV: &str = "STITCH_PRIVATE_KEY_FILE";

/// A signer backed by a raw local key.
pub struct LocalSigner {
    key: SigningKey,
    address: Address,
}

impl LocalSigner {
    pub fn new(key: SigningKey) -> Self {
        let address = address_from_signing_key(&key);
        Self { key, address }
    }

    /// Load the key from the environment (`STITCH_PRIVATE_KEY_FILE` wins over
    /// `STITCH_PRIVATE_KEY`).
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = load_key_material()?;
        Ok(Self::new(parse_private_key(&raw)?))
    }
}

#[async_trait]
impl Signer for LocalSigner {
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        // Local signing is synchronous; no real await.
        sign_digest(&self.key, digest)
    }

    fn address(&self) -> Address {
        self.address
    }
}

fn load_key_material() -> anyhow::Result<String> {
    load_key_material_from_vars(
        std::env::var(PRIVATE_KEY_FILE_ENV),
        std::env::var(PRIVATE_KEY_ENV),
    )
}

fn load_key_material_from_vars(
    key_file: Result<String, VarError>,
    key: Result<String, VarError>,
) -> anyhow::Result<String> {
    match key_file {
        Ok(path) => return read_private_key_file(&path),
        Err(VarError::NotPresent) => {}
        Err(VarError::NotUnicode(_)) => {
            return Err(anyhow!("{PRIVATE_KEY_FILE_ENV} must be valid unicode"));
        }
    }
    key.with_context(|| format!("set {PRIVATE_KEY_FILE_ENV} or {PRIVATE_KEY_ENV}"))
}

fn read_private_key_file(path: &str) -> anyhow::Result<String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("{PRIVATE_KEY_FILE_ENV} is empty"));
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {PRIVATE_KEY_FILE_ENV} {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::unix_now;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn temp_key_file(contents: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "stitch-test-key-{}-{}.txt",
            std::process::id(),
            unix_now()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn key_material_uses_direct_env_when_no_file_is_set() {
        let raw = load_key_material_from_vars(Err(VarError::NotPresent), Ok(TEST_KEY.into()))
            .expect("key loads");
        assert_eq!(raw, TEST_KEY);
    }

    #[test]
    fn key_material_file_takes_precedence_over_direct_env() {
        let path = temp_key_file(&format!("0x{TEST_KEY}\n"));
        let raw = load_key_material_from_vars(
            Ok(path.to_string_lossy().into_owned()),
            Ok("0x0000000000000000000000000000000000000000000000000000000000000001".into()),
        )
        .expect("key file loads");
        std::fs::remove_file(path).unwrap();
        assert_eq!(raw.trim(), format!("0x{TEST_KEY}"));
    }

    #[test]
    fn key_material_requires_some_source() {
        let err = load_key_material_from_vars(Err(VarError::NotPresent), Err(VarError::NotPresent))
            .expect_err("missing key source should fail");
        assert!(err.to_string().contains(PRIVATE_KEY_FILE_ENV));
        assert!(err.to_string().contains(PRIVATE_KEY_ENV));
    }

    #[test]
    fn empty_key_file_path_is_an_error() {
        let err =
            load_key_material_from_vars(Ok(" ".into()), Ok(TEST_KEY.into())).expect_err("empty");
        assert!(err.to_string().contains(PRIVATE_KEY_FILE_ENV));
    }

    #[tokio::test]
    async fn local_signer_signs_and_recovers() {
        use super::super::recover_address;
        use alloy_primitives::b256;

        let signer = LocalSigner::from_env_for_test();
        let digest = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        let sig = signer.sign_digest(digest).await.unwrap();
        assert_eq!(recover_address(digest, &sig).unwrap(), signer.address());
    }

    impl LocalSigner {
        fn from_env_for_test() -> Self {
            LocalSigner::new(parse_private_key(TEST_KEY).unwrap())
        }
    }
}
