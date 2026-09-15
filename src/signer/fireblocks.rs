// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Fireblocks signer, built around `TYPED_MESSAGE` rather than raw signing.
//!
//! Fireblocks gates Raw Signing behind a commercial conversation: it is off by
//! default in production workspaces and only a Customer Success Manager can turn
//! it on. Typed message signing is not gated that way — the operator writes a
//! Typed Message policy rule in the console themselves and is done. So this
//! backend signs the EIP-712 *structure* (see [`crate::protocol::typed_data`])
//! and never asks Fireblocks to sign opaque bytes.
//!
//! That covers everything the quoting path needs: RFQ quotes, resting ladder
//! orders, the venue session handshake, and enrolment are all EIP-712. What it
//! does not cover is the EIP-1559 transaction hash in [`crate::chain::tx`] —
//! that is not typed data, so it genuinely needs raw signing. A bot configured
//! with `raw_signing = false` (the default) refuses those with an error that
//! says so, and [`crate::config`] rejects the combination up front rather than
//! at the first fill.
//!
//! Unlike MPCVault there is no sidecar to run: the API Co-Signer is workspace
//! infrastructure the operator's Fireblocks account already has, so from here it
//! is plain HTTPS, same as Turnkey.
//!
//! Signing is asynchronous — create a transaction, then poll it — which is the
//! one structural difference from Turnkey's single synchronous call, and the
//! reason [`FireblocksConfig::poll_interval_ms`] matters for the RFQ reply
//! budget.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{hex, Address, B256};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde_json::{json, Value};
use zeroize::Zeroize;

use super::{
    finalize_signature, parse_address, parse_hex32, FireblocksConfig, Signer, SignerSecrets,
};
use crate::protocol::typed_data::Eip712Payload;

/// The workspace API key (a UUID). An identifier, not a secret — it is useless
/// without the RSA key — so it rides in the environment inline, the same way
/// Turnkey's API *public* key does.
///
/// `pub(crate)` because the config writer and the panel's container
/// provisioning both have to name this variable, and a fourth spelling of the
/// same string is exactly the drift the shared secret-filename const exists to
/// prevent.
pub(crate) const API_KEY_ENV: &str = "FIREBLOCKS_API_KEY";
/// The workspace RSA private key, PEM. Secret; `_FILE` variant preferred.
const API_PRIVATE_KEY_ENV: &str = "FIREBLOCKS_API_PRIVATE_KEY";
const API_PRIVATE_KEY_FILE_ENV: &str = "FIREBLOCKS_API_PRIVATE_KEY_FILE";

/// Which asset a signing request is filed under. For typed messages this picks
/// the key format, not a network — one EVM vault account has the same address
/// on every EVM chain — so this covers every corridor and there is no
/// chain-to-asset table to keep current.
pub(crate) const DEFAULT_ASSET_ID: &str = "ETH";

/// Fireblocks' global endpoint. Regional workspaces override it; see
/// `FIREBLOCKS_API_HOSTS` in [`super`] for the full allowlist.
pub(crate) const DEFAULT_API_BASE_URL: &str = "https://api.fireblocks.io";

/// Poll hard. A quote has a ~750ms reply budget end to end, so the difference
/// between a 50ms and a 1s first poll is the difference between quoting and not.
pub(crate) const DEFAULT_POLL_INTERVAL_MS: u64 = 50;

/// How many polls stay at the configured interval before backing off. Covers
/// the window in which an RFQ quote could still use the answer.
const FAST_POLLS: u32 = 15;
/// Ceiling for the backed-off poll interval.
const MAX_POLL_INTERVAL_MS: u64 = 500;

/// A Fireblocks JWT must expire within 30s of issuance; leave headroom for
/// clock skew rather than sitting on the limit.
const JWT_TTL_SECS: u64 = 25;

