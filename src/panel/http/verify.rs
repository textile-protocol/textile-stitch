// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Confirm the operator's email address, and pick up the seats it earns.
//!
//! Connect registers the bot and mints a key. The one thing left is proving
//! the operator owns the address they gave: the panel posts it to the venue,
//! the venue mails a confirm link, and clicking it seats this maker on every
//! RFQ corridor on every chain — now and as more are listed. Nobody at Textile
//! approves anything. Check status applies the seats without rotating the key.

use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::settings::{config_path, read_toml, save_and_restart};
use super::{ApiError, AppState};
use crate::config::{rfq_default_flag_in_dir, Config};
use crate::setup;
use crate::venue::enroll::{
    apply_enrollment, maker_status_url, maker_verify_email_url, venue_error_message,
    venue_origin_from_config, EnrollCorridorPair, EnrollOutcome, EnrollResponse,
};

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VerifyBody {
    #[serde(default)]
    pub venue_url: Option<String>,
    #[serde(default)]
    pub contact_email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MakerStatusResponse {
    /// The whole gate. False means the confirm link is still unclicked.
    #[serde(default)]
    email_verified: bool,
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
    #[serde(default)]
    contact_email: Option<String>,
}

/// What the venue says back to a submitted address.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyEmailResponse {
    #[serde(default)]
    contact_email: Option<String>,
    #[serde(default)]
    email_verified: bool,
}

/// Body for POST /v2/maker/verify-email.
///
/// `bot_name` is the name the operator gave this bot. The venue's maker slug is
/// generated and means nothing to them, so their own name is what Textile's
/// emails are headed with.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifyEmailPayload<'a> {
    contact_email: &'a str,
    bot_name: &'a str,
}

