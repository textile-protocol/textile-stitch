// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Google Cloud KMS signer. The operator key is an `EC_SIGN_SECP256K1_SHA256`
//! key version in Cloud KMS, normally at the `HSM` protection level, so the
//! private half lives in Cloud HSM and never leaves it.
//!
//! Signing is one synchronous `asymmetricSign` call, the same shape as Turnkey.
//! The bot computes the EIP-712 / EIP-1559 digest itself and sends those 32
//! bytes as the request's `digest.sha256`. Cloud KMS signs the field as given
//! and does not hash it again, which is what lets a keccak digest through. The
//! answer is a DER signature with no recovery id, so [`finalize_signature`]
//! finds `v` by recovering against the operator address and rejects anything
//! that recovers elsewhere.
//!
//! Every request carries an OAuth access token. It is cached, and replaced by a
//! background fetch shortly before it expires, so a refresh never sits on the
//! signing path. Credentials are looked up in this order, first hit wins:
//!
//! 1. the file the panel names for this bot ([`SignerSecrets`]), and nothing
//!    else when it names one;
//! 2. `GCP_KMS_CREDENTIALS_FILE`, or the JSON inline in `GCP_KMS_CREDENTIALS`;
//! 3. `GOOGLE_APPLICATION_CREDENTIALS`;
//! 4. gcloud's application-default file (`gcloud auth application-default login`);
//! 5. the GCP metadata server, when the bot runs on GCP with a service account
//!    attached.
//!
//! A credentials file is a service-account key or gcloud's `authorized_user`
//! file. Workload Identity Federation (`external_account`) and impersonation
//! configs are refused by name rather than half-supported.
//!
//! The identity needs `roles/cloudkms.signerVerifier` on the key:
//! `cloudkms.cryptoKeyVersions.useToSign` to sign, and `viewPublicKey` for the
//! startup check that the key version really controls `operator_address`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use zeroize::{Zeroize, Zeroizing};

use super::{
    address_from_spki_der, env_path, finalize_der_signature, parse_address, GcpKmsConfig, Signer,
    SignerSecrets,
};

/// Credentials JSON inline. Secret; the `_FILE` variant is preferred, and the
/// container entrypoint files this one to disk before the bot starts.
const CREDENTIALS_ENV: &str = "GCP_KMS_CREDENTIALS";
/// Path to the credentials JSON. `pub(crate)` because the config writer and the
/// panel's container provisioning both name it.
pub(crate) const CREDENTIALS_FILE_ENV: &str = "GCP_KMS_CREDENTIALS_FILE";
/// Google's own variable for a credentials file, honoured after ours.
const ADC_ENV: &str = "GOOGLE_APPLICATION_CREDENTIALS";
/// Overrides the metadata server host, as Google's client libraries allow.
const METADATA_HOST_ENV: &str = "GCE_METADATA_HOST";
const DEFAULT_METADATA_HOST: &str = "metadata.google.internal";

pub(crate) const DEFAULT_API_BASE_URL: &str = "https://cloudkms.googleapis.com";

/// The only algorithm whose signatures Ethereum can recover. A P-256 key signs
/// just as happily, and every signature would recover to a random address.
const SECP256K1_ALGORITHM: &str = "EC_SIGN_SECP256K1_SHA256";

/// Narrowest scope that covers `asymmetricSign` and `getPublicKey`. Service
/// account tokens are asked for this scope alone, and so are metadata server
/// tokens, with `enforce_scopes=true` (without it the metadata server ignores
/// `scopes` and hands out every scope the account has). So a leaked token is
/// good for Cloud KMS and nothing else in the project. gcloud user credentials
/// keep whatever scope their refresh token was granted, normally
/// `cloud-platform`.
const KMS_SCOPE: &str = "https://www.googleapis.com/auth/cloudkms";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// Lifetime requested for a service-account token. Google's maximum.
const ASSERTION_TTL_SECS: u64 = 3600;
/// How far an assertion's `iat` is backdated, so a host clock running a little
/// fast doesn't get it refused as issued in the future.
const ASSERTION_CLOCK_SKEW_SECS: u64 = 30;

/// Hosts a credentials file may send its `token_uri` to. The service-account
/// assertion posted there is good for an hour of Cloud KMS access, so it only
/// goes to Google. `STITCH_ALLOW_CUSTOM_SIGNER_API=1` overrides, as it does
/// for the API host.
const TOKEN_HOSTS: &[&str] = &[
    "oauth2.googleapis.com",
    "accounts.google.com",
    "www.googleapis.com",
];

/// Start replacing a token this long before Google says it expires. The old one
/// keeps signing while the new one is fetched.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// Token requests per fetch. Transient failures are retried with doubling
/// backoff, 0.25 + 0.5 + 1 + 2 + 4 s: about 8 s, enough for a new GKE Pod's
/// metadata server to start answering. The fetch runs as its own task, so a
/// quote never waits on these retries; a refused credential isn't retried.
const TOKEN_ATTEMPTS: u32 = 6;
const TOKEN_BACKOFF: Duration = Duration::from_millis(250);
/// Stop sending a token this long before it expires, so none dies in flight.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// After a failed background refresh, wait this long before the next attempt
/// rather than retrying on every sign.
const REFRESH_RETRY: Duration = Duration::from_secs(10);

/// Attempts per Cloud KMS call. Retries cover a connection, request or reply
/// lost in transit, a stale token (401), the per-second Cloud HSM quota (429)
/// and transient 5xx. The RFQ engine bounds
/// the whole sign by its reply budget, so a retry never outlives a quote.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

pub struct GcpKmsSigner {
    client: KmsClient,
    operator_address: Address,
    max_concurrent_signs: usize,
}

impl GcpKmsSigner {
    /// Build from config and the credentials the environment (or the panel)
    /// points at, then run the startup checks in [`Self::checked`].
    pub async fn from_config_with(
        cfg: &GcpKmsConfig,
        secrets: &SignerSecrets,
    ) -> anyhow::Result<Self> {
        super::validate_signer_api_base_url("gcpkms", &cfg.api_base_url)?;
        validate_key_version(&cfg.key_version)?;
        let expected = parse_address(&cfg.operator_address)?;
        let source = TokenSource::locate(secrets.gcpkms_credentials_file.as_deref())?;
        let client = KmsClient::new(&cfg.api_base_url, &cfg.key_version, source);
        let (signer, key) = Self::checked(client, expected, cfg.max_concurrent_signs).await?;
        tracing::info!(
            key_version = %signer.client.key_version,
            protection_level = %key.protection_level,
            identity = %signer.client.auth.source().identity(),
            address = ?key.address,
            "Cloud KMS signer ready"
        );
        Ok(signer)
    }

    /// A `getPublicKey` round trip to check the key version controls
    /// `expected`, then one throwaway signature ([`super::prove_signing`]) to
    /// check these credentials may sign with it, not only read it. A wrong key,
    /// a missing IAM grant or an unreachable Cloud KMS stops the bot at startup
    /// with a message instead of turning into silent no-quotes at the first RFQ.
    async fn checked(
        client: KmsClient,
        expected: Address,
        max_concurrent_signs: usize,
    ) -> anyhow::Result<(Self, KeyInfo)> {
        let key = client.public_key().await?;
        anyhow::ensure!(
            key.address == expected,
            "Cloud KMS key version {} controls {:?}, but [signer].operator_address is {expected:?}. \
             Point key_version at the right key, or set operator_address to the key's address.",
            client.key_version,
            key.address
        );
        let signer = Self {
            client,
            operator_address: expected,
            max_concurrent_signs,
        };
        super::prove_signing(&signer).await.with_context(|| {
            format!(
                "Cloud KMS key version {} has the right address, but signing a test message \
                 with it failed",
                signer.client.key_version
            )
        })?;
        Ok((signer, key))
    }
}

#[async_trait]
impl Signer for GcpKmsSigner {
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        let der = self.client.sign(digest).await?;
        finalize_der_signature(digest, &der, self.operator_address).context("Cloud KMS signature")
    }

    fn address(&self) -> Address {
        self.operator_address
    }

    fn max_concurrent_signs(&self) -> usize {
        self.max_concurrent_signs
    }
}

