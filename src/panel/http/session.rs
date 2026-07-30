// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Signing in and out.
//!
//! These three routes are the only ones reachable without a credential, because
//! they're how a credential is obtained. `GET /api/session` also tells the SPA
//! which methods are configured, so it renders a password form only when there is
//! a password to check.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{identify, ApiError, AppState};
use crate::panel::auth::{
    self, clear_cookie_header, session_cookie_header, AuthError, Identity, SESSION_COOKIE,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBody {
    /// Whether this request is authenticated.
    authenticated: bool,
    /// Who the panel thinks you are: a tailnet login, or `password`.
    identity: Option<String>,
    /// True when a password login is possible, so the UI shows the form.
    password_login: bool,
    /// True when the panel accepts a proxy identity header, so the UI can explain
    /// why it's waiting on `tailscale serve` instead of showing a blank page.
    tailnet_login: bool,
}

/// Who am I? Never fails: an unauthenticated caller gets `authenticated: false`
/// plus which login methods exist.
pub async fn current(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let identity = identify(&state, &req).ok();
    Json(SessionBody {
        authenticated: identity.is_some(),
        identity: identity.as_ref().map(|i| i.label().to_string()),
        password_login: state.cfg.auth.password_hash().is_some(),
        tailnet_login: state.cfg.trust_identity_header
            && !state.cfg.auth.allowed_users().is_empty(),
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct LoginRequest {
    password: String,
}

/// Exchange a password for a session cookie.
pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let Some(hash) = state.cfg.auth.password_hash() else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "this panel doesn't use password login. Reach it through `tailscale serve` so it \
             sees your tailnet identity, or set STITCH_PANEL_PASSWORD_HASH and restart it.",
        ));
    };

    // Bounded: this route is reachable without a credential and each check is
    // deliberately expensive, so the cap is what keeps a guessing attacker from
    // taking the Docker controls down with it.
    let verified =
        auth::verify_password_bounded(&state.login_permits, hash, &body.password).await?;
    match verified {
        Some(true) => {}
        Some(false) => {
            // No detail, no timing games, no hint about which part was wrong.
            tracing::warn!("a password login was refused");
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                AuthError::Rejected.message(),
            ));
        }
        None => {
            tracing::warn!("a password login was shed: too many verifications in flight");
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many sign-in attempts are being checked right now. Try again in a moment — \
                 the panel caps these on purpose, because each one is deliberately expensive.",
            ));
        }
    }

    let token = state.sessions.create();
    tracing::info!("a password login succeeded");
    Ok((
        [(header::SET_COOKIE, session_cookie_header(&token))],
        Json(SessionBody {
            authenticated: true,
            identity: Some(Identity::Password.label().to_string()),
            password_login: true,
            tailnet_login: false,
        }),
    )
        .into_response())
}

/// Drop the session this request carries, if any. Idempotent, so the UI can call
/// it without first checking whether it is signed in.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| crate::panel::auth::cookie_value(c, SESSION_COOKIE));
    if let Some(token) = token {
        state.sessions.revoke(&token);
    }
    (
        [(header::SET_COOKIE, clear_cookie_header())],
        Json(serde_json::json!({ "authenticated": false })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{harness, Harness};
    use crate::panel::auth::hash_password;
    use crate::panel::config::AuthMode;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// A harness whose panel accepts the given password.
    fn with_password(tag: &str, password: &str) -> Harness {
        let mut h = harness(tag);
        let mut cfg = (*h.state.cfg).clone();
        cfg.auth = AuthMode::Password {
            hash: hash_password(password).unwrap(),
        };
        h.state.cfg = Arc::new(cfg);
        h.app = super::super::router(h.state.clone());
        h
    }

    async fn post(h: &Harness, path: &str, body: &str) -> (StatusCode, String, Option<String>) {
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string(), cookie)
    }

    #[tokio::test]
    async fn the_right_password_mints_a_session_cookie() {
        let h = with_password("login-ok", "let me in");
        let (status, body, cookie) = post(&h, "/api/login", r#"{"password":"let me in"}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cookie = cookie.expect("a session cookie must be set");
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert_eq!(
            h.state.sessions.len(),
            2,
            "the harness session plus this one"
        );
    }

    #[tokio::test]
    async fn login_sheds_load_rather_than_exhausting_the_runtime() {
        // `/api/login` answers before the caller has a credential, and each check is
        // 19 MiB of memory-hard Argon2 on a blocking thread. Unbounded, a handful of
        // parallel POSTs takes the Docker controls down with it — so past the cap the
        // panel refuses instead of queueing.
        let h = with_password("login-shed", "let me in");
        // Hold every permit, as saturating verifications would.
        let held: Vec<_> = (0..crate::panel::auth::MAX_CONCURRENT_VERIFICATIONS)
            .map(|_| {
                h.state
                    .login_permits
                    .try_acquire()
                    .expect("a fresh panel has all its permits")
            })
            .collect();

        let (status, body, cookie) = post(&h, "/api/login", r#"{"password":"let me in"}"#).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(cookie.is_none(), "no session may be minted");
        assert!(body.contains("Try again"), "{body}");

        // And the right password still works once a permit frees up.
        drop(held);
        let (status, body, cookie) = post(&h, "/api/login", r#"{"password":"let me in"}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(cookie.is_some());
    }

    #[tokio::test]
    async fn the_wrong_password_is_rejected_without_a_cookie() {
        let h = with_password("login-bad", "let me in");
        let (status, body, cookie) = post(&h, "/api/login", r#"{"password":"nope"}"#).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(cookie.is_none(), "no cookie on a failed login");
        // The message must not hint at what was wrong.
        assert!(!body.contains("password"), "{body}");
    }

    #[tokio::test]
    async fn a_login_attempt_on_a_tailnet_only_panel_says_so() {
        let mut h = harness("login-tailnet");
        let mut cfg = (*h.state.cfg).clone();
        cfg.auth = AuthMode::Tailnet {
            allowed_users: vec!["alice@example.com".into()],
        };
        h.state.cfg = Arc::new(cfg);
        h.app = super::super::router(h.state.clone());
        let (status, body, _) = post(&h, "/api/login", r#"{"password":"x"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("tailscale serve"), "{body}");
    }

    #[tokio::test]
    async fn logout_revokes_the_session_it_carries() {
        let h = harness("logout");
        assert_eq!(h.state.sessions.len(), 1);
        let (status, _) = h.post_json("/api/logout", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(h.state.sessions.is_empty());
        // And the API is closed again.
        let (status, _) = h.get("/api/bots").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_session_route_reports_which_logins_exist() {
        let h = with_password("session-shape", "pw");
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let v = Harness::parse(&String::from_utf8_lossy(&bytes));
        assert_eq!(v["authenticated"], false);
        assert_eq!(v["passwordLogin"], true);
        assert_eq!(v["tailnetLogin"], false);
    }

    #[tokio::test]
    async fn the_session_route_names_an_authenticated_caller() {
        let h = harness("session-me");
        let (status, body) = h.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        let v = Harness::parse(&body);
        assert_eq!(v["authenticated"], true);
        assert_eq!(v["identity"], "password");
    }
}
