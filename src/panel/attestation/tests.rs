// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The fixtures are real: the bundles `actions/attest-build-provenance` pushed to
//! GHCR for the mirror's `main` build of 833f1809 (runs 44576143279 and
//! 44576143395), fetched from the `sha256-<hex>` fallback tag. Policy failures a
//! real signature can't be produced for (another repo, ref or runner) are tested
//! on the claims and statement checks directly.

use super::*;
use base64::Engine as _;
use serde_json::{json, Value};

pub const BOT_BUNDLE: &str = include_str!("testdata/bot-8fd05fd7.sigstore.json");
pub const PANEL_BUNDLE: &str = include_str!("testdata/panel-d5fccbe8.sigstore.json");
pub const BOT_DIGEST: &str =
    "sha256:8fd05fd7d0e9e4ecc028a92c69526bb34d15c7ece1cf8bf13b34bc370e3d871d";
pub const PANEL_DIGEST: &str =
    "sha256:d5fccbe805939e80a102ae5f89f4e036e97aaa0df49129bd112b69b67b18fccc";
const COMMIT: &str = "833f1809b9e77c6784e1625f38ead70fea5d24ea";
const GITHUB_INSTANCE_ROOT: &str = include_str!("testdata/github-instance-trusted-root.json");

fn edit(json: &str, f: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(json).unwrap();
    f(&mut v);
    v.to_string()
}

fn root_with(f: impl FnOnce(&mut Value)) -> Verifier {
    verifier_for(&edit(TRUSTED_ROOT_JSON, f)).unwrap()
}

#[test]
fn the_real_bot_attestation_passes_for_its_digest() {
    let v = verify(ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).unwrap();
    assert_eq!(
        v.workflow,
        "https://github.com/textile-protocol/textile-stitch/.github/workflows/container.yml@refs/heads/main"
    );
    assert_eq!(v.commit, COMMIT);
    assert_eq!(v.digest, BOT_DIGEST);
    assert_eq!(v.signed_at, "2026-11-19T17:27:59Z");
}

#[test]
fn the_real_panel_attestation_passes_for_its_digest() {
    let v = verify(ImageKind::Panel, PANEL_DIGEST, PANEL_BUNDLE).unwrap();
    assert!(v.workflow.ends_with("/container-panel.yml@refs/heads/main"));
    assert_eq!(v.commit, COMMIT);
}

#[test]
fn an_attestation_for_one_image_never_passes_for_the_other() {
    let e = verify(ImageKind::Bot, PANEL_DIGEST, PANEL_BUNDLE).unwrap_err();
    assert!(e.contains("identity"), "{e}");
    let e = verify(ImageKind::Panel, BOT_DIGEST, BOT_BUNDLE).unwrap_err();
    assert!(e.contains("identity"), "{e}");
}

#[test]
fn another_digest_is_refused() {
    let e = verify(ImageKind::Bot, PANEL_DIGEST, BOT_BUNDLE).unwrap_err();
    assert!(e.contains("does not match"), "{e}");
    let other = format!("sha256:{}", "0".repeat(64));
    assert!(verify(ImageKind::Bot, &other, BOT_BUNDLE).is_err());
    for bad in ["8fd05fd7", "sha512:00", "sha256:XYZ", ""] {
        assert!(verify(ImageKind::Bot, bad, BOT_BUNDLE).is_err(), "{bad}");
    }
}

#[test]
fn a_rewritten_statement_breaks_the_signature() {
    let b64 = base64::engine::general_purpose::STANDARD;
    let forged = edit(BOT_BUNDLE, |v| {
        let payload = v["dsseEnvelope"]["payload"].as_str().unwrap();
        let mut statement: Value = serde_json::from_slice(&b64.decode(payload).unwrap()).unwrap();
        statement["subject"][0]["digest"]["sha256"] =
            json!(PANEL_DIGEST.trim_start_matches("sha256:"));
        v["dsseEnvelope"]["payload"] = json!(b64.encode(statement.to_string()));
    });
    assert!(verify(ImageKind::Bot, PANEL_DIGEST, &forged).is_err());
    assert!(verify(ImageKind::Bot, BOT_DIGEST, &forged).is_err());
}