/// What a panel Verify run proves, and how long one signature took.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedKey {
    /// The address the key version controls, confirmed by recovering a real
    /// signature rather than only read from the public key.
    pub address: String,
    /// `HSM`, `HSM_SINGLE_TENANT`, `SOFTWARE` or `EXTERNAL`, as Cloud KMS
    /// reports it, so the operator can see the key is the custody they meant.
    pub protection_level: String,
    /// One `asymmetricSign` round trip with a warm token: the number to hold
    /// against the venue's reply budget.
    pub latency_ms: u64,
}

/// Prove a key version and credentials work end to end, before a bot exists.
///
/// Reads the public key to learn the address, then signs a payload under its
/// own domain that authorises nothing (see
/// [`crate::protocol::typed_data::signer_check_payload`]). The signature has
/// to recover to that address, so success also proves the derivation. Costs
/// one Cloud KMS sign, no gas, no chain.
pub async fn verify(
    key_version: &str,
    credentials_json: &str,
    api_base_url: Option<&str>,
) -> anyhow::Result<VerifiedKey> {
    let base = api_base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_API_BASE_URL);
    super::validate_signer_api_base_url("gcpkms", base)?;
    validate_key_version(key_version)?;
    let source = TokenSource::from_json(credentials_json)?;
    let client = KmsClient::new(base, key_version, source);
    let key = client.public_key().await?;
    let probe = GcpKmsSigner {
        client,
        operator_address: key.address,
        max_concurrent_signs: 1,
    };
    let payload =
        crate::protocol::typed_data::signer_check_payload(B256::from(rand::random::<[u8; 32]>()));
    // The token is warm after `public_key`, so this times the sign alone.
    let started = Instant::now();
    probe
        .sign_typed(&payload)
        .await
        .context("Cloud KMS could not sign a test message with this key version")?;
    Ok(VerifiedKey {
        address: format!("{:?}", key.address),
        protection_level: key.protection_level,
        latency_ms: started.elapsed().as_millis() as u64,
    })
}

/// Check a credentials JSON before anything is written to disk: it parses, it
/// is a type this backend supports, and its token endpoint is Google's. No
/// network.
pub(crate) fn validate_credentials_json(raw: &str) -> anyhow::Result<()> {
    TokenSource::from_json(raw).map(|_| ())
}

/// Check `[signer].key_version` is a full key *version* resource name.
///
/// Cloud KMS signs with a specific version, so the key's own name (one level
/// up) is the likeliest mistake and gets its own message. Each id is held to
/// the characters Cloud KMS allows, because the name is spliced into the
/// request path.
pub fn validate_key_version(raw: &str) -> anyhow::Result<()> {
    const SHAPE: &str =
        "projects/PROJECT/locations/LOCATION/keyRings/RING/cryptoKeys/KEY/cryptoKeyVersions/N";
    let name = raw.trim();
    anyhow::ensure!(
        !name.contains("://"),
        "[signer].key_version is a resource name, not a URL: {SHAPE}"
    );
    let parts: Vec<&str> = name.split('/').collect();
    let literals = [
        "projects",
        "locations",
        "keyRings",
        "cryptoKeys",
        "cryptoKeyVersions",
    ];
    if parts.len() == 8 && parts[6] == "cryptoKeys" {
        anyhow::bail!(
            "[signer].key_version names a key, not a key version. Add the version, e.g. \
             {name}/cryptoKeyVersions/1"
        );
    }
    anyhow::ensure!(
        parts.len() == 10
            && parts
                .iter()
                .step_by(2)
                .zip(literals)
                .all(|(got, want)| *got == want),
        "[signer].key_version must look like {SHAPE}, got {name:?}"
    );
    let id_ok = |id: &str, extra: &[char]| {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || extra.contains(&c))
    };
    // Domain-scoped project ids look like `example.com:project`.
    anyhow::ensure!(
        id_ok(parts[1], &['.', ':']),
        "[signer].key_version has an invalid project id {:?}",
        parts[1]
    );
    for (label, id) in [
        ("location", parts[3]),
        ("key ring", parts[5]),
        ("key", parts[7]),
    ] {
        anyhow::ensure!(
            id_ok(id, &[]),
            "[signer].key_version has an invalid {label} {id:?}"
        );
    }
    anyhow::ensure!(
        !parts[9].is_empty() && parts[9].chars().all(|c| c.is_ascii_digit()),
        "[signer].key_version must end in a numeric version, got {:?}",
        parts[9]
    );
    Ok(())
}

/// Regional Cloud KMS endpoints are `<location>-cloudkms.googleapis.com`. Held
/// to a location's characters, so nothing but a Google host can match.
pub(crate) fn is_regional_api_host(host: &str) -> bool {
    host.to_ascii_lowercase()
        .strip_suffix("-cloudkms.googleapis.com")
        .is_some_and(|location| {
            !location.is_empty()
                && location
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// A Cloud KMS client bound to one key version.
struct KmsClient {
    http: reqwest::Client,
    base_url: String,
    key_version: String,
    auth: Auth,
}

/// What `getPublicKey` says about a key version.
struct KeyInfo {
    address: Address,
    protection_level: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicKeyResponse {
    pem: String,
    algorithm: String,
    #[serde(default)]
    protection_level: String,
}

impl KmsClient {
    fn new(base_url: &str, key_version: &str, source: TokenSource) -> Self {
        // Bounded, and refuses redirects so a bearer token is only ever sent
        // to the host that was validated.
        let http = crate::net::http_client();
        Self {
            auth: Auth::new(source, http.clone()),
            http,
            base_url: normalize_base_url(base_url),
            key_version: key_version.trim().to_string(),
        }
    }

    /// The key version's address, checked to be a secp256k1 key.
    async fn public_key(&self) -> anyhow::Result<KeyInfo> {
        let url = format!("{}/v1/{}/publicKey", self.base_url, self.key_version);
        let reply = self
            .call(Method::GET, &url, None)
            .await
            .context("reading the key version's public key")?;
        let key: PublicKeyResponse = serde_json::from_value(reply)
            .map_err(|_| anyhow!("Cloud KMS returned a public key response without a PEM"))?;
        anyhow::ensure!(
            key.algorithm == SECP256K1_ALGORITHM,
            "Cloud KMS key version {} uses {}, but an Ethereum key has to be {SECP256K1_ALGORITHM}. \
             Create one with --purpose asymmetric-signing \
             --default-algorithm ec-sign-secp256k1-sha256 --protection-level hsm.",
            self.key_version,
            key.algorithm
        );
        if !key.protection_level.starts_with("HSM") {
            tracing::warn!(
                key_version = %self.key_version,
                protection_level = %key.protection_level,
                "the Cloud KMS key is not HSM-protected"
            );
        }
        Ok(KeyInfo {
            address: address_from_pem(&key.pem)?,
            protection_level: key.protection_level,
        })
    }

    /// DER signature over `digest`.
    ///
    /// No CRC32C fields: the caller recovers the signature against the
    /// operator address over the digest it computed, which catches corruption
    /// in either direction and a wrong key besides.
    async fn sign(&self, digest: B256) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/v1/{}:asymmetricSign", self.base_url, self.key_version);
        let body = json!({ "digest": { "sha256": BASE64.encode(digest.as_slice()) } });
        let reply = self.call(Method::POST, &url, Some(&body)).await?;
        let signature = reply["signature"]
            .as_str()
            .ok_or_else(|| anyhow!("Cloud KMS answered without a signature"))?;
        BASE64
            .decode(signature)
            .context("Cloud KMS returned a signature that is not base64")
    }

    /// One Cloud KMS request, with the retries described at [`MAX_ATTEMPTS`].
    async fn call(&self, method: Method, url: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let mut attempt = 1;
        loop {
            let token = self.auth.bearer().await?;
            let mut request = self
                .http
                .request(method.clone(), url)
                .bearer_auth(token.as_str());
            if let Some(project) = self.auth.source().quota_project() {
                request = request.header("x-goog-user-project", project);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let (status, text) = match exchange(request).await {
                Ok(reply) => reply,
                Err(e) if is_transient(&e) && attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(RETRY_BACKOFF * attempt).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("calling Cloud KMS for {}", self.key_version))
                }
            };
            if status.is_success() {
                return serde_json::from_str(&text).map_err(|_| {
                    anyhow!(
                        "Cloud KMS returned a non-JSON body: {}",
                        google_error_message(&text)
                    )
                });
            }
            if attempt < MAX_ATTEMPTS && is_retryable(status) {
                if status == StatusCode::UNAUTHORIZED {
                    // Revoked or expired early: drop it and fetch a new one.
                    self.auth.invalidate(token.as_str());
                } else {
                    tokio::time::sleep(RETRY_BACKOFF * attempt).await;
                }
                attempt += 1;
                continue;
            }
            anyhow::bail!(
                "{}",
                explain_kms_error(status, &text, &self.key_version, self.auth.source())
            );
        }
    }
}

/// Send `request` and read the whole reply. One step, so a reply that breaks
/// partway through is retried like one that never connected.
async fn exchange(request: reqwest::RequestBuilder) -> reqwest::Result<(StatusCode, String)> {
    let response = request.send().await?;
    let status = response.status();
    Ok((status, response.text().await?))
}

/// A transport failure another attempt can fix: the connection couldn't be
/// made, the request or reply broke in transit, or the call timed out. Cloud
/// KMS calls change nothing, so a sign whose reply was lost is safe to send
/// again. A refused redirect or a request that can't be built fails the same
/// way every time.
///
/// `text()` reports a reply cut off partway as a decode error. This client
/// parses the JSON itself, so here that kind only means the body didn't arrive.
fn is_transient(e: &reqwest::Error) -> bool {
    !e.is_redirect()
        && !e.is_builder()
        && (e.is_connect() || e.is_timeout() || e.is_request() || e.is_body() || e.is_decode())
}

/// Strip a trailing slash and the `/v1` Google's REST reference shows, since
/// every path built here already starts with `/v1`.
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 429 | 500 | 502 | 503 | 504)
}

