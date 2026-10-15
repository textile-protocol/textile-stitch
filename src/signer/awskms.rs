// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! AWS KMS signer. The operator key is an asymmetric `ECC_SECG_P256K1` KMS key
//! with `SIGN_VERIFY` usage. AWS KMS generates and uses it inside its own HSMs,
//! and the private half never leaves them.
//!
//! Signing is one `Sign` call with `MessageType = DIGEST` and `ECDSA_SHA_256`.
//! The bot computes the 32-byte digest itself and KMS skips its own hashing
//! step, which is what lets a keccak digest through. The answer is DER with no
//! recovery id and no low-s guarantee, so it goes through
//! [`finalize_der_signature`], which normalizes it and only accepts a signature
//! that recovers to the operator address.
//!
//! Requests go through the official SDK, which owns SigV4 and the credential
//! chain. Credentials, first hit wins:
//!
//! 1. the credentials file the panel names for this bot ([`SignerSecrets`]),
//!    and nothing else when it names one;
//! 2. `AWS_KMS_CREDENTIALS_FILE`, or the same file's contents inline in
//!    `AWS_KMS_CREDENTIALS`;
//! 3. the SDK's default chain: `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`,
//!    the shared profile, web identity (EKS), the ECS task role, then the EC2
//!    instance role.
//!
//! A credentials file is the standard AWS INI format, read from its `[default]`
//! profile.
//!
//! The identity needs `kms:Sign` and `kms:GetPublicKey` on the key. The second
//! backs the startup check that the key really controls `operator_address`.
//!
//! The endpoint is always set explicitly: `https://kms.<region>.amazonaws.com`,
//! or a FIPS or VPC endpoint from `[signer].api_base_url`. That keeps an
//! `AWS_ENDPOINT_URL` in the environment from moving signing off AWS.

use std::path::Path;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use aws_config::profile::ProfileFileCredentialsProvider;
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sdk_kms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::types::{MessageType, SigningAlgorithmSpec};
use zeroize::Zeroizing;

use super::{
    address_from_spki_der, env_path, finalize_der_signature, parse_address, AwsKmsConfig, Signer,
    SignerSecrets,
};

/// The credentials file's contents inline. Secret; the `_FILE` variant is
/// preferred, and the container entrypoint files this one to disk.
const CREDENTIALS_ENV: &str = "AWS_KMS_CREDENTIALS";
/// Path to an AWS credentials file for this bot alone. `pub(crate)` because
/// the config writer and the panel's container provisioning both name it.
pub(crate) const CREDENTIALS_FILE_ENV: &str = "AWS_KMS_CREDENTIALS_FILE";

/// The only key spec whose signatures Ethereum can recover. A P-256 key signs
/// just as happily, and every signature would recover to a random address.
const SECP256K1_KEY_SPEC: &str = "ECC_SECG_P256K1";
const SIGN_VERIFY: &str = "SIGN_VERIFY";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds an on-chain leg's sign. The RFQ engine cuts a quote's sign off at
/// its reply budget well before this.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
/// Attempts per KMS call, retried by the SDK on throttling and transient
/// errors. A short first backoff, because a quote can only wait so long.
const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);

pub struct AwsKmsSigner {
    client: aws_sdk_kms::Client,
    key_id: String,
    operator_address: Address,
    max_concurrent_signs: usize,
}

impl AwsKmsSigner {
    /// Build from config and the credentials the environment (or the panel)
    /// points at, then run the startup checks in [`Self::checked`].
    pub async fn from_config_with(
        cfg: &AwsKmsConfig,
        secrets: &SignerSecrets,
    ) -> anyhow::Result<Self> {
        let key_id = cfg.key_id.trim();
        validate_key_id(key_id)?;
        let expected = parse_address(&cfg.operator_address)?;
        let region = key_region(key_id, cfg.region.as_deref())?;
        let credentials = Credentials::locate(secrets.awskms_credentials_file.as_deref())?;
        let origin = credentials.origin();
        let conf = sdk_config(region, credentials).await?;
        let region = conf
            .region()
            .map(|r| r.to_string())
            .context("AWS KMS needs a region: use the key ARN, or set [signer].region")?;
        let client = client_for(&conf, &endpoint_for(&region, cfg.api_base_url.as_deref())?);
        let signer = Self::checked(client, key_id, expected, cfg.max_concurrent_signs).await?;
        tracing::info!(
            key_id,
            region,
            credentials = %origin,
            address = ?expected,
            "AWS KMS signer ready"
        );
        Ok(signer)
    }

