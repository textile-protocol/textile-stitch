// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Connect the bot to Textile RFQ by signing MakerEnroll with the funding wallet.
//!
//! The panel signs, POSTs `/v2/maker/enroll`, and writes `[rfq]` plus
//! `rfq-api.key`. The browser never sees the key.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::settings::{config_path, read_toml, save_and_restart};
use super::{ApiError, AppState};
use crate::config::{rfq_default_flag_in_dir, Config};
use crate::panel::inventory::{Bot, Layout};
use crate::panel::provision;
use crate::setup;
use crate::signer::{
    build_signer_with, parse_private_key, DynSigner, LocalSigner, SignerConfig, SignerSecrets,
};

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnrollBody {
    /// Override the venue enroll URL. Tests use this; the UI does not.
    #[serde(default)]
    pub venue_url: Option<String>,
}

async fn signer_for_bot(cfg: &Config, config_path: &Path) -> Result<DynSigner, ApiError> {
    match cfg.signer.clone().unwrap_or(SignerConfig::Local) {
        SignerConfig::Local => {
            let key = provision::find_beside(config_path, "stitch.key").ok_or_else(|| {
                ApiError::bad_request(
                    "this bot has no stitch.key beside its config, so it cannot sign enroll",
                )
            })?;
            let raw = std::fs::read_to_string(&key)
                .map_err(|e| ApiError::internal(&anyhow::anyhow!("reading stitch.key: {e}")))?;
            let parsed = parse_private_key(&raw).map_err(ApiError::bad_request)?;
            Ok(Arc::new(LocalSigner::new(parsed)))
        }
        // Remote signers read their secret from the process env, which is right
        // for the bot (one config per process) and wrong for the panel (one
        // process, many bots). This used to set the env var, build, and restore
        // it — but `build_signer` awaits in the middle, so two concurrent
        // Connects for different bots interleaved: one built against the
        // other's secret, and the restores wrote a stale bot-specific path
        // back. Naming the file per call removes the shared channel rather than
        // trying to serialize writes to it.
        _ => build_signer_with(cfg, &secrets_beside(config_path))
            .await
            .map_err(|e| ApiError::bad_request(format!("could not build the bot signer: {e:#}"))),
    }
}

/// The signer secrets sitting next to this bot's config, if any.
fn secrets_beside(config_path: &Path) -> SignerSecrets {
    SignerSecrets {
        turnkey_api_private_key_file: provision::find_beside(config_path, "turnkey-api.key"),
        mpcvault_api_token_file: provision::find_beside(config_path, "mpcvault-api.token"),
    }
}

/// A flat-layout Docker bot cannot see `rfq-api.key`.
///
/// `flat_bot_mounts` binds exactly two paths — `stitch.toml` and the signer
/// secret — so the key Connect writes beside the host config never appears in
/// the container, and `save_and_restart` restarts rather than recreates, so no
/// new mount or env arrives either. Connecting anyway would report success,
/// stamp `book_enabled = false`, and leave the bot with no RFQ *and* no ladder.
///
/// Recreating with an extra mount is not the escape hatch: flat layout keeps the
/// slot-nonce ledger inside the container, which is why Update and Roll back
/// already refuse it. Migration is the fix, same as those paths.
///
/// Docker only. In process mode the child reads the host config directly, so the
/// sibling key resolves whatever the layout is.
fn refuse_connect_on_unmigrated_flat_layout(
    bot: &Bot,
    runtime: crate::panel::PanelRuntime,
) -> Result<(), ApiError> {
    if runtime != crate::panel::PanelRuntime::Docker || bot.layout != Layout::FlatFiles {
        return Ok(());
    }
    Err(ApiError::conflict(format!(
        "{} still uses the flat file layout, so its container only mounts stitch.toml and the \
         signer key — it could not read the maker credential Connect writes. Migrate it to the \
         per-bot directory layout first, then Connect.",
        bot.name
    )))
}

