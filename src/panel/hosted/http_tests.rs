// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The hosted profile through the real router, against the fake runtime.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use super::policy::{Rule, ROUTES};
use super::{Envelope, HostedConfig, Profile, BREAK_GLASS_ORIGIN};
use crate::panel::attestation::tests::fetch::fake_registry;
use crate::panel::attestation::tests::{BOT_BUNDLE, BOT_DIGEST, PANEL_BUNDLE};
use crate::panel::auth::SESSION_COOKIE;
use crate::panel::docker::fake::{container, dir_layout_mounts, Call};
use crate::panel::docker::ContainerState;
use crate::panel::http::router;
use crate::panel::http::testkit::{harness, keep_book_on, Harness, TEST_KEY};
use crate::panel::naming::LABEL_BOT;
use crate::setup;

const PUBLIC: &str = "https://stitch-panel.example.ts.net";
const REPO: &str = "ghcr.io/textile-protocol/textile-stitch";

fn envelope() -> Envelope {
    Envelope::parse(
        r#"{"spread_bps": {"min": 5, "max": 300}, "ttl_secs": {"min": 30, "max": 600}}"#,
    )
    .unwrap()
}

fn hosted(tag: &str) -> Harness {
    let mut h = harness(tag);
    let mut cfg = (*h.state.cfg).clone();
    cfg.profile = Profile::Hosted(HostedConfig {
        public_origin: PUBLIC.into(),
        image_repository: REPO.into(),
        envelope: envelope(),
        broker_socket: "/nonexistent".into(),
        publish_bind: None,
        attestation_registry: crate::panel::attestation::registry::GHCR.into(),
    });
    h.state.cfg = std::sync::Arc::new(cfg);
    h.app = router(h.state.clone());
    h
}

fn seed(h: &Harness, name: &str, state: ContainerState) {
    let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
    setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
    keep_book_on(&h.root.join(name).join("stitch.toml"));
    let mut c = container(&format!("stitch-{name}"), state);
    c.image = format!("{REPO}@sha256:{}", "1".repeat(64));
    c.labels.insert(LABEL_BOT.to_string(), name.to_string());
    c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
    h.docker.add_container(c);
}