    /// A `GetPublicKey` round trip to check the key controls `expected`, then
    /// one throwaway signature ([`super::prove_signing`]) to check these
    /// credentials may sign with it, not only read it. A wrong key, a missing
    /// permission or unusable credentials stop the bot at startup with a message
    /// instead of turning into silent no-quotes at the first RFQ.
    async fn checked(
        client: aws_sdk_kms::Client,
        key_id: &str,
        expected: Address,
        max_concurrent_signs: usize,
    ) -> anyhow::Result<Self> {
        let address = public_key(&client, key_id).await?;
        anyhow::ensure!(
            address == expected,
            "AWS KMS key {key_id} controls {address:?}, but [signer].operator_address is \
             {expected:?}. Point key_id at the right key, or set operator_address to the key's \
             address."
        );
        let signer = Self {
            client,
            key_id: key_id.to_string(),
            operator_address: expected,
            max_concurrent_signs,
        };
        super::prove_signing(&signer).await.with_context(|| {
            format!("AWS KMS key {key_id} has the right address, but signing a test message with it failed")
        })?;
        Ok(signer)
    }
}

#[async_trait]
impl Signer for AwsKmsSigner {
    async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        let out = self
            .client
            .sign()
            .key_id(&self.key_id)
            .message(Blob::new(digest.as_slice()))
            .message_type(MessageType::Digest)
            .signing_algorithm(SigningAlgorithmSpec::EcdsaSha256)
            .send()
            .await
            .map_err(|e| explain(e, &self.key_id))?;
        let der = out
            .signature()
            .ok_or_else(|| anyhow!("AWS KMS answered without a signature"))?;
        finalize_der_signature(digest, der.as_ref(), self.operator_address)
            .context("AWS KMS signature")
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
    /// The address the key controls, confirmed by recovering a real signature
    /// rather than only read from the public key.
    pub address: String,
    /// One `Sign` round trip on warm credentials: the number to hold against
    /// the venue's reply budget.
    pub latency_ms: u64,
}

/// Prove a key and an access key work end to end, before a bot exists.
///
/// Reads the public key to learn the address, then signs a payload under its
/// own domain that authorises nothing (see
/// [`crate::protocol::typed_data::signer_check_payload`]). The signature has to
/// recover to that address, so success also proves the derivation. Costs one
/// KMS sign, no gas, no chain. Nothing here reads the panel host's own AWS
/// credentials or region.
pub async fn verify(
    key_id: &str,
    region: Option<&str>,
    api_base_url: Option<&str>,
    access_key_id: &str,
    secret_access_key: &str,
) -> anyhow::Result<VerifiedKey> {
    let key_id = key_id.trim();
    validate_key_id(key_id)?;
    let region = key_region(key_id, region)?
        .context("AWS KMS needs the key's region: paste the key ARN rather than its id or alias")?;
    let endpoint = endpoint_for(&region, api_base_url)?;
    let credentials = Credentials::File {
        contents: credentials_ini(access_key_id, secret_access_key)?,
        origin: "the access key given to Verify".to_string(),
    };
    let conf = sdk_config(Some(region), credentials).await?;
    let client = client_for(&conf, &endpoint);
    let address = public_key(&client, key_id).await?;
    let probe = AwsKmsSigner {
        client,
        key_id: key_id.to_string(),
        operator_address: address,
        max_concurrent_signs: 1,
    };
    let payload =
        crate::protocol::typed_data::signer_check_payload(B256::from(rand::random::<[u8; 32]>()));
    // Credentials are loaded and cached by `public_key`, so this times the sign.
    let started = Instant::now();
    probe
        .sign_typed(&payload)
        .await
        .context("AWS KMS could not sign a test message with this key")?;
    Ok(VerifiedKey {
        address: format!("{address:?}"),
        latency_ms: started.elapsed().as_millis() as u64,
    })
}

