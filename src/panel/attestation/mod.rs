// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Build provenance checks for Stitch images, offline.
//!
//! The public mirror `textile-protocol/textile-stitch` attests every image it
//! publishes with `actions/attest-build-provenance`, and pushes the Sigstore
//! bundle to GHCR next to the image. [`verify`] decides whether one such bundle
//! proves that a given image digest was built by Textile's container workflow
//! for that image, from `main`, on a GitHub-hosted runner.
//!
//! Everything it needs is in the bundle and in this binary: the signing
//! certificate, the Rekor inclusion proof, checkpoint and signed entry
//! timestamp, and the trust root ([`TRUSTED_ROOT_JSON`], Sigstore's public-good
//! `trusted_root.json`, pinned here and changed only by a reviewed release). No
//! TUF refresh, Rekor lookup or other network call happens here, which is what
//! lets the runtime broker run it with no network at all.
//!
//! What is checked, in this order, and any failure refuses:
//!
//! 1. Sigstore verification (`sigstore-verify`): the certificate chains to the
//!    pinned Fulcio roots and was valid at the Rekor-signed integration time,
//!    its SCT verifies against the pinned CT log keys, the Rekor entry's
//!    inclusion proof, checkpoint and signed entry timestamp verify against
//!    the pinned Rekor keys and match the bundle, the DSSE signature verifies
//!    with the certificate's key, and one of the in-toto subjects carries the
//!    requested sha256 digest. The certificate's SAN must equal the expected
//!    workflow identity exactly, and its OIDC issuer must be GitHub Actions.
//! 2. The certificate's Fulcio CI claims ([`check_claims`]): source repository,
//!    source ref, the build config and signer workflow, and a GitHub-hosted
//!    runner.
//! 3. The signed statement ([`check_statement`]): in-toto Statement v1, SLSA
//!    provenance v1, and exactly one subject, named for the expected repository
//!    and carrying the requested digest.
//!
//! The expected values per image are fixed in [`ImageKind`]. Nothing about the
//! policy comes from the environment or from the caller.

use std::sync::OnceLock;

use serde::Deserialize;
use sigstore_verify::crypto::CertificateInfo;
use sigstore_verify::trust_root::TrustedRoot;
use sigstore_verify::types::{Bundle, SignatureContent};
use sigstore_verify::{IdentityMatcher, VerificationPolicy, Verifier};

pub mod registry;

/// Sigstore's public-good trusted root (Fulcio CAs, Rekor and CT log keys,
/// timestamp authority), as published in `sigstore/root-signing`
/// `targets/trusted_root.json`. Refresh it with a reviewed release, never at
/// runtime: see docs/production-access.md#image-attestations.
pub const TRUSTED_ROOT_JSON: &str = include_str!("sigstore_trusted_root.json");

/// The repository whose workflows may sign Stitch images.
pub const SOURCE_REPOSITORY: &str = "https://github.com/textile-protocol/textile-stitch";
/// The only ref a signed build may come from.
pub const SOURCE_REF: &str = "refs/heads/main";
/// GitHub Actions' OIDC issuer, as Fulcio records it.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";
/// Fulcio's runner environment claim for GitHub's own runners.
pub const RUNNER_ENVIRONMENT: &str = "github-hosted";
/// The DSSE payload type of an in-toto statement.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const PREDICATE_TYPE: &str = "https://slsa.dev/provenance/v1";

/// Upper bound on one bundle's size. Real ones are about 11 KiB.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024;
/// Upper bound on how many bundles one request may carry (a digest gets one per
/// attest run; reruns add more).
pub const MAX_BUNDLES: usize = 8;

/// Which Stitch image is being checked. Each has exactly one repository and one
/// workflow allowed to sign it, so an attestation for one never passes for the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// The bot, `ghcr.io/textile-protocol/textile-stitch`, built by
    /// `container.yml`. The only kind the runtime broker swaps to.
    Bot,
    /// The panel and broker image, `ghcr.io/textile-protocol/textile-stitch-panel`,
    /// built by `container-panel.yml`.
    Panel,
}

impl ImageKind {
    /// The image repository, as it appears in an image reference and as the
    /// in-toto subject name.
    pub fn repository(self) -> &'static str {
        match self {
            Self::Bot => "ghcr.io/textile-protocol/textile-stitch",
            Self::Panel => "ghcr.io/textile-protocol/textile-stitch-panel",
        }
    }

    /// The workflow file, relative to the source repository.
    pub fn workflow(self) -> &'static str {
        match self {
            Self::Bot => ".github/workflows/container.yml",
            Self::Panel => ".github/workflows/container-panel.yml",
        }
    }

    /// The certificate SAN a valid attestation carries: the workflow at `main`.
    pub fn identity(self) -> String {
        format!("{SOURCE_REPOSITORY}/{}@{SOURCE_REF}", self.workflow())
    }

    /// The kind whose repository is exactly `repository`, if any.
    pub fn for_repository(repository: &str) -> Option<Self> {
        [Self::Bot, Self::Panel]
            .into_iter()
            .find(|k| k.repository() == repository)
    }
}