#[test]
fn a_swapped_certificate_or_missing_log_entry_is_refused() {
    let panel: Value = serde_json::from_str(PANEL_BUNDLE).unwrap();
    let swapped = edit(BOT_BUNDLE, |v| {
        v["verificationMaterial"]["certificate"] =
            panel["verificationMaterial"]["certificate"].clone();
    });
    assert!(verify(ImageKind::Bot, BOT_DIGEST, &swapped).is_err());
    assert!(verify(ImageKind::Panel, BOT_DIGEST, &swapped).is_err());
    let no_log = edit(BOT_BUNDLE, |v| {
        v["verificationMaterial"]["tlogEntries"] = json!([]);
    });
    assert!(verify(ImageKind::Bot, BOT_DIGEST, &no_log).is_err());
    let late = edit(BOT_BUNDLE, |v| {
        v["verificationMaterial"]["tlogEntries"][0]["integratedTime"] = json!("1795209279");
    });
    assert!(verify(ImageKind::Bot, BOT_DIGEST, &late).is_err());
}

#[test]
fn a_certificate_from_an_untrusted_ca_is_refused() {
    // GitHub's own Sigstore instance (private repos) is a real CA, just not ours.
    let github = verifier_for(GITHUB_INSTANCE_ROOT).unwrap();
    assert!(verify_with(&github, ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).is_err());
    let no_fulcio = root_with(|v| v["certificateAuthorities"] = json!([]));
    assert!(verify_with(&no_fulcio, ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).is_err());
    let no_rekor = root_with(|v| v["tlogs"] = json!([]));
    assert!(verify_with(&no_rekor, ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).is_err());
    let no_ct = root_with(|v| v["ctlogs"] = json!([]));
    assert!(verify_with(&no_ct, ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).is_err());
}

#[test]
fn a_certificate_outside_its_ca_window_is_refused() {
    // The CA's validity ends before the bundle was signed: the same check that
    // refuses an expired or retired CA.
    let expired = root_with(|v| {
        for ca in v["certificateAuthorities"].as_array_mut().unwrap() {
            ca["validFor"]["end"] = json!("2026-01-01T00:00:00Z");
        }
    });
    assert!(verify_with(&expired, ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).is_err());
    // And the pinned root, unedited, still passes.
    let pinned = verifier_for(TRUSTED_ROOT_JSON).unwrap();
    assert!(verify_with(&pinned, ImageKind::Bot, BOT_DIGEST, BOT_BUNDLE).is_ok());
}

#[test]
fn garbage_and_oversized_bundles_are_refused() {
    assert!(verify(ImageKind::Bot, BOT_DIGEST, "").is_err());
    assert!(verify(ImageKind::Bot, BOT_DIGEST, "{}").is_err());
    let huge = format!("{}{}", BOT_BUNDLE, " ".repeat(MAX_BUNDLE_BYTES));
    assert!(verify(ImageKind::Bot, BOT_DIGEST, &huge)
        .unwrap_err()
        .contains("limit"));
}

#[test]
fn verify_any_needs_one_valid_bundle() {
    let e = verify_any(ImageKind::Bot, BOT_DIGEST, &[]).unwrap_err();
    assert!(e.contains("no build attestation found"), "{e}");
    let mixed = vec![PANEL_BUNDLE.to_string(), BOT_BUNDLE.to_string()];
    assert_eq!(
        verify_any(ImageKind::Bot, BOT_DIGEST, &mixed)
            .unwrap()
            .commit,
        COMMIT
    );
    let wrong = vec![PANEL_BUNDLE.to_string(), "{}".to_string()];
    let e = verify_any(ImageKind::Bot, BOT_DIGEST, &wrong).unwrap_err();
    assert!(e.contains("no valid build attestation"), "{e}");
    let many = vec![BOT_BUNDLE.to_string(); MAX_BUNDLES + 1];
    assert!(verify_any(ImageKind::Bot, BOT_DIGEST, &many).is_err());
}

fn good_claims(kind: ImageKind) -> Claims {
    Claims {
        identity: Some(kind.identity()),
        issuer: Some(ISSUER.into()),
        source_repository: Some(SOURCE_REPOSITORY.into()),
        source_ref: Some(SOURCE_REF.into()),
        source_commit: Some(COMMIT.into()),
        build_config: Some(kind.identity()),
        build_signer: Some(kind.identity()),
        runner_environment: Some(RUNNER_ENVIRONMENT.into()),
    }
}

