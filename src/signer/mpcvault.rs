// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! MPCVault signer.
//!
//! Unlike Turnkey, MPCVault's automated signing needs out-of-process pieces:
//! a per-operator `client-signer` Docker sidecar (holds an Ed25519 MPC key
//! share, one per vault) plus a callback HTTP server the sidecar calls to
//! approve each request. This module runs that callback server in-process and
//! drives the REST flow:
//!
//!   1. `POST /v1/createSigningRequest`  — a Raw Message whose `content` is our
//!      32-byte digest, tagged with the sidecar's public key so MPCVault routes
//!      the approval callback to it.
//!   2. (server-side) MPCVault → sidecar → our callback server → approve.
//!   3. `POST /v1/executeSigningRequests` — returns the ECDSA `{R, S, V}`
//!      synchronously for API wallets with a client signer.
//!   4. [`finalize_signature`] normalizes v and verifies recovery to the
//!      configured operator address.
//!
//! We sign the raw digest (`ECDSA_HASH_FUNCTION_USE_MESSAGE_DIRECTLY`) and
//! broadcast EIP-1559 ourselves; MPCVault does not broadcast Raw Messages.
//!
//! The callback server fails closed: MPCVault treats HTTP 200 as approval, so it
//! returns 200 only when the request's raw-message `content` (the bytes that will
//! actually be signed) is a digest this process currently has in flight, and 403
//! otherwise. It correlates on that signed field specifically, not a substring
//! search over the whole body — otherwise another vault user could copy one of
//! our in-flight digests into a decoy field while signing a different payload. A
//! request the bot did not create (another vault user, a stolen API token) is
//! rejected rather than signed. The body is the serialized SigningRequest
//! protobuf the client-signer POSTs (Content-Type application/octet-stream); JSON
//! is also accepted in case a gateway is in front.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use axum::{body::Bytes, extract::State, http::StatusCode, routing::get, Router};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info, warn};

use super::{
    finalize_signature, parse_address, parse_hex32, parse_v, read_env_secret, MpcVaultConfig,
    Signer,
};

const API_TOKEN_ENV: &str = "MPCVAULT_API_TOKEN";
const API_TOKEN_FILE_ENV: &str = "MPCVAULT_API_TOKEN_FILE";

/// Digests we have asked MPCVault to sign and are waiting on, used to correlate
/// the sidecar's approval callback.
type Pending = Arc<Mutex<HashSet<[u8; 32]>>>;

/// Tracks a digest in the pending set for the lifetime of one sign attempt, and
/// removes it on drop. Using `Drop` (rather than a manual remove after the await)
/// makes the entry cancellation-safe: if the signing task is aborted mid-await,
/// the guard still runs and the callback gate stops trusting the digest.
struct PendingGuard {
    pending: Pending,
    key: [u8; 32],
}

impl PendingGuard {
    fn new(pending: Pending, key: [u8; 32]) -> Self {
        pending
            .lock()
            .expect("pending set not poisoned")
            .insert(key);
        Self { pending, key }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .expect("pending set not poisoned")
            .remove(&self.key);
    }
}

pub struct MpcVaultSigner {
    http: reqwest::Client,
    api_base_url: String,
    api_token: String,
    vault_uuid: String,
    client_signer_pubkey: String,
    operator_address: Address,
    max_concurrent_signs: usize,
    pending: Pending,
}

impl MpcVaultSigner {
    /// Build from config + the API token in the env, and start the callback
    /// approval server the client-signer sidecar calls.
    pub async fn from_config(cfg: &MpcVaultConfig) -> anyhow::Result<Self> {
        let api_token =
            read_env_secret(API_TOKEN_FILE_ENV, API_TOKEN_ENV).context("MPCVault API token")?;
        let operator_address = parse_address(&cfg.operator_address)?;
        let timeout = Duration::from_secs(cfg.poll_timeout_secs.max(5));
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .context("building MPCVault HTTP client")?;

        let pending: Pending = Arc::new(Mutex::new(HashSet::new()));
        let addr: SocketAddr = cfg.callback_listen_addr.parse().with_context(|| {
            format!(
                "invalid callback_listen_addr {:?}",
                cfg.callback_listen_addr
            )
        })?;
        start_callback_server(addr, pending.clone()).await?;
        info!(
            %addr,
            vault = %cfg.vault_uuid,
            "MPCVault callback approval server listening; ensure the client-signer sidecar's callback-url points here"
        );

        Ok(Self {
            http,
            api_base_url: cfg.api_base_url.clone(),
            api_token,
            vault_uuid: cfg.vault_uuid.clone(),
            client_signer_pubkey: cfg.client_signer_pubkey.clone(),
            operator_address,
            max_concurrent_signs: cfg.max_concurrent_signs,
            pending,
        })
    }