/// What a passing attestation proved. Logged by the broker and the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The signing workflow identity (certificate SAN).
    pub workflow: String,
    /// The commit the image was built from.
    pub commit: String,
    /// `sha256:<hex>`, the digest that was requested and attested.
    pub digest: String,
    /// When Rekor integrated the entry (RFC 3339).
    pub signed_at: String,
}

/// The certificate claims [`check_claims`] looks at, copied out of the verified
/// certificate so the policy can be tested on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims {
    pub identity: Option<String>,
    pub issuer: Option<String>,
    pub source_repository: Option<String>,
    pub source_ref: Option<String>,
    pub source_commit: Option<String>,
    pub build_config: Option<String>,
    pub build_signer: Option<String>,
    pub runner_environment: Option<String>,
}

impl Claims {
    fn from_certificate(cert: &CertificateInfo) -> Self {
        let ci = &cert.ci_claims;
        Self {
            identity: cert.identity.as_ref().map(|i| i.as_str().to_string()),
            issuer: cert.issuer.clone(),
            source_repository: ci.source_repository_uri.clone(),
            source_ref: ci.source_repository_ref.clone(),
            source_commit: ci.source_repository_digest.clone(),
            build_config: ci.build_config_uri.clone(),
            build_signer: ci.build_signer_uri.clone(),
            runner_environment: ci.runner_environment.clone(),
        }
    }
}

/// The pinned trust root, parsed once. An error here (a malformed pinned root)
/// refuses every verification.
fn pinned_verifier() -> Result<&'static Verifier, String> {
    static VERIFIER: OnceLock<Result<Verifier, String>> = OnceLock::new();
    VERIFIER
        .get_or_init(|| verifier_for(TRUSTED_ROOT_JSON))
        .as_ref()
        .map_err(Clone::clone)
}

fn verifier_for(trusted_root_json: &str) -> Result<Verifier, String> {
    let root = TrustedRoot::from_json(trusted_root_json)
        .map_err(|e| format!("the pinned Sigstore trust root doesn't parse: {e}"))?;
    Verifier::new(&root).map_err(|e| format!("the pinned Sigstore trust root is unusable: {e}"))
}

/// The 64 lowercase hex characters of a `sha256:` digest, or an error.
fn digest_hex(digest: &str) -> Result<&str, String> {
    let hex = digest.strip_prefix("sha256:").unwrap_or("");
    let ok = hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(hex)
    } else {
        Err(format!("{digest} is not a sha256:<64 hex> digest"))
    }
}

/// Whether one bundle proves `digest` (`sha256:<hex>`) is a `kind` image built
/// by its workflow. See the module docs for what is checked.
pub fn verify(kind: ImageKind, digest: &str, bundle_json: &str) -> Result<Verified, String> {
    verify_with(pinned_verifier()?, kind, digest, bundle_json)
}

fn verify_with(
    verifier: &Verifier,
    kind: ImageKind,
    digest: &str,
    bundle_json: &str,
) -> Result<Verified, String> {
    let hex = digest_hex(digest)?;
    if bundle_json.len() > MAX_BUNDLE_BYTES {
        return Err(format!(
            "attestation bundle is {} bytes, over the {MAX_BUNDLE_BYTES}-byte limit",
            bundle_json.len()
        ));
    }
    let bundle = Bundle::from_json(bundle_json)
        .map_err(|e| format!("attestation bundle doesn't parse: {e}"))?;
    let artifact =
        sigstore_verify::types::Sha256Hash::from_hex(hex).map_err(|e| format!("{digest}: {e}"))?;
    let identity = kind.identity();
    let policy = VerificationPolicy::new(IdentityMatcher::Uri(identity.clone()), ISSUER);
    let result = verifier
        .verify(artifact, &bundle, &policy)
        .map_err(|e| format!("Sigstore verification failed: {e}"))?;
    // `verify` only returns Ok with these done; checked again so a library change
    // that relaxes one can't pass silently.
    if !(result.certificate_verified()
        && result.sct_verified()
        && result.tlog_verified()
        && result.identity_policy_checked())
    {
        return Err("Sigstore verification skipped a required check".into());
    }
    let cert = result
        .certificate()
        .ok_or("verified bundle has no certificate")?;
    let claims = Claims::from_certificate(cert);
    check_claims(kind, &claims)?;
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return Err("attestation bundle holds a message signature, not a DSSE envelope".into());
    };
    if envelope.payload_type != PAYLOAD_TYPE {
        return Err(format!(
            "DSSE payload type is {}, expected {PAYLOAD_TYPE}",
            envelope.payload_type
        ));
    }
    check_statement(kind, hex, envelope.payload.as_bytes())?;
    let signed_at = result
        .integrated_time()
        .map(|t| t.to_string())
        .ok_or("verified bundle has no Rekor integration time")?;
    Ok(Verified {
        workflow: identity,
        commit: claims.source_commit.unwrap_or_default(),
        digest: format!("sha256:{hex}"),
        signed_at,
    })
}