#[test]
fn claims_must_name_the_mirror_main_and_a_github_hosted_runner() {
    assert!(check_claims(ImageKind::Bot, &good_claims(ImageKind::Bot)).is_ok());
    let fork = "https://github.com/someone/textile-stitch";
    let cases: Vec<(&str, Box<dyn Fn(&mut Claims)>)> = vec![
        (
            "self-hosted runner",
            Box::new(|c| c.runner_environment = Some("self-hosted".into())),
        ),
        ("no runner claim", Box::new(|c| c.runner_environment = None)),
        (
            "another repo",
            Box::new(move |c| c.source_repository = Some(fork.into())),
        ),
        (
            "another ref",
            Box::new(|c| c.source_ref = Some("refs/heads/dev".into())),
        ),
        (
            "a tag",
            Box::new(|c| c.source_ref = Some("refs/tags/v1.0.0".into())),
        ),
        (
            "another workflow",
            Box::new(|c| {
                let other =
                    format!("{SOURCE_REPOSITORY}/.github/workflows/release.yml@{SOURCE_REF}");
                c.identity = Some(other.clone());
                c.build_config = Some(other.clone());
                c.build_signer = Some(other);
            }),
        ),
        (
            "a reusable workflow signer",
            Box::new(|c| {
                c.build_signer =
                    Some("https://github.com/x/y/.github/workflows/z.yml@refs/heads/main".into())
            }),
        ),
        (
            "another issuer",
            Box::new(|c| c.issuer = Some("https://accounts.google.com".into())),
        ),
        ("no commit", Box::new(|c| c.source_commit = None)),
        (
            "a short commit",
            Box::new(|c| c.source_commit = Some("833f1809".into())),
        ),
    ];
    for (what, change) in cases {
        let mut claims = good_claims(ImageKind::Bot);
        change(&mut claims);
        assert!(
            check_claims(ImageKind::Bot, &claims).is_err(),
            "{what} passed"
        );
    }
    assert!(check_claims(ImageKind::Bot, &good_claims(ImageKind::Panel)).is_err());
}

fn statement(kind: ImageKind, hex: &str) -> Value {
    json!({
        "_type": STATEMENT_TYPE,
        "predicateType": PREDICATE_TYPE,
        "subject": [{ "name": kind.repository(), "digest": { "sha256": hex } }],
        "predicate": {},
    })
}

#[test]
fn the_statement_must_be_slsa_provenance_about_exactly_this_image() {
    let hex = BOT_DIGEST.trim_start_matches("sha256:");
    let check = |v: &Value| check_statement(ImageKind::Bot, hex, v.to_string().as_bytes());
    assert!(check(&statement(ImageKind::Bot, hex)).is_ok());
    let mut v = statement(ImageKind::Bot, hex);
    v["predicateType"] = json!("https://slsa.dev/provenance/v0.2");
    assert!(check(&v).is_err());
    let mut v = statement(ImageKind::Bot, hex);
    v["_type"] = json!("https://in-toto.io/Statement/v0.1");
    assert!(check(&v).is_err());
    assert!(check(&statement(ImageKind::Panel, hex)).is_err());
    assert!(check(&statement(ImageKind::Bot, &"0".repeat(64))).is_err());
    let mut v = statement(ImageKind::Bot, hex);
    let extra = v["subject"][0].clone();
    v["subject"].as_array_mut().unwrap().push(extra);
    assert!(check(&v).is_err());
    let mut v = statement(ImageKind::Bot, hex);
    v["subject"] = json!([]);
    assert!(check(&v).is_err());
}

#[test]
fn kinds_map_one_to_one_onto_repositories() {
    assert_eq!(
        ImageKind::for_repository("ghcr.io/textile-protocol/textile-stitch"),
        Some(ImageKind::Bot)
    );
    assert_eq!(
        ImageKind::for_repository("ghcr.io/textile-protocol/textile-stitch-panel"),
        Some(ImageKind::Panel)
    );
    assert_eq!(
        ImageKind::for_repository("ghcr.io/someone/textile-stitch"),
        None
    );
    assert_eq!(
        ImageKind::for_repository("docker.io/textile-protocol/textile-stitch"),
        None
    );
}

pub(crate) mod fetch {
    use super::*;
    use axum::extract::Path as UrlPath;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use sha2::{Digest, Sha256};

