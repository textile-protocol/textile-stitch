// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Connect the bot to Textile RFQ by signing MakerEnroll with the funding wallet.
//!
//! The panel signs, POSTs `/v2/maker/enroll`, and writes `[rfq]` plus
//! `rfq-api.key`. The browser never sees the key.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::hex;
use axum::extract::{Path as UrlPath, State};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::settings::{config_path, read_toml, save_and_restart};
use super::{ApiError, AppState};
use crate::config::{rfq_default_flag_in_dir, Config, RFQ_PANEL_GATE};
use crate::eip712::{maker_enroll_digest, maker_enroll_environment};
use crate::panel::provision;
use crate::setup;
use crate::signer::{build_signer, parse_private_key, DynSigner, LocalSigner, SignerConfig};

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct EnrollCorridorPair {
    slug: String,
    collateral_token: String,
    debt_token: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnrollBody {
    /// Override the venue enroll URL. Tests use this; the UI does not.
    #[serde(default)]
    pub venue_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrollResponse {
    maker_id: String,
    maker_slug: String,
    environment: String,
    api_key: String,
    stream_url: String,
    #[serde(default)]
    validation_contract: Option<String>,
    #[serde(default)]
    corridors: Vec<String>,
    /// Token metadata for each seated slug. Custom (non-catalog) bots
    /// match this instead of `identify_corridor`, which is None for them.
    #[serde(default)]
    corridor_pairs: Vec<EnrollCorridorPair>,
    #[serde(default)]
    flagged: bool,
}

/// Derive `https://host/v2/maker/enroll` from a stream URL or an API origin.
pub fn maker_enroll_url(stream_or_origin: &str) -> String {
    let trimmed = stream_or_origin.trim();
    let http = if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    };
    let http = http.trim_end_matches('/');
    if let Some(base) = http.strip_suffix("/v2/maker/stream") {
        return format!("{base}/v2/maker/enroll");
    }
    if http.ends_with("/v2/maker/enroll") {
        return http.to_string();
    }
    format!("{http}/v2/maker/enroll")
}

fn enroll_url_from_config(cfg: &Config, override_url: Option<&str>) -> String {
    if let Some(url) = override_url.map(str::trim).filter(|u| !u.is_empty()) {
        return url.to_string();
    }
    if let Some(rfq) = &cfg.rfq {
        if !rfq.url.trim().is_empty() {
            return maker_enroll_url(&rfq.url);
        }
    }
    maker_enroll_url(&cfg.indexer_url)
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
        _ => {
            let restores = point_remote_signer_env(config_path);
            let result = build_signer(cfg).await;
            restores.undo();
            result.map_err(|e| {
                ApiError::bad_request(format!("could not build the bot signer: {e:#}"))
            })
        }
    }
}

struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvRestore {
    fn undo(self) {
        for (key, prev) in self.0 {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn point_remote_signer_env(config_path: &Path) -> EnvRestore {
    let mut restores = Vec::new();
    let set = |key: &'static str, path: &Path, restores: &mut Vec<_>| {
        restores.push((key, std::env::var_os(key)));
        std::env::set_var(key, path);
    };
    if let Some(path) = provision::find_beside(config_path, "turnkey-api.key") {
        set("TURNKEY_API_PRIVATE_KEY_FILE", &path, &mut restores);
    }
    if let Some(path) = provision::find_beside(config_path, "mpcvault-api.token") {
        set("MPCVAULT_API_TOKEN_FILE", &path, &mut restores);
    }
    EnvRestore(restores)
}

pub async fn enroll(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<EnrollBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = Config::from_toml(&current_toml)
        .map_err(|e| ApiError::bad_request(format!("this config isn't valid: {e:#}")))?;
    let rfq_default = cfg.rfq_default_unlocked() || rfq_default_flag_in_dir(&state.cfg.bots_dir);
    if !cfg.rfq_panel_unlocked() && !rfq_default {
        return Err(ApiError::bad_request(format!(
            "RFQ is locked on this bot. In the raw config set [experimental] rfq_panel = \"{RFQ_PANEL_GATE}\", \
             or drop {gate} in {file} next to the bot folders.",
            gate = crate::config::RFQ_DEFAULT_GATE,
            file = crate::config::PANEL_FLAGS_FILE,
        )));
    }

    let signer = signer_for_bot(&cfg, &path).await?;
    let address = signer.address();
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ApiError::internal(&anyhow::anyhow!("{e}")))?
        .as_millis() as u64;
    let environment = maker_enroll_environment(cfg.chain_id);
    let digest = maker_enroll_digest(environment, address, cfg.chain_id, issued_at);
    let signature = signer
        .sign_digest(digest)
        .await
        .map_err(|e| ApiError::bad_request(format!("signing enroll failed: {e:#}")))?;

    let venue = enroll_url_from_config(&cfg, body.venue_url.as_deref());
    let payload = json!({
        "chainId": cfg.chain_id,
        "signingAddress": format!("{address:?}"),
        "issuedAt": issued_at,
        "signature": hex::encode_prefixed(signature),
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| ApiError::internal(&anyhow::anyhow!("enroll HTTP client: {e}")))?;
    let response = client
        .post(&venue)
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            ApiError::bad_request(format!("could not reach Textile enroll at {venue}: {e}"))
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|e| {
        ApiError::bad_request(format!("Textile enroll returned an unreadable body: {e}"))
    })?;
    if !status.is_success() {
        let message = venue_error_message(&text)
            .unwrap_or_else(|| format!("Textile enroll failed ({status})"));
        return Err(ApiError::bad_request(message));
    }
    let enrolled: EnrollResponse = serde_json::from_str(&text).map_err(|e| {
        ApiError::bad_request(format!("Textile enroll returned an unexpected body: {e}"))
    })?;
    if enrolled.api_key.trim().is_empty() || enrolled.maker_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Textile enroll did not return a maker id and key",
        ));
    }

    let configured = setup::identify_corridor(&current_toml).map(|c| c.id.to_string());
    let pool = cfg
        .pools
        .first()
        .map(|p| (p.collateral.as_str(), p.debt.as_str()));
    let corridor = pick_enroll_corridor(
        enrolled.flagged,
        configured.as_deref(),
        pool,
        &enrolled.corridors,
        &enrolled.corridor_pairs,
    );
    let waiting = corridor.is_none();
    let corridor = corridor.unwrap_or_default();

    let current = setup::read_settings_at(&current_toml, 0).map_err(ApiError::bad_request)?;
    // Empty / flagged: keep the credential, do not start RFQ, and restore
    // the book on an RFQ-default bot so it is not dark.
    let patch = setup::rfq_connect_patch(
        &current,
        enrolled.stream_url.clone(),
        enrolled.maker_id.clone(),
        enrolled.validation_contract.clone().unwrap_or_default(),
        corridor,
        rfq_default,
        !waiting,
    );
    let edited = setup::apply_settings(&current_toml, &patch)
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

    let message = if enrolled.flagged {
        format!(
            "Registered as {} ({}). Textile has flagged this maker — you will not receive private quotes.",
            enrolled.maker_slug, enrolled.environment
        )
    } else if waiting {
        format!(
            "Registered as {} ({}). No RFQ corridor is live on this chain yet.",
            enrolled.maker_slug, enrolled.environment
        )
    } else if rfq_default {
        format!(
            "Connected to Textile as {} ({}). This bot now quotes RFQ only — it will not rest orders on the public book.",
            enrolled.maker_slug, enrolled.environment
        )
    } else {
        format!(
            "Connected to Textile as {} ({}).",
            enrolled.maker_slug, enrolled.environment
        )
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

/// Live only when the venue seated the pair this bot is configured for.
/// A chain-level Dual-run list can include other pairs; binding one of
/// those slugs would disable the book and then reject every quote.
///
/// Token match wins so a custom (non-catalog) pool can still go live.
/// Catalog-id match is the fallback for older enroll payloads that only
/// send slugs.
fn pick_enroll_corridor(
    flagged: bool,
    configured: Option<&str>,
    pool: Option<(&str, &str)>,
    corridors: &[String],
    pairs: &[EnrollCorridorPair],
) -> Option<String> {
    if flagged {
        return None;
    }
    if let Some((collateral, debt)) = pool {
        if let Some(pair) = pairs
            .iter()
            .find(|p| tokens_match(collateral, debt, &p.collateral_token, &p.debt_token))
        {
            return Some(pair.slug.clone());
        }
    }
    configured.and_then(|id| corridors.iter().find(|c| c.as_str() == id).cloned())
}

fn tokens_match(coll_a: &str, debt_a: &str, coll_b: &str, debt_b: &str) -> bool {
    (eq_addr(coll_a, coll_b) && eq_addr(debt_a, debt_b))
        || (eq_addr(coll_a, debt_b) && eq_addr(debt_a, coll_b))
}

fn eq_addr(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn venue_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(slug: &str, collateral: &str, debt: &str) -> EnrollCorridorPair {
        EnrollCorridorPair {
            slug: slug.into(),
            collateral_token: collateral.into(),
            debt_token: debt.into(),
        }
    }

    #[test]
    fn pick_enroll_corridor_requires_the_configured_pair() {
        let empty: &[EnrollCorridorPair] = &[];
        assert_eq!(
            pick_enroll_corridor(
                false,
                Some("cngn-usdt-bsc"),
                None,
                &["cngn-usdt-bsc".into()],
                empty,
            ),
            Some("cngn-usdt-bsc".into())
        );
        assert_eq!(
            pick_enroll_corridor(
                false,
                Some("cngn-usdt-bsc"),
                None,
                &["wars-usdt-bsc".into()],
                empty,
            ),
            None
        );
        assert_eq!(
            pick_enroll_corridor(
                true,
                Some("cngn-usdt-bsc"),
                None,
                &["cngn-usdt-bsc".into()],
                empty,
            ),
            None
        );
        assert_eq!(
            pick_enroll_corridor(false, None, None, &["cngn-usdt-bsc".into()], empty),
            None
        );
    }

    #[test]
    fn pick_enroll_corridor_matches_custom_tokens() {
        let seats = [pair(
            "ops-custom-bsc",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )];
        assert_eq!(
            pick_enroll_corridor(
                false,
                None,
                Some((
                    "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa",
                    "0xBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBb",
                )),
                &["ops-custom-bsc".into()],
                &seats,
            ),
            Some("ops-custom-bsc".into())
        );
        assert_eq!(
            pick_enroll_corridor(
                false,
                None,
                Some((
                    "0xcccccccccccccccccccccccccccccccccccccccc",
                    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )),
                &["ops-custom-bsc".into()],
                &seats,
            ),
            None
        );
        assert_eq!(
            pick_enroll_corridor(
                true,
                None,
                Some((
                    "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )),
                &["ops-custom-bsc".into()],
                &seats,
            ),
            None
        );
    }

    #[test]
    fn enroll_url_from_stream_and_origin() {
        assert_eq!(
            maker_enroll_url("wss://api.textilecredit.com/v2/maker/stream"),
            "https://api.textilecredit.com/v2/maker/enroll"
        );
        assert_eq!(
            maker_enroll_url("ws://localhost:10000/v2/maker/stream"),
            "http://localhost:10000/v2/maker/enroll"
        );
        assert_eq!(
            maker_enroll_url("https://api.textilecredit.com"),
            "https://api.textilecredit.com/v2/maker/enroll"
        );
        assert_eq!(
            maker_enroll_url("https://api.textilecredit.com/v2/maker/enroll"),
            "https://api.textilecredit.com/v2/maker/enroll"
        );
    }

    use super::super::testkit::{harness, Harness, TEST_KEY};
    use crate::panel::docker::fake::{container, dir_layout_mounts};
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

    fn unlock_rfq_panel(h: &Harness, name: &str) {
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("{toml}\n[experimental]\nrfq_panel = \"{RFQ_PANEL_GATE}\"\n"),
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
    async fn connect_refuses_when_the_panel_gate_is_locked() {
        let h = harness("rfq-enroll-locked");
        seed(&h, "bot-a");
        let (status, body) = h.post_json("/api/bots/bot-a/rfq/enroll", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains(RFQ_PANEL_GATE),
            "the error must name the raw-config token: {body}"
        );
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
            !toml.contains("book_enabled = false"),
            "without the default gate, Connect must leave the public ladder on"
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
        assert_eq!(v["settings"]["bookEnabled"], true);
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
                .contains("No RFQ corridor is live on this chain"),
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
        assert!(v["message"].as_str().unwrap().contains("RFQ only"));
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
        assert_eq!(v["settings"]["bookEnabled"], true);
        assert_eq!(v["enrollment"]["flagged"], true);
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(
            !toml.contains("book_enabled = false"),
            "flagged RFQ-default reconnect must restore the book: {toml}"
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