/// Transaction statuses that mean the request is dead and polling should stop.
/// `BLOCKED` and `REJECTED` are the policy answers — the ones an operator who
/// has not written a Typed Message rule will actually hit.
const TERMINAL_FAILURES: &[&str] = &[
    "FAILED",
    "BLOCKED",
    "REJECTED",
    "CANCELLED",
    "CANCELLING",
    "TIMEOUT",
];

#[derive(Clone)]
pub struct FireblocksSigner {
    http: reqwest::Client,
    api_key: String,
    jwt_key: std::sync::Arc<jsonwebtoken::EncodingKey>,
    base_url: String,
    vault_account_id: String,
    asset_id: String,
    operator_address: Address,
    raw_signing: bool,
    poll_interval: Duration,
    poll_timeout: Duration,
    max_concurrent_signs: usize,
}

impl FireblocksSigner {
    pub fn from_config(cfg: &FireblocksConfig) -> anyhow::Result<Self> {
        Self::from_config_with(cfg, &SignerSecrets::default())
    }

    /// [`Self::from_config`] with an explicit secret file. See [`SignerSecrets`].
    pub fn from_config_with(
        cfg: &FireblocksConfig,
        secrets: &SignerSecrets,
    ) -> anyhow::Result<Self> {
        super::validate_signer_api_base_url("fireblocks", &cfg.api_base_url)?;
        // An explicit key is the whole answer. The panel knows which bot it is
        // building for and reads the key out of that bot's `stitch.env`; falling
        // back to the environment there would either fail or, worse, pick up a
        // globally inherited key belonging to a different workspace.
        let api_key = match secrets.fireblocks_api_key.as_deref().map(str::trim) {
            Some(key) if !key.is_empty() => key.to_string(),
            _ => super::read_env_secret("FIREBLOCKS_API_KEY_FILE", API_KEY_ENV)
                .context("Fireblocks API key")?,
        };
        anyhow::ensure!(!api_key.is_empty(), "{API_KEY_ENV} is empty");
        let jwt_key = std::sync::Arc::new(load_jwt_key(
            secrets.fireblocks_api_private_key_file.as_deref(),
        )?);
        let http = reqwest::Client::builder()
            // Bounded per request so a hung create can't outlive the poll
            // budget it is supposed to fit inside.
            .timeout(Duration::from_secs(cfg.poll_timeout_secs.max(1)))
            .build()
            .context("building the Fireblocks HTTP client")?;
        Ok(Self {
            http,
            api_key,
            jwt_key,
            base_url: normalize_base_url(&cfg.api_base_url),
            vault_account_id: cfg.vault_account_id.trim().to_string(),
            asset_id: cfg.asset_id.trim().to_string(),
            operator_address: parse_address(&cfg.operator_address)?,
            raw_signing: cfg.raw_signing,
            poll_interval: Duration::from_millis(cfg.poll_interval_ms.max(10)),
            poll_timeout: Duration::from_secs(cfg.poll_timeout_secs.max(1)),
            max_concurrent_signs: cfg.max_concurrent_signs,
        })
    }

    /// The address this signer controls, for callers that have not built the
    /// trait object yet (the panel's verify route).
    pub fn operator_address(&self) -> Address {
        self.operator_address
    }

