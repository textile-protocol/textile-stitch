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

/// A contract call is a block, not a quote. Nothing is racing a 750ms reply
/// budget here, and every poll costs an RSA-signed token against a workspace
/// rate limit, so this is seconds where signing is milliseconds.
const CONTRACT_CALL_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait for a contract call to mine before handing the operator
/// back the transaction id. Generous enough to cover a slow chain and a policy
/// rule that asks a human, short enough that a browser request does not hang
/// on it indefinitely.
const CONTRACT_CALL_TIMEOUT: Duration = Duration::from_secs(180);
/// `/v1/blockchains` is a workspace-wide list, not a per-account one, so one
/// page covers every chain any real workspace has.
const BLOCKCHAIN_PAGE_LIMIT: usize = 200;
/// Cap the cursor walk rather than trust a cursor that echoes itself.
const MAX_BLOCKCHAIN_PAGES: usize = 20;

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

/// Which signing operation a request was for.
///
/// This is an enum rather than a label string because the policy remedy differs
/// by operation: a Typed Message rule does not authorize a `RAW` request, so
/// quoting the typed-message advice at a raw rejection sends the operator to
/// write a rule that cannot unblock them. Matching exhaustively means the next
/// operation added has to answer for itself instead of silently inheriting
/// another one's remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    TypedMessage,
    Raw,
    ContractCall,
}

impl Operation {
    /// The Transaction Authorization Policy rule that would let this request
    /// through.
    ///
    /// No branch suggests getting Raw Signing enabled. A raw rejection only
    /// reaches an operator who already set `raw_signing = true`, so it names the
    /// rule that covers the request they made; a signer that will sign arbitrary
    /// bytes has arbitrary transaction authority over the vault, and that is not
    /// something to recommend to someone who has not already chosen it.
    fn policy_remedy(self) -> &'static str {
        match self {
            Operation::TypedMessage => {
                "Add a Typed Message policy rule for it in the Fireblocks console."
            }
            Operation::Raw => {
                "A Typed Message rule does not cover this request: raw signing needs its own \
                 policy rule for that vault account."
            }
            // Same policy engine, different rule. Pointing a contract call at the
            // Typed Message rule sends the operator to a screen that is already
            // correct, so name the one that is actually missing.
            Operation::ContractCall => {
                "A contract call needs a Contract Call rule. Note this is NOT raw signing — a \
                 Contract Call rule needs no entitlement from Fireblocks, just a rule in the \
                 console."
            }
        }
    }
}