/// A browser request from `origin`, with the harness's session.
async fn send(
    h: &Harness,
    method: &str,
    path: &str,
    origin: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let host = origin.split_once("://").unwrap().1;
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", host)
        .header("cookie", format!("{SESSION_COOKIE}={}", h.token));
    if method != "GET" {
        req = req.header("origin", origin);
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = h
        .app
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn mutating_calls(h: &Harness) -> Vec<Call> {
    h.docker
        .calls()
        .into_iter()
        .filter(|c| {
            !matches!(
                c,
                Call::Logs { .. } | Call::LocalImageDigests(_) | Call::ReadFiles { .. }
            )
        })
        .collect()
}

#[tokio::test]
async fn every_refused_route_is_refused_before_its_handler_runs() {
    let h = hosted("hosted-refused");
    seed(&h, "bot-a", ContainerState::Running);
    let config = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
    for (method, template, rule) in ROUTES {
        let Rule::Refuse(reason) = rule else { continue };
        let path = template
            .replace("{name}", "bot-a")
            .replace("{id}", "1")
            .replace("{index}", "0");
        let (status, body) = send(&h, method, &path, PUBLIC, Some(json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
        assert!(body.contains(reason), "{method} {path}: {body}");
    }
    assert!(mutating_calls(&h).is_empty(), "{:?}", mutating_calls(&h));
    assert_eq!(
        std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
        config
    );
}

#[tokio::test]
async fn status_logs_changes_and_lifecycle_work() {
    let h = hosted("hosted-allowed");
    seed(&h, "bot-a", ContainerState::Exited);
    for path in [
        "/api/bots",
        "/api/bots/bot-a",
        "/api/changes",
        "/api/bots/bot-a/settings",
        "/api/bots/bot-a/logs?follow=false",
    ] {
        let (status, body) = send(&h, "GET", path, PUBLIC, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let (status, body) = send(
        &h,
        "POST",
        "/api/bots/bot-a/start",
        BREAK_GLASS_ORIGIN,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(h.docker.calls().iter().any(|c| matches!(c, Call::Start(_))));
    let (status, body) = send(&h, "POST", "/api/bots/bot-a/stop", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(h
        .docker
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Stop { .. })));
}

#[tokio::test]
async fn quoting_edits_stay_inside_the_envelope() {
    let h = hosted("hosted-envelope");
    seed(&h, "bot-a", ContainerState::Exited);
    let path = "/api/bots/bot-a/settings";

    let (status, body) = send(
        &h,
        "PATCH",
        path,
        PUBLIC,
        Some(json!({"buy": {"kind": "bps", "value": "42"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(Harness::parse(&body)["settings"]["buy"]["value"], "42");

    for (patch, why) in [
        (json!({"buy": {"kind": "bps", "value": "900"}}), "outside"),
        (json!({"ttlSecs": 5000}), "outside"),
        (json!({"refreshThresholdBps": 7}), "no bound"),
        (json!({"rpcUrl": "https://rpc.attacker.example"}), "RPC URL"),
        (
            json!({"rpcFallbackUrls": ["https://rpc.attacker.example"]}),
            "fallback RPCs",
        ),
        (
            json!({"feedUrl": "https://feed.attacker.example"}),
            "price feed",
        ),
        (json!({"rfqApiKey": "secret"}), "key files"),
    ] {
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let (status, body) = send(&h, "PATCH", path, PUBLIC, Some(patch.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{patch}: {body}");
        assert!(body.contains(why), "{patch}: {body}");
        assert_eq!(
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
            before,
            "{patch}"
        );
    }
    assert!(!h.root.join("bot-a").join(setup::RFQ_API_KEY_FILE).exists());
}

/// The real bot digest the attestation fixture covers.
fn attested() -> String {
    format!("{REPO}@{BOT_DIGEST}")
}

/// A hosted harness whose attestation registry is a local GHCR stand-in serving
/// the real bot bundle for [`attested`].
async fn hosted_attested(tag: &str) -> Harness {
    hosted_with_registry(tag, fake_registry(BOT_DIGEST, BOT_BUNDLE, false).await)
}

fn hosted_with_registry(tag: &str, registry: String) -> Harness {
    let mut h = hosted(tag);
    let mut cfg = (*h.state.cfg).clone();
    if let Profile::Hosted(hosted) = &mut cfg.profile {
        hosted.attestation_registry = registry;
    }
    h.state.cfg = std::sync::Arc::new(cfg);
    h.app = router(h.state.clone());
    h
}

/// A digest with no attestation, with the wrong one, or with the registry
/// unreachable is refused before the config lock, the wallet or the runtime are
/// touched.
#[tokio::test]
async fn an_image_without_a_valid_attestation_is_refused_before_anything_moves() {
    let unattested = format!("{REPO}@sha256:{}", "2".repeat(64));
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}")
    };
    let cases = [
        (
            "no attestation published",
            hosted_attested("hosted-att-none").await,
            unattested.clone(),
            "no build attestation found",
        ),
        (
            "the panel image's attestation",
            hosted_with_registry(
                "hosted-att-panel",
                fake_registry(BOT_DIGEST, PANEL_BUNDLE, false).await,
            ),
            attested(),
            "identity",
        ),
        (
            "registry unreachable",
            hosted_with_registry("hosted-att-down", closed),
            attested(),
            "unreachable",
        ),
    ];
    for (what, h, image, expect) in cases {
        seed(&h, "bot-a", ContainerState::Running);
        let (status, body) = send(
            &h,
            "POST",
            "/api/bots/bot-a/image",
            PUBLIC,
            Some(json!({ "image": image })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}: {body}");
        assert!(body.contains(expect), "{what}: {body}");
        assert!(mutating_calls(&h).is_empty(), "{what}");
        assert!(h.docker.attestations_sent().is_empty(), "{what}");
        let bot = h.state.bot("bot-a").await.unwrap();
        assert!(h.state.wallet_locks.try_claim_for(&bot).is_some(), "{what}");
    }
}

#[tokio::test]
async fn update_and_rollback_take_only_a_digest() {
    let h = hosted_attested("hosted-image").await;
    seed(&h, "bot-a", ContainerState::Running);
    let path = "/api/bots/bot-a/image";
    for image in [
        format!("{REPO}:latest"),
        format!("{REPO}:sha-abc1234"),
        format!("{REPO}:latest@sha256:{}", "2".repeat(64)),
        format!("ghcr.io/someone-else/stitch@sha256:{}", "2".repeat(64)),
    ] {
        let (status, body) = send(&h, "POST", path, PUBLIC, Some(json!({ "image": image }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{image}: {body}");
    }
    assert!(mutating_calls(&h).is_empty());
    let digest = attested();
    let (status, body) = send(&h, "POST", path, PUBLIC, Some(json!({ "image": digest }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from commit 833f1809"), "{body}");
    assert!(!body.contains("not checked"), "{body}");
    assert_eq!(
        h.docker.attestations_sent(),
        vec![vec![BOT_BUNDLE.to_string()]],
        "the broker gets the bundles to verify itself"
    );
    assert!(h
        .docker
        .calls()
        .iter()
        .any(|c| matches!(c, Call::ReplaceImage { name, new_image } if name == "stitch-bot-a" && *new_image == digest)));
    let (status, _) = send(&h, "POST", path, PUBLIC, Some(json!({ "image": digest }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "same digest twice");
}

fn image_request(h: &Harness, image: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/bots/bot-a/image")
        .header("host", PUBLIC.split_once("://").unwrap().1)
        .header("origin", PUBLIC)
        .header("cookie", format!("{SESSION_COOKIE}={}", h.token))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "image": image }).to_string()))
        .unwrap()
}

#[tokio::test]
async fn a_swap_holds_the_operator_wallet_so_a_launch_on_it_is_refused() {
    // bot-a and bot-b share TEST_KEY, so one operator wallet.
    let h = hosted_attested("hosted-image-claim").await;
    seed(&h, "bot-a", ContainerState::Running);
    seed(&h, "bot-b", ContainerState::Exited);
    let gate = h.docker.gate_replace();
    let digest = attested();
    let swap = tokio::spawn(h.app.clone().oneshot(image_request(&h, &digest)));
    gate.entered.notified().await;

    // Mid-swap: the wallet is claimed, and a Start on the other bot is refused
    // before it reaches Docker.
    let bot_b = h.state.bot("bot-b").await.unwrap();
    assert!(h.state.wallet_locks.try_claim_for(&bot_b).is_none());
    let (status, body) = send(&h, "POST", "/api/bots/bot-b/start", PUBLIC, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("operator wallet is busy"), "{body}");
    assert!(!h
        .docker
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Start(n) if n == "stitch-bot-b")));

    gate.release.notify_one();
    let res = swap.await.unwrap().unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // Released once the swap returns.
    assert!(h.state.wallet_locks.try_claim_for(&bot_b).is_some());
}

#[tokio::test]
async fn a_failed_swap_with_a_known_outcome_leaves_the_bot_alone() {
    use crate::panel::docker::swap::SwapOutcome;
    for outcome in [SwapOutcome::Unchanged, SwapOutcome::Restored] {
        let h = hosted_attested("hosted-image-known").await;
        seed(&h, "bot-a", ContainerState::Running);
        h.docker.fail_replace("registry said no", Some(outcome));
        let digest = attested();
        let (status, body) = send(
            &h,
            "POST",
            "/api/bots/bot-a/image",
            PUBLIC,
            Some(json!({ "image": digest })),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "{outcome:?}"
        );
        let bot = h.state.bot("bot-a").await.unwrap();
        assert!(h.state.wallet_locks.try_claim_for(&bot).is_some());
    }
}

#[tokio::test]
async fn a_failed_swap_with_an_unknown_outcome_stops_the_bot_before_releasing_the_wallet() {
    let h = hosted_attested("hosted-image-unknown").await;
    seed(&h, "bot-a", ContainerState::Running);
    h.docker
        .fail_replace("the runtime broker isn't answering", None);
    let digest = attested();
    let (status, body) = send(
        &h,
        "POST",
        "/api/bots/bot-a/image",
        PUBLIC,
        Some(json!({ "image": digest })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(h
        .docker
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Stop { name, .. } if name == "stitch-bot-a")));
    let bot = h.state.bot("bot-a").await.unwrap();
    assert!(h.state.wallet_locks.try_claim_for(&bot).is_some());
}

#[tokio::test]
async fn a_paused_bot_is_refused_a_swap_before_the_runtime_is_asked() {
    let h = hosted_attested("hosted-image-paused").await;
    seed(&h, "bot-a", ContainerState::Paused);
    let digest = attested();
    let (status, body) = send(
        &h,
        "POST",
        "/api/bots/bot-a/image",
        PUBLIC,
        Some(json!({ "image": digest })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("paused"), "{body}");
    assert!(!h
        .docker
        .calls()
        .iter()
        .any(|c| matches!(c, Call::ReplaceImage { .. })));
}

#[tokio::test]
async fn a_stopped_bot_swaps_without_claiming_its_wallet() {
    let h = hosted_attested("hosted-image-stopped").await;
    seed(&h, "bot-a", ContainerState::Exited);
    let gate = h.docker.gate_replace();
    let digest = attested();
    let swap = tokio::spawn(h.app.clone().oneshot(image_request(&h, &digest)));
    gate.entered.notified().await;
    let bot = h.state.bot("bot-a").await.unwrap();
    assert!(h.state.wallet_locks.try_claim_for(&bot).is_some());
    gate.release.notify_one();
    assert_eq!(swap.await.unwrap().unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn only_the_tailnet_and_break_glass_origins_get_in() {
    let h = hosted("hosted-origin");
    seed(&h, "bot-a", ContainerState::Running);
    for origin in [
        "https://attacker.example",
        "http://localhost:9999",
        "http://127.0.0.1:8420",
    ] {
        let (status, body) = send(&h, "POST", "/api/bots/bot-a/stop", origin, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}: {body}");
    }
    // Right host, wrong or missing Origin.
    let req = Request::builder()
        .method("POST")
        .uri("/api/bots/bot-a/stop")
        .header("host", "localhost:8420")
        .header("cookie", format!("{SESSION_COOKIE}={}", h.token))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        h.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert!(mutating_calls(&h).is_empty());
}

#[tokio::test]
async fn the_session_says_which_profile_this_is() {
    let h = hosted("hosted-session");
    let (_, body) = send(&h, "GET", "/api/session", PUBLIC, None).await;
    assert_eq!(Harness::parse(&body)["profile"], "hosted");
    let full = harness("full-session");
    let (_, body) = full.get("/api/session").await;
    assert_eq!(Harness::parse(&body)["profile"], "full");
}

/// The full profile (every existing install) never sees the hosted layers: no
/// digest route, no policy refusals, no Host/Origin pinning.
#[tokio::test]
async fn the_full_profile_is_untouched() {
    let h = harness("full-untouched");
    seed(&h, "bot-a", ContainerState::Running);
    let (status, body) = h
        .post_json(
            "/api/bots/bot-a/image",
            json!({ "image": format!("{REPO}@sha256:{}", "2".repeat(64)) }),
        )
        .await;
    assert!(!status.is_success(), "{status}: {body}");
    assert!(h
        .docker
        .calls()
        .iter()
        .all(|c| !matches!(c, Call::ReplaceImage { .. })));
    for (method, template, rule) in ROUTES {
        // Updates and versions ask the registry; the rest answer locally.
        if !matches!(rule, Rule::Refuse(_))
            || *method != "GET"
            || template.contains("updates")
            || template.contains("versions")
        {
            continue;
        }
        let path = template.replace("{name}", "bot-a");
        let (_, body) = h.get(&path).await;
        assert!(!body.contains("hosted panel"), "{path}: {body}");
    }
    // No Host header at all, as in every existing test: still served.
    let (status, body) = h.get("/api/bots").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