    async fn post<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        let url = format!("{}/v1/{}", self.api_base_url.trim_end_matches('/'), method);
        let resp = self
            .http
            .post(&url)
            .header("X-Mtoken", &self.api_token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("MPCVault {method} request"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .with_context(|| format!("MPCVault {method} response body"))?;
        if !status.is_success() {
            anyhow::bail!("MPCVault {method} HTTP {status}: {text}");
        }
        serde_json::from_str::<T>(&text)
            .with_context(|| format!("decoding MPCVault {method} response: {text}"))
    }

    async fn do_sign(&self, digest: B256, content_b64: &str) -> anyhow::Result<[u8; 65]> {
        let create_body = json!({
            "rawMessage": {
                "from": self.operator_address.to_string(),
                "content": content_b64,
                "ecdsaHashFunction": "ECDSA_HASH_FUNCTION_USE_MESSAGE_DIRECTLY",
            },
            "vaultUuid": self.vault_uuid,
            "callbackClientSignerPublicKey": self.client_signer_pubkey,
        });
        let created: CreateResp = self.post("createSigningRequest", &create_body).await?;
        if let Some(err) = created.error {
            anyhow::bail!("MPCVault createSigningRequest error: {err}");
        }
        let uuid = created
            .signing_request
            .ok_or_else(|| anyhow!("MPCVault createSigningRequest returned no signing_request"))?
            .uuid;

        let exec: ExecResp = self
            .post("executeSigningRequests", &json!({ "uuid": uuid }))
            .await?;
        if let Some(err) = exec.error {
            anyhow::bail!("MPCVault executeSigningRequests error: {err}");
        }
        let sig = exec
            .signatures
            .and_then(|c| c.signatures.into_iter().next())
            .and_then(|s| s.ecdsa_signature)
            .ok_or_else(|| {
                anyhow!("MPCVault executeSigningRequests returned no ECDSA signature")
            })?;
        let r = parse_hex32(&sig.r).context("MPCVault signature R")?;
        let s = parse_hex32(&sig.s).context("MPCVault signature S")?;
        finalize_signature(digest, &r, &s, parse_v(&sig.v), self.operator_address)
    }
}

#[async_trait]
impl Signer for MpcVaultSigner {
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        let content = STANDARD.encode(digest.as_slice());
        // Track the digest for the callback gate for exactly as long as we are
        // asking MPCVault to sign it. A Drop guard, not a manual remove, so the
        // entry clears even if this task is cancelled mid-await — the poster drops
        // its JoinSet (aborting siblings) when one signature in a batch fails.
        // A manual remove would be skipped on cancellation, leaking a stale digest
        // that the callback gate would keep approving.
        let _tracked = PendingGuard::new(self.pending.clone(), digest.0);
        self.do_sign(digest, &content).await
    }

    fn address(&self) -> Address {
        self.operator_address
    }

    fn max_concurrent_signs(&self) -> usize {
        self.max_concurrent_signs
    }
}

async fn start_callback_server(addr: SocketAddr, pending: Pending) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .fallback(handle_callback)
        .with_state(pending);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding MPCVault callback server to {addr}"))?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "MPCVault callback server stopped");
        }
    });
    Ok(())
}

/// Approve or reject a signing request the sidecar forwards. MPCVault treats
/// HTTP 200 as approval and a 4xx/5xx as rejection. We fail closed: approve only
/// when the request's actual signed payload is a digest this process currently
/// has in flight, otherwise reject. This is the policy gate — it stops a request
/// the bot did not create (another vault user, a stolen API token) from signing.
async fn handle_callback(State(pending): State<Pending>, body: Bytes) -> StatusCode {
    let matched = signed_digest_from_callback(&body).is_some_and(|d| {
        pending
            .lock()
            .expect("pending set not poisoned")
            .contains(&d)
    });
    if matched {
        info!("MPCVault callback approved (signed payload matches an in-flight request)");
        StatusCode::OK
    } else {
        warn!(
            bytes = body.len(),
            "MPCVault callback rejected: signed payload is not an in-flight digest"
        );
        StatusCode::FORBIDDEN
    }
}