/// Trimmed value, or None when it is missing or blank.
fn filled(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
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

/// Send (or resend) the confirmation link for this bot's operator address.
pub async fn verify_email(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<VerifyBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = Config::from_toml(&current_toml)
        .map_err(|e| ApiError::bad_request(format!("this config isn't valid: {e:#}")))?;
    let contact_email = filled(body.contact_email.as_deref()).ok_or_else(|| {
        ApiError::bad_request(
            "add an email address you own — confirming it is what puts this bot on the venue",
        )
    })?;

    let dir = path.parent().ok_or_else(|| {
        ApiError::internal(&anyhow::anyhow!(
            "{}'s config has no parent directory",
            bot.name
        ))
    })?;
    let api_key = read_bot_key(dir).await?;
    let origin = venue_origin_from_config(&cfg, body.venue_url.as_deref());
    let venue = maker_verify_email_url(&origin);

    let response = venue_client()?
        .post(&venue)
        .bearer_auth(&api_key)
        .json(&VerifyEmailPayload {
            contact_email,
            bot_name: &bot.name,
        })
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("could not reach Textile at {venue}: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ApiError::bad_request(format!("Textile returned an unreadable body: {e}")))?;
    if !status.is_success() {
        let message = venue_error_message(&text)
            .unwrap_or_else(|| format!("Textile could not send the confirmation email ({status})"));
        return Err(ApiError::bad_request(message));
    }
    let sent: VerifyEmailResponse = serde_json::from_str(&text).unwrap_or(VerifyEmailResponse {
        contact_email: None,
        email_verified: false,
    });
    let address = sent
        .contact_email
        .as_deref()
        .unwrap_or(contact_email)
        .to_string();

    let message = if sent.email_verified {
        format!(
            "{address} is already confirmed. This bot goes live as soon as it picks the seats up."
        )
    } else {
        format!(
            "Check {address} and click the link we sent. That is the only step left — the bot goes live the moment you do."
        )
    };
    Ok(Json(json!({
        "message": message,
        "contactEmail": address,
        "emailVerified": sent.email_verified,
    }))
    .into_response())
}

/// Ask the venue where this maker stands, and go live if the address is
/// confirmed and the seats cover a pool this bot can quote.
pub async fn maker_status(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<VerifyBody>,
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
    let venue = maker_status_url(&origin);

    let response = venue_client()?
        .get(&venue)
        .bearer_auth(&api_key)
        .send()
        .await
        .map_err(|e| {
            ApiError::bad_request(format!("could not reach Textile status at {venue}: {e}"))
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|e| {
        ApiError::bad_request(format!("Textile status returned an unreadable body: {e}"))
    })?;
    if !status.is_success() {
        let message = venue_error_message(&text)
            .unwrap_or_else(|| format!("Textile status failed ({status})"));
        return Err(ApiError::bad_request(message));
    }
    let reported: MakerStatusResponse = serde_json::from_str(&text).map_err(|e| {
        ApiError::bad_request(format!("Textile status returned an unexpected body: {e}"))
    })?;

    if !reported.email_verified || reported.flagged {
        let message = if reported.flagged {
            format!(
                "Textile blocked {}. You will not receive private quotes.",
                reported.maker_slug
            )
        } else {
            match reported.contact_email.as_deref() {
                Some(email) => format!(
                    "Confirm your email: we sent a link to {email}. The bot goes live the moment you click it."
                ),
                None => "Give Textile an email address and confirm it — that is what puts this bot on the venue.".to_string(),
            }
        };
        return Ok(Json(json!({
            "message": message,
            "emailVerified": reported.email_verified,
            "contactEmail": reported.contact_email,
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
    // Confirming does not rotate the key, and the venue may omit fields it
    // isn't changing, so blanks fall back to what's already in the config
    // rather than erasing it.
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
                "{} is confirmed on Textile but this bot cannot quote yet: no pool is both seated on an RFQ corridor and able to build a book with funds behind it.",
                reported.maker_slug
            ),
            "emailVerified": true,
            "contactEmail": reported.contact_email,
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
                "{} is confirmed on Textile. This bot is live on RFQ.",
                reported.maker_slug
            ),
            "emailVerified": true,
            "contactEmail": reported.contact_email,
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
    use crate::panel::docker::fake::{container, dir_layout_mounts};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::LABEL_BOT;
    use crate::setup;
    use crate::venue::enroll::maker_enroll_url;
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

    async fn mock_venue(
        expect_key: &'static str,
        email_verified: bool,
        flagged: bool,
        corridors: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/v2/maker/verify-email",
                post(
                    move |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(auth, format!("Bearer {expect_key}"));
                        // The venue validates this as a required string, so a
                        // blank field must never be sent as null.
                        assert!(
                            body["contactEmail"].as_str().is_some(),
                            "sent no address: {body}"
                        );
                        Json(json!({
                            "contactEmail": body["contactEmail"],
                            "emailVerified": false,
                            "sent": true,
                        }))
                    },
                ),
            )
            .route(
                "/v2/maker/status",
                get(move |headers: axum::http::HeaderMap| {
                    let corridors = corridors.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(auth, format!("Bearer {expect_key}"));
                        Json(json!({
                            "emailVerified": email_verified,
                            "contactEmail": "desk@acme-fx.com",
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

    #[test]
    fn venue_urls_share_an_origin() {
        assert_eq!(
            maker_verify_email_url("wss://api.textilecredit.com/v2/maker/stream"),
            "https://api.textilecredit.com/v2/maker/verify-email"
        );
        assert_eq!(
            maker_status_url("https://api.textilecredit.com/v2/maker/enroll"),
            "https://api.textilecredit.com/v2/maker/status"
        );
        assert_eq!(
            maker_enroll_url("http://127.0.0.1:9/v2/maker/verify-email"),
            "http://127.0.0.1:9/v2/maker/enroll"
        );
    }

    #[tokio::test]
    async fn an_email_address_is_required() {
        let h = harness("rfq-verify-contact");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        for payload in [json!({}), json!({ "contactEmail": "   " })] {
            let (status, body) = h
                .post_json("/api/bots/bot-a/rfq/verify-email", payload)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("email"), "{body}");
        }
    }

    #[tokio::test]
    async fn submitting_an_address_posts_the_key_and_does_not_echo_it() {
        let h = harness("rfq-verify-request");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, _server) = mock_venue("tx_live_enroll_secret", false, false, vec![]).await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rfq/verify-email",
                json!({ "venueUrl": venue, "contactEmail": " Desk@acme-fx.com " }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.contains("tx_live_enroll_secret"), "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["emailVerified"], false);
        assert!(
            v["message"].as_str().unwrap().contains("Desk@acme-fx.com"),
            "tells them where the link went: {body}"
        );
    }

    #[tokio::test]
    async fn status_waits_while_the_address_is_unconfirmed() {
        let h = harness("rfq-verify-unconfirmed");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", false, false, vec!["cngn-usdt-bsc"]).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/status", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["emailVerified"], false);
        assert!(v["settings"].is_null(), "nothing is written yet: {body}");
        assert!(
            v["message"].as_str().unwrap().contains("desk@acme-fx.com"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn status_goes_live_once_the_address_is_confirmed() {
        let h = harness("rfq-verify-confirmed");
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
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", true, false, vec!["cngn-usdt-bsc"]).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/status", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["emailVerified"], true);
        assert_eq!(v["settings"]["rfqEnabled"], true);
        assert_eq!(v["settings"]["rfqCorridor"], "cngn-usdt-bsc");
        assert!(!body.contains("tx_live_enroll_secret"));
    }

    #[tokio::test]
    async fn status_does_not_go_live_on_a_pool_that_cannot_quote() {
        // Check status is the second door onto the decision Connect makes, so
        // it seats through `apply_enrollment` rather than its own copy. A seat
        // on a pool with no capacity is not a reason to enable RFQ and take the
        // ladder down: the bot would then quote on neither surface.
        let h = harness("rfq-verify-no-capacity");
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
        let (venue, _server) =
            mock_venue("tx_live_enroll_secret", true, false, vec!["cngn-usdt-bsc"]).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/status", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["emailVerified"], true);
        assert!(
            v["settings"].is_null(),
            "nothing is written for a maker that cannot quote yet: {body}"
        );

        let after = Config::from_toml(&std::fs::read_to_string(&toml_path).unwrap()).unwrap();
        assert!(!after.rfq_active(), "RFQ stays off");
        assert!(after.book_enabled, "and the ladder is left alone");
    }

    #[tokio::test]
    async fn a_blocked_maker_is_told_so_and_stays_off() {
        let h = harness("rfq-verify-blocked");
        seed(&h, "bot-a");
        unlock_rfq_panel(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, _server) = mock_venue("tx_live_enroll_secret", true, true, vec![]).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/rfq/status", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["enrollment"]["flagged"], true);
        assert!(v["message"].as_str().unwrap().contains("blocked"), "{body}");
        assert!(v["settings"].is_null(), "{body}");
    }
}
