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

/// A hosted panel wired like the compose file: its bots directory is a private
/// copy, refilled from an in-process broker that owns the real one (`h.root`).
fn hosted(tag: &str) -> Harness {
    hosted_with_broker_envelope(tag, envelope())
}

/// Same, with the broker holding `broker_envelope` while the panel keeps the
/// usual one. They're the same file in production; differing here shows which
/// check decides.
fn hosted_with_broker_envelope(tag: &str, broker_envelope: Envelope) -> Harness {
    let mut h = harness(tag);
    let mut cfg = (*h.state.cfg).clone();
    cfg.bots_dir = crate::panel::http::testkit::temp_root(&format!("{tag}-copy"));
    cfg.host_bots_dir = h.root.clone();
    let policy =
        crate::panel::broker::Policy::new("stitch-bot-a,stitch-bot-b,stitch-bot-c", REPO).unwrap();
    let broker = crate::panel::broker::Broker::new(h.docker.clone(), policy).with_config(
        crate::panel::broker_config::ConfigPolicy {
            host_root: h.root.clone(),
            root: h.root.clone(),
            envelope: broker_envelope,
            // SAFETY: getuid has no preconditions and can't fail.
            bot_uid: unsafe { libc::getuid() },
        },
    );
    h.state.config_store = Some(std::sync::Arc::new(crate::panel::broker::InProcessBroker(
        broker.router(),
    )));
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

/// The signer hosted bots use: custody that names its operator address in the
/// config, with no key file in the bot directory (#2832's layout).
const FIREBLOCKS: &str = r#"
[signer]
provider = "fireblocks"
vault_account_id = "0"
operator_address = "0x6b8499a0e002d3ece3fdbb320a127c9f0f4d4fe5"
"#;

fn seed(h: &Harness, name: &str, state: ContainerState) {
    let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
    setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
    let toml = h.root.join(name).join("stitch.toml");
    let text = std::fs::read_to_string(&toml).unwrap();
    assert!(!text.contains("[signer]"), "{text}");
    std::fs::write(&toml, format!("{text}\n{FIREBLOCKS}")).unwrap();
    std::fs::remove_file(h.root.join(name).join("stitch.key")).unwrap();
    keep_book_on(&toml);
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
async fn a_hosted_settings_save_is_written_by_the_broker_and_logged() {
    let h = hosted("hosted-save-broker");
    seed(&h, "bot-a", ContainerState::Exited);
    let (status, body) = send(
        &h,
        "PATCH",
        "/api/bots/bot-a/settings",
        PUBLIC,
        Some(json!({"buy": {"kind": "bps", "value": "42"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The host's file changed, and the change log beside it says the panel did it.
    let host_file = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
    assert_eq!(
        setup::read_settings_at(&host_file, 0).unwrap().buy.value,
        "42"
    );
    let journal = std::fs::read_to_string(h.root.join("bot-a/change-log.jsonl")).unwrap();
    let record = journal
        .lines()
        .find(|l| l.contains("\"source\":\"panel.settings\""))
        .unwrap_or_else(|| panic!("{journal}"));
    assert!(
        record.contains("buy_offset_bps") && record.contains("bot-a"),
        "{record}"
    );
    // The panel's copy follows, and the change log view reads it.
    let copy = h.state.cfg.bots_dir.join("bot-a/stitch.toml");
    assert_ne!(copy, h.root.join("bot-a/stitch.toml"));
    assert_eq!(std::fs::read_to_string(copy).unwrap(), host_file);
    let (status, body) = send(&h, "GET", "/api/changes?bot=bot-a", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("buy_offset_bps"), "{body}");
}

/// Forwards to the real broker, but moves the host file's sell spread to 77
/// just before a save reaches it: a host edit landing between the panel's
/// read and the broker's write.
struct HostEditsFirst {
    inner: std::sync::Arc<dyn super::mirror::ConfigStore>,
    host: std::path::PathBuf,
}

#[async_trait::async_trait]
impl super::mirror::ConfigStore for HostEditsFirst {
    async fn configs(&self) -> anyhow::Result<Vec<crate::panel::broker_config::WireBotFiles>> {
        self.inner.configs().await
    }

    async fn save(
        &self,
        container: &str,
        save: &crate::panel::broker_config::WireQuotingSave,
    ) -> anyhow::Result<crate::panel::broker_config::WireSaved> {
        let text = std::fs::read_to_string(&self.host).unwrap();
        let mut patch = setup::read_settings_at(&text, 0).unwrap().to_patch();
        patch.sell.value = "77".into();
        std::fs::write(&self.host, setup::apply_settings(&text, &patch).unwrap()).unwrap();
        self.inner.save(container, save).await
    }
}

#[tokio::test]
async fn a_partial_hosted_save_leaves_fields_it_did_not_name_as_the_host_has_them() {
    let mut h = hosted("hosted-partial-save");
    seed(&h, "bot-a", ContainerState::Exited);
    let host = h.root.join("bot-a/stitch.toml");
    let before = setup::read_settings_at(&std::fs::read_to_string(&host).unwrap(), 0).unwrap();
    assert_ne!(before.sell.value, "77");
    h.state.config_store = Some(std::sync::Arc::new(HostEditsFirst {
        inner: h.state.config_store.clone().unwrap(),
        host: host.clone(),
    }));
    h.app = router(h.state.clone());

    let (status, body) = send(
        &h,
        "PATCH",
        "/api/bots/bot-a/settings",
        PUBLIC,
        Some(json!({"ttlSecs": 120})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = setup::read_settings_at(&std::fs::read_to_string(&host).unwrap(), 0).unwrap();
    assert_eq!(after.ttl_secs, 120);
    assert_eq!(
        after.sell.value, "77",
        "a TTL-only save put the panel's older spread back"
    );
    assert_eq!(after.buy, before.buy);
    assert_eq!(after.buy_sizing, before.buy_sizing);
}

#[tokio::test]
async fn the_broker_refuses_what_its_envelope_refuses_even_when_the_panel_allows_it() {
    // A panel whose own check would pass (spreads 5 to 300 bps) against a broker
    // that allows no spread edits: the broker's answer stands, nothing is written.
    let h = hosted_with_broker_envelope("hosted-broker-decides", Envelope::default());
    seed(&h, "bot-a", ContainerState::Exited);
    let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
    let (status, body) = send(
        &h,
        "PATCH",
        "/api/bots/bot-a/settings",
        PUBLIC,
        Some(json!({"buy": {"kind": "bps", "value": "42"}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("envelope"), "{body}");
    assert_eq!(
        std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
        before
    );
}

#[tokio::test]
async fn the_panel_copy_holds_only_the_config_and_its_change_log() {
    let h = hosted("hosted-copy");
    seed(&h, "bot-a", ContainerState::Running);
    let dir = h.root.join("bot-a");
    std::fs::write(dir.join(setup::RFQ_API_KEY_FILE), "tx_live_secret").unwrap();
    std::fs::write(dir.join("stitch.key"), TEST_KEY).unwrap();
    std::fs::write(dir.join("fireblocks-api.key"), "fb-secret").unwrap();
    let (status, body) = send(&h, "GET", "/api/bots", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let copied: Vec<String> = std::fs::read_dir(h.state.cfg.bots_dir.join("bot-a"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(copied.contains(&"stitch.toml".to_string()), "{copied:?}");
    // Plus the mirror's own maker-key marker, which holds no key.
    assert!(copied.contains(&super::mirror::MAKER_KEY_MARKER.to_string()));
    for name in &copied {
        assert!(
            crate::panel::broker_config::ALLOWED_FILES.contains(&name.as_str())
                || name == super::mirror::MAKER_KEY_MARKER,
            "{name} reached the panel"
        );
        let text = std::fs::read_to_string(h.state.cfg.bots_dir.join("bot-a").join(name)).unwrap();
        assert!(
            !text.contains("tx_live_secret") && !text.contains("fb-secret"),
            "{name}"
        );
    }
    assert!(!body.contains("tx_live_secret") && !body.contains("fb-secret"));
}

#[tokio::test]
async fn a_hosted_panel_never_writes_a_config_itself() {
    let h = hosted("hosted-no-direct-write");
    seed(&h, "bot-a", ContainerState::Exited);
    let path = h.state.cfg.bots_dir.join("bot-a/stitch.toml");
    let origin = crate::changelog::Origin {
        source: crate::changelog::Source::PanelSettings,
        actor: "x".into(),
        bot: Some("bot-a".into()),
    };
    let err =
        crate::panel::http::changes::write_config(&h.state, &path, "x = 1", &origin).unwrap_err();
    assert_eq!(err.status, StatusCode::FORBIDDEN);
    assert!(
        crate::panel::http::changes::record_written(&h.state, &path, None, "x", &origin).is_some()
    );
}

/// An RFQ-only bot (the catalog default): no ladder, `[rfq]` on, and so only
/// startable with a maker key beside its config.
fn seed_rfq_only(h: &Harness, name: &str, key: Option<&str>) {
    seed(h, name, ContainerState::Exited);
    let dir = h.root.join(name);
    let toml = dir.join("stitch.toml");
    let text = std::fs::read_to_string(&toml)
        .unwrap()
        .replace("book_enabled = true", "book_enabled = false");
    std::fs::write(
        &toml,
        text + "\n[rfq]\nenabled = true\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\n\
                maker_id = \"mk_test\"\n\
                validation_contract = \"0x00000000000000000000000000000000000000aa\"\n",
    )
    .unwrap();
    if let Some(key) = key {
        setup::write_rfq_api_key(&dir, key).unwrap();
    }
}

fn started(h: &Harness) -> bool {
    h.docker.calls().iter().any(|c| matches!(c, Call::Start(_)))
}

#[tokio::test]
async fn an_rfq_only_bot_with_a_maker_key_starts_on_a_hosted_panel() {
    let h = hosted("hosted-rfq-key");
    seed_rfq_only(&h, "bot-a", Some("tx_live_hosted_maker_secret"));
    let (status, fleet) = send(&h, "GET", "/api/bots", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{fleet}");
    let (status, settings) = send(&h, "GET", "/api/bots/bot-a/settings", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert!(settings.contains("\"rfqApiKeySet\":true"), "{settings}");
    let (status, body) = send(&h, "POST", "/api/bots/bot-a/start", PUBLIC, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(started(&h));
    for text in [&fleet, &settings, &body] {
        assert!(!text.contains("tx_live_hosted_maker_secret"), "{text}");
    }
    // Neither the key nor anything holding it reached the panel's copy.
    let copy = h.state.cfg.bots_dir.join("bot-a");
    assert!(!copy.join(setup::RFQ_API_KEY_FILE).exists());
    for entry in std::fs::read_dir(&copy).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
        assert!(!text.contains("tx_live_hosted_maker_secret"));
    }
}

#[tokio::test]
async fn an_rfq_only_bot_without_a_maker_key_is_refused_start_on_a_hosted_panel() {
    let h = hosted("hosted-rfq-nokey");
    seed_rfq_only(&h, "bot-a", None);
    let (status, body) = send(&h, "POST", "/api/bots/bot-a/start", PUBLIC, Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Connect this bot to Textile"), "{body}");
    assert!(!started(&h));
    let (_, settings) = send(&h, "GET", "/api/bots/bot-a/settings", PUBLIC, None).await;
    assert!(settings.contains("\"rfqApiKeySet\":false"), "{settings}");
}

const KEYED_RPC: &str = "https://mainnet.infura.io/v3/9aa3d95b3bc440fa88ea12eaa4456161";
const KEYED_FALLBACK: &str =
    "https://ops:hunter2@bsc-mainnet.core.chainstack.com/3c6e0b8a9c15224a8228b9a98ca1531d";
// The feed carries a digit-free key in its path as well as one in its query.
const KEYED_FEED: &str =
    "https://feed.example/v1/abcdefghijklmnopqrstuvwxyzABCDEF/price?pair=cngn-usdt&apikey=sk_live_feed_secret";
const SECRETS: [&str; 5] = [
    "9aa3d95b3bc440fa88ea12eaa4456161",
    "hunter2",
    "3c6e0b8a9c15224a8228b9a98ca1531d",
    "sk_live_feed_secret",
    "abcdefghijklmnopqrstuvwxyzABCDEF",
];

/// Point `name`'s config at provider URLs that carry keys, the way a host
/// administrator would.
fn use_keyed_urls(root: &std::path::Path, name: &str) {
    let toml = root.join(name).join("stitch.toml");
    let text = std::fs::read_to_string(&toml).unwrap();
    let mut patch = setup::read_settings_at(&text, 0).unwrap().to_patch();
    patch.rpc_url = KEYED_RPC.into();
    patch.rpc_fallback_urls = Some(vec![KEYED_FALLBACK.into()]);
    patch.feed_url = KEYED_FEED.into();
    std::fs::write(&toml, setup::apply_settings(&text, &patch).unwrap()).unwrap();
}

fn no_secret_in(text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[tokio::test]
async fn hosted_settings_show_urls_with_their_keys_masked() {
    let h = hosted("hosted-masked-urls");
    seed(&h, "bot-a", ContainerState::Exited);
    use_keyed_urls(&h.root, "bot-a");
    let (status, body) = send(&h, "GET", "/api/bots/bot-a/settings", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    no_secret_in(&body);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["rpcUrl"], "https://mainnet.infura.io/REDACTED");
    assert_eq!(
        v["rpcFallbackUrls"][0],
        "https://bsc-mainnet.core.chainstack.com/REDACTED"
    );
    assert_eq!(v["feedUrl"], "https://feed.example/REDACTED");
}

#[tokio::test]
async fn a_hosted_save_that_sends_masked_urls_back_keeps_the_stored_ones() {
    let h = hosted("hosted-masked-roundtrip");
    seed(&h, "bot-a", ContainerState::Exited);
    use_keyed_urls(&h.root, "bot-a");
    let (_, shown) = send(&h, "GET", "/api/bots/bot-a/settings", PUBLIC, None).await;
    let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
    // The whole form back, masked URLs untouched, with one spread edit.
    let (status, body) = send(
        &h,
        "PATCH",
        "/api/bots/bot-a/settings",
        PUBLIC,
        Some(json!({
            "rpcUrl": shown["rpcUrl"],
            "rpcFallbackUrls": shown["rpcFallbackUrls"],
            "feedUrl": shown["feedUrl"],
            "buy": {"kind": "bps", "value": "42"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    no_secret_in(&body);
    let host = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
    assert!(!host.contains(crate::panel::hosted::redact::MASK), "{host}");
    let after = setup::read_settings_at(&host, 0).unwrap();
    assert_eq!(after.rpc_url, KEYED_RPC);
    assert_eq!(after.rpc_fallback_urls, vec![KEYED_FALLBACK.to_string()]);
    assert_eq!(after.feed_url, KEYED_FEED);
    assert_eq!(after.buy.value, "42");
}

#[tokio::test]
async fn a_hosted_save_of_an_edited_masked_url_is_refused_and_writes_nothing() {
    let h = hosted("hosted-masked-edit");
    seed(&h, "bot-a", ContainerState::Exited);
    use_keyed_urls(&h.root, "bot-a");
    let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
    for body in [
        json!({"rpcUrl": "https://other.infura.io/REDACTED"}),
        // Another field's mask is no echo of this one.
        json!({"rpcUrl": "https://feed.example/REDACTED"}),
        json!({"rpcFallbackUrls": ["https://REDACTED@x.example/"]}),
        json!({"feedUrl": "https://feed.example/v1/price?apikey=REDACTED&pair=x"}),
    ] {
        let (status, text) =
            send(&h, "PATCH", "/api/bots/bot-a/settings", PUBLIC, Some(body)).await;
        assert!(status.is_client_error(), "{status}: {text}");
    }
    assert_eq!(
        std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
        before
    );
}

#[tokio::test]
async fn the_hosted_change_log_masks_urls_in_its_values() {
    let h = hosted("hosted-masked-changes");
    seed(&h, "bot-a", ContainerState::Exited);
    // A first save so the log has a baseline, then the host moves the feed
    // onto a path-keyed URL, which the next save catches up as a host edit.
    for value in ["41", "42"] {
        if value == "42" {
            let toml = h.root.join("bot-a/stitch.toml");
            let text = std::fs::read_to_string(&toml).unwrap();
            let mut patch = setup::read_settings_at(&text, 0).unwrap().to_patch();
            patch.feed_url =
                "https://feed.example/v1/3c6e0b8a9c15224a8228b9a98ca1531d/price".into();
            std::fs::write(&toml, setup::apply_settings(&text, &patch).unwrap()).unwrap();
        }
        let (status, body) = send(
            &h,
            "PATCH",
            "/api/bots/bot-a/settings",
            PUBLIC,
            Some(json!({"buy": {"kind": "bps", "value": value}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = send(&h, "GET", "/api/changes?bot=bot-a", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("feed_url"), "{body}");
    assert!(body.contains("https://feed.example/REDACTED"), "{body}");
    no_secret_in(&body);
}

/// Point the bot's RPC at a keyed URL nothing answers on, so status reads
/// fail with errors that name it.
fn use_dead_keyed_rpc(root: &std::path::Path, name: &str) -> String {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let rpc = format!("http://127.0.0.1:{port}/v3/9aa3d95b3bc440fa88ea12eaa4456161");
    let toml = root.join(name).join("stitch.toml");
    let text = std::fs::read_to_string(&toml).unwrap();
    let mut patch = setup::read_settings_at(&text, 0).unwrap().to_patch();
    patch.rpc_url = rpc.clone();
    patch.feed_url = "http://127.0.0.1:9/v1/3c6e0b8a9c15224a8228b9a98ca1531d/price".into();
    std::fs::write(&toml, setup::apply_settings(&text, &patch).unwrap()).unwrap();
    rpc
}

#[tokio::test]
async fn hosted_status_errors_mask_the_urls_they_name() {
    for route in ["/api/bots/bot-a/funding", "/api/bots/bot-a/allowances"] {
        // The full panel names the URL in its error: that's what's at stake.
        let full = harness("full-status-urls");
        seed(&full, "bot-a", ContainerState::Exited);
        use_dead_keyed_rpc(&full.root, "bot-a");
        let (_, raw) = full.get(route).await;
        assert!(
            raw.contains("9aa3d95b3bc440fa88ea12eaa4456161")
                || raw.contains("3c6e0b8a9c15224a8228b9a98ca1531d"),
            "{route} names no URL on the full profile, so this test proves nothing: {raw}"
        );

        let h = hosted("hosted-status-urls");
        seed(&h, "bot-a", ContainerState::Exited);
        use_dead_keyed_rpc(&h.root, "bot-a");
        let (_, body) = send(&h, "GET", route, PUBLIC, None).await;
        no_secret_in(&body);
        assert!(
            body.contains("127.0.0.1"),
            "{route}: the host stays: {body}"
        );
    }
}

#[tokio::test]
async fn every_hosted_json_response_is_masked() {
    use axum::routing::get;
    let keyed = "https://mainnet.infura.io/v3/9aa3d95b3bc440fa88ea12eaa4456161";
    let app = axum::Router::new()
        .route(
            "/err",
            get(move || async move {
                crate::panel::http::ApiError::bad_request(format!("the RPC at {keyed} failed"))
            }),
        )
        .route(
            "/plain",
            get(|| async { "not json: https://x/9aa3d95b3bc440fa88ea12eaa4456161" }),
        )
        .route_layer(axum::middleware::from_fn(super::redact::mask_responses));
    let call = |path: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
    };
    let (status, body) = call("/err").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("https://mainnet.infura.io/REDACTED"),
        "{body}"
    );
    no_secret_in(&body);
    // Only JSON is rewritten; text bodies are none of the panel's API.
    let (_, body) = call("/plain").await;
    assert!(body.contains("9aa3d95b3bc440fa88ea12eaa4456161"));
}

#[tokio::test]
async fn the_hosted_log_tail_masks_urls_in_each_line() {
    let line =
        "WARN rpc error at https://mainnet.infura.io/v3/9aa3d95b3bc440fa88ea12eaa4456161: timeout";
    let h = hosted("hosted-log-urls");
    seed(&h, "bot-a", ContainerState::Running);
    h.docker
        .set_log_lines(vec![crate::panel::docker::fake::out(line)]);
    let (status, body) = send(&h, "GET", "/api/bots/bot-a/logs?follow=false", PUBLIC, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("https://mainnet.infura.io/REDACTED"),
        "{body}"
    );
    no_secret_in(&body);
    // A full panel tails the line as the bot printed it.
    let full = harness("full-log-urls");
    seed(&full, "bot-a", ContainerState::Running);
    full.docker
        .set_log_lines(vec![crate::panel::docker::fake::out(line)]);
    let (_, raw) = full.get("/api/bots/bot-a/logs?follow=false").await;
    assert!(raw.contains("9aa3d95b3bc440fa88ea12eaa4456161"), "{raw}");
}

#[tokio::test]
async fn a_full_panel_still_shows_urls_as_they_are() {
    let h = harness("full-urls");
    seed(&h, "bot-a", ContainerState::Exited);
    use_keyed_urls(&h.root, "bot-a");
    let (status, body) = h.get("/api/bots/bot-a/settings").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["rpcUrl"], KEYED_RPC);
    assert_eq!(v["rpcFallbackUrls"][0], KEYED_FALLBACK);
    assert_eq!(v["feedUrl"], KEYED_FEED);
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