/// How the operator sees the operation named in an error.
impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Operation::TypedMessage => "typed-message",
            Operation::Raw => "raw",
            Operation::ContractCall => "contract-call",
        })
    }
}

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
    async fn sign_via(&self, body: Value, what: Operation) -> anyhow::Result<(RawSig, Duration)> {
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
    fn check_terminal(&self, tx: &Value, what: Operation) -> anyhow::Result<Option<RawSig>> {
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
    fn explain_failure(&self, status: &str, sub_status: &str, what: Operation) -> String {
        let detail = if sub_status.is_empty() {
            String::new()
        } else {
            format!(" ({sub_status})")
        };
        let base = format!("Fireblocks refused the {what} request: {status}{detail}");
        if !matches!(status, "BLOCKED" | "REJECTED") {
            return base;
        }
        format!(
            "{base}. This is normally the Transaction Authorization Policy: signing needs a \
             rule that allows it for vault account {} and the API user this key belongs to. \
             {}",
            self.vault_account_id,
            what.policy_remedy()
        )
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

    /// The Fireblocks asset id for an EVM chain, read from the workspace.
    ///
    /// Deliberately not a table of `56 => "BNB_BSC"` guesses. Fireblocks files
    /// a transaction under an `assetId`, and for a contract call that id *is*
    /// the network — get it wrong and the approve either bounces with
    /// `ENV_UNSUPPORTED_ASSET` or, far worse, lands on a chain the operator did
    /// not mean. `/v1/blockchains` carries `onchain.chainId` next to the
    /// `legacyId` the Transaction API wants, so the workspace answers the
    /// question and a chain Textile adds later needs no code change here.
    ///
    /// Mainnet and its testnet share a chain id nowhere, so matching on the id
    /// alone is unambiguous.
    pub async fn evm_asset_id(&self, chain_id: u64) -> anyhow::Result<String> {
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_BLOCKCHAIN_PAGES {
            let path = match &cursor {
                Some(c) => format!(
                    "/v1/blockchains?protocol=EVM&pageSize={BLOCKCHAIN_PAGE_LIMIT}&pageCursor={}",
                    percent_encode(c)
                ),
                None => format!("/v1/blockchains?protocol=EVM&pageSize={BLOCKCHAIN_PAGE_LIMIT}"),
            };
            let page = self
                .send(reqwest::Method::GET, &path, None)
                .await
                .context("listing the blockchains this Fireblocks workspace supports")?;
            let rows = page["data"]
                .as_array()
                .ok_or_else(|| anyhow!("Fireblocks returned no blockchain list; got {page}"))?;
            if let Some(id) = rows.iter().find_map(|b| match_chain(b, chain_id)) {
                return Ok(id);
            }
            match page["next"].as_str().filter(|c| !c.is_empty()) {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        anyhow::bail!(
            "this Fireblocks workspace lists no EVM blockchain with chain id {chain_id}. A \
             Sandbox is testnet-only, so a mainnet corridor cannot be sent from one; otherwise \
             the chain may not be enabled on the workspace."
        )
    }

    /// Have Fireblocks build, sign and broadcast one EVM contract call.
    ///
    /// This is not a [`Signer`] method and cannot be one. Everywhere else the
    /// bot builds a transaction, signs the hash, and broadcasts through its own
    /// RPC — which is exactly what typed-message signing cannot do. A
    /// `CONTRACT_CALL` inverts the flow: Fireblocks builds, prices, nonces,
    /// signs and broadcasts it, and hands back a hash. That needs no raw
    /// signing entitlement, which is the whole point, but it also means the
    /// caller never sees a signature and the `Signer` trait has nowhere to put
    /// it. So this hangs off the concrete client, for the panel to drive.
    ///
    /// Waits for `COMPLETED` rather than taking `txHash` the moment it appears:
    /// the caller's next move is to re-read the allowance, and a hash from
    /// `BROADCASTING` would have it read a chain that has not applied the call.
    pub async fn contract_call(
        &self,
        asset_id: &str,
        to: Address,
        calldata: &[u8],
        note: &str,
    ) -> anyhow::Result<SentTransaction> {
        let body = self.contract_call_body(asset_id, to, calldata, note);
        let started = Instant::now();
        let created = self
            .send(reqwest::Method::POST, "/v1/transactions", Some(&body))
            .await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| {
                anyhow!(
                    "Fireblocks did not return a transaction id for the contract call: {created}"
                )
            })?
            .to_string();
        if let Some(hash) = self.settled(&created, &id)? {
            return Ok(SentTransaction {
                id,
                tx_hash: hash,
                elapsed: started.elapsed(),
            });
        }

        let deadline = started + CONTRACT_CALL_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "Fireblocks transaction {id} had not completed after {:?}. It may still be \
                     in flight — check the Fireblocks console before sending it again. The usual \
                     cause is a Contract Call policy rule that routes to a human approver rather \
                     than auto-approving.",
                    CONTRACT_CALL_TIMEOUT
                );
            }
            tokio::time::sleep(CONTRACT_CALL_POLL_INTERVAL).await;
            let tx = self
                .send(
                    reqwest::Method::GET,
                    &format!("/v1/transactions/{id}"),
                    None,
                )
                .await?;
            if let Some(hash) = self.settled(&tx, &id)? {
                return Ok(SentTransaction {
                    id,
                    tx_hash: hash,
                    elapsed: started.elapsed(),
                });
            }
        }
    }

    /// The request body, split out so the shape can be asserted without a
    /// server. `assetId` here is the *chain*, unlike the signing paths where it
    /// only picks a key format.
    fn contract_call_body(
        &self,
        asset_id: &str,
        to: Address,
        calldata: &[u8],
        note: &str,
    ) -> Value {
        json!({
            "operation": "CONTRACT_CALL",
            "assetId": asset_id,
            "source": { "type": "VAULT_ACCOUNT", "id": self.vault_account_id },
            "destination": {
                "type": "ONE_TIME_ADDRESS",
                "oneTimeAddress": { "address": format!("{to:#x}") },
            },
            // The call moves no native value; the approve is entirely calldata.
            "amount": "0",
            "note": note,
            "extraParameters": { "contractCallData": format!("0x{}", hex::encode(calldata)) },
        })
    }

    /// `Ok(Some(hash))` once the call is mined, `Ok(None)` while in flight,
    /// `Err` on a status that will never mine.
    fn settled(&self, tx: &Value, id: &str) -> anyhow::Result<Option<String>> {
        let status = tx["status"].as_str().unwrap_or_default();
        if TERMINAL_FAILURES.contains(&status) {
            let sub = tx["subStatus"].as_str().unwrap_or_default();
            anyhow::bail!(
                "{}",
                self.explain_failure(status, sub, Operation::ContractCall)
            );
        }
        if status != "COMPLETED" {
            return Ok(None);
        }
        tx["txHash"]
            .as_str()
            .filter(|h| !h.is_empty())
            .map(|h| Some(h.to_string()))
            .ok_or_else(|| {
                anyhow!("Fireblocks reported transaction {id} COMPLETED with no txHash: {tx}")
            })
    }
}