/// The AWS credentials file for an access key, as the panel stores it.
///
/// Both halves are held to the characters AWS issues, which also keeps a pasted
/// value from smuggling extra lines into the file.
pub(crate) fn credentials_ini(
    access_key_id: &str,
    secret_access_key: &str,
) -> anyhow::Result<Zeroizing<String>> {
    let access_key_id = access_key_id.trim();
    let secret_access_key = secret_access_key.trim();
    anyhow::ensure!(
        (16..=128).contains(&access_key_id.len())
            && access_key_id
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "that doesn't look like an AWS access key id (AKIA…)"
    );
    anyhow::ensure!(
        (16..=128).contains(&secret_access_key.len())
            && secret_access_key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '+' | '=')),
        "that doesn't look like an AWS secret access key"
    );
    Ok(Zeroizing::new(format!(
        "[default]\naws_access_key_id = {access_key_id}\naws_secret_access_key = {secret_access_key}\n"
    )))
}

/// Check `[signer].key_id` names a KMS key: a key ARN, an alias ARN, a key id
/// or `alias/<name>`. Light on purpose: KMS has the last word, and the value
/// travels in a JSON body rather than a URL.
pub fn validate_key_id(raw: &str) -> anyhow::Result<()> {
    let key_id = raw.trim();
    anyhow::ensure!(!key_id.is_empty(), "[signer].key_id is required");
    anyhow::ensure!(
        key_id.len() <= 2048 && !key_id.chars().any(char::is_whitespace),
        "[signer].key_id {key_id:?} is not a KMS key id, alias or ARN"
    );
    if key_id.starts_with("arn:") {
        region_from_arn(key_id).with_context(|| {
            format!(
                "[signer].key_id {key_id:?} is not a KMS key ARN \
                 (arn:aws:kms:REGION:ACCOUNT:key/ID or …:alias/NAME)"
            )
        })?;
    }
    Ok(())
}

/// The region a key ARN names, if it is one.
fn region_from_arn(key_id: &str) -> Option<String> {
    let parts: Vec<&str> = key_id.trim().splitn(6, ':').collect();
    match parts.as_slice() {
        ["arn", _partition, "kms", region, _account, resource]
            if is_region(region)
                && (resource.starts_with("key/") || resource.starts_with("alias/")) =>
        {
            Some(region.to_string())
        }
        _ => None,
    }
}

/// The key's region: the one its ARN names, else `[signer].region`. Both set
/// and different is a mistake to catch here rather than as a 400 later.
pub(crate) fn key_region(key_id: &str, configured: Option<&str>) -> anyhow::Result<Option<String>> {
    let configured = configured
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    if let Some(region) = &configured {
        anyhow::ensure!(
            is_region(region),
            "[signer].region {region:?} is not an AWS region"
        );
    }
    match (region_from_arn(key_id), configured) {
        (Some(from_arn), Some(region)) if from_arn != region => anyhow::bail!(
            "[signer].region is {region}, but the key ARN is in {from_arn}. Drop region, or fix it."
        ),
        (from_arn, region) => Ok(from_arn.or(region)),
    }
}

fn is_region(region: &str) -> bool {
    !region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The KMS endpoint to send to: the configured one, or the region's standard
/// endpoint. Either way it has to be an AWS KMS host.
fn endpoint_for(region: &str, configured: Option<&str>) -> anyhow::Result<String> {
    let url = match configured.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => {
            anyhow::ensure!(is_region(region), "{region:?} is not an AWS region");
            let domain = if region.starts_with("cn-") {
                "amazonaws.com.cn"
            } else {
                "amazonaws.com"
            };
            format!("https://kms.{region}.{domain}")
        }
    };
    super::validate_signer_api_base_url("awskms", &url)?;
    Ok(url)
}

/// AWS KMS endpoints: `kms.<region>` or `kms-fips.<region>`, or a VPC
/// endpoint `vpce-….kms.<region>.vpce`, under `amazonaws.com(.cn)`.
pub(crate) fn is_kms_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let Some(rest) = host
        .strip_suffix(".amazonaws.com")
        .or_else(|| host.strip_suffix(".amazonaws.com.cn"))
    else {
        return false;
    };
    let labels: Vec<&str> = rest.split('.').collect();
    match labels.as_slice() {
        [service, region] => matches!(*service, "kms" | "kms-fips") && is_region(region),
        [vpce, "kms", region, "vpce"] => {
            vpce.starts_with("vpce-") && is_region(vpce) && is_region(region)
        }
        _ => false,
    }
}