pub async fn enroll(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<EnrollBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    refuse_connect_on_unmigrated_flat_layout(&bot, state.cfg.runtime)?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = Config::from_toml(&current_toml)
        .map_err(|e| ApiError::bad_request(format!("this config isn't valid: {e:#}")))?;
    let rfq_default = cfg.rfq_default_unlocked() || rfq_default_flag_in_dir(&state.cfg.bots_dir);

    let signer = signer_for_bot(&cfg, &path).await?;
    let venue = crate::enroll::enroll_url_from_config(&cfg, body.venue_url.as_deref());
    let enrolled = crate::enroll::register_maker(&cfg, &signer, &venue)
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let (edited, outcome) =
        crate::enroll::apply_enrollment(&current_toml, &cfg, &enrolled, rfq_default)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    let dir = path.parent().ok_or_else(|| {
        ApiError::internal(&anyhow::anyhow!(
            "{}'s config has no parent directory",
            bot.name
        ))
    })?;
    setup::write_rfq_api_key(dir, &enrolled.api_key)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    crate::panel::provision::hand_over_paths_to_bot(
        dir,
        &[
            setup::RFQ_API_KEY_FILE.to_string(),
            "stitch.env".to_string(),
        ],
        state.cfg.bot_uid,
    )
    .map_err(|e| ApiError::internal(&e))?;

    let message = match outcome {
        crate::enroll::EnrollOutcome::Flagged => format!(
            "Registered as {} ({}). Textile has flagged this maker — you will not receive Swap quotes.",
            enrolled.maker_slug, enrolled.environment
        ),
        crate::enroll::EnrollOutcome::Waiting => format!(
            "Registered as {} ({}). Request access so Textile can review this maker. You will \
             not receive Swap quotes until they approve you.",
            enrolled.maker_slug, enrolled.environment
        ),
        crate::enroll::EnrollOutcome::Live if rfq_default => format!(
            "Connected to Textile as {} ({}). This bot now quotes Swap only — it will not rest orders on the public book.",
            enrolled.maker_slug, enrolled.environment
        ),
        crate::enroll::EnrollOutcome::Live => format!(
            "Connected to Textile as {} ({}).",
            enrolled.maker_slug, enrolled.environment
        ),
    };

    save_and_restart(
        &state,
        &bot,
        &path,
        &edited,
        0,
        Some(json!({
            "message": message,
            "enrollment": {
                "makerSlug": enrolled.maker_slug,
                "environment": enrolled.environment,
                "corridors": enrolled.corridors,
                "flagged": enrolled.flagged,
            }
        })),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::Value;

    use super::super::testkit::{harness, harness_process, Harness, TEST_KEY};
    use crate::panel::docker::fake::{container, dir_layout_mounts, flat_layout_mounts};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::LABEL_BOT;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::Router;
    use serde_json::json;

    fn seed(h: &Harness, name: &str) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let mut c = container(&format!("stitch-{name}"), ContainerState::Running);
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    /// An adopted bot still on the flat file layout: config and key sit in the
    /// bots root under per-bot names, and the container mounts just those two.
    fn seed_flat(h: &Harness, name: &str) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(
            h.root.join(format!("stitch.{name}.toml")),
            corridor.toml_template,
        )
        .unwrap();
        std::fs::write(h.root.join(format!("stitch.{name}.key")), TEST_KEY).unwrap();
        let mut c = container(&format!("stitch-{name}"), ContainerState::Running);
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), name);
        h.docker.add_container(c);
    }

    #[tokio::test]
    async fn connect_refuses_a_flat_layout_docker_bot() {
        // The container mounts only stitch.toml and the signer key, so the
        // rfq-api.key Connect writes beside the host config is invisible to it —
        // and a restart brings no new mount. Connecting would report success and
        // leave the bot with neither RFQ nor its ladder.
        let h = harness("rfq-enroll-flat");
        seed_flat(&h, "bot1");

        let (status, body) = h.post_json("/api/bots/bot1/rfq/enroll", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("flat file layout"), "{body}");
        assert!(body.contains("Migrate"), "{body}");
        assert!(
            !h.root.join("rfq-api.key").exists(),
            "nothing may be written before the refusal"
        );
    }

    #[tokio::test]
    async fn connect_allows_a_flat_layout_bot_in_process_mode() {
        // No container, no mounts: the child reads the host config directly, so
        // the sibling key resolves whatever the layout is. Refusing here would
        // block a bot that would have quoted.
        let h = harness_process("rfq-enroll-flat-process");
        seed_flat(&h, "bot1");
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", vec!["cngn-usdt-bsc"], false).await;

        let (status, body) = h
            .post_json("/api/bots/bot1/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    fn unlock_rfq_panel(h: &Harness, name: &str) {
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!(
                "{toml}\n[experimental]\nrfq_panel = \"{}\"\n",
                crate::config::RFQ_PANEL_GATE
            ),
        )
        .unwrap();
    }

    async fn mock_venue(
        api_key: &'static str,
        corridors: Vec<&'static str>,
        flagged: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        mock_venue_with_pairs(api_key, corridors, vec![], flagged).await
    }

    async fn mock_venue_with_pairs(
        api_key: &'static str,
        corridors: Vec<&'static str>,
        corridor_pairs: Vec<Value>,
        flagged: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/v2/maker/enroll",
            post(move |Json(body): Json<Value>| {
                let corridors = corridors.clone();
                let corridor_pairs = corridor_pairs.clone();
                async move {
                    assert!(body["signature"].as_str().unwrap().starts_with("0x"));
                    assert_eq!(body["chainId"], 56);
                    Json(json!({
                        "makerId": "clmakerenroll1",
                        "makerSlug": "stitch-56-f39fd6e5",
                        "environment": "LIVE",
                        "apiKey": api_key,
                        "streamUrl": "wss://api.textilecredit.com/v2/maker/stream",
                        "validationContract": "0xBCA5E344077AaC751A1C548a45F28215bB7ec165",
                        "corridors": corridors,
                        "corridorPairs": corridor_pairs,
                        "flagged": flagged,
                    }))
                }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}/v2/maker/enroll"), handle)
    }

    #[tokio::test]
    async fn connect_does_not_require_a_gate_token() {
        let h = harness("rfq-enroll-unlocked");
        seed(&h, "bot-a");
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", vec!["cngn-usdt-bsc"], false).await;
        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["settings"]["rfqEnabled"], true);
    }

    #[tokio::test]
    async fn connect_writes_rfq_and_never_echoes_the_key() {
        let h = harness("rfq-enroll-connect");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", vec!["cngn-usdt-bsc"], false).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.contains("tx_live_enroll_secret"),
            "the raw key must never appear in the HTTP body: {body}"
        );
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], true);
        assert_eq!(v["settings"]["rfqPanelUnlocked"], true);
        assert_eq!(v["settings"]["rfqMakerId"], "clmakerenroll1");
        assert_eq!(v["settings"]["rfqCorridor"], "cngn-usdt-bsc");
        assert_eq!(v["settings"]["rfqApiKeySet"], true);
        assert!(v["settings"].get("rfqApiKey").is_none());
        assert_eq!(v["enrollment"]["makerSlug"], "stitch-56-f39fd6e5");
        assert_eq!(v["enrollment"]["environment"], "LIVE");

        let stored = std::fs::read_to_string(h.root.join("bot-a").join("rfq-api.key")).unwrap();
        assert_eq!(stored.trim(), "tx_live_enroll_secret");
        let toml = std::fs::read_to_string(h.root.join("bot-a").join("stitch.toml")).unwrap();
        assert!(toml.contains("[rfq]"));
        assert!(toml.contains("clmakerenroll1"));
        assert!(!toml.contains("tx_live_enroll_secret"));
        assert!(
            toml.contains("book_enabled = false"),
            "Connect writes RFQ-only"
        );

        let (status, body) = h.get("/api/bots/bot-a/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["rfqApiKeySet"], true);
        assert!(v.get("rfqApiKey").is_none());
        assert!(!body.contains("tx_live_enroll_secret"));
    }

    fn rewrite_pool_tokens(h: &Harness, name: &str, collateral: &str, debt: &str) {
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace("0xa8AEA66B361a8d53e8865c62D142167Af28Af058", collateral)
            .replace("0x55d398326f99059fF775485246999027B3197955", debt);
        std::fs::write(&path, toml).unwrap();
        assert!(
            setup::identify_corridor(&std::fs::read_to_string(&path).unwrap()).is_none(),
            "rewritten tokens must look custom, not catalog"
        );
    }

    #[tokio::test]
    async fn connect_goes_live_on_a_custom_pair_via_tokens() {
        let h = harness("rfq-enroll-custom");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        rewrite_pool_tokens(
            &h,
            "bot-a",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let (venue, _server) = mock_venue_with_pairs(
            "tx_live_custom",
            vec!["ops-custom-bsc"],
            vec![json!({
                "slug": "ops-custom-bsc",
                "chainId": 56,
                "collateralToken": "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa",
                "debtToken": "0xBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBb",
            })],
            false,
        )
        .await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], true);
        assert_eq!(v["settings"]["rfqCorridor"], "ops-custom-bsc");
    }

    #[tokio::test]
    async fn connect_does_not_go_live_on_an_unrelated_corridor() {
        let h = harness("rfq-enroll-unrelated");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        let (venue, _server) = mock_venue("tx_live_unrelated", vec!["wars-usdt-bsc"], false).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], false);
        assert_eq!(v["settings"]["rfqCorridor"], "");
        assert_eq!(v["settings"]["bookEnabled"], false);
    }

    #[tokio::test]
    async fn connect_with_empty_corridors_registers_and_does_not_go_live() {
        let h = harness("rfq-enroll-waiting");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        let (venue, _server) = mock_venue("tx_live_enroll_secret", vec![], false).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], false);
        assert_eq!(v["settings"]["rfqMakerId"], "clmakerenroll1");
        assert_eq!(v["settings"]["rfqCorridor"], "");
        assert_eq!(v["settings"]["rfqApiKeySet"], true);
        assert_eq!(v["enrollment"]["corridors"], json!([]));
        assert!(
            v["message"]
                .as_str()
                .unwrap_or("")
                .contains("Request access so Textile can review"),
            "waiting copy missing: {body}"
        );

        let toml = std::fs::read_to_string(h.root.join("bot-a").join("stitch.toml")).unwrap();
        assert!(toml.contains("clmakerenroll1"));
        let rfq = toml.split("[rfq]").nth(1).unwrap_or("");
        assert!(
            !rfq.contains("cngn-usdt-bsc"),
            "must not write the book corridor into [rfq]: {toml}"
        );
    }

    #[tokio::test]
    async fn reconnect_does_not_care_about_zero_max_orders() {
        let h = harness("rfq-enroll-zero-max");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace("buy_max_orders = 40", "buy_max_orders = 0");
        std::fs::write(&path, toml).unwrap();
        let (venue, _server) = mock_venue("tx_live_zero_max", vec!["cngn-usdt-bsc"], false).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.contains("max orders"),
            "Connect must not validate ladder sizing: {body}"
        );
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("buy_max_orders = 0"));
    }

    #[tokio::test]
    async fn the_default_gate_makes_connect_rfq_only() {
        let h = harness("rfq-enroll-default");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!(
                "{toml}\n[experimental]\nrfq_default = \"{}\"\n",
                crate::config::RFQ_DEFAULT_GATE
            ),
        )
        .unwrap();
        let (venue, _server) =
            mock_venue("tx_live_default_gate", vec!["cngn-usdt-bsc"], false).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["bookEnabled"], false);
        assert_eq!(v["settings"]["rfqDefaultUnlocked"], true);
        assert!(v["message"].as_str().unwrap().contains("Swap only"));
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("book_enabled = false"));
    }

    #[tokio::test]
    async fn flagged_rfq_default_reconnect_restores_the_book() {
        let h = harness("rfq-enroll-flagged-default");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml =
            setup::apply_rfq_default_preset(&std::fs::read_to_string(&path).unwrap()).unwrap();
        std::fs::write(&path, toml).unwrap();
        let (venue, _server) =
            mock_venue("tx_live_flagged_default", vec!["cngn-usdt-bsc"], true).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], false);
        assert_eq!(v["settings"]["bookEnabled"], false);
        assert_eq!(v["enrollment"]["flagged"], true);
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(
            toml.contains("book_enabled = false"),
            "flagged reconnect must not turn the book back on: {toml}"
        );
    }

    #[tokio::test]
    async fn connect_when_flagged_registers_and_does_not_go_live() {
        let h = harness("rfq-enroll-flagged");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", vec!["cngn-usdt-bsc"], true).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/enroll", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["settings"]["rfqEnabled"], false);
        assert_eq!(v["settings"]["rfqCorridor"], "");
        assert_eq!(v["enrollment"]["flagged"], true);
        assert!(
            v["message"]
                .as_str()
                .unwrap_or("")
                .contains("flagged this maker"),
            "flagged copy missing: {body}"
        );
    }
}