    fn sha(bytes: &[u8]) -> String {
        format!(
            "sha256:{}",
            super::super::registry::hex_encode(&Sha256::digest(bytes))
        )
    }

    /// A GHCR stand-in serving `bundle` as `image`'s attestation, the way the
    /// real one lays it out. `corrupt` serves different bytes than the layer
    /// digest names.
    pub async fn fake_registry(image: &'static str, bundle: &'static str, corrupt: bool) -> String {
        let blob = sha(bundle.as_bytes());
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": [{ "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json", "digest": blob, "size": bundle.len() }],
            "subject": { "mediaType": "application/vnd.oci.image.index.v1+json", "digest": image, "size": 1 },
        })
        .to_string();
        let manifest_digest = sha(manifest.as_bytes());
        let index = json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "artifactType": "application/vnd.dev.sigstore.bundle.v0.3+json",
                "annotations": { "dev.sigstore.bundle.predicateType": PREDICATE_TYPE },
            }],
        })
        .to_string();
        let fallback = format!("sha256-{}", image.trim_start_matches("sha256:"));
        let app = Router::new()
            .route("/token", get(|| async { r#"{"token":"anon"}"# }))
            .route(
                "/v2/{org}/{repo}/manifests/{reference}",
                get(
                    move |UrlPath((_, _, reference)): UrlPath<(String, String, String)>| {
                        let (index, manifest) = (index.clone(), manifest.clone());
                        let (fallback, manifest_digest) =
                            (fallback.clone(), manifest_digest.clone());
                        async move {
                            if reference == fallback {
                                index.into_response()
                            } else if reference == manifest_digest {
                                manifest.into_response()
                            } else {
                                StatusCode::NOT_FOUND.into_response()
                            }
                        }
                    },
                ),
            )
            .route(
                "/v2/{org}/{repo}/blobs/{digest}",
                get(move || async move {
                    let body = if corrupt {
                        format!("{bundle} ")
                    } else {
                        bundle.to_string()
                    };
                    Response::new(axum::body::Body::from(body))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn bundles_are_fetched_from_the_fallback_tag_and_verify() {
        let base = fake_registry(BOT_DIGEST, BOT_BUNDLE, false).await;
        let registry = registry::Registry::at(&base).unwrap();
        let bundles = registry
            .attestations(ImageKind::Bot.repository(), BOT_DIGEST)
            .await
            .unwrap();
        assert_eq!(bundles, vec![BOT_BUNDLE.to_string()]);
        assert!(verify_any(ImageKind::Bot, BOT_DIGEST, &bundles).is_ok());
    }

    #[tokio::test]
    async fn an_image_with_no_attestation_gets_none_and_is_refused() {
        let base = fake_registry(BOT_DIGEST, BOT_BUNDLE, false).await;
        let registry = registry::Registry::at(&base).unwrap();
        let other = format!("sha256:{}", "e".repeat(64));
        let bundles = registry
            .attestations(ImageKind::Bot.repository(), &other)
            .await
            .unwrap();
        assert!(bundles.is_empty());
        assert!(verify_any(ImageKind::Bot, &other, &bundles).is_err());
    }

    #[tokio::test]
    async fn a_blob_that_doesnt_match_its_digest_is_an_error() {
        let base = fake_registry(BOT_DIGEST, BOT_BUNDLE, true).await;
        let registry = registry::Registry::at(&base).unwrap();
        let e = registry
            .attestations(ImageKind::Bot.repository(), BOT_DIGEST)
            .await
            .unwrap_err();
        assert!(e.contains("arrived with digest"), "{e}");
    }

    #[tokio::test]
    async fn an_unreachable_registry_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let registry = registry::Registry::at(&format!("http://{addr}")).unwrap();
        let e = registry
            .attestations(ImageKind::Bot.repository(), BOT_DIGEST)
            .await
            .unwrap_err();
        assert!(e.contains("unreachable"), "{e}");
    }

    #[tokio::test]
    async fn repositories_off_ghcr_are_refused_before_any_request() {
        let registry = registry::Registry::at("http://127.0.0.1:9").unwrap();
        let e = registry
            .attestations("docker.io/textile-protocol/textile-stitch", BOT_DIGEST)
            .await
            .unwrap_err();
        assert!(e.contains("not on ghcr.io"), "{e}");
    }
}