/// Where a bot's AWS credentials come from.
enum Credentials {
    /// An AWS credentials file's contents, read from `[default]` and nothing
    /// else, so a stray `AWS_PROFILE` or instance role can't stand in for it.
    File {
        contents: Zeroizing<String>,
        origin: String,
    },
    /// The SDK's default chain.
    DefaultChain,
}

impl Credentials {
    /// Find credentials in the order the module docs give.
    fn locate(file_override: Option<&Path>) -> anyhow::Result<Self> {
        if let Some(path) = file_override {
            return Self::read(path);
        }
        if let Some(path) = env_path(CREDENTIALS_FILE_ENV) {
            return Self::read(&path);
        }
        if let Some(inline) = std::env::var(CREDENTIALS_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(Credentials::File {
                contents: Zeroizing::new(inline),
                origin: CREDENTIALS_ENV.to_string(),
            });
        }
        Ok(Credentials::DefaultChain)
    }

    /// Read now, so an unreadable file fails at startup with its name.
    fn read(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading AWS credentials {}", path.display()))?;
        Ok(Credentials::File {
            contents: Zeroizing::new(contents),
            origin: format!("credentials file {}", path.display()),
        })
    }

    /// Where the credentials came from, for the startup log. Never a secret.
    fn origin(&self) -> String {
        match self {
            Credentials::File { origin, .. } => origin.clone(),
            Credentials::DefaultChain => "the AWS default credential chain".to_string(),
        }
    }
}

/// SDK config with Stitch's timeouts and retries, the given region (or the
/// SDK's own region lookup), and the given credentials.
async fn sdk_config(region: Option<String>, credentials: Credentials) -> anyhow::Result<SdkConfig> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .operation_timeout(OPERATION_TIMEOUT)
                .build(),
        )
        .retry_config(
            RetryConfig::standard()
                .with_max_attempts(MAX_ATTEMPTS)
                .with_initial_backoff(INITIAL_BACKOFF),
        );
    if let Some(region) = region {
        loader = loader.region(Region::new(region));
    }
    if let Credentials::File { contents, .. } = credentials {
        let files = EnvConfigFiles::builder()
            .with_contents(EnvConfigFileKind::Credentials, contents.as_str())
            .build();
        loader = loader.credentials_provider(
            ProfileFileCredentialsProvider::builder()
                .profile_files(files)
                .profile_name("default")
                .build(),
        );
    }
    Ok(loader.load().await)
}

/// A KMS client on `endpoint`, which the caller has validated.
fn client_for(conf: &SdkConfig, endpoint: &str) -> aws_sdk_kms::Client {
    let kms = aws_sdk_kms::config::Builder::from(conf)
        .endpoint_url(endpoint)
        .build();
    aws_sdk_kms::Client::from_conf(kms)
}

/// The key's address, checked to be a secp256k1 signing key.
async fn public_key(client: &aws_sdk_kms::Client, key_id: &str) -> anyhow::Result<Address> {
    let out = client
        .get_public_key()
        .key_id(key_id)
        .send()
        .await
        .map_err(|e| explain(e, key_id))
        .context("reading the key's public key")?;
    let spec = out.key_spec().map(|s| s.as_str()).unwrap_or("unknown");
    anyhow::ensure!(
        spec == SECP256K1_KEY_SPEC,
        "AWS KMS key {key_id} is {spec}, but an Ethereum key has to be {SECP256K1_KEY_SPEC}. \
         Create one with --key-spec ECC_SECG_P256K1 --key-usage SIGN_VERIFY."
    );
    let usage = out.key_usage().map(|u| u.as_str()).unwrap_or("unknown");
    anyhow::ensure!(
        usage == SIGN_VERIFY,
        "AWS KMS key {key_id} has key usage {usage}, but it has to be {SIGN_VERIFY}"
    );
    let der = out
        .public_key()
        .ok_or_else(|| anyhow!("AWS KMS returned no public key for {key_id}"))?;
    address_from_spki_der(der.as_ref()).context("AWS KMS public key")
}