/// The first bundle in `bundles` that proves `digest`, or every reason none did.
/// An empty list refuses: no attestation, no swap.
pub fn verify_any(kind: ImageKind, digest: &str, bundles: &[String]) -> Result<Verified, String> {
    if bundles.is_empty() {
        return Err(format!(
            "no build attestation found for {}@{digest}",
            kind.repository()
        ));
    }
    if bundles.len() > MAX_BUNDLES {
        return Err(format!(
            "{} attestation bundles sent; at most {MAX_BUNDLES} are checked",
            bundles.len()
        ));
    }
    let mut reasons = Vec::with_capacity(bundles.len());
    for bundle in bundles {
        match verify(kind, digest, bundle) {
            Ok(v) => return Ok(v),
            Err(e) => reasons.push(e),
        }
    }
    Err(format!(
        "no valid build attestation for {}@{digest} from {}: {}",
        kind.repository(),
        kind.identity(),
        reasons.join("; ")
    ))
}

/// The runtime broker's gate on a swap to `image` (`<repository>@sha256:<hex>`,
/// already checked by [`crate::panel::broker::check_digest_ref`]). Bots only
/// ever move onto the bot image, so the expected kind is fixed to
/// [`ImageKind::Bot`], and a broker configured for any other repository refuses
/// every swap rather than accepting something no policy covers.
pub fn verify_swap(repository: &str, image: &str, bundles: &[String]) -> Result<Verified, String> {
    let kind = ImageKind::Bot;
    if repository != kind.repository() {
        return Err(format!(
            "no attestation policy covers {repository}: hosted bots only move onto attested {} images",
            kind.repository()
        ));
    }
    let (_, digest) = image
        .split_once('@')
        .ok_or_else(|| format!("{image} is not a digest reference"))?;
    verify_any(kind, digest, bundles)
}

fn require(claim: &str, got: Option<&str>, expected: &str) -> Result<(), String> {
    match got {
        Some(v) if v == expected => Ok(()),
        Some(v) => Err(format!("certificate {claim} is {v}, expected {expected}")),
        None => Err(format!(
            "certificate has no {claim} claim, expected {expected}"
        )),
    }
}

/// The certificate must name exactly `kind`'s workflow in the mirror, at `main`,
/// signed through GitHub Actions on a GitHub-hosted runner.
pub fn check_claims(kind: ImageKind, claims: &Claims) -> Result<(), String> {
    let identity = kind.identity();
    require("identity", claims.identity.as_deref(), &identity)?;
    require("issuer", claims.issuer.as_deref(), ISSUER)?;
    require(
        "source repository",
        claims.source_repository.as_deref(),
        SOURCE_REPOSITORY,
    )?;
    require("source ref", claims.source_ref.as_deref(), SOURCE_REF)?;
    require("build config", claims.build_config.as_deref(), &identity)?;
    require("build signer", claims.build_signer.as_deref(), &identity)?;
    require(
        "runner environment",
        claims.runner_environment.as_deref(),
        RUNNER_ENVIRONMENT,
    )?;
    match claims.source_commit.as_deref() {
        Some(c) if c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(()),
        _ => Err("certificate has no source commit".into()),
    }
}

#[derive(Deserialize)]
struct Statement {
    #[serde(rename = "_type")]
    statement_type: String,
    #[serde(rename = "predicateType")]
    predicate_type: String,
    subject: Vec<Subject>,
}

#[derive(Deserialize)]
struct Subject {
    name: String,
    digest: std::collections::HashMap<String, String>,
}

/// The signed statement must be SLSA provenance v1 about exactly one subject:
/// `kind`'s repository at `hex`.
pub fn check_statement(kind: ImageKind, hex: &str, payload: &[u8]) -> Result<(), String> {
    let statement: Statement = serde_json::from_slice(payload)
        .map_err(|e| format!("attested statement doesn't parse: {e}"))?;
    if statement.statement_type != STATEMENT_TYPE {
        return Err(format!(
            "statement type is {}, expected {STATEMENT_TYPE}",
            statement.statement_type
        ));
    }
    if statement.predicate_type != PREDICATE_TYPE {
        return Err(format!(
            "predicate type is {}, expected {PREDICATE_TYPE}",
            statement.predicate_type
        ));
    }
    let [subject] = statement.subject.as_slice() else {
        return Err(format!(
            "statement has {} subjects, expected exactly one",
            statement.subject.len()
        ));
    };
    if subject.name != kind.repository() {
        return Err(format!(
            "statement subject is {}, expected {}",
            subject.name,
            kind.repository()
        ));
    }
    match subject.digest.get("sha256") {
        Some(d) if d == hex => Ok(()),
        Some(d) => Err(format!(
            "statement subject digest is sha256:{d}, expected sha256:{hex}"
        )),
        None => Err("statement subject has no sha256 digest".into()),
    }
}

#[cfg(test)]
pub(crate) mod tests;