/// Google's message plus the fix for the failures an operator actually hits.
fn explain_kms_error(
    status: StatusCode,
    body: &str,
    key_version: &str,
    source: &TokenSource,
) -> String {
    let detail = google_error_message(body);
    // Google's messages often end in a full stop of their own.
    let detail = detail.trim_end_matches('.');
    let hint = match status.as_u16() {
        401 => format!(
            " Google refused the access token for {}. Check the credentials are current and \
             the service account is enabled.",
            source.identity()
        ),
        403 => format!(
            " If this is a permission error, grant {} roles/cloudkms.signerVerifier on the key \
             (signing needs useToSign; the startup check needs viewPublicKey).",
            source.identity()
        ),
        404 => " Check [signer].key_version is the full resource name, version included, in \
                the key's own location."
            .to_string(),
        429 => " The Cloud KMS quota for this project and region is used up; Cloud HSM signing \
                has a per-second limit. Lower [signer].max_concurrent_signs or ask Google for \
                more quota."
            .to_string(),
        _ => String::new(),
    };
    format!("Cloud KMS returned {status} for {key_version}: {detail}.{hint}")
}

/// `error.message` from a Google API error body, else the body itself, cut
/// short so an HTML error page can't flood the log.
fn google_error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v["error"]["message"]
            .as_str()
            .or_else(|| v["error_description"].as_str())
            .or_else(|| v["error"].as_str())
    });
    let text = message.unwrap_or(body).trim();
    match text.char_indices().nth(300) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// DER ECDSA signature to the canonical 65 bytes, bound to `expected`.
/// Ethereum address of a `getPublicKey` PEM (SubjectPublicKeyInfo).
fn address_from_pem(pem: &str) -> anyhow::Result<Address> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = BASE64
        .decode(body)
        .context("Cloud KMS returned a public key PEM that is not base64")?;
    address_from_spki_der(&der).context("Cloud KMS public key")
}

/// A cached access token. Handed out freely until `refresh_at`; after that
/// still handed out, while a background fetch replaces it, until `expires_at`.
struct CachedToken {
    value: Zeroizing<String>,
    refresh_at: Instant,
    expires_at: Instant,
}

/// A token fetch in flight, shared by every sign waiting on it.
type TokenFetch = Shared<BoxFuture<'static, Result<Zeroizing<String>, Arc<anyhow::Error>>>>;

#[derive(Default)]
struct TokenSlot {
    token: Option<CachedToken>,
    /// At most one fetch at a time. It runs as its own task and fills the slot
    /// itself, so a sign the RFQ engine abandons at its reply budget can't
    /// cancel the fetch halfway and leave the next sign to start over.
    pending: Option<TokenFetch>,
}

/// Token source plus a one-slot cache, shared with the task that refreshes it.
struct Auth {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    source: TokenSource,
    http: reqwest::Client,
    /// A std mutex, never held across an await.
    slot: std::sync::Mutex<TokenSlot>,
}

impl Auth {
    fn new(source: TokenSource, http: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(AuthInner {
                source,
                http,
                slot: std::sync::Mutex::new(TokenSlot::default()),
            }),
        }
    }

    fn source(&self) -> &TokenSource {
        &self.inner.source
    }

    /// A token to send now.
    ///
    /// Past `refresh_at` the cached token is still returned and one background
    /// fetch replaces it, so a refresh never sits on the signing path. A caller
    /// only waits when there is no usable token at all, and then on a fetch
    /// that outlives it.
    async fn bearer(&self) -> anyhow::Result<Zeroizing<String>> {
        let pending = {
            let mut slot = self.inner.lock();
            let now = Instant::now();
            let cached = slot
                .token
                .as_ref()
                .filter(|t| now < t.expires_at)
                .map(|t| (t.value.clone(), now < t.refresh_at));
            match cached {
                Some((value, true)) => return Ok(value),
                Some((value, false)) => {
                    // Dropping the handle doesn't stop the task; it fills the
                    // slot when it lands.
                    drop(AuthInner::fetch_once(&self.inner, &mut slot));
                    return Ok(value);
                }
                None => AuthInner::fetch_once(&self.inner, &mut slot),
            }
        };
        pending.await.map_err(|e| anyhow!("{e:#}"))
    }

    /// Forget `rejected` if it is still the cached token. A 401 on a token that
    /// another sign has already replaced says nothing about the new one.
    fn invalidate(&self, rejected: &str) {
        let mut slot = self.inner.lock();
        if slot
            .token
            .as_ref()
            .is_some_and(|t| t.value.as_str() == rejected)
        {
            slot.token = None;
        }
    }
}

impl AuthInner {
    fn lock(&self) -> MutexGuard<'_, TokenSlot> {
        // Nothing panics while holding it, and a poisoned slot is still usable.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The fetch in flight, or a new one.
    fn fetch_once(inner: &Arc<Self>, slot: &mut TokenSlot) -> TokenFetch {
        if let Some(pending) = &slot.pending {
            return pending.clone();
        }
        let task_inner = inner.clone();
        // The task can't take the lock until the caller, which holds it now,
        // has stored `pending`, so it can't clear `pending` before it is set.
        let task = tokio::spawn(async move {
            let fetched = task_inner.source.fetch(&task_inner.http).await;
            let mut slot = task_inner.lock();
            slot.pending = None;
            let now = Instant::now();
            match fetched {
                Ok(fresh) => {
                    let value = fresh.value.clone();
                    slot.token = Some(CachedToken {
                        value: fresh.value,
                        refresh_at: now + reuse_for(fresh.lifetime),
                        expires_at: now + usable_for(fresh.lifetime),
                    });
                    Ok(value)
                }
                Err(e) => {
                    // Keep serving a token that is still good, and try again
                    // in a while rather than on every sign.
                    if let Some(token) = slot.token.as_mut() {
                        token.refresh_at = (now + REFRESH_RETRY).min(token.expires_at);
                    }
                    Err(Arc::new(e))
                }
            }
        });
        let pending = async move {
            task.await.unwrap_or_else(|e| {
                Err(Arc::new(anyhow!("the Google token fetch task failed: {e}")))
            })
        }
        .boxed()
        .shared();
        slot.pending = Some(pending.clone());
        pending
    }
}

