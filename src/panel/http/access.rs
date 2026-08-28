// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Request Textile to seat this maker, and poll until they do.
//!
//! Connect only registers a key. This is the last setup step: the panel
//! posts contact details to the venue, which emails ops. Check status
//! applies corridors once approved, without rotating the key.

use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::settings::{config_path, read_toml, save_and_restart};
use super::{ApiError, AppState};
use crate::config::{rfq_default_flag_in_dir, Config};
use crate::enroll::{
    apply_enrollment, maker_access_request_url, maker_access_status_url, venue_error_message,
    venue_origin_from_config, EnrollCorridorPair, EnrollOutcome, EnrollResponse,
};
use crate::setup;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccessBody {
    #[serde(default)]
    pub venue_url: Option<String>,
    #[serde(default)]
    pub contact_email: Option<String>,
    #[serde(default)]
    pub contact_whatsapp: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessStatusResponse {
    access_status: String,
    #[serde(default)]
    flagged: bool,
    #[serde(default)]
    maker_id: String,
    maker_slug: String,
    environment: String,
    stream_url: String,
    #[serde(default)]
    validation_contract: Option<String>,
    #[serde(default)]
    corridors: Vec<String>,
    #[serde(default)]
    corridor_pairs: Vec<EnrollCorridorPair>,
}

/// Body for POST /v2/maker/access-request. The form leaves most of these
/// blank, and blank has to mean absent: the venue validates them as optional
/// strings, so a `null` reads as the wrong type and 400s the whole request.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccessRequestPayload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    contact_email: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contact_whatsapp: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corridor: Option<&'a str>,
}

/// Trimmed value, or None when it is missing or blank.
fn filled(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Email is the channel Textile answers a review on, so it is the required
/// one. WhatsApp is a bonus number for them to ping.
fn require_email(email: Option<&str>) -> Result<(), ApiError> {
    if filled(email).is_some() {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "add an email address so Textile can reply to your access request — WhatsApp is optional",
    ))
}

fn venue_client() -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| ApiError::internal(&anyhow::anyhow!("venue HTTP client: {e}")))
}

async fn read_bot_key(dir: &std::path::Path) -> Result<String, ApiError> {
    setup::read_rfq_api_key(dir).map_err(|_| {
        ApiError::bad_request("Connect to Textile first — this bot has no RFQ API key yet")
    })
}

