// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Fetch an image's attestation bundles from GHCR, anonymously.
//!
//! `actions/attest-build-provenance` pushes each bundle as an OCI artifact whose
//! `subject` is the image. GHCR doesn't serve the OCI referrers API, so the
//! bundles are found the way the OCI spec's fallback describes: an index tagged
//! `sha256-<hex>` in the image's repository lists them. Each listed manifest's
//! single layer is the bundle JSON.
//!
//! This runs in the hosted panel, which has outbound HTTPS; the runtime broker
//! has no network. That's fine for trust: a bundle proves itself (see
//! [`super::verify`]), so whoever fetched it can only withhold one, which
//! refuses the swap. Nothing fetched here is trusted before the broker verifies
//! it.
//!
//! Network: `ghcr.io:443`, plus `pkg-containers.githubusercontent.com:443`,
//! where GHCR redirects blob downloads. A `docker pull` of the image uses the
//! same two hosts. No other redirect is followed.

use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{MAX_BUNDLES, MAX_BUNDLE_BYTES, PREDICATE_TYPE};

pub const GHCR: &str = "https://ghcr.io";
/// Where GHCR redirects blob downloads.
const BLOB_HOST: &str = "pkg-containers.githubusercontent.com";
const BUNDLE_MEDIA_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
/// Index and manifest documents are small; anything larger is refused.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

pub struct Registry {
    base: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

#[derive(Deserialize)]
struct Index {
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    layers: Vec<Descriptor>,
    subject: Option<Descriptor>,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    #[serde(rename = "artifactType", default)]
    artifact_type: Option<String>,
    digest: String,
    #[serde(default)]
    annotations: std::collections::HashMap<String, String>,
}

impl Registry {
    /// The real GHCR.
    pub fn ghcr() -> Result<Self, String> {
        Self::at(GHCR)
    }

    /// A registry at `base` (tests point this at a local fake).
    pub fn at(base: &str) -> Result<Self, String> {
        let redirects = reqwest::redirect::Policy::custom(|attempt| {
            let allowed = attempt.url().scheme() == "https"
                && attempt.url().host_str() == Some(BLOB_HOST)
                && attempt.previous().len() < 3;
            if allowed {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(redirects)
            .user_agent("stitch-panel")
            .build()
            .map_err(|e| format!("building the registry client: {e}"))?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// Every SLSA provenance bundle GHCR lists for `repository@digest`, up to
    /// [`MAX_BUNDLES`]. An image with none gives an empty list; any registry or
    /// network error is an error. Neither lets a swap through.
    pub async fn attestations(
        &self,
        repository: &str,
        digest: &str,
    ) -> Result<Vec<String>, String> {
        let path = repository.strip_prefix("ghcr.io/").ok_or_else(|| {
            format!("{repository} is not on ghcr.io; its attestations can't be fetched")
        })?;
        let hex = super::digest_hex(digest)?;
        let token = self.token(path).await?;
        let index_url = format!("{}/v2/{path}/manifests/sha256-{hex}", self.base);
        let Some(index) = self
            .document::<Index>(&index_url, INDEX_MEDIA_TYPE, &token)
            .await?
        else {
            return Ok(Vec::new());
        };
        let listed = index.manifests.iter().filter(|m| {
            m.artifact_type.as_deref() == Some(BUNDLE_MEDIA_TYPE)
                && m.annotations
                    .get("dev.sigstore.bundle.predicateType")
                    .map(String::as_str)
                    == Some(PREDICATE_TYPE)
        });
        let mut bundles = Vec::new();
        for entry in listed.take(MAX_BUNDLES) {
            bundles.push(self.bundle(path, digest, &entry.digest, &token).await?);
        }
        Ok(bundles)
    }

    async fn token(&self, path: &str) -> Result<String, String> {
        let url = format!(
            "{}/token?scope=repository:{path}:pull&service=ghcr.io",
            self.base
        );
        let response = self.send(self.client.get(&url)).await?;
        let body = read_capped(response, MAX_DOCUMENT_BYTES).await?;
        serde_json::from_slice::<Token>(&body)
            .map(|t| t.token)
            .map_err(|e| format!("registry token reply doesn't parse: {e}"))
    }

    /// GET a JSON document. `None` on 404.
    async fn document<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        accept: &str,
        token: &str,
    ) -> Result<Option<T>, String> {
        let request = self
            .client
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, accept);
        let response = request
            .send()
            .await
            .map_err(|e| format!("registry unreachable: {e}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = ok_status(response)?;
        let body = read_capped(response, MAX_DOCUMENT_BYTES).await?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| format!("registry document at {url} doesn't parse: {e}"))
    }

    async fn bundle(
        &self,
        path: &str,
        image_digest: &str,
        manifest_digest: &str,
        token: &str,
    ) -> Result<String, String> {
        super::digest_hex(manifest_digest)?;
        let url = format!("{}/v2/{path}/manifests/{manifest_digest}", self.base);
        let manifest: Manifest = self
            .document(&url, MANIFEST_MEDIA_TYPE, token)
            .await?
            .ok_or_else(|| {
                format!("attestation manifest {manifest_digest} is listed but missing")
            })?;
        if manifest.subject.as_ref().map(|s| s.digest.as_str()) != Some(image_digest) {
            return Err(format!(
                "attestation manifest {manifest_digest} is not about {image_digest}"
            ));
        }
        let layer = manifest
            .layers
            .iter()
            .find(|l| l.media_type == BUNDLE_MEDIA_TYPE)
            .ok_or_else(|| format!("attestation manifest {manifest_digest} has no bundle layer"))?;
        let expected = super::digest_hex(&layer.digest)?.to_string();
        let url = format!("{}/v2/{path}/blobs/{}", self.base, layer.digest);
        let response = self.send(self.client.get(&url).bearer_auth(token)).await?;
        let body = read_capped(response, MAX_BUNDLE_BYTES).await?;
        let got = hex_encode(&Sha256::digest(&body));
        if got != expected {
            return Err(format!(
                "attestation blob {} arrived with digest sha256:{got}",
                layer.digest
            ));
        }
        String::from_utf8(body).map_err(|_| "attestation bundle is not UTF-8".to_string())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let response = request
            .send()
            .await
            .map_err(|e| format!("registry unreachable: {e}"))?;
        ok_status(response)
    }
}

fn ok_status(response: reqwest::Response) -> Result<reqwest::Response, String> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        Err(format!(
            "registry answered {status} for {}",
            response.url().path()
        ))
    }
}

async fn read_capped(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(format!("registry reply is over the {max}-byte limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("registry reply cut off: {e}"))?
    {
        if body.len() + chunk.len() > max {
            return Err(format!("registry reply is over the {max}-byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