/// How long to reuse a token Google says lasts `lifetime`: up to
/// [`TOKEN_REFRESH_MARGIN`] before it expires, or half its life when it is too
/// short for that (the metadata server can hand out a token near its end).
fn reuse_for(lifetime: Duration) -> Duration {
    if lifetime > TOKEN_REFRESH_MARGIN * 2 {
        lifetime - TOKEN_REFRESH_MARGIN
    } else {
        lifetime / 2
    }
}

/// How long a token Google says lasts `lifetime` may be sent at all:
/// [`TOKEN_EXPIRY_MARGIN`] short of it, or a quarter short for a token that is
/// nearly spent already.
fn usable_for(lifetime: Duration) -> Duration {
    lifetime.saturating_sub(TOKEN_EXPIRY_MARGIN.min(lifetime / 4))
}

struct FreshToken {
    value: Zeroizing<String>,
    lifetime: Duration,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Where access tokens come from.
enum TokenSource {
    /// A service-account key: sign a JWT assertion, trade it for a token.
    ServiceAccount {
        email: String,
        key_id: Option<String>,
        key: Arc<jsonwebtoken::EncodingKey>,
        token_uri: String,
    },
    /// gcloud's `authorized_user` file: trade the refresh token for a token.
    AuthorizedUser {
        client_id: String,
        client_secret: Zeroizing<String>,
        refresh_token: Zeroizing<String>,
        token_uri: String,
        quota_project: Option<String>,
    },
    /// The GCP metadata server, for a bot running on GCP with an attached
    /// service account. No secret on disk at all. `downscope` turns off for
    /// good if this metadata server refuses `enforce_scopes`.
    Metadata {
        base_url: String,
        downscope: AtomicBool,
    },
}

/// The fields of the credential types this backend reads. Secret fields are
/// wiped on drop, so a parse that fails halfway leaves nothing behind.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "type", default)]
    kind: String,
    client_email: Option<String>,
    private_key: Option<String>,
    private_key_id: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    quota_project_id: Option<String>,
}

impl Drop for CredentialsFile {
    fn drop(&mut self) {
        self.private_key.zeroize();
        self.client_secret.zeroize();
        self.refresh_token.zeroize();
    }
}

impl TokenSource {
    /// Find credentials in the order the module docs give.
    fn locate(file_override: Option<&Path>) -> anyhow::Result<Self> {
        // An explicit file is the whole answer. On a multi-bot panel the
        // environment holds nothing, or another bot's credentials.
        if let Some(path) = file_override {
            return Self::from_file(path);
        }
        if let Some(path) = env_path(CREDENTIALS_FILE_ENV) {
            return Self::from_file(&path);
        }
        if let Some(mut inline) = std::env::var(CREDENTIALS_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            let source = Self::from_json(&inline).context(CREDENTIALS_ENV);
            inline.zeroize();
            return source;
        }
        if let Some(path) = env_path(ADC_ENV) {
            return Self::from_file(&path);
        }
        if let Some(path) = gcloud_adc_file().filter(|p| p.is_file()) {
            return Self::from_file(&path);
        }
        let host = std::env::var(METADATA_HOST_ENV)
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_METADATA_HOST.to_string());
        Ok(TokenSource::metadata(format!("http://{host}")))
    }

    fn from_file(path: &Path) -> anyhow::Result<Self> {
        let mut raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading GCP credentials {}", path.display()))?;
        let source =
            Self::from_json(&raw).with_context(|| format!("GCP credentials {}", path.display()));
        raw.zeroize();
        source
    }

    fn from_json(raw: &str) -> anyhow::Result<Self> {
        let mut file: CredentialsFile = serde_json::from_str(raw.trim())
            .map_err(|_| anyhow!("the GCP credentials are not a JSON credentials file"))?;
        let required = |value: Option<String>, field: &str| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("the GCP {} credentials have no {field}", file.kind))
        };
        match file.kind.as_str() {
            "service_account" => {
                let mut pem = file.private_key.take().unwrap_or_default();
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|_| {
                    anyhow!(
                        "the service account's private_key is not an RSA PEM. Download a new \
                         JSON key for the service account and use the file as issued."
                    )
                });
                pem.zeroize();
                let token_uri = file
                    .token_uri
                    .clone()
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string());
                validate_token_uri(&token_uri)?;
                Ok(TokenSource::ServiceAccount {
                    email: required(file.client_email.clone(), "client_email")?,
                    key_id: file.private_key_id.clone().filter(|id| !id.is_empty()),
                    key: Arc::new(key?),
                    token_uri,
                })
            }
            "authorized_user" => {
                let token_uri = file
                    .token_uri
                    .clone()
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string());
                validate_token_uri(&token_uri)?;
                Ok(TokenSource::AuthorizedUser {
                    client_id: required(file.client_id.clone(), "client_id")?,
                    client_secret: Zeroizing::new(required(
                        file.client_secret.take(),
                        "client_secret",
                    )?),
                    refresh_token: Zeroizing::new(required(
                        file.refresh_token.take(),
                        "refresh_token",
                    )?),
                    token_uri,
                    quota_project: file.quota_project_id.clone().filter(|p| !p.is_empty()),
                })
            }
            "external_account" | "external_account_authorized_user" => anyhow::bail!(
                "Workload Identity Federation credentials are not supported by the Cloud KMS \
                 signer yet. Run the bot on GCP with a service account attached, or use a \
                 service-account JSON key."
            ),
            "impersonated_service_account" => anyhow::bail!(
                "impersonated service account credentials are not supported by the Cloud KMS \
                 signer. Use the target service account's own JSON key, or run the bot on GCP \
                 as that service account."
            ),
            "" => anyhow::bail!("the GCP credentials have no \"type\" field"),
            other => anyhow::bail!("unsupported GCP credentials type {other:?}"),
        }
    }

    fn metadata(base_url: String) -> Self {
        TokenSource::Metadata {
            base_url,
            downscope: AtomicBool::new(true),
        }
    }

    /// Who the tokens belong to, for logs and error messages. Never a secret.
    fn identity(&self) -> String {
        match self {
            TokenSource::ServiceAccount { email, .. } => format!("service account {email}"),
            TokenSource::AuthorizedUser { client_id, .. } => {
                format!("the gcloud user credentials (OAuth client {client_id})")
            }
            TokenSource::Metadata { .. } => {
                "the service account attached to this machine".to_string()
            }
        }
    }

    /// The project billed for a user's API calls. Google wants it named on
    /// requests made with user credentials; service accounts bill their own.
    fn quota_project(&self) -> Option<&str> {
        match self {
            TokenSource::AuthorizedUser { quota_project, .. } => quota_project.as_deref(),
            _ => None,
        }
    }

    /// A fresh access token, retrying what [`token_failure`] calls transient.
    async fn fetch(&self, http: &reqwest::Client) -> anyhow::Result<FreshToken> {
        let mut attempt = 0;
        loop {
            match self.fetch_once(http).await {
                Ok(token) => return Ok(token),
                Err((TokenFailure::ScopeRefused, e)) => {
                    // Keep the bot working on a metadata server that can't
                    // downscope: the account's own scopes still have to grant
                    // Cloud KMS, and its IAM roles still bound what it can do.
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "the metadata server won't issue a Cloud KMS-only token; \\
                         using the service account's default scopes"
                    );
                    if let TokenSource::Metadata { downscope, .. } = self {
                        downscope.store(false, Ordering::Relaxed);
                    }
                }
                Err((TokenFailure::Transient, _)) if attempt + 1 < TOKEN_ATTEMPTS => {
                    tokio::time::sleep(TOKEN_BACKOFF * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
                Err((_, e)) => return Err(e),
            }
        }
    }

    /// One token request, and how to treat it if it fails.
    async fn fetch_once(
        &self,
        http: &reqwest::Client,
    ) -> Result<FreshToken, (TokenFailure, anyhow::Error)> {
        let fatal = |e: anyhow::Error| (TokenFailure::Fatal, e);
        let mut scoped = false;
        let sent = match self {
            TokenSource::ServiceAccount { token_uri, .. } => {
                let assertion = self.assertion().await.map_err(fatal)?;
                http.post(token_uri)
                    .form(&[
                        ("grant_type", JWT_BEARER_GRANT),
                        ("assertion", assertion.as_str()),
                    ])
                    .send()
                    .await
            }
            TokenSource::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
                ..
            } => {
                http.post(token_uri)
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                        ("refresh_token", refresh_token.as_str()),
                    ])
                    .send()
                    .await
            }
            TokenSource::Metadata {
                base_url,
                downscope,
            } => {
                let mut request = http
                    .get(format!(
                        "{base_url}/computeMetadata/v1/instance/service-accounts/default/token"
                    ))
                    .header("Metadata-Flavor", "Google");
                if downscope.load(Ordering::Relaxed) {
                    scoped = true;
                    request = request.query(&[("scopes", KMS_SCOPE), ("enforce_scopes", "true")]);
                }
                request.send().await
            }
        };
        let metadata = matches!(self, TokenSource::Metadata { .. });
        let response = sent.map_err(|e| {
            let hint = if metadata {
                " No credentials file was configured, so the bot asked the GCP metadata \
                 server. Off GCP, set GCP_KMS_CREDENTIALS_FILE (or \
                 GOOGLE_APPLICATION_CREDENTIALS) to a service-account JSON key."
            } else {
                ""
            };
            let error = anyhow!(
                "could not reach Google for an access token for {}: {e}.{hint}",
                self.identity()
            );
            (TokenFailure::Transient, error)
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            (
                TokenFailure::Transient,
                anyhow!("reading the Google token response: {e}"),
            )
        })?;
        if !status.is_success() {
            let error = anyhow!(
                "Google refused an access token for {}: {status}: {}",
                self.identity(),
                google_error_message(&text)
            );
            return Err((token_failure(metadata, scoped, status), error));
        }
        let mut parsed: TokenResponse = serde_json::from_str(&text).map_err(|_| {
            fatal(anyhow!(
                "Google returned a token response without an access_token"
            ))
        })?;
        let value = Zeroizing::new(std::mem::take(&mut parsed.access_token));
        if value.is_empty() {
            return Err(fatal(anyhow!("Google returned an empty access token")));
        }
        Ok(FreshToken {
            value,
            lifetime: Duration::from_secs(parsed.expires_in.unwrap_or(ASSERTION_TTL_SECS)),
        })
    }

    /// The RS256 assertion a service account trades for a token.
    ///
    /// The RSA signature runs on a blocking thread, as the Fireblocks JWT does.
    /// It happens once an hour, but a few milliseconds on a reactor thread
    /// would still stall the session driving every other RFQ.
    async fn assertion(&self) -> anyhow::Result<String> {
        let TokenSource::ServiceAccount {
            email,
            key_id,
            key,
            token_uri,
        } = self
        else {
            anyhow::bail!("only a service account signs an assertion");
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("reading the clock for the Google assertion")?
            .as_secs();
        let claims = assertion_claims(email, token_uri, now);
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = key_id.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || jsonwebtoken::encode(&header, &claims, &key))
            .await
            .context("the Google assertion signing task panicked")?
            .map_err(|e| anyhow!("signing the Google service-account assertion: {e}"))
    }
}