pub async fn request_access(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<AccessBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = Config::from_toml(&current_toml)
        .map_err(|e| ApiError::bad_request(format!("this config isn't valid: {e:#}")))?;
    require_email(body.contact_email.as_deref())?;

    let dir = path.parent().ok_or_else(|| {
        ApiError::internal(&anyhow::anyhow!(
            "{}'s config has no parent directory",
            bot.name
        ))
    })?;
    let api_key = read_bot_key(dir).await?;
    let origin = venue_origin_from_config(&cfg, body.venue_url.as_deref());
    let venue = maker_access_request_url(&origin);
    // The identity the config carries, falling back to the catalog. Recomputing
    // it from the catalog alone would name no corridor for a market listed after
    // this Stitch release — leaving Textile to guess which one to seat.
    let corridor = setup::config_identity(&current_toml).map(|c| c.id);

    let response = venue_client()?
        .post(&venue)
        .bearer_auth(&api_key)
        .json(&AccessRequestPayload {
            contact_email: filled(body.contact_email.as_deref()),
            contact_whatsapp: filled(body.contact_whatsapp.as_deref()),
            note: filled(body.note.as_deref()),
            corridor: filled(corridor.as_deref()),
        })
        .send()
        .await
        .map_err(|e| {
            ApiError::bad_request(format!(
                "could not reach Textile access request at {venue}: {e}"
            ))
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|e| {
        ApiError::bad_request(format!(
            "Textile access request returned an unreadable body: {e}"
        ))
    })?;
    if !status.is_success() {
        let message = venue_error_message(&text)
            .unwrap_or_else(|| format!("Textile access request failed ({status})"));
        return Err(ApiError::bad_request(message));
    }

    Ok(Json(json!({
        "message": "Request sent. Textile will review it and email you if they need anything.",
        "accessStatus": "PENDING",
    }))
    .into_response())
}

pub async fn access_status(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<AccessBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = Config::from_toml(&current_toml)
        .map_err(|e| ApiError::bad_request(format!("this config isn't valid: {e:#}")))?;
    let rfq_default = cfg.rfq_default_unlocked() || rfq_default_flag_in_dir(&state.cfg.bots_dir);

    let dir = path.parent().ok_or_else(|| {
        ApiError::internal(&anyhow::anyhow!(
            "{}'s config has no parent directory",
            bot.name
        ))
    })?;
    let api_key = read_bot_key(dir).await?;
    let origin = venue_origin_from_config(&cfg, body.venue_url.as_deref());
    let venue = maker_access_status_url(&origin);

    let response = venue_client()?
        .get(&venue)
        .bearer_auth(&api_key)
        .send()
        .await
        .map_err(|e| {
            ApiError::bad_request(format!(
                "could not reach Textile access status at {venue}: {e}"
            ))
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|e| {
        ApiError::bad_request(format!(
            "Textile access status returned an unreadable body: {e}"
        ))
    })?;
    if !status.is_success() {
        let message = venue_error_message(&text)
            .unwrap_or_else(|| format!("Textile access status failed ({status})"));
        return Err(ApiError::bad_request(message));
    }
    let reported: AccessStatusResponse = serde_json::from_str(&text).map_err(|e| {
        ApiError::bad_request(format!(
            "Textile access status returned an unexpected body: {e}"
        ))
    })?;

    if reported.access_status != "APPROVED" || reported.flagged {
        let message = if reported.flagged || reported.access_status == "REJECTED" {
            format!(
                "Textile rejected {}. You will not receive private quotes.",
                reported.maker_slug
            )
        } else if reported.access_status == "PENDING" {
            "Textile still has your request. Nothing to do until they approve it.".to_string()
        } else {
            "No access request yet. Send one so Textile can review this maker.".to_string()
        };
        return Ok(Json(json!({
            "message": message,
            "accessStatus": reported.access_status,
            "enrollment": {
                "makerSlug": reported.maker_slug,
                "environment": reported.environment,
                "corridors": reported.corridors,
                "flagged": reported.flagged,
            }
        }))
        .into_response());
    }

    // Seat through the same code Connect uses. Check status is the second door
    // onto one decision — which pools get a slug, and whether that's enough to
    // turn RFQ on and the ladder off — and a hand-rolled copy here drifted:
    // it seated only the first pool and called any slug live, so it could take
    // the ladder down for a pool that can't quote, or sit at Waiting while a
    // later pool was the seated one.
    //
    // Approval doesn't rotate the key, and the venue may omit fields it isn't
    // changing, so blanks fall back to what's already in the config rather than
    // erasing it.
    let current = setup::read_settings_at(&current_toml, 0).map_err(ApiError::bad_request)?;
    let enrolled = EnrollResponse {
        maker_id: if reported.maker_id.trim().is_empty() {
            current.rfq_maker_id.clone()
        } else {
            reported.maker_id.clone()
        },
        maker_slug: reported.maker_slug.clone(),
        environment: reported.environment.clone(),
        api_key: String::new(),
        stream_url: reported.stream_url.clone(),
        validation_contract: Some(
            reported
                .validation_contract
                .clone()
                .unwrap_or_else(|| current.rfq_validation_contract.clone()),
        ),
        corridors: reported.corridors.clone(),
        corridor_pairs: reported.corridor_pairs.clone(),
        flagged: reported.flagged,
    };
    let (edited, outcome) = apply_enrollment(&current_toml, &cfg, &enrolled, rfq_default)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    if outcome != EnrollOutcome::Live {
        return Ok(Json(json!({
            "message": format!(
                "Textile approved {} but this bot cannot quote on it yet: no pool is both seated on an RFQ corridor and able to build a book with funds behind it.",
                reported.maker_slug
            ),
            "accessStatus": "APPROVED",
            "enrollment": {
                "makerSlug": reported.maker_slug,
                "environment": reported.environment,
                "corridors": reported.corridors,
                "flagged": reported.flagged,
            }
        }))
        .into_response());
    }

    save_and_restart(
        &state,
        &bot,
        &path,
        &edited,
        0,
        Some(json!({
            "message": format!(
                "Textile approved {}. This bot is live on RFQ.",
                reported.maker_slug
            ),
            "accessStatus": "APPROVED",
            "enrollment": {
                "makerSlug": reported.maker_slug,
                "environment": reported.environment,
                "corridors": reported.corridors,
                "flagged": reported.flagged,
            }
        })),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{harness, Harness, TEST_KEY};
    use super::*;
    use crate::config::RFQ_PANEL_GATE;
    use crate::enroll::maker_enroll_url;
    use crate::panel::docker::fake::{container, dir_layout_mounts};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::LABEL_BOT;
    use crate::setup;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::Router;
    use serde_json::{json, Value};

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

    async fn mock_access_venue(
        expect_key: &'static str,
        status: &'static str,
        flagged: bool,
        corridors: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/v2/maker/access-request",
                post(
                    move |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(auth, format!("Bearer {expect_key}"));
                        assert!(
                            body["contactEmail"].as_str().is_some()
                                || body["contactWhatsapp"].as_str().is_some()
                        );
                        // The venue validates these as optional strings, so a
                        // blank field must be absent rather than null.
                        assert!(
                            !body
                                .as_object()
                                .expect("object body")
                                .values()
                                .any(Value::is_null),
                            "sent a null field: {body}"
                        );
                        Json(json!({ "accessStatus": "PENDING", "requestId": "clreq1" }))
                    },
                ),
            )
            .route(
                "/v2/maker/access-status",
                get(move |headers: axum::http::HeaderMap| {
                    let corridors = corridors.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(auth, format!("Bearer {expect_key}"));
                        Json(json!({
                            "accessStatus": status,
                            "flagged": flagged,
                            "makerId": "clmakerenroll1",
                            "makerSlug": "stitch-56-f39fd6e5",
                            "environment": "LIVE",
                            "streamUrl": "wss://api.textilecredit.com/v2/maker/stream",
                            "validationContract": "0xBCA5E344077AaC751A1C548a45F28215bB7ec165",
                            "corridors": corridors,
                            "corridorPairs": corridors.iter().map(|slug| json!({
                                "slug": slug,
                                "chainId": 56,
                                "collateralToken": "0x4444444444444444444444444444444444444444",
                                "debtToken": "0x3333333333333333333333333333333333333333",
                            })).collect::<Vec<_>>(),
                        }))
                    }
                }),
            );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), handle)
    }

    /// A venue that records the corridor each access request named, so a test
    /// can assert Textile is told which market to seat.
    async fn recording_access_venue(
        expect_key: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Option<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorder = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/v2/maker/access-request",
            post(
                move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let recorder = recorder.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(auth, format!("Bearer {expect_key}"));
                        *recorder.lock().unwrap() = Some(body["corridor"].clone());
                        Json(json!({ "accessStatus": "PENDING", "requestId": "clreq1" }))
                    }
                },
            ),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), seen, handle)
    }

    /// The shipped cNGN/USDT BSC preset, restamped as a corridor Textile listed
    /// after this release: same pair, but named by a registry row id the catalog
    /// has never seen.
    fn seed_registry_corridor(h: &Harness, name: &str) {
        seed(h, name);
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap();
        let stamp = concat!(
            "collateral_decimals = 6\n",
            "corridor_id = \"cmregistryrow\"\n",
            "corridor_name = \"cNGN / USDT\"\n",
            "corridor_network = \"BNB Smart Chain\"",
        );
        std::fs::write(&path, toml.replace("collateral_decimals = 6", stamp)).unwrap();
    }

    #[tokio::test]
    async fn an_access_request_names_the_corridor_the_config_carries() {
        // Without the stamp the panel would have to recompute the corridor from
        // the catalog compiled into it, which cannot name a market listed after
        // this release — so Textile would get a request with no corridor on it
        // and no way to know which market to seat.
        let h = harness("rfq-access-corridor");
        seed_registry_corridor(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, seen, _server) = recording_access_venue("tx_live_enroll_secret").await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rfq/access-request",
                json!({ "venueUrl": venue, "contactEmail": "desk@example.com" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(json!("cmregistryrow")),
            "the registry id, not the catalog slug it happens to share a pair with"
        );
    }

    #[tokio::test]
    async fn an_unstamped_bot_still_names_its_catalog_corridor() {
        let h = harness("rfq-access-preset-corridor");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, seen, _server) = recording_access_venue("tx_live_enroll_secret").await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rfq/access-request",
                json!({ "venueUrl": venue, "contactEmail": "desk@example.com" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(json!("cngn-usdt-bsc")),
            "the catalog fallback still works for a shipped preset"
        );
    }

    #[test]
    fn venue_urls_share_an_origin() {
        assert_eq!(
            maker_access_request_url("wss://api.textilecredit.com/v2/maker/stream"),
            "https://api.textilecredit.com/v2/maker/access-request"
        );
        assert_eq!(
            maker_access_status_url("https://api.textilecredit.com/v2/maker/enroll"),
            "https://api.textilecredit.com/v2/maker/access-status"
        );
        assert_eq!(
            maker_enroll_url("http://127.0.0.1:9/v2/maker/access-request"),
            "http://127.0.0.1:9/v2/maker/enroll"
        );
    }

    #[tokio::test]
    async fn request_access_needs_an_email_and_whatsapp_stays_optional() {
        let h = harness("rfq-access-contact");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        for payload in [json!({}), json!({ "contactWhatsapp": "+15551234567" })] {
            let (status, body) = h
                .post_json("/api/bots/bot-a/rfq/access-request", payload)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("email"), "{body}");
        }
    }

    #[tokio::test]
    async fn request_access_posts_the_key_and_does_not_echo_it() {
        let h = harness("rfq-access-request");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, _server) =
            mock_access_venue("tx_live_enroll_secret", "PENDING", false, vec![]).await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rfq/access-request",
                json!({
                    "venueUrl": venue,
                    "contactEmail": "desk@example.com",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.contains("tx_live_enroll_secret"), "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["accessStatus"], "PENDING");
    }

    #[tokio::test]
    async fn check_status_goes_live_when_approved() {
        let h = harness("rfq-access-approved");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let toml_path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&toml_path).unwrap();
        std::fs::write(
            &toml_path,
            format!(
                "{toml}\n[rfq]\nenabled = false\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\nmaker_id = \"clmakerenroll1\"\nvalidation_contract = \"0xBCA5E344077AaC751A1C548a45F28215bB7ec165\"\n"
            ),
        )
        .unwrap();
        let (venue, _server) = mock_access_venue(
            "tx_live_enroll_secret",
            "APPROVED",
            false,
            vec!["cngn-usdt-bsc"],
        )
        .await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rfq/access-status",
                json!({ "venueUrl": venue }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["accessStatus"], "APPROVED");
        assert_eq!(v["settings"]["rfqEnabled"], true);
        assert_eq!(v["settings"]["rfqCorridor"], "cngn-usdt-bsc");
        assert!(!body.contains("tx_live_enroll_secret"));
    }

    #[tokio::test]
    async fn check_status_does_not_go_live_on_a_pool_that_cannot_quote() {
        // Check status is the second door onto the decision Connect makes, so
        // it seats through `apply_enrollment` rather than its own copy. An
        // approval on a pool with no capacity is not a reason to enable RFQ and
        // take the ladder down: the bot would then quote on neither surface.
        let h = harness("rfq-access-no-capacity");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let toml_path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&toml_path)
            .unwrap()
            // No capacity on either side...
            .replace(
                "buy_total_liquidity_debt = \"max\"",
                "buy_total_liquidity_debt = \"0\"",
            )
            .replace(
                "sell_total_liquidity_collateral = \"max\"",
                "sell_total_liquidity_collateral = \"0\"",
            )
            // ...and the ladder on, so "left alone" is observable.
            .replace("book_enabled = false", "book_enabled = true");
        std::fs::write(
            &toml_path,
            format!(
                "{toml}\n[rfq]\nenabled = false\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\nmaker_id = \"clmakerenroll1\"\nvalidation_contract = \"0xBCA5E344077AaC751A1C548a45F28215bB7ec165\"\n"
            ),
        )
        .unwrap();
        let (venue, _server) = mock_access_venue(
            "tx_live_enroll_secret",
            "APPROVED",
            false,
            vec!["cngn-usdt-bsc"],
        )
        .await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rfq/access-status",
                json!({ "venueUrl": venue }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["accessStatus"], "APPROVED");
        assert!(
            v["settings"].is_null(),
            "nothing is written for a maker that cannot quote yet: {body}"
        );

        let after = Config::from_toml(&std::fs::read_to_string(&toml_path).unwrap()).unwrap();
        assert!(!after.rfq_active(), "RFQ stays off");
        assert!(after.book_enabled, "and the ladder is left alone");
    }
}