/// The digest MPCVault is actually being asked to sign, taken from the request's
/// raw-message `content` field only — not from anywhere else in the body.
/// Correlating on the signed field (rather than a substring search) is what stops
/// another vault user from copying one of our in-flight digests into a
/// user-controlled field (e.g. `notes`) while asking to sign a different payload:
/// we read the `content` that will actually be signed and ignore sibling fields.
///
/// The real client-signer POSTs the SigningRequest as protobuf (Content-Type
/// application/octet-stream), so we decode that; JSON is also accepted in case a
/// gateway is in front. Returns `None` (reject) when neither yields a raw-message
/// content we can decode.
fn signed_digest_from_callback(body: &[u8]) -> Option<[u8; 32]> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(d) = raw_message_content(&v) {
            return Some(d);
        }
    }
    protobuf_signed_digest(body)
}

/// Read the raw-message `content` from a serialized protobuf SigningRequest.
/// `SigningRequest.raw_message` is field 25 (`CreateSigningRequestRequest`'s is
/// 19, accepted as a fallback) and `RawMessage.content` is field 2 in both. Only
/// that content field is read; sibling fields (`from`, `notes`, ...) are ignored.
fn protobuf_signed_digest(body: &[u8]) -> Option<[u8; 32]> {
    let raw_message = pb_field_bytes(body, 25).or_else(|| pb_field_bytes(body, 19))?;
    let content = pb_field_bytes(raw_message, 2)?;
    <[u8; 32]>::try_from(content).ok()
}

/// Payload of the first length-delimited (wire type 2) field with the given
/// number at the top level of a protobuf message. Minimal walker — just enough to
/// reach one nested bytes field, no proto dependency. `None` on malformed input.
fn pb_field_bytes(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut i = 0;
    while i < buf.len() {
        let (tag, n) = pb_varint(&buf[i..])?;
        i += n;
        let field_num = tag >> 3;
        match tag & 0x7 {
            0 => {
                let (_, n) = pb_varint(&buf[i..])?; // varint value
                i += n;
            }
            1 => i = i.checked_add(8)?, // 64-bit
            5 => i = i.checked_add(4)?, // 32-bit
            2 => {
                let (len, n) = pb_varint(&buf[i..])?;
                i += n;
                let end = i.checked_add(usize::try_from(len).ok()?)?;
                let payload = buf.get(i..end)?;
                if field_num == field {
                    return Some(payload);
                }
                i = end;
            }
            _ => return None, // groups (3/4) unsupported / malformed
        }
    }
    None
}

/// Decode a base-128 varint, returning (value, bytes_read).
fn pb_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for (idx, &b) in buf.iter().enumerate().take(10) {
        value |= u64::from(b & 0x7f) << (idx * 7);
        if b & 0x80 == 0 {
            return Some((value, idx + 1));
        }
    }
    None
}

/// Recursively locate a `rawMessage`/`raw_message` object and decode its
/// `content` into a 32-byte digest. Only the content of a raw-message object is
/// trusted; sibling fields are never read.
fn raw_message_content(v: &serde_json::Value) -> Option<[u8; 32]> {
    match v {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                if key == "rawMessage" || key == "raw_message" {
                    if let Some(d) = val
                        .get("content")
                        .and_then(|c| c.as_str())
                        .and_then(decode_b64_32)
                    {
                        return Some(d);
                    }
                }
                if let Some(d) = raw_message_content(val) {
                    return Some(d);
                }
            }
            None
        }
        serde_json::Value::Array(items) => items.iter().find_map(raw_message_content),
        _ => None,
    }
}