/// What a failed token request means for the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenFailure {
    /// Worth retrying after a backoff.
    Transient,
    /// The metadata server refused `enforce_scopes`; ask again without it.
    ScopeRefused,
    /// The credential itself was refused. No retry fixes that.
    Fatal,
}

/// How to treat a token request's error status. The metadata server can 404
/// or 503 while a new GKE Pod's identity is still coming up or while its host
/// is in maintenance, and Google says to retry. A 400 to a downscoped metadata
/// request means this metadata server doesn't support `enforce_scopes`.
/// Anything else from a token endpoint is the credential being refused.
fn token_failure(metadata: bool, scoped: bool, status: StatusCode) -> TokenFailure {
    match status.as_u16() {
        429 | 500 | 502 | 503 | 504 => TokenFailure::Transient,
        404 if metadata => TokenFailure::Transient,
        400 if metadata && scoped => TokenFailure::ScopeRefused,
        _ => TokenFailure::Fatal,
    }
}

/// Claims for a service-account token request (Google's OAuth 2.0
/// server-to-server flow).
fn assertion_claims(email: &str, token_uri: &str, now: u64) -> Value {
    let issued = now.saturating_sub(ASSERTION_CLOCK_SKEW_SECS);
    json!({
        "iss": email,
        "scope": KMS_SCOPE,
        "aud": token_uri,
        "iat": issued,
        "exp": issued + ASSERTION_TTL_SECS,
    })
}

/// A credentials file's `token_uri` has to be a Google https endpoint.
fn validate_token_uri(raw: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(raw.trim())
        .with_context(|| format!("the GCP credentials have an invalid token_uri {raw:?}"))?;
    let host = parsed.host_str().unwrap_or_default();
    let google =
        parsed.scheme() == "https" && TOKEN_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h));
    if google {
        return Ok(());
    }
    if super::custom_signer_api_allowed() {
        tracing::warn!(
            token_uri = raw,
            "allowing a non-Google token_uri because STITCH_ALLOW_CUSTOM_SIGNER_API=1"
        );
        return Ok(());
    }
    anyhow::bail!(
        "the GCP credentials send tokens to {raw:?}, which is not a Google token endpoint \
         (allowed: {}). The assertion posted there grants Cloud KMS access, so it only goes to \
         Google. Set STITCH_ALLOW_CUSTOM_SIGNER_API=1 if you really mean it.",
        TOKEN_HOSTS.join(", ")
    )
}