    /// Stamp one request with the Fireblocks JWT and send it.
    ///
    /// The token is per-request: it commits to the path *and* a hash of the
    /// exact body, so it cannot be replayed against a different call.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let raw_body = body.map(|b| b.to_string()).unwrap_or_default();
        let token = self.jwt(path, &raw_body).await?;
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .header("X-API-Key", &self.api_key)
            .bearer_auth(token);
        if body.is_some() {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(raw_body);
        }
        let response = req
            .send()
            .await
            .with_context(|| format!("calling Fireblocks {path}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("reading the Fireblocks response for {path}"))?;
        if !status.is_success() {
            anyhow::bail!("Fireblocks {path} returned {status}: {}", text.trim());
        }
        serde_json::from_str(&text).with_context(|| {
            format!(
                "Fireblocks {path} returned a non-JSON body: {}",
                text.trim()
            )
        })
    }

    /// Build the RS256 token Fireblocks expects. `bodyHash` is the hex SHA-256
    /// of the body exactly as sent (the empty string for a GET).
    ///
    /// The RSA signature runs on a blocking thread. It is a few milliseconds of
    /// CPU at RSA-2048 and more at the 4096-bit keys Fireblocks' own quickstart
    /// generates, and it happens once per request — including every poll — so on
    /// a reactor thread it would stall the WebSocket session driving every
    /// *other* RFQ, for the same reason the network wait is bounded in
    /// `rfq::Engine::quote`. The token cannot be cached: Fireblocks wants a
    /// fresh `nonce`, and it commits to this request's path and body hash.
    async fn jwt(&self, path: &str, body: &str) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("reading the clock for the Fireblocks JWT")?
            .as_secs();
        let claims = json!({
            "uri": path,
            // Replay protection. Fireblocks only requires uniqueness, not
            // unpredictability, but a random draw gives both.
            "nonce": rand::random::<u64>(),
            "iat": now,
            "exp": now + JWT_TTL_SECS,
            "sub": self.api_key,
            "bodyHash": hex::encode(Sha256::digest(body.as_bytes())),
        });
        let key = self.jwt_key.clone();
        tokio::task::spawn_blocking(move || {
            jsonwebtoken::encode(
                &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
                &claims,
                &key,
            )
        })
        .await
        .context("the Fireblocks JWT signing task panicked")?
        .map_err(|e| anyhow!("signing the Fireblocks JWT: {e}"))
    }

    /// Create a signing transaction and poll it until a signature appears.
    async fn sign_via(&self, body: Value, what: &str) -> anyhow::Result<(RawSig, Duration)> {
        let started = Instant::now();
        let created = self
            .send(reqwest::Method::POST, "/v1/transactions", Some(&body))
            .await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| {
                anyhow!(
                    "Fireblocks did not return a transaction id for the {what} request: {created}"
                )
            })?
            .to_string();

        // A status the create call already reports as dead never becomes alive.
        if let Some(sig) = self.check_terminal(&created, what)? {
            return Ok((sig, started.elapsed()));
        }