/// Decode a base64 (standard or url-safe, padded or not) string into 32 bytes.
fn decode_b64_32(s: &str) -> Option<[u8; 32]> {
    for engine in [STANDARD, URL_SAFE, STANDARD_NO_PAD, URL_SAFE_NO_PAD] {
        if let Ok(bytes) = engine.decode(s) {
            if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                return Some(arr);
            }
        }
    }
    None
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateResp {
    signing_request: Option<SigningReqLite>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SigningReqLite {
    uuid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecResp {
    signatures: Option<SignatureContainer>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SignatureContainer {
    #[serde(default)]
    signatures: Vec<SignResponseJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignResponseJson {
    ecdsa_signature: Option<EcdsaSigJson>,
}

#[derive(Deserialize)]
struct EcdsaSigJson {
    #[serde(rename = "R")]
    r: String,
    #[serde(rename = "S")]
    s: String,
    #[serde(rename = "V")]
    v: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_execute_response_signature() {
        let body = json!({
            "signatures": {
                "signatures": [
                    { "ecdsaSignature": { "R": "0x01", "S": "0x02", "V": "01" } }
                ]
            }
        })
        .to_string();
        let parsed: ExecResp = serde_json::from_str(&body).unwrap();
        let sig = parsed
            .signatures
            .unwrap()
            .signatures
            .into_iter()
            .next()
            .unwrap()
            .ecdsa_signature
            .unwrap();
        assert_eq!(sig.r, "0x01");
        assert_eq!(sig.s, "0x02");
        assert_eq!(sig.v, "01");
    }

    #[test]
    fn parses_create_response_uuid() {
        let body = json!({ "signingRequest": { "uuid": "abc-123" } }).to_string();
        let parsed: CreateResp = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.signing_request.unwrap().uuid, "abc-123");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn callback_correlates_only_on_raw_message_content() {
        let ours = [7u8; 32];
        let mut set = HashSet::new();
        set.insert(ours);

        // Legitimate callback: our digest IS the raw-message content.
        let ok = json!({ "signingRequest": { "rawMessage": { "from": "0xabc", "content": STANDARD.encode(ours) } } })
            .to_string();
        assert_eq!(signed_digest_from_callback(ok.as_bytes()), Some(ours));

        // Attack: our digest is copied into a decoy field while the actual signed
        // content is a DIFFERENT payload. We read only `content`, so the decoy is
        // ignored and the request does not correlate to our in-flight digest.
        let attack = json!({ "signingRequest": {
            "notes": STANDARD.encode(ours),
            "rawMessage": { "content": STANDARD.encode([9u8; 32]) }
        }})
        .to_string();
        let signed = signed_digest_from_callback(attack.as_bytes());
        assert_eq!(signed, Some([9u8; 32]));
        assert!(
            !signed.is_some_and(|d| set.contains(&d)),
            "decoy digest in a non-content field must not be approved"
        );

        // Reject when the body is not JSON or has no raw-message content.
        assert_eq!(signed_digest_from_callback(b"not json"), None);
        assert_eq!(
            signed_digest_from_callback(json!({ "foo": "bar" }).to_string().as_bytes()),
            None
        );
    }

    // Minimal protobuf encoders for building test SigningRequest bodies.
    fn pb_varint_enc(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                out.push(b | 0x80);
            } else {
                out.push(b);
                break;
            }
        }
        out
    }

    fn pb_len_delim(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = pb_varint_enc((field << 3) | 2);
        out.extend(pb_varint_enc(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn callback_decodes_protobuf_signing_request_content() {
        let digest = [7u8; 32];

        // RawMessage { from = "0xabc" (field 1), content = digest (field 2) }
        let mut raw_message = pb_len_delim(1, b"0xabc");
        raw_message.extend(pb_len_delim(2, &digest));

        // SigningRequest { uuid (field 1 string), status = 2 (field 2 varint),
        //   raw_message (field 25) } — serialized exactly as the client-signer POSTs.
        let mut body = pb_len_delim(1, b"uuid-xyz");
        body.extend([(2 << 3), 2]); // status varint = 2
        body.extend(pb_len_delim(25, &raw_message));

        assert_eq!(signed_digest_from_callback(&body), Some(digest));

        // Decoy in `from` (field 1), different signed `content` (field 2): we read
        // content, so extraction yields the real (different) payload, not the decoy.
        let mut decoy_rm = pb_len_delim(1, &digest); // from = our digest (decoy)
        decoy_rm.extend(pb_len_delim(2, &[9u8; 32])); // content = different
        let decoy_body = pb_len_delim(25, &decoy_rm);
        assert_eq!(signed_digest_from_callback(&decoy_body), Some([9u8; 32]));

        // Malformed/empty protobuf → reject.
        assert_eq!(signed_digest_from_callback(&[0xff, 0xff, 0xff]), None);
    }

    #[test]
    fn pending_guard_clears_on_scope_exit() {
        let pending: Pending = Arc::new(Mutex::new(HashSet::new()));
        {
            let _g = PendingGuard::new(pending.clone(), [1u8; 32]);
            assert!(pending.lock().unwrap().contains(&[1u8; 32]));
        }
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_guard_clears_when_signing_task_is_cancelled() {
        let pending: Pending = Arc::new(Mutex::new(HashSet::new()));
        let p = pending.clone();
        // A task that inserts the guard then parks mid-await, like a remote sign
        // call in flight when the poster aborts the batch.
        let handle = tokio::spawn(async move {
            let _g = PendingGuard::new(p, [2u8; 32]);
            std::future::pending::<()>().await;
        });
        // Let it run far enough to insert, then cancel it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(pending.lock().unwrap().contains(&[2u8; 32]));
        handle.abort();
        let _ = handle.await;
        assert!(
            pending.lock().unwrap().is_empty(),
            "cancelled task must not leak its pending digest"
        );
    }
}