/// Where `gcloud auth application-default login` writes its file.
fn gcloud_adc_file() -> Option<PathBuf> {
    const FILE: &str = "application_default_credentials.json";
    if let Some(dir) = env_path("CLOUDSDK_CONFIG") {
        return Some(dir.join(FILE));
    }
    if cfg!(windows) {
        env_path("APPDATA").map(|d| d.join("gcloud").join(FILE))
    } else {
        env_path("HOME").map(|d| d.join(".config").join("gcloud").join(FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{address_from_signing_key, parse_private_key, recover_address};
    use k256::ecdsa::signature::hazmat::PrehashSigner;
    use k256::ecdsa::Signature;
    use k256::pkcs8::EncodePublicKey;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const VERSION: &str =
        "projects/acme-mm/locations/us-east1/keyRings/stitch/cryptoKeys/operator/cryptoKeyVersions/1";
    const RSA_PEM: &str = include_str!("../setup/testdata/fireblocks-test-rsa.pem");

    fn service_account_json(token_uri: &str) -> String {
        json!({
            "type": "service_account",
            "project_id": "acme-mm",
            "private_key_id": "kid-1",
            "private_key": RSA_PEM,
            "client_email": "stitch@acme-mm.iam.gserviceaccount.com",
            "client_id": "1234",
            "token_uri": token_uri,
        })
        .to_string()
    }

    /// The PEM `getPublicKey` returns: SubjectPublicKeyInfo, 64-column base64.
    fn pem_of(key: &k256::ecdsa::SigningKey) -> String {
        let der = k256::PublicKey::from(*key.verifying_key())
            .to_public_key_der()
            .unwrap();
        let b64 = BASE64.encode(der.as_bytes());
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            lines.join("\n")
        )
    }

    #[test]
    fn a_full_key_version_name_is_accepted() {
        validate_key_version(VERSION).unwrap();
        validate_key_version(&format!("  {VERSION}\n")).unwrap();
        validate_key_version(
            "projects/example.com:acme/locations/global/keyRings/r_1/cryptoKeys/k-2/cryptoKeyVersions/12",
        )
        .unwrap();
    }

    #[test]
    fn a_key_name_without_its_version_says_what_to_add() {
        let key = VERSION.trim_end_matches("/cryptoKeyVersions/1");
        let err = validate_key_version(key).unwrap_err().to_string();
        assert!(err.contains("names a key, not a key version"), "{err}");
        assert!(err.contains("/cryptoKeyVersions/1"), "{err}");
    }

    #[test]
    fn malformed_key_versions_are_refused() {
        for bad in [
            "",
            "https://cloudkms.googleapis.com/v1/projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1",
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/",
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1:destroy",
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1/../2",
            "projects/p/locations/l/keyring/r/cryptoKeys/k/cryptoKeyVersions/1",
            "projects/p/locations/l?x=1/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1",
            "projects//locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1",
        ] {
            assert!(validate_key_version(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn only_google_kms_hosts_count_as_regional() {
        assert!(is_regional_api_host("us-east1-cloudkms.googleapis.com"));
        assert!(is_regional_api_host(
            "nam-eur-asia1-cloudkms.googleapis.com"
        ));
        for host in [
            "cloudkms.googleapis.com",
            "-cloudkms.googleapis.com",
            "evil.example-cloudkms.googleapis.com",
            "us-east1-cloudkms.googleapis.com.evil.example",
            "us-east1-cloudkms.example.com",
        ] {
            assert!(!is_regional_api_host(host), "{host} must not match");
        }
    }

    #[test]
    fn the_pem_resolves_to_the_keys_address() {
        let key = parse_private_key(KEY).unwrap();
        assert_eq!(
            address_from_pem(&pem_of(&key)).unwrap(),
            address_from_signing_key(&key)
        );
        assert!(
            address_from_pem("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----").is_err()
        );
    }

    #[test]
    fn a_service_account_key_parses() {
        let source = TokenSource::from_json(&service_account_json(DEFAULT_TOKEN_URI)).unwrap();
        assert_eq!(
            source.identity(),
            "service account stitch@acme-mm.iam.gserviceaccount.com"
        );
        assert!(source.quota_project().is_none());
    }

    #[test]
    fn gcloud_user_credentials_parse_and_carry_their_quota_project() {
        let raw = json!({
            "type": "authorized_user",
            "client_id": "cid.apps.googleusercontent.com",
            "client_secret": "shh",
            "refresh_token": "1//refresh",
            "quota_project_id": "acme-mm",
        })
        .to_string();
        let source = TokenSource::from_json(&raw).unwrap();
        assert_eq!(source.quota_project(), Some("acme-mm"));
        assert!(!source.identity().contains("shh"));
    }

    #[test]
    fn unsupported_or_broken_credentials_say_why() {
        let wif = json!({ "type": "external_account", "audience": "x" }).to_string();
        let err = TokenSource::from_json(&wif).err().unwrap().to_string();
        assert!(err.contains("Workload Identity Federation"), "{err}");

        let no_key = json!({ "type": "service_account", "client_email": "a@b" }).to_string();
        let err = TokenSource::from_json(&no_key).err().unwrap().to_string();
        assert!(err.contains("not an RSA PEM"), "{err}");

        let user = json!({ "type": "authorized_user", "client_id": "c" }).to_string();
        let err = TokenSource::from_json(&user).err().unwrap().to_string();
        assert!(err.contains("client_secret"), "{err}");

        assert!(TokenSource::from_json("not json").is_err());
        assert!(TokenSource::from_json("{}").is_err());
    }

    #[test]
    fn a_token_uri_off_google_is_refused() {
        let err = TokenSource::from_json(&service_account_json("https://evil.example/token"))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("not a Google token endpoint"), "{err}");
        assert!(TokenSource::from_json(&service_account_json(
            "http://oauth2.googleapis.com/token"
        ))
        .is_err());
        TokenSource::from_json(&service_account_json(
            "https://accounts.google.com/o/oauth2/token",
        ))
        .unwrap();
    }

    #[test]
    fn the_assertion_asks_for_the_kms_scope_only() {
        let claims = assertion_claims("a@b", DEFAULT_TOKEN_URI, 1_000);
        assert_eq!(claims["scope"], KMS_SCOPE);
        assert_eq!(claims["aud"], DEFAULT_TOKEN_URI);
        // Backdated for a fast clock, and still Google's one-hour maximum.
        assert_eq!(claims["iat"], 1_000 - ASSERTION_CLOCK_SKEW_SECS);
        assert_eq!(
            claims["exp"],
            1_000 - ASSERTION_CLOCK_SKEW_SECS + ASSERTION_TTL_SECS
        );
    }

    #[test]
    fn tokens_are_refreshed_ahead_of_expiry() {
        assert_eq!(
            reuse_for(Duration::from_secs(3600)),
            Duration::from_secs(3300)
        );
        assert_eq!(
            reuse_for(Duration::from_secs(400)),
            Duration::from_secs(200)
        );
        assert_eq!(reuse_for(Duration::ZERO), Duration::ZERO);
        // Sent until 30s before expiry, so a background refresh has the rest
        // of the margin to land.
        assert_eq!(
            usable_for(Duration::from_secs(3600)),
            Duration::from_secs(3570)
        );
        assert_eq!(usable_for(Duration::from_secs(20)), Duration::from_secs(15));
    }

    fn metadata_auth() -> Auth {
        Auth::new(
            TokenSource::metadata("http://127.0.0.1:9".into()),
            reqwest::Client::new(),
        )
    }

    #[test]
    fn only_transient_token_failures_are_retried() {
        use TokenFailure::*;
        // The metadata server starting up or in maintenance, and any 429/5xx.
        assert_eq!(
            token_failure(true, true, StatusCode::SERVICE_UNAVAILABLE),
            Transient
        );
        assert_eq!(token_failure(true, false, StatusCode::NOT_FOUND), Transient);
        assert_eq!(
            token_failure(false, false, StatusCode::TOO_MANY_REQUESTS),
            Transient
        );
        // A metadata server that can't downscope, but only when we asked it to.
        assert_eq!(
            token_failure(true, true, StatusCode::BAD_REQUEST),
            ScopeRefused
        );
        assert_eq!(token_failure(true, false, StatusCode::BAD_REQUEST), Fatal);
        // A refused credential (invalid_grant) and an OAuth 404 stay refused.
        assert_eq!(token_failure(false, false, StatusCode::BAD_REQUEST), Fatal);
        assert_eq!(token_failure(false, false, StatusCode::UNAUTHORIZED), Fatal);
        assert_eq!(token_failure(false, false, StatusCode::NOT_FOUND), Fatal);
    }

    /// A KMS client that takes its tokens from the fake's metadata server.
    fn metadata_client(google: &FakeGoogle) -> KmsClient {
        KmsClient::new(
            &google.base,
            VERSION,
            TokenSource::metadata(google.base.clone()),
        )
    }

    #[tokio::test]
    async fn a_metadata_server_still_starting_is_retried_and_asked_for_kms_only() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                token_failures: 2,
                token_failure: (503, "starting"),
                ..Fake::default()
            },
        )
        .await;
        let client = metadata_client(&google);
        assert_eq!(client.public_key().await.unwrap().address, expected);
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 3);
        let queries = google.metadata_queries.lock().unwrap();
        assert!(
            queries.iter().all(|q| q.contains("enforce_scopes=true")
                && q.contains("scopes=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloudkms")),
            "{queries:?}"
        );
    }

    #[tokio::test]
    async fn a_metadata_server_that_cant_downscope_falls_back_once() {
        let key = parse_private_key(KEY).unwrap();
        let google = fake_google(
            key,
            Fake {
                reject_enforce_scopes: true,
                ..Fake::default()
            },
        )
        .await;
        let client = metadata_client(&google);
        client.public_key().await.unwrap();
        // Refused downscoped, then the default token.
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 2);
        // The next fetch goes straight to the form this server accepts.
        client.auth.invalidate("test-token");
        client.public_key().await.unwrap();
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 3);
        let queries = google.metadata_queries.lock().unwrap();
        assert!(
            !queries.last().unwrap().contains("enforce_scopes"),
            "{queries:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_service_account_is_not_retried() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                token_failures: 1,
                token_failure: (400, "invalid_grant"),
                ..Fake::default()
            },
        )
        .await;
        let err = signer_against(&google, expected).await.err().unwrap();
        assert!(format!("{err:#}").contains("fake invalid_grant"), "{err:#}");
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_401_only_drops_the_token_it_was_for() {
        let auth = metadata_auth();
        let later = Instant::now() + Duration::from_secs(600);
        auth.inner.lock().token = Some(CachedToken {
            value: Zeroizing::new("new".into()),
            refresh_at: later,
            expires_at: later,
        });
        // Another sign already replaced the token this 401 was about.
        auth.invalidate("old");
        assert!(auth.inner.lock().token.is_some());
        auth.invalidate("new");
        assert!(auth.inner.lock().token.is_none());
    }

    #[test]
    fn errors_quote_google_and_name_the_fix() {
        let body = r#"{"error":{"code":403,"message":"Permission 'cloudkms.cryptoKeyVersions.useToSign' denied","status":"PERMISSION_DENIED"}}"#;
        let source = TokenSource::metadata("http://x".into());
        let msg = explain_kms_error(StatusCode::FORBIDDEN, body, VERSION, &source);
        assert!(msg.contains("useToSign' denied"), "{msg}");
        assert!(msg.contains("roles/cloudkms.signerVerifier"), "{msg}");
        let long = "x".repeat(5_000);
        assert!(google_error_message(&long).chars().count() <= 301);
    }

    /// A stand-in for Google: a token endpoint and the two Cloud KMS methods,
    /// backed by a local secp256k1 key.
    struct FakeGoogle {
        base: String,
        /// Requests to either token endpoint: OAuth `/token` or the metadata server.
        token_requests: Arc<AtomicUsize>,
        sign_requests: Arc<AtomicUsize>,
        /// The query string of every metadata token request.
        metadata_queries: Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// How the fake behaves. `fail_first_signs` sign calls answer `kms_status`
    /// before it starts signing; every token answer waits `token_delay`, and
    /// the first `token_failures` token requests answer `token_failure`.
    struct Fake {
        algorithm: &'static str,
        fail_first_signs: usize,
        kms_status: u16,
        /// The first this-many signs send half a reply, then drop the connection.
        cut_first_signs: usize,
        token_delay: Duration,
        token_failures: usize,
        token_failure: (u16, &'static str),
        /// A metadata server that refuses `enforce_scopes` with a 400.
        reject_enforce_scopes: bool,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                algorithm: SECP256K1_ALGORITHM,
                fail_first_signs: 0,
                kms_status: 200,
                cut_first_signs: 0,
                token_delay: Duration::ZERO,
                token_failures: 0,
                token_failure: (503, "unavailable"),
                reject_enforce_scopes: false,
            }
        }
    }

    async fn fake_google(key: k256::ecdsa::SigningKey, fake: Fake) -> FakeGoogle {
        use axum::extract::{RawQuery, State};
        use axum::http::{HeaderMap, StatusCode as Http};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Form, Json, Router};
        use k256::elliptic_curve::ff::PrimeField;

        #[derive(Clone)]
        struct S {
            key: k256::ecdsa::SigningKey,
            algorithm: &'static str,
            tokens: Arc<AtomicUsize>,
            signs: Arc<AtomicUsize>,
            fail_first: usize,
            status: u16,
            cut_first: usize,
            token_delay: Duration,
            token_failures: usize,
            token_failure: (u16, &'static str),
            reject_enforce_scopes: bool,
            metadata_queries: Arc<std::sync::Mutex<Vec<String>>>,
        }

        impl S {
            /// Count a token request; `Some` failure while it is one of the first
            /// `token_failures`.
            async fn token_request(&self) -> Option<axum::response::Response> {
                let n = self.tokens.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.token_delay).await;
                (n < self.token_failures).then(|| {
                    let (status, error) = self.token_failure;
                    (
                        Http::from_u16(status).unwrap(),
                        Json(
                            json!({ "error": error, "error_description": format!("fake {error}") }),
                        ),
                    )
                        .into_response()
                })
            }
        }

        let state = S {
            key,
            algorithm: fake.algorithm,
            tokens: Arc::new(AtomicUsize::new(0)),
            signs: Arc::new(AtomicUsize::new(0)),
            fail_first: fake.fail_first_signs,
            status: fake.kms_status,
            cut_first: fake.cut_first_signs,
            token_delay: fake.token_delay,
            token_failures: fake.token_failures,
            token_failure: fake.token_failure,
            reject_enforce_scopes: fake.reject_enforce_scopes,
            metadata_queries: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let token_requests = state.tokens.clone();
        let sign_requests = state.signs.clone();
        let metadata_queries = state.metadata_queries.clone();

        fn authorized(headers: &HeaderMap) -> bool {
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer test-token")
        }

        let app = Router::new()
            .route(
                "/token",
                post(
                    |State(s): State<S>, Form(form): Form<std::collections::HashMap<String, String>>| async move {
                        if let Some(failure) = s.token_request().await {
                            return failure;
                        }
                        assert_eq!(form["grant_type"], JWT_BEARER_GRANT);
                        // Header and claims are plain base64url JSON; the
                        // signature is RS256 under the test key.
                        let claims = form["assertion"].split('.').nth(1).unwrap().to_string();
                        let claims: Value = serde_json::from_slice(
                            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                                .decode(claims)
                                .unwrap(),
                        )
                        .unwrap();
                        assert_eq!(claims["iss"], "stitch@acme-mm.iam.gserviceaccount.com");
                        assert_eq!(claims["scope"], KMS_SCOPE);
                        Json(json!({ "access_token": "test-token", "expires_in": 3599, "token_type": "Bearer" }))
                            .into_response()
                    },
                ),
            )
            .route(
                "/computeMetadata/v1/instance/service-accounts/default/token",
                get(
                    |State(s): State<S>, headers: HeaderMap, RawQuery(query): RawQuery| async move {
                        assert_eq!(
                            headers.get("metadata-flavor").and_then(|v| v.to_str().ok()),
                            Some("Google")
                        );
                        let query = query.unwrap_or_default();
                        s.metadata_queries.lock().unwrap().push(query.clone());
                        if s.reject_enforce_scopes && query.contains("enforce_scopes") {
                            s.tokens.fetch_add(1, Ordering::SeqCst);
                            return (Http::BAD_REQUEST, "unknown parameter enforce_scopes")
                                .into_response();
                        }
                        if let Some(failure) = s.token_request().await {
                            return failure;
                        }
                        Json(json!({ "access_token": "test-token", "expires_in": 3599, "token_type": "Bearer" }))
                            .into_response()
                    },
                ),
            )
            .route(
                &format!("/v1/{VERSION}/publicKey"),
                get(|State(s): State<S>, headers: HeaderMap| async move {
                    if !authorized(&headers) {
                        return (Http::UNAUTHORIZED, Json(json!({}))).into_response();
                    }
                    Json(json!({
                        "pem": pem_of(&s.key),
                        "algorithm": s.algorithm,
                        "protectionLevel": "HSM",
                        "name": VERSION,
                    }))
                    .into_response()
                }),
            )
            .route(
                &format!("/v1/{VERSION}:asymmetricSign"),
                post(
                    |State(s): State<S>, headers: HeaderMap, Json(body): Json<Value>| async move {
                        let n = s.signs.fetch_add(1, Ordering::SeqCst);
                        if n < s.cut_first {
                            // Headers and half the JSON go out, then the stream
                            // fails: the connection drops partway through the reply.
                            use futures_util::StreamExt;
                            let chunks = futures_util::stream::iter([true, false]).then(|first| async move {
                                if first {
                                    Ok(axum::body::Bytes::from_static(b"{\"signature\": \""))
                                } else {
                                    tokio::time::sleep(Duration::from_millis(20)).await;
                                    Err(std::io::Error::other("connection lost"))
                                }
                            });
                            return axum::body::Body::from_stream(chunks).into_response();
                        }
                        if n < s.fail_first {
                            return (
                                Http::from_u16(s.status).unwrap(),
                                Json(json!({ "error": { "message": "try again" } })),
                            )
                                .into_response();
                        }
                        if !authorized(&headers) {
                            return (Http::UNAUTHORIZED, Json(json!({}))).into_response();
                        }
                        let digest = BASE64
                            .decode(body["digest"]["sha256"].as_str().unwrap())
                            .unwrap();
                        let sig: Signature = s.key.sign_prehash(&digest).unwrap();
                        // Hand back the high-s twin: Cloud KMS promises low-s,
                        // but the signer must not rely on it.
                        let (r, low_s) = sig.split_scalars();
                        let sig = Signature::from_scalars(r.to_repr(), (-(*low_s)).to_repr())
                            .unwrap();
                        Json(json!({
                            "signature": BASE64.encode(sig.to_der().as_bytes()),
                            "name": VERSION,
                            "protectionLevel": "HSM",
                        }))
                        .into_response()
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeGoogle {
            base: format!("http://{addr}"),
            token_requests,
            sign_requests,
            metadata_queries,
        }
    }

    /// A client for the fake, skipping the https/host checks the real
    /// constructors apply.
    fn client_against(google: &FakeGoogle) -> KmsClient {
        let TokenSource::ServiceAccount {
            email, key_id, key, ..
        } = TokenSource::from_json(&service_account_json(DEFAULT_TOKEN_URI)).unwrap()
        else {
            unreachable!()
        };
        let source = TokenSource::ServiceAccount {
            email,
            key_id,
            key,
            token_uri: format!("{}/token", google.base),
        };
        KmsClient::new(&google.base, VERSION, source)
    }

    /// A signer through the real startup checks.
    async fn signer_against(
        google: &FakeGoogle,
        expected: Address,
    ) -> anyhow::Result<GcpKmsSigner> {
        GcpKmsSigner::checked(client_against(google), expected, 8)
            .await
            .map(|(signer, _)| signer)
    }

    /// A signer that skips the startup checks, for tests about a later sign.
    fn signer_after_startup(google: &FakeGoogle, expected: Address) -> GcpKmsSigner {
        GcpKmsSigner {
            client: client_against(google),
            operator_address: expected,
            max_concurrent_signs: 8,
        }
    }

    #[tokio::test]
    async fn signs_through_kms_and_reuses_one_token() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(key, Fake::default()).await;
        let signer = signer_against(&google, expected).await.unwrap();

        for byte in 0..4u8 {
            let digest = B256::repeat_byte(byte + 1);
            let sig = signer.sign_digest(digest).await.unwrap();
            assert_eq!(recover_address(digest, &sig).unwrap(), expected);
            // Low-s, whatever the fake handed back.
            let s = Signature::from_slice(&sig[..64]).unwrap();
            assert!(s.normalize_s().is_none(), "signature must be low-s");
        }
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 1);
        // Four quotes and the startup check.
        assert_eq!(google.sign_requests.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn a_busy_hsm_is_retried_within_the_attempt_budget() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                fail_first_signs: 2,
                kms_status: 429,
                ..Fake::default()
            },
        )
        .await;
        let signer = signer_after_startup(&google, expected);
        let digest = B256::repeat_byte(7);
        let sig = signer.sign_digest(digest).await.unwrap();
        assert_eq!(recover_address(digest, &sig).unwrap(), expected);
        assert_eq!(google.sign_requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_rejected_token_is_replaced_and_the_sign_retried() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                fail_first_signs: 1,
                kms_status: 401,
                ..Fake::default()
            },
        )
        .await;
        let signer = signer_after_startup(&google, expected);
        let digest = B256::repeat_byte(3);
        let sig = signer.sign_digest(digest).await.unwrap();
        assert_eq!(recover_address(digest, &sig).unwrap(), expected);
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 2);
        assert_eq!(google.sign_requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_token_due_for_refresh_keeps_signing_while_it_is_replaced() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                token_delay: Duration::from_millis(400),
                ..Fake::default()
            },
        )
        .await;
        let signer = signer_against(&google, expected).await.unwrap();
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 1);

        // Due for refresh, still valid.
        if let Some(token) = signer.client.auth.inner.lock().token.as_mut() {
            token.refresh_at = Instant::now() - Duration::from_secs(1);
        }
        let started = Instant::now();
        let digest = B256::repeat_byte(5);
        let sig = signer.sign_digest(digest).await.unwrap();
        assert_eq!(recover_address(digest, &sig).unwrap(), expected);
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the sign waited on the refresh: {:?}",
            started.elapsed()
        );

        // The refresh landed behind it, once.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 2);
        let refresh_at = signer
            .client
            .auth
            .inner
            .lock()
            .token
            .as_ref()
            .unwrap()
            .refresh_at;
        assert!(refresh_at > Instant::now() + Duration::from_secs(60));
    }

    #[tokio::test]
    async fn a_token_fetch_outlives_the_sign_that_started_it() {
        let key = parse_private_key(KEY).unwrap();
        let google = fake_google(
            key,
            Fake {
                token_delay: Duration::from_millis(200),
                ..Fake::default()
            },
        )
        .await;
        let TokenSource::ServiceAccount {
            email, key_id, key, ..
        } = TokenSource::from_json(&service_account_json(DEFAULT_TOKEN_URI)).unwrap()
        else {
            unreachable!()
        };
        let client = KmsClient::new(
            &google.base,
            VERSION,
            TokenSource::ServiceAccount {
                email,
                key_id,
                key,
                token_uri: format!("{}/token", google.base),
            },
        );

        // The RFQ engine gives up on the sign before the token arrives.
        let abandoned = tokio::time::timeout(Duration::from_millis(50), client.auth.bearer()).await;
        assert!(abandoned.is_err(), "the bearer should still be waiting");

        // The fetch finished anyway, and the next sign uses its token.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(client.auth.bearer().await.unwrap().as_str(), "test-token");
        assert_eq!(google.token_requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_named_credentials_file_is_the_whole_answer() {
        let dir = std::env::temp_dir().join(format!("stitch-gcpkms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gcpkms-credentials.json");
        std::fs::write(&file, service_account_json(DEFAULT_TOKEN_URI)).unwrap();
        let source = TokenSource::locate(Some(&file)).unwrap();
        assert!(matches!(source, TokenSource::ServiceAccount { .. }));
        // Unreadable is an error, never a fall-through to the environment.
        let missing = dir.join("absent.json");
        let err = TokenSource::locate(Some(&missing)).err().unwrap();
        assert!(format!("{err:#}").contains("absent.json"), "{err:#}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_v1_suffix_on_the_base_url_is_tolerated() {
        for raw in [
            "https://cloudkms.googleapis.com",
            "https://cloudkms.googleapis.com/",
            "https://cloudkms.googleapis.com/v1",
            " https://cloudkms.googleapis.com/v1/ ",
        ] {
            assert_eq!(normalize_base_url(raw), "https://cloudkms.googleapis.com");
        }
    }

    #[tokio::test]
    async fn a_key_that_can_be_read_but_not_signed_with_is_refused_at_startup() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                fail_first_signs: 1,
                kms_status: 403,
                ..Fake::default()
            },
        )
        .await;
        let err = format!(
            "{:#}",
            signer_against(&google, expected).await.err().unwrap()
        );
        assert!(err.contains("signing a test message"), "{err}");
        assert!(err.contains("roles/cloudkms.signerVerifier"), "{err}");
        assert_eq!(google.sign_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_reply_cut_off_partway_is_retried() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                cut_first_signs: 2,
                ..Fake::default()
            },
        )
        .await;
        let signer = signer_after_startup(&google, expected);
        let digest = B256::repeat_byte(4);
        let sig = signer.sign_digest(digest).await.unwrap();
        assert_eq!(recover_address(digest, &sig).unwrap(), expected);
        assert_eq!(google.sign_requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_permission_error_is_not_retried_and_names_the_role() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                fail_first_signs: 1,
                kms_status: 403,
                ..Fake::default()
            },
        )
        .await;
        // Revoked after startup.
        let signer = signer_after_startup(&google, expected);
        let err = signer
            .sign_digest(B256::repeat_byte(9))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("roles/cloudkms.signerVerifier"), "{err}");
        assert_eq!(google.sign_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_key_that_is_not_secp256k1_is_refused_at_startup() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let google = fake_google(
            key,
            Fake {
                algorithm: "EC_SIGN_P256_SHA256",
                ..Fake::default()
            },
        )
        .await;
        let err = signer_against(&google, expected)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("EC_SIGN_P256_SHA256"), "{err}");
    }

    #[tokio::test]
    async fn a_key_for_another_address_is_refused_at_startup() {
        let key = parse_private_key(KEY).unwrap();
        let google = fake_google(key, Fake::default()).await;
        let other = Address::repeat_byte(0xaa);
        assert!(signer_against(&google, other).await.is_err());
    }
}