        let deadline = started + self.poll_timeout;
        let mut interval = self.poll_interval;
        let mut polls: u32 = 0;
        loop {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "Fireblocks did not return a signature for the {what} request within {:?} \
                     (transaction {id}). If this is the RFQ quote path, the venue has already \
                     stopped listening — raise [signer].poll_timeout_secs only for diagnosis, \
                     not to make quoting work.",
                    self.poll_timeout
                );
            }
            tokio::time::sleep(interval).await;
            // Past the reply budget nobody can use this signature, but the
            // transaction already exists and Fireblocks rate-limits per
            // workspace per minute — so a stuck request shouldn't spend six
            // hundred polls (and six hundred RSA-signed tokens) discovering
            // that. Stay fast through the window a quote could still use, then
            // ramp.
            polls += 1;
            if polls >= FAST_POLLS {
                interval = (interval * 3 / 2).min(Duration::from_millis(MAX_POLL_INTERVAL_MS));
            }
            let tx = self
                .send(
                    reqwest::Method::GET,
                    &format!("/v1/transactions/{id}"),
                    None,
                )
                .await?;
            if let Some(sig) = self.check_terminal(&tx, what)? {
                return Ok((sig, started.elapsed()));
            }
        }
    }

    /// `Ok(Some(sig))` once the signature is there, `Ok(None)` while still in
    /// flight, `Err` on a status that will never produce one.
    ///
    /// The signature is taken as soon as it appears rather than waiting for
    /// `COMPLETED`. For a signing-only transaction there is nothing left to do
    /// after the co-signer produces it, and every skipped poll is latency we do
    /// not have. Safety does not rest on the status either way: the caller
    /// verifies the signature recovers to the configured operator address over
    /// the digest we computed.
    fn check_terminal(&self, tx: &Value, what: &str) -> anyhow::Result<Option<RawSig>> {
        if let Some(sig) = tx["signedMessages"]
            .as_array()
            .and_then(|m| m.first())
            .map(|m| &m["signature"])
            .filter(|s| !s.is_null())
        {
            return Ok(Some(RawSig::parse(sig)?));
        }
        let status = tx["status"].as_str().unwrap_or_default();
        if TERMINAL_FAILURES.contains(&status) {
            let sub = tx["subStatus"].as_str().unwrap_or_default();
            anyhow::bail!("{}", self.explain_failure(status, sub, what));
        }
        Ok(None)
    }

    /// Turn a Fireblocks rejection into something the operator can act on.
    ///
    /// A policy rejection is the failure every new Fireblocks bot hits, and the
    /// bare status does not hint at the fix, so name it.
    fn explain_failure(&self, status: &str, sub_status: &str, what: &str) -> String {
        let detail = if sub_status.is_empty() {
            String::new()
        } else {
            format!(" ({sub_status})")
        };
        let base = format!("Fireblocks refused the {what} request: {status}{detail}");
        if matches!(status, "BLOCKED" | "REJECTED") {
            format!(
                "{base}. This is normally the Transaction Authorization Policy: signing needs a \
                 rule that allows it for vault account {} and the API user this key belongs to. \
                 For typed-message signing add a Typed Message policy rule in the Fireblocks \
                 console; raw signing additionally has to be enabled on the workspace by \
                 Fireblocks before any rule will help.",
                self.vault_account_id
            )
        } else {
            base
        }
    }

    /// The shared `source` / `assetId` envelope both operations take.
    fn envelope(&self, operation: &str, messages: Value) -> Value {
        json!({
            "operation": operation,
            "assetId": self.asset_id,
            "source": { "type": "VAULT_ACCOUNT", "id": self.vault_account_id },
            "extraParameters": { "rawMessageData": { "messages": messages } },
        })
    }
}

#[async_trait]
impl Signer for FireblocksSigner {
    /// Sign the EIP-712 structure via `TYPED_MESSAGE` — the path that needs no
    /// raw-signing entitlement. Fireblocks hashes the typed data itself; we
    /// verify what comes back against the digest we computed independently, so
    /// a disagreement between the two encodings fails here rather than
    /// producing a signature over something we did not mean.
    async fn sign_typed(&self, payload: &Eip712Payload) -> anyhow::Result<[u8; 65]> {
        let body = self.envelope(
            "TYPED_MESSAGE",
            json!([{ "content": payload.typed_data(), "type": "EIP712" }]),
        );
        let (sig, elapsed) = self.sign_via(body, "typed-message").await?;
        tracing::debug!(?elapsed, "Fireblocks typed-message signature");
        finalize_signature(
            payload.digest(),
            &sig.r,
            &sig.s,
            sig.v,
            self.operator_address,
        )
        .context("the Fireblocks typed-message signature did not match the digest the bot computed")
    }

    /// Opaque 32-byte digests — the EIP-1559 transaction hash, and nothing else
    /// the bot signs. Only reachable with `raw_signing = true`, because raw
    /// signing is the entitlement this backend exists to avoid needing.
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        anyhow::ensure!(
            self.raw_signing,
            "this bot signs with Fireblocks typed messages, which cannot sign an on-chain \
             transaction — a transaction hash is not EIP-712 typed data. Either send the \
             transaction from the Fireblocks console (Permit2 approvals are a one-time \
             ERC-20 approve), or ask Fireblocks to enable Raw Signing on the workspace and \
             set [signer].raw_signing = true."
        );
        let body = self.envelope(
            "RAW",
            json!([{ "content": hex::encode(digest.as_slice()) }]),
        );
        let (sig, elapsed) = self.sign_via(body, "raw").await?;
        tracing::debug!(?elapsed, "Fireblocks raw signature");
        finalize_signature(digest, &sig.r, &sig.s, sig.v, self.operator_address)
    }

    fn address(&self) -> Address {
        self.operator_address
    }

    fn max_concurrent_signs(&self) -> usize {
        self.max_concurrent_signs
    }
}