/// A contract call Fireblocks broadcast on the operator's behalf.
#[derive(Debug, Clone)]
pub struct SentTransaction {
    /// The Fireblocks transaction id, for looking the call up in the console.
    pub id: String,
    /// The on-chain hash, once mined.
    pub tx_hash: String,
    /// Submit to `COMPLETED`, for the operator to judge the policy by.
    pub elapsed: Duration,
}

/// The `legacyId` of a `/v1/blockchains` row, when it is the EVM chain asked for.
fn match_chain(blockchain: &Value, chain_id: u64) -> Option<String> {
    let onchain = blockchain.get("onchain")?;
    if onchain["protocol"].as_str()? != "EVM" {
        return None;
    }
    // Declared a string in the schema, but a number costs nothing to accept
    // and the alternative is a silent no-match.
    let listed = match &onchain["chainId"] {
        Value::String(s) => s.parse::<u64>().ok()?,
        Value::Number(n) => n.as_u64()?,
        _ => return None,
    };
    (listed == chain_id)
        .then(|| blockchain["legacyId"].as_str())?
        .map(str::to_string)
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
        let (sig, elapsed) = self.sign_via(body, Operation::TypedMessage).await?;
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
    /// the bot signs. Only reachable with `raw_signing = true`, which exists for
    /// operators who already hold the entitlement; the error below does not
    /// suggest acquiring it, because a signer that will sign arbitrary bytes has
    /// arbitrary transaction authority over the vault.
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        anyhow::ensure!(
            self.raw_signing,
            "this bot signs with Fireblocks typed messages, which cannot sign an on-chain \
             transaction — a transaction hash is not EIP-712 typed data. A Permit2 approval \
             does not come through here: the panel has Fireblocks send those as a contract \
             call. Anything that signs per fill should sign with a hot wallet instead."
        );
        let body = self.envelope(
            "RAW",
            json!([{ "content": hex::encode(digest.as_slice()) }]),
        );
        let (sig, elapsed) = self.sign_via(body, Operation::Raw).await?;
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

/// A vault's EVM address, and the asset wallet that produced it.
///
/// The asset matters as much as the address: Fireblocks files a signing request
/// under an `assetId`, and one that doesn't exist in this environment is
/// refused with `ENV_UNSUPPORTED_ASSET` — which is what a Sandbox does with
/// mainnet `ETH`. So whatever asset answered the address lookup is the asset
/// the signer has to keep using, and the one written into the config.
pub(crate) struct ResolvedVaultAddress {
    pub(crate) address: Address,
    pub(crate) asset_id: String,
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
    /// The asset wallet that produced it — and therefore the one the bot has to
    /// keep signing under. Returned so the panel can write it into the config
    /// instead of leaving the default to fail at runtime the way Verify would.
    pub asset_id: String,
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
    pub async fn address(
        &self,
        vault_account_id: &str,
        asset_id: &str,
    ) -> anyhow::Result<ResolvedVaultAddress> {
        // Fast path: the asset we were told to use.
        let asked = match self.evm_address_for(vault_account_id, asset_id).await {
            Ok(Some(address)) => {
                return Ok(ResolvedVaultAddress {
                    address,
                    asset_id: asset_id.trim().to_string(),
                })
            }
            Ok(None) => None,
            // A vault with no wallet for this asset may 404 rather than answer
            // with an empty list. Hold the error rather than raise it — the
            // fallback below usually turns it into a success, and if it doesn't
            // this is the more informative thing to report.
            Err(e) => Some(e),
        };

        // Fall back to whatever EVM wallet the vault *does* have.
        //
        // Every EVM asset in a vault account shares one address, so any of them
        // answers the question correctly — `ETH`, `ETH_TEST5`, `CELO`, whatever
        // the operator happened to add. Without this the panel insists on the
        // one asset id it guessed: a Sandbox workspace is testnet-only, cannot
        // hold mainnet `ETH` at all, and had no way to say so, because the form
        // never offered an asset field.
        if let Some(resolved) = self.any_evm_address(vault_account_id).await? {
            return Ok(resolved);
        }

        Err(match asked {
            Some(e) => e.context(format!(
                "Fireblocks vault account {vault_account_id} has no EVM wallet. Add an EVM asset \
                 to it in the Fireblocks console — Ethereum on a mainnet or testnet workspace, \
                 or a testnet asset such as ETH_TEST5 on a Sandbox — then try again."
            )),
            None => anyhow!(
                "Fireblocks vault account {vault_account_id} has no EVM wallet. Add an EVM asset \
                 to it in the Fireblocks console — Ethereum on a mainnet or testnet workspace, \
                 or a testnet asset such as ETH_TEST5 on a Sandbox — then try again."
            ),
        })
    }

    /// The EVM address of one asset wallet, or `None` if this vault has no
    /// wallet for that asset — or has one that isn't an EVM address at all.
    ///
    /// Non-EVM reads as absent on purpose: a BTC or SOL wallet cannot be the
    /// operator address, and `parse_address` rejecting it is exactly the test
    /// the caller wants when it is sweeping a vault's assets looking for one it
    /// can use.
    async fn evm_address_for(
        &self,
        vault_account_id: &str,
        asset_id: &str,
    ) -> anyhow::Result<Option<Address>> {
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
        // `addresses_paginated` wraps the list; the older endpoint returned a
        // bare array. Accept either so this does not break on a version bump.
        let Some(first) = page["addresses"]
            .as_array()
            .or_else(|| page.as_array())
            .and_then(|a| a.first())
        else {
            return Ok(None);
        };
        Ok(first["address"]
            .as_str()
            .and_then(|raw| parse_address(raw).ok()))
    }

    /// Sweep the vault's own asset list for something with an EVM address.
    ///
    /// Bounded: a vault can hold a lot of assets and we only need one, so stop
    /// at the first hit and cap the probes rather than walking everything.
    async fn any_evm_address(
        &self,
        vault_account_id: &str,
    ) -> anyhow::Result<Option<ResolvedVaultAddress>> {
        let account = self
            .signer
            .send(
                reqwest::Method::GET,
                &format!("/v1/vault/accounts/{}", vault_account_id.trim()),
                None,
            )
            .await
            .with_context(|| format!("reading Fireblocks vault account {vault_account_id}"))?;
        let assets = account["assets"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for asset in assets.iter().take(MAX_ASSET_PROBES) {
            let Some(id) = asset["id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Ok(Some(address)) = self.evm_address_for(vault_account_id, id).await {
                tracing::debug!(asset = id, "resolved the vault address from its EVM wallet");
                return Ok(Some(ResolvedVaultAddress {
                    address,
                    asset_id: id.to_string(),
                }));
            }
        }
        Ok(None)
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
        let resolved = self.address(vault_account_id, asset_id).await?;
        // Sign under the asset that actually answered, not the one we asked
        // for. They differ whenever the fallback ran — a Sandbox holding
        // ETH_TEST5 rather than ETH — and filing the request under an asset the
        // environment doesn't have is refused outright.
        //
        // Struct update rather than a hand-copied field list: a probe that
        // silently drifts from the signer it stands in for would report a
        // latency (or a success) that the real thing won't reproduce.
        let probe = FireblocksSigner {
            vault_account_id: vault_account_id.trim().to_string(),
            asset_id: resolved.asset_id.clone(),
            operator_address: resolved.address,
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
            address: format!("{:?}", resolved.address),
            asset_id: resolved.asset_id,
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
/// How many of a vault's assets to probe when hunting for an EVM wallet. One
/// hit is all we need, and a vault holding more EVM-less assets than this is
/// not the operator wallet anyone meant to point at.
const MAX_ASSET_PROBES: usize = 20;

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
    use alloy_primitives::U256;
    use serde_json::json;

    /// A client with no working credentials, for asserting on request shapes.
    ///
    /// Nothing here reaches the network: the JWT key is never used by the body
    /// builders, and the failure messages read only the vault id.
    fn test_signer() -> FireblocksSigner {
        FireblocksSigner {
            http: reqwest::Client::new(),
            api_key: "test-key".into(),
            // A throwaway RSA key so the struct is constructible; unused here.
            jwt_key: std::sync::Arc::new(jsonwebtoken::EncodingKey::from_secret(b"unused")),
            base_url: DEFAULT_API_BASE_URL.to_string(),
            vault_account_id: "0".into(),
            asset_id: DEFAULT_ASSET_ID.to_string(),
            operator_address: Address::ZERO,
            raw_signing: false,
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
            poll_timeout: Duration::from_secs(1),
            max_concurrent_signs: 1,
        }
    }

    /// A `BLOCKED`/`REJECTED` answer has to name a rule that would actually
    /// unblock the request that was refused. `sign_digest` files its request as
    /// `RAW`, and a Typed Message rule does not authorize one, so quoting the
    /// typed-message remedy there leaves an operator writing a rule that cannot
    /// help while the taker and closer stay stuck.
    #[test]
    fn the_policy_remedy_matches_the_operation_that_was_refused() {
        let typed = Operation::TypedMessage.policy_remedy();
        assert!(typed.contains("Typed Message policy rule"), "{typed}");

        let raw = Operation::Raw.policy_remedy();
        assert!(
            raw.contains("raw signing needs its own"),
            "a raw rejection must point at the raw rule: {raw}"
        );
        assert!(
            !raw.contains("Add a Typed Message policy rule"),
            "a Typed Message rule does not authorize a RAW request: {raw}"
        );
        // A contract call is a third rule again — and explicitly not the
        // entitlement, which is the thing operators assume it needs.
        let call = Operation::ContractCall.policy_remedy();
        assert!(call.contains("Contract Call rule"), "{call}");
        assert!(call.contains("NOT raw signing"), "{call}");
        assert!(!call.contains("Typed Message"), "wrong rule: {call}");

        // No branch sends the operator off to buy Raw Signing.
        for remedy in [typed, raw, call] {
            assert!(!remedy.contains("Raw Signing"), "{remedy}");
        }
    }

    /// The operation is interpolated into every error the operator reads, so the
    /// enum has to render the same wire-ish names the messages always used.
    #[test]
    fn an_operation_renders_the_name_the_operator_sees() {
        assert_eq!(Operation::TypedMessage.to_string(), "typed-message");
        assert_eq!(Operation::Raw.to_string(), "raw");
        assert_eq!(Operation::ContractCall.to_string(), "contract-call");
    }

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

    /// A vault's asset wallets are mixed; only the EVM ones can be the operator
    /// address. `parse_address` rejecting a BTC or SOL address is the test, so
    /// those have to read as absent rather than as an error.
    /// The panel reads `assetId` off this and writes it into the config, so the
    /// wire name is a contract with `web/src/api.ts`, not an implementation
    /// detail.
    #[test]
    fn verify_reports_the_asset_it_signed_under() {
        let body = serde_json::to_value(VerifiedSigner {
            address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".into(),
            asset_id: "ETH_TEST5".into(),
            latency_ms: 312,
        })
        .unwrap();
        assert_eq!(body["assetId"], "ETH_TEST5");
        assert_eq!(body["latencyMs"], 312);
        assert!(body["address"].is_string());
    }

    #[test]
    fn only_an_evm_address_counts_as_a_hit() {
        let evm = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
        assert!(parse_address(evm).is_ok());
        for other in [
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
            "DdzFFzCqrht5W8DHMAHqfp2yUMiRDLhvJHhPQRLDTrvY",
            "",
        ] {
            assert!(
                parse_address(other).is_err(),
                "{other:?} is not an EVM address and must not be accepted"
            );
        }
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

    /// The chain id is what stops an approve landing on the wrong network, so
    /// the match is deliberately strict about what counts as a hit.
    #[test]
    fn resolves_a_chain_id_to_the_asset_id_fireblocks_files_under() {
        let bsc = json!({
            "legacyId": "BNB_BSC",
            "displayName": "BNB Smart Chain",
            "onchain": { "protocol": "EVM", "chainId": "56", "test": false },
        });
        assert_eq!(match_chain(&bsc, 56).as_deref(), Some("BNB_BSC"));
        assert_eq!(match_chain(&bsc, 1), None, "a different chain is not a hit");

        // The schema says string, so that is the shape to expect — but a number
        // costs nothing to accept and the alternative is a silent no-match that
        // reads as "your workspace doesn't have this chain".
        let numeric = json!({
            "legacyId": "ETH",
            "onchain": { "protocol": "EVM", "chainId": 1, "test": false },
        });
        assert_eq!(match_chain(&numeric, 1).as_deref(), Some("ETH"));

        // Non-EVM rows share the response and must never match: a chain id
        // means nothing on them.
        let solana = json!({
            "legacyId": "SOL",
            "onchain": { "protocol": "SOL", "chainId": "56", "test": false },
        });
        assert_eq!(match_chain(&solana, 56), None);

        // A row with no chain id at all (most non-EVM chains) is skipped, not
        // a panic.
        let bare = json!({ "legacyId": "BTC", "onchain": { "protocol": "BTC", "test": false } });
        assert_eq!(match_chain(&bare, 56), None);
    }

    /// The approve is a contract call, not a transfer: no value moves, the
    /// token is the destination, and the whole intent is in the calldata.
    #[test]
    fn a_contract_call_carries_no_value_and_targets_the_token() {
        let signer = test_signer();
        let token: Address = "0x55d398326f99059fF775485246999027B3197955"
            .parse()
            .unwrap();
        let permit2: Address = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
            .parse()
            .unwrap();
        let calldata = crate::closer::executor::encode_approve(permit2, U256::MAX);
        let body = signer.contract_call_body("BNB_BSC", token, &calldata, "note");

        assert_eq!(body["operation"], "CONTRACT_CALL");
        assert_ne!(
            body["operation"], "RAW",
            "raw signing is the thing we avoid"
        );
        assert_eq!(body["assetId"], "BNB_BSC", "the chain, not the key format");
        assert_eq!(body["source"]["type"], "VAULT_ACCOUNT");
        assert_eq!(body["destination"]["type"], "ONE_TIME_ADDRESS");
        assert_eq!(
            body["destination"]["oneTimeAddress"]["address"],
            "0x55d398326f99059ff775485246999027b3197955",
            "the token contract, never the spender"
        );
        assert_eq!(body["amount"], "0", "an approve sends no native value");

        let data = body["extraParameters"]["contractCallData"]
            .as_str()
            .expect("calldata is a hex string");
        assert!(
            data.starts_with("0x095ea7b3"),
            "approve(address,uint256): {data}"
        );
        assert!(
            data.contains("000000000022d473030f116ddee9f6b43ac78ba3"),
            "the spender is Permit2: {data}"
        );
        assert!(
            data.ends_with(&"f".repeat(64)),
            "an unlimited allowance: {data}"
        );
        assert!(
            body["extraParameters"].get("rawMessageData").is_none(),
            "rawMessageData belongs to RAW and TYPED_MESSAGE, not a contract call"
        );
    }

    /// A contract call that is refused by policy needs a different remedy from
    /// a refused signature, and pointing at the wrong one sends the operator to
    /// a console screen that is already correct.
    #[test]
    fn a_blocked_contract_call_names_the_rule_that_is_missing() {
        let signer = test_signer();
        let msg = signer.explain_failure("BLOCKED", "", Operation::ContractCall);
        assert!(msg.contains("Contract Call rule"), "{msg}");
        assert!(
            msg.contains("NOT raw signing"),
            "the entitlement is the thing operators assume they need: {msg}"
        );
        assert!(!msg.contains("Typed Message"), "wrong rule: {msg}");

        // The signing path keeps its own advice.
        let signing = signer.explain_failure("BLOCKED", "", Operation::TypedMessage);
        assert!(signing.contains("Typed Message policy rule"), "{signing}");

        // A non-policy failure gets no rule advice at all.
        let failed =
            signer.explain_failure("FAILED", "INSUFFICIENT_FUNDS", Operation::ContractCall);
        assert!(!failed.contains("Contract Call rule"), "{failed}");
        assert!(failed.contains("INSUFFICIENT_FUNDS"), "{failed}");
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
