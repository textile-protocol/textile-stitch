// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Turnkey signer: signs digests via Turnkey's `sign_raw_payload` API (TEE key
//! custody). One synchronous round trip per signature, authenticated by
//! request-stamping with the operator's P-256 API key (handled by the Turnkey
//! Rust SDK). No sidecar, no inbound server — each operator self-custodies their
//! own Turnkey org and API key.
//!
//! We sign the raw 32-byte digest (`HASH_FUNCTION_NO_OP`); the bot keeps doing
//! its own EIP-712 / EIP-1559 digest computation and broadcast. Turnkey returns
//! `{r, s, v}`; [`finalize_signature`] turns it into the canonical 65-byte form
//! and verifies it recovers to the configured operator address.

use alloy_primitives::{hex, Address, B256};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use turnkey_client::generated::immutable::common::v1::{HashFunction, PayloadEncoding};
use turnkey_client::generated::SignRawPayloadIntentV2;
use turnkey_client::{TurnkeyClient, TurnkeyP256ApiKey};

use super::{
    finalize_signature, parse_address, parse_hex32, parse_v, read_env_secret, Signer, TurnkeyConfig,
};

/// Public key of the Turnkey API key pair (not secret).
const API_PUBLIC_KEY_ENV: &str = "TURNKEY_API_PUBLIC_KEY";
/// Private key of the Turnkey API key pair (secret; `_FILE` variant preferred).
const API_PRIVATE_KEY_ENV: &str = "TURNKEY_API_PRIVATE_KEY";
const API_PRIVATE_KEY_FILE_ENV: &str = "TURNKEY_API_PRIVATE_KEY_FILE";

pub struct TurnkeySigner {
    client: TurnkeyClient<TurnkeyP256ApiKey>,
    organization_id: String,
    sign_with: String,
    operator_address: Address,
    max_concurrent_signs: usize,
}

impl TurnkeySigner {
    /// Build from config + the API key pair in the environment.
    pub fn from_config(cfg: &TurnkeyConfig) -> anyhow::Result<Self> {
        let api_key = load_api_key()?;
        let client = TurnkeyClient::builder()
            .api_key(api_key)
            .base_url(cfg.api_base_url.clone())
            .build()
            .map_err(|e| anyhow!("building Turnkey client: {e}"))?;
        let operator_address = parse_address(&cfg.operator_address)?;
        Ok(Self {
            client,
            organization_id: cfg.organization_id.clone(),
            sign_with: cfg.sign_with.clone(),
            operator_address,
            max_concurrent_signs: cfg.max_concurrent_signs,
        })
    }
}

#[async_trait]
impl Signer for TurnkeySigner {
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        let result = self
            .client
            .sign_raw_payload(
                self.organization_id.clone(),
                self.client.current_timestamp(),
                SignRawPayloadIntentV2 {
                    sign_with: self.sign_with.clone(),
                    // We already hashed; sign the 32-byte digest verbatim.
                    payload: hex::encode(digest.as_slice()),
                    encoding: PayloadEncoding::Hexadecimal,
                    hash_function: HashFunction::NoOp,
                },
            )
            .await
            .map_err(|e| anyhow!("Turnkey sign_raw_payload failed: {e}"))?
            .result;

        let r = parse_hex32(&result.r).context("Turnkey signature r")?;
        let s = parse_hex32(&result.s).context("Turnkey signature s")?;
        finalize_signature(digest, &r, &s, parse_v(&result.v), self.operator_address)
    }

    fn address(&self) -> Address {
        self.operator_address
    }

    fn max_concurrent_signs(&self) -> usize {
        self.max_concurrent_signs
    }
}

fn load_api_key() -> anyhow::Result<TurnkeyP256ApiKey> {
    let private = read_env_secret(API_PRIVATE_KEY_FILE_ENV, API_PRIVATE_KEY_ENV)
        .context("Turnkey API private key")?;
    let public = std::env::var(API_PUBLIC_KEY_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    TurnkeyP256ApiKey::from_strings(private, public)
        .map_err(|e| anyhow!("invalid Turnkey API key pair: {e}"))
}