/// Credentials on their own, before a bot exists to attach them to.
///
/// The panel's setup flow has the API key and the RSA key but not yet a vault
/// account or an operator address — the whole point is to discover those rather
/// than make the operator type them. Every Fireblocks call needs the JWT stamp
/// (there is no read-only mode), so the same pair that will later sign is what
/// reads the vault list.
pub struct Discovery {
    signer: FireblocksSigner,
}

/// One vault account the operator can pick from.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredVault {
    pub id: String,
    pub name: String,
}

/// What a Verify run proves, and how long it took.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedSigner {
    /// The address the vault resolves to, confirmed by recovering a real
    /// signature rather than merely read back from the API.
    pub address: String,
    /// Round trip for one typed-message signature. The number that decides
    /// whether this signer can quote — see the RFQ reply budget.
    pub latency_ms: u64,
}

impl Discovery {
    /// Build a client from credentials alone. `operator_address` is not known
    /// yet, so it is parked at zero; nothing on the discovery paths reads it,
    /// and [`Self::verify`] establishes the real one by recovery.
    pub fn new(
        api_key: &str,
        api_private_key_pem: &str,
        api_base_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let base = api_base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_API_BASE_URL);
        super::validate_signer_api_base_url("fireblocks", base)?;
        let api_key = api_key.trim().to_string();
        anyhow::ensure!(!api_key.is_empty(), "the Fireblocks API key is required");
        let jwt_key = std::sync::Arc::new(parse_rsa_pem(api_private_key_pem)?);
        Ok(Self {
            signer: FireblocksSigner {
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
                    .build()
                    .context("building the Fireblocks HTTP client")?,
                api_key,
                jwt_key,
                base_url: normalize_base_url(base),
                // Not known until the operator picks a vault; `verify` fills
                // these in on the probe it builds.
                vault_account_id: String::new(),
                asset_id: DEFAULT_ASSET_ID.to_string(),
                operator_address: Address::ZERO,
                raw_signing: false,
                poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
                poll_timeout: Duration::from_secs(DISCOVERY_TIMEOUT_SECS),
                max_concurrent_signs: 1,
            },
        })
    }

    /// Every vault account in the workspace, so the panel can offer a dropdown
    /// instead of asking for an id.
    ///
    /// Follows the cursor to the end. The dropdown is presented as the complete
    /// list, so stopping at the first page would silently make any vault past
    /// the first hundred unreachable from the panel — with no hint the list was
    /// truncated.
    pub async fn vaults(&self) -> anyhow::Result<Vec<DiscoveredVault>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        // A workspace is not unbounded, but a malformed cursor that echoes
        // itself would be, so cap the walk rather than loop forever.
        for _ in 0..MAX_VAULT_PAGES {
            let path = match &after {
                Some(cursor) => format!(
                    "/v1/vault/accounts_paged?limit={VAULT_PAGE_LIMIT}&after={}",
                    percent_encode(cursor)
                ),
                None => format!("/v1/vault/accounts_paged?limit={VAULT_PAGE_LIMIT}"),
            };
            let page = self
                .signer
                .send(reqwest::Method::GET, &path, None)
                .await
                .context("listing Fireblocks vault accounts")?;
            let accounts = page["accounts"]
                .as_array()
                .ok_or_else(|| anyhow!("Fireblocks returned no vault account list; got {page}"))?;
            out.extend(accounts.iter().filter_map(parse_vault));

            // `paging.after` is absent (or empty) on the last page.
            let next = page["paging"]["after"]
                .as_str()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            match next {
                // A cursor that doesn't advance would page forever over the
                // same accounts; treat it as the end.
                Some(cursor) if Some(&cursor) != after.as_ref() => after = Some(cursor),
                _ => return Ok(out),
            }
        }
        Ok(out)
    }

    /// The EVM address a vault account holds.
    ///
    /// On EVM chains one vault account has a single address across every
    /// network, so this is the operator address for every corridor, not just
    /// the one `asset_id` names.
    pub async fn address(&self, vault_account_id: &str, asset_id: &str) -> anyhow::Result<Address> {
        let path = addresses_path(vault_account_id, asset_id);
        let page = self
            .signer
            .send(reqwest::Method::GET, &path, None)
            .await
            .with_context(|| {
                format!(
                    "reading the {asset_id} address of Fireblocks vault account \
                     {vault_account_id}"
                )
            })?;
        // `addresses_paged` wraps the list; the older endpoint returned a bare
        // array. Accept either so this does not break on an API version bump.
        let first = page["addresses"]
            .as_array()
            .or_else(|| page.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| {
                anyhow!(
                    "Fireblocks vault account {vault_account_id} has no {asset_id} wallet yet. \
                     Add the {asset_id} asset to that vault in the Fireblocks console, then \
                     try again."
                )
            })?;
        let raw = first["address"].as_str().ok_or_else(|| {
            anyhow!("Fireblocks returned no address for vault {vault_account_id}")
        })?;
        parse_address(raw)
    }

    /// Prove the whole path works, end to end, and time it.
    ///
    /// One real typed-message signature answers every question the setup form
    /// otherwise leaves open until the bot's first quote: the API key and RSA
    /// key are valid, the co-signer is online, the Typed Message policy rule
    /// exists, the vault resolves to the address we are about to write into
    /// `stitch.toml`, and how long a signature actually takes. It signs a
    /// payload that authorises nothing (see
    /// [`crate::protocol::typed_data::signer_check_payload`]), costs no gas and
    /// touches no chain.
    pub async fn verify(
        &self,
        vault_account_id: &str,
        asset_id: Option<&str>,
    ) -> anyhow::Result<VerifiedSigner> {
        let asset_id = asset_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_ASSET_ID);
        let address = self.address(vault_account_id, asset_id).await?;
        // Struct update rather than a hand-copied field list: a probe that
        // silently drifts from the signer it stands in for would report a
        // latency (or a success) that the real thing won't reproduce.
        let probe = FireblocksSigner {
            vault_account_id: vault_account_id.trim().to_string(),
            asset_id: asset_id.to_string(),
            operator_address: address,
            ..self.signer.clone()
        };
        let payload = crate::protocol::typed_data::signer_check_payload(B256::from(
            rand::random::<[u8; 32]>(),
        ));
        let started = Instant::now();
        // `sign_typed` verifies recovery against `address` internally, so
        // success *is* the proof that the vault controls it.
        probe
            .sign_typed(&payload)
            .await
            .context("Fireblocks could not sign a test message for this vault account")?;
        Ok(VerifiedSigner {
            address: format!("{address:?}"),
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
}

/// How long discovery waits on a call. Setup is interactive, so this is about
/// not hanging the panel, not about a quote deadline.
const DISCOVERY_TIMEOUT_SECS: u64 = 30;

/// `accounts_paged`'s documented default. Fireblocks publishes no maximum, so
/// asking for more than the documented value risks a 400 that would break vault
/// listing outright — worse than one extra round trip on a large workspace.
const VAULT_PAGE_LIMIT: usize = 200;
/// Enough for 250k vault accounts. The bound exists so a cursor bug cannot spin
/// the panel, not because a real workspace would approach it.
const MAX_VAULT_PAGES: usize = 500;

/// Strip the trailing slash, and the `/v1` Fireblocks' own docs include.
///
/// Every path this client builds already starts with `/v1`, but Fireblocks
/// publishes its endpoints as `https://api.fireblocks.io/v1`, so an operator
/// copying the base URL out of the docs into `stitch.toml` would produce
/// `/v1/v1/...` and a 404 on the first call. Accept both spellings rather than
/// make that their problem to debug.
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

/// Path of the vault-account address listing.
///
/// `addresses_paginated`, not `addresses_paged`. Fireblocks is not consistent
/// between the two endpoints — vault accounts really are `accounts_paged` — and
/// the wrong spelling 404s, which breaks Verify for every panel-created bot.
/// Extracted so the spelling is pinned by a test rather than by care.
fn addresses_path(vault_account_id: &str, asset_id: &str) -> String {
    format!(
        "/v1/vault/accounts/{}/{}/addresses_paginated?limit=1",
        vault_account_id.trim(),
        asset_id.trim()
    )
}

/// One account object from `accounts_paged`, or `None` if it has no usable id.
fn parse_vault(a: &Value) -> Option<DiscoveredVault> {
    // The id is a number in some responses and a string in others.
    let id = match &a["id"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some(DiscoveredVault {
        id,
        name: a["name"].as_str().unwrap_or("").to_string(),
    })
}

/// Percent-encode a pagination cursor for a query string.
///
/// Fireblocks cursors are opaque and have contained `+` and `=`; splicing one in
/// raw would decode to a different cursor server-side and silently restart or
/// skip a page. Only unreserved characters pass through untouched.
fn percent_encode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A provider signature before it is turned into the canonical 65 bytes.
#[derive(Debug)]
pub(crate) struct RawSig {
    pub(crate) r: [u8; 32],
    pub(crate) s: [u8; 32],
    pub(crate) v: Option<u8>,
}

impl RawSig {
    /// Parse `{ "r": …, "s": …, "v": … }`. Fireblocks sends r/s as hex strings
    /// (with or without `0x`) and v as a number, but v is only ever a hint —
    /// [`finalize_signature`] establishes the real parity by recovery.
    pub(crate) fn parse(sig: &Value) -> anyhow::Result<Self> {
        let hex_field = |name: &str| -> anyhow::Result<[u8; 32]> {
            let raw = sig[name]
                .as_str()
                .ok_or_else(|| anyhow!("Fireblocks signature is missing {name}: {sig}"))?;
            parse_hex32(raw).with_context(|| format!("Fireblocks signature {name}"))
        };
        let v = match &sig["v"] {
            Value::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
            Value::String(s) => super::parse_v(s),
            _ => None,
        };
        Ok(Self {
            r: hex_field("r")?,
            s: hex_field("s")?,
            v,
        })
    }
}

fn load_jwt_key(
    private_key_file: Option<&std::path::Path>,
) -> anyhow::Result<jsonwebtoken::EncodingKey> {
    let mut pem = super::read_secret(
        private_key_file,
        API_PRIVATE_KEY_FILE_ENV,
        API_PRIVATE_KEY_ENV,
    )
    .context("Fireblocks API private key")?;
    let key = parse_rsa_pem(&pem);
    pem.zeroize();
    key
}

/// Parse the workspace RSA key, with the one error message every caller wants.
///
/// Three places need this — building a signer, building a discovery client, and
/// validating before the config writer touches disk — and a half-pasted PEM is
/// the most likely setup mistake, so the guidance lives here rather than being
/// reworded at each site.
pub(crate) fn parse_rsa_pem(pem: &str) -> anyhow::Result<jsonwebtoken::EncodingKey> {
    jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|e| {
        anyhow!(
            "the Fireblocks API private key is not a usable RSA PEM ({e}). Paste the whole \
             fireblocks_secret.key file issued with the API key, including the \
             '-----BEGIN PRIVATE KEY-----' and END lines."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fireblocks spells these two endpoints differently and the wrong one 404s
    /// silently at setup time, so pin both against the published SDK routes.
    /// Fireblocks documents its base URL with `/v1` on the end; our paths carry
    /// their own. Both spellings must land on the same place, or the first call
    /// an operator makes after a manual config edit 404s.
    #[test]
    fn base_url_accepts_the_form_fireblocks_documents() {
        for raw in [
            "https://api.fireblocks.io",
            "https://api.fireblocks.io/",
            "https://api.fireblocks.io/v1",
            "https://api.fireblocks.io/v1/",
            "  https://api.fireblocks.io/v1  ",
        ] {
            assert_eq!(
                normalize_base_url(raw),
                "https://api.fireblocks.io",
                "{raw:?} must normalize to the bare host"
            );
        }
        // Regional and sandbox hosts normalize the same way.
        assert_eq!(
            normalize_base_url("https://sandbox-api.fireblocks.io/v1"),
            "https://sandbox-api.fireblocks.io"
        );
        assert_eq!(
            normalize_base_url("https://eu-api.fireblocks.io/v1/"),
            "https://eu-api.fireblocks.io"
        );
    }

    #[test]
    fn uses_the_endpoint_spellings_fireblocks_actually_serves() {
        let path = addresses_path(" 7 ", " ETH ");
        assert_eq!(path, "/v1/vault/accounts/7/ETH/addresses_paginated?limit=1");
        assert!(
            !path.contains("addresses_paged"),
            "addresses_paged is the wrong spelling and 404s: {path}"
        );
        // ...while the vault list really is the `_paged` one.
        assert_eq!(
            VAULT_PAGE_LIMIT, 200,
            "the documented default; no max is published"
        );
    }

    #[test]
    fn percent_encodes_the_characters_a_cursor_actually_contains() {
        // Fireblocks cursors are opaque base64-ish blobs. `+` splices into a
        // query string as a space and `=` as a separator, so both must escape
        // or the next page request asks for a different cursor than we got.
        assert_eq!(percent_encode("abc123"), "abc123");
        assert_eq!(percent_encode("a+b=c"), "a%2Bb%3Dc");
        assert_eq!(percent_encode("a/b?c&d"), "a%2Fb%3Fc%26d");
        // Unreserved characters are left alone rather than needlessly escaped.
        assert_eq!(percent_encode("-_.~"), "-_.~");
    }

    #[test]
    fn parses_a_vault_account_whatever_type_its_id_is() {
        assert_eq!(
            parse_vault(&json!({ "id": "7", "name": "Ops" }))
                .unwrap()
                .id,
            "7"
        );
        // Some responses send the id as a number.
        assert_eq!(parse_vault(&json!({ "id": 7 })).unwrap().id, "7");
        // A nameless vault is still selectable — the id is what matters.
        assert_eq!(parse_vault(&json!({ "id": 7 })).unwrap().name, "");
        // No usable id means no entry, rather than a blank row in the dropdown.
        assert!(parse_vault(&json!({ "name": "Ops" })).is_none());
    }

    #[test]
    fn parses_a_fireblocks_signature() {
        let sig = RawSig::parse(&json!({
            "r": "36c0b1b40bcd032c871ca176243f5ff7e603a9ce91ff8dae62d79ab8dee6817a",
            "s": "0x1f2e3d4c5b6a798807060504030201000f0e0d0c0b0a09080706050403020100",
            "v": 1,
        }))
        .expect("parses");
        assert_eq!(sig.r[0], 0x36, "r decodes with no 0x prefix");
        assert_eq!(sig.s[0], 0x1f, "s decodes with a 0x prefix");
        assert_eq!(sig.v, Some(1));
    }

    #[test]
    fn a_short_component_is_left_padded_not_rejected() {
        // Fireblocks trims leading zero bytes on occasion; parse_hex32 pads.
        let sig = RawSig::parse(&json!({ "r": "01", "s": "02", "v": 0 })).expect("parses");
        assert_eq!(sig.r[31], 1);
        assert_eq!(sig.s[31], 2);
    }

    #[test]
    fn a_missing_component_is_an_error_not_a_zero_signature() {
        let err = RawSig::parse(&json!({ "s": "02", "v": 0 })).expect_err("must fail");
        assert!(format!("{err:#}").contains('r'), "{err:#}");
    }
}