/// `rust_log` with the AWS SDK held to warnings, unless it already says
/// something about the SDK's own crates.
///
/// The SDK logs its credential chain at info on every start, access key id
/// included. Not a secret, but it is noise in an operator's log and in every
/// log they paste into a support thread.
pub fn quiet_sdk_logs(rust_log: &str) -> String {
    let base = match rust_log.trim() {
        "" => "info",
        set => set,
    };
    if base.contains("aws_") {
        base.to_string()
    } else {
        format!("{base},aws_config=warn,aws_runtime=warn,aws_smithy_runtime=warn")
    }
}

/// The SDK's error, plus the fix for the failures an operator actually hits.
///
/// AWS's own `code: message` when there is one. Otherwise (network, timeout,
/// no credentials) the error's chain of causes, without the SDK's `Debug`
/// dump of the raw response that its own display helper appends.
fn explain<E, R>(err: SdkError<E, R>, key_id: &str) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    let what = match (err.code(), err.message()) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_string(),
        (None, _) => error_chain(&err),
    };
    let hint = kms_hint(err.code());
    // AWS's messages end in a full stop of their own.
    let what = what.trim_end_matches('.');
    anyhow!("AWS KMS request for {key_id} failed: {what}.{hint}")
}

/// An error and its sources, colon-separated, the way `{:#}` renders anyhow.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

fn kms_hint(code: Option<&str>) -> &'static str {
    match code {
        Some("AccessDeniedException") => {
            " Allow kms:Sign and kms:GetPublicKey on this key for the bot's identity, in the \
             key policy or in an IAM policy the key policy defers to."
        }
        Some("NotFoundException") => {
            " Check [signer].key_id, and that the region is the key's own."
        }
        Some("DisabledException") => " The key is disabled. Enable it before signing with it.",
        Some("KMSInvalidStateException") => {
            " The key can't sign in its current state; it may be pending deletion."
        }
        Some("InvalidKeyUsageException") => {
            " The key has to be ECC_SECG_P256K1 with SIGN_VERIFY usage."
        }
        Some("ThrottlingException") => {
            " The account's ECC request quota in this region is used up (1,000 a second by \
             default, shared by every ECC key). Lower [signer].max_concurrent_signs or ask AWS \
             for more."
        }
        Some(
            "UnrecognizedClientException"
            | "InvalidClientTokenId"
            | "InvalidSignatureException"
            | "IncompleteSignature"
            | "ExpiredTokenException"
            | "SignatureDoesNotMatch",
        ) => {
            " AWS refused the credentials. Check the access key is active and the secret \
             matches it."
        }
        None => {
            " If no credentials were loaded, set AWS_KMS_CREDENTIALS_FILE, or run the bot on \
             AWS with an IAM role attached."
        }
        Some(_) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{address_from_signing_key, parse_private_key, recover_address};
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use k256::ecdsa::signature::hazmat::PrehashSigner;
    use k256::ecdsa::Signature;
    use k256::elliptic_curve::ff::PrimeField;
    use k256::pkcs8::EncodePublicKey;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const KEY_ARN: &str =
        "arn:aws:kms:us-east-1:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab";
    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    #[test]
    fn key_ids_arns_and_aliases_are_accepted() {
        for ok in [
            KEY_ARN,
            "arn:aws:kms:eu-west-1:111122223333:alias/stitch-operator",
            "arn:aws-us-gov:kms:us-gov-west-1:111122223333:key/mrk-1234abcd",
            "1234abcd-12ab-34cd-56ef-1234567890ab",
            "alias/stitch-operator",
        ] {
            validate_key_id(ok).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
        }
        for bad in [
            "",
            "arn:aws:s3:::bucket",
            "arn:aws:kms:us-east-1:111122223333:secret/x",
            "key id with spaces",
        ] {
            assert!(validate_key_id(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn the_region_comes_from_the_arn_and_must_agree_with_config() {
        assert_eq!(
            key_region(KEY_ARN, None).unwrap().as_deref(),
            Some("us-east-1")
        );
        assert_eq!(
            key_region("alias/x", Some("eu-west-1")).unwrap().as_deref(),
            Some("eu-west-1")
        );
        assert_eq!(key_region("alias/x", None).unwrap(), None);
        assert_eq!(
            key_region(KEY_ARN, Some("us-east-1")).unwrap().as_deref(),
            Some("us-east-1")
        );
        let err = key_region(KEY_ARN, Some("us-west-2")).unwrap_err();
        assert!(err.to_string().contains("us-east-1"), "{err}");
        assert!(key_region("alias/x", Some("US East")).is_err());
    }

    #[test]
    fn the_default_endpoint_is_the_regions_kms_host() {
        assert_eq!(
            endpoint_for("us-east-1", None).unwrap(),
            "https://kms.us-east-1.amazonaws.com"
        );
        assert_eq!(
            endpoint_for("cn-north-1", None).unwrap(),
            "https://kms.cn-north-1.amazonaws.com.cn"
        );
        assert_eq!(
            endpoint_for(
                "us-east-1",
                Some("https://kms-fips.us-east-1.amazonaws.com/")
            )
            .unwrap(),
            "https://kms-fips.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn only_aws_kms_hosts_are_accepted() {
        for ok in [
            "kms.us-east-1.amazonaws.com",
            "kms-fips.us-gov-west-1.amazonaws.com",
            "kms.cn-north-1.amazonaws.com.cn",
            "vpce-0a1b2c3d-abcd1234.kms.us-east-1.vpce.amazonaws.com",
        ] {
            assert!(is_kms_host(ok), "{ok} must be accepted");
        }
        for bad in [
            "kms.us-east-1.amazonaws.com.evil.example",
            "kms.s3.us-east-1.amazonaws.com",
            "evil.kms.us-east-1.amazonaws.com",
            "sts.us-east-1.amazonaws.com",
            "kms.us-east-1.evil.example",
            "vpce-x.s3.us-east-1.vpce.amazonaws.com",
        ] {
            assert!(!is_kms_host(bad), "{bad} must be refused");
        }
        assert!(endpoint_for("us-east-1", Some("https://kms.evil.example")).is_err());
        assert!(endpoint_for("us-east-1", Some("http://kms.us-east-1.amazonaws.com")).is_err());
    }

    #[test]
    fn an_access_key_becomes_a_default_profile() {
        let ini = credentials_ini(ACCESS_KEY, SECRET_KEY).unwrap();
        assert!(ini.starts_with("[default]\n"));
        assert!(ini.contains(&format!("aws_access_key_id = {ACCESS_KEY}")));
        // A pasted value can't add lines to the file.
        assert!(credentials_ini(ACCESS_KEY, "abc\n[other]\nx = y").is_err());
        assert!(credentials_ini("akia lowercase", SECRET_KEY).is_err());
        assert!(credentials_ini("", SECRET_KEY).is_err());
    }

    #[test]
    fn the_sdk_is_held_to_warnings_unless_rust_log_names_it() {
        assert_eq!(
            quiet_sdk_logs(""),
            "info,aws_config=warn,aws_runtime=warn,aws_smithy_runtime=warn"
        );
        assert!(quiet_sdk_logs("debug,stitch_bot=trace").starts_with("debug,stitch_bot=trace,"));
        assert_eq!(
            quiet_sdk_logs("info,aws_config=debug"),
            "info,aws_config=debug"
        );
    }

    #[test]
    fn errors_name_the_fix() {
        assert!(kms_hint(Some("AccessDeniedException")).contains("kms:Sign"));
        assert!(kms_hint(Some("ThrottlingException")).contains("max_concurrent_signs"));
        assert!(kms_hint(None).contains("AWS_KMS_CREDENTIALS_FILE"));
        assert_eq!(kms_hint(Some("SomethingElse")), "");
    }

    #[test]
    fn a_named_credentials_file_is_read_at_startup() {
        let dir = std::env::temp_dir().join(format!("stitch-awskms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("awskms-credentials.ini");
        std::fs::write(
            &file,
            credentials_ini(ACCESS_KEY, SECRET_KEY).unwrap().as_str(),
        )
        .unwrap();
        assert!(matches!(
            Credentials::locate(Some(&file)).unwrap(),
            Credentials::File { .. }
        ));
        // Unreadable is an error, never a fall-through to the default chain.
        let err = Credentials::locate(Some(&dir.join("absent.ini")))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("absent.ini"), "{err:#}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stand-in for AWS KMS backed by a local secp256k1 key: the JSON 1.1
    /// protocol, dispatched on `X-Amz-Target`.
    struct FakeKms {
        base: String,
        signs: Arc<AtomicUsize>,
        authorizations: Arc<Mutex<Vec<String>>>,
    }

    struct Fake {
        key_spec: &'static str,
        /// The first this-many signs answer with `error`.
        fail_first_signs: usize,
        error: (u16, &'static str),
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                key_spec: SECP256K1_KEY_SPEC,
                fail_first_signs: 0,
                error: (400, "ThrottlingException"),
            }
        }
    }

    async fn fake_kms(key: k256::ecdsa::SigningKey, fake: Fake) -> FakeKms {
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        #[derive(Clone)]
        struct S {
            key: k256::ecdsa::SigningKey,
            key_spec: &'static str,
            signs: Arc<AtomicUsize>,
            fail_first: usize,
            error: (u16, &'static str),
            authorizations: Arc<Mutex<Vec<String>>>,
        }

        fn reply(status: u16, body: Value, error_type: Option<&str>) -> Response {
            let mut response = (
                StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/x-amz-json-1.1")],
                body.to_string(),
            )
                .into_response();
            if let Some(error_type) = error_type {
                response
                    .headers_mut()
                    .insert("x-amzn-errortype", error_type.parse().unwrap());
            }
            response
        }

        async fn handle(State(s): State<S>, headers: HeaderMap, body: String) -> Response {
            let header = |name: &str| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string()
            };
            s.authorizations
                .lock()
                .unwrap()
                .push(header("authorization"));
            let request: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(request["KeyId"], KEY_ARN);
            match header("x-amz-target").as_str() {
                "TrentService.GetPublicKey" => {
                    let spki = k256::PublicKey::from(*s.key.verifying_key())
                        .to_public_key_der()
                        .unwrap();
                    reply(
                        200,
                        json!({
                            "KeyId": KEY_ARN,
                            "PublicKey": BASE64.encode(spki.as_bytes()),
                            "KeySpec": s.key_spec,
                            "KeyUsage": "SIGN_VERIFY",
                            "SigningAlgorithms": ["ECDSA_SHA_256"],
                        }),
                        None,
                    )
                }
                "TrentService.Sign" => {
                    if s.signs.fetch_add(1, Ordering::SeqCst) < s.fail_first {
                        let (status, error_type) = s.error;
                        return reply(
                            status,
                            json!({ "__type": error_type, "message": "nope" }),
                            Some(error_type),
                        );
                    }
                    assert_eq!(request["MessageType"], "DIGEST");
                    assert_eq!(request["SigningAlgorithm"], "ECDSA_SHA_256");
                    let digest = BASE64.decode(request["Message"].as_str().unwrap()).unwrap();
                    let sig: Signature = s.key.sign_prehash(&digest).unwrap();
                    // Hand back the high-s twin: AWS KMS promises nothing about s.
                    let (r, low_s) = sig.split_scalars();
                    let high = Signature::from_scalars(r.to_repr(), (-(*low_s)).to_repr()).unwrap();
                    reply(
                        200,
                        json!({
                            "KeyId": KEY_ARN,
                            "Signature": BASE64.encode(high.to_der().as_bytes()),
                            "SigningAlgorithm": "ECDSA_SHA_256",
                        }),
                        None,
                    )
                }
                other => reply(
                    400,
                    json!({ "__type": "UnknownOperationException", "message": other }),
                    Some("UnknownOperationException"),
                ),
            }
        }

        let state = S {
            key,
            key_spec: fake.key_spec,
            signs: Arc::new(AtomicUsize::new(0)),
            fail_first: fake.fail_first_signs,
            error: fake.error,
            authorizations: Arc::new(Mutex::new(Vec::new())),
        };
        let signs = state.signs.clone();
        let authorizations = state.authorizations.clone();
        let app = Router::new().route("/", post(handle)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeKms {
            base: format!("http://{addr}"),
            signs,
            authorizations,
        }
    }

    /// A client for the fake with a credentials file's contents, skipping the
    /// endpoint allowlist the real constructors apply.
    async fn client_against(kms: &FakeKms) -> aws_sdk_kms::Client {
        let credentials = Credentials::File {
            contents: credentials_ini(ACCESS_KEY, SECRET_KEY).unwrap(),
            origin: "test".into(),
        };
        let conf = sdk_config(Some("us-east-1".into()), credentials)
            .await
            .unwrap();
        client_for(&conf, &kms.base)
    }

    /// A signer through the real startup checks.
    async fn signer_against(kms: &FakeKms, expected: Address) -> anyhow::Result<AwsKmsSigner> {
        AwsKmsSigner::checked(client_against(kms).await, KEY_ARN, expected, 8).await
    }

    /// A signer that skips the startup checks, for tests about a later sign.
    async fn signer_after_startup(kms: &FakeKms, expected: Address) -> AwsKmsSigner {
        AwsKmsSigner {
            client: client_against(kms).await,
            key_id: KEY_ARN.into(),
            operator_address: expected,
            max_concurrent_signs: 8,
        }
    }

    #[tokio::test]
    async fn signs_through_kms_with_the_credentials_file() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let kms = fake_kms(key, Fake::default()).await;
        let signer = signer_against(&kms, expected).await.unwrap();

        for byte in 1..=3u8 {
            let digest = B256::repeat_byte(byte);
            let sig = signer.sign_digest(digest).await.unwrap();
            assert_eq!(recover_address(digest, &sig).unwrap(), expected);
            let s = Signature::from_slice(&sig[..64]).unwrap();
            assert!(s.normalize_s().is_none(), "signature must be low-s");
        }
        // SigV4-signed with the access key from the file, not anything ambient.
        let authorizations = kms.authorizations.lock().unwrap();
        assert!(authorizations
            .iter()
            .all(|a| a.starts_with("AWS4-HMAC-SHA256")
                && a.contains(&format!("Credential={ACCESS_KEY}/"))
                && a.contains("/us-east-1/kms/aws4_request")));
    }

    #[tokio::test]
    async fn throttling_is_retried_within_the_attempt_budget() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let kms = fake_kms(
            key,
            Fake {
                fail_first_signs: 2,
                ..Fake::default()
            },
        )
        .await;
        let signer = signer_after_startup(&kms, expected).await;
        let digest = B256::repeat_byte(7);
        let sig = signer.sign_digest(digest).await.unwrap();
        assert_eq!(recover_address(digest, &sig).unwrap(), expected);
        assert_eq!(kms.signs.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_key_that_can_be_read_but_not_signed_with_is_refused_at_startup() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let kms = fake_kms(
            key,
            Fake {
                fail_first_signs: 1,
                error: (400, "AccessDeniedException"),
                ..Fake::default()
            },
        )
        .await;
        let err = format!("{:#}", signer_against(&kms, expected).await.err().unwrap());
        assert!(err.contains("signing a test message"), "{err}");
        assert!(err.contains("kms:Sign"), "{err}");
        assert_eq!(kms.signs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn access_denied_is_not_retried_and_names_the_permissions() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let kms = fake_kms(
            key,
            Fake {
                fail_first_signs: 1,
                error: (400, "AccessDeniedException"),
                ..Fake::default()
            },
        )
        .await;
        // Revoked after startup.
        let signer = signer_after_startup(&kms, expected).await;
        let err = signer
            .sign_digest(B256::repeat_byte(9))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("AccessDeniedException: nope"), "{err}");
        assert!(err.contains("kms:Sign"), "{err}");
        // AWS's code and message, not the SDK's debug dump of the response.
        assert!(!err.contains("raw:"), "{err}");
        assert_eq!(kms.signs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_key_that_is_not_secp256k1_is_refused_at_startup() {
        let key = parse_private_key(KEY).unwrap();
        let expected = address_from_signing_key(&key);
        let kms = fake_kms(
            key,
            Fake {
                key_spec: "ECC_NIST_P256",
                ..Fake::default()
            },
        )
        .await;
        let err = signer_against(&kms, expected)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("ECC_NIST_P256"), "{err}");
    }

    #[tokio::test]
    async fn a_key_for_another_address_is_refused_at_startup() {
        let key = parse_private_key(KEY).unwrap();
        let kms = fake_kms(key, Fake::default()).await;
        assert!(signer_against(&kms, Address::repeat_byte(0xaa))
            .await
            .is_err());
    }
}
