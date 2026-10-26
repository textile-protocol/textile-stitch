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
    /// Legacy proxy login is still sufficient: show the persistent migration banner.
    password_setup_required: bool,
    /// How the panel supervises bots: `"docker"` or `"process"` (desktop).
    /// The login screen uses this to show a Docker-socket warning only when it
    /// applies.
    runtime: &'static str,
    /// The panel binary's own version, so the UI can show what it's running.
    version: &'static str,
    /// The full path to the local config data — the directory holding the
    /// per-bot config folders as it exists on the host, so the UI can show an
    /// operator exactly where their bots live on disk.
    config_dir: String,
    /// Fleet RFQ-default flag (`{bots_dir}/panel.toml`). New bots start RFQ-only
    /// and the add-bot copy mentions Connect.
    rfq_default: bool,
}

fn session_body(state: &AppState, identity: Option<&Identity>) -> SessionBody {
    let cfg = state.auth_config();
    let legacy = cfg.trust_identity_header && !cfg.auth.allowed_users().is_empty();
    SessionBody {
        authenticated: identity.is_some(),
        identity: identity.map(|i| i.label().to_string()),
        password_login: cfg.auth.password_hash().is_some(),
        tailnet_login: legacy,
        password_setup_required: legacy,
        runtime: state.cfg.runtime.as_str(),
        version: env!("CARGO_PKG_VERSION"),
        config_dir: state.cfg.host_bots_dir.display().to_string(),
        rfq_default: crate::config::rfq_default_flag_in_dir(&state.cfg.bots_dir),
    }
}

/// Who am I? Never fails: an unauthenticated caller gets `authenticated: false`
/// plus which login methods exist.
pub async fn current(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let identity = identify(&state, &req).ok();
    Json(session_body(&state, identity.as_ref())).into_response()
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
    let cfg = state.auth_config();
    let Some(hash) = cfg.auth.password_hash() else {
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

    // Coordinate with migration: an old fallback-password check already in
    // flight must not mint a valid session after identity login has been removed.
    let migrated = state
        .migrated_password
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let current_hash = migrated.as_deref().or(state.cfg.auth.password_hash());
    if current_hash != Some(hash) {
        return Err(ApiError::conflict("panel login changed; sign in again"));
    }
    let token = state.sessions.create();
    drop(migrated);
    tracing::info!("a password login succeeded");
    Ok((
        [(header::SET_COOKIE, session_cookie_header(&token))],
        Json(session_body(&state, Some(&Identity::Password))),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct SetPasswordRequest {
    password: String,
    confirmation: String,
}

/// Only an already authenticated operator may migrate a legacy panel. This route
/// is behind both the API credential guard and the browser same-origin guard.
pub async fn set_password(
    State(state): State<AppState>,
    Json(body): Json<SetPasswordRequest>,
) -> Result<Response, ApiError> {
    let setup = state
        .password_setup_lock
        .clone()
        .try_lock_owned()
        .map_err(|_| {
            ApiError::conflict("a panel password is already being saved; refresh the page")
        })?;
    let cfg = state.auth_config();
    if !cfg.trust_identity_header || cfg.auth.allowed_users().is_empty() {
        return Err(ApiError::conflict(
            "this panel already requires password login",
        ));
    }
    if body.password != body.confirmation {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "the passwords do not match",
        ));
    }
    if body.password.chars().count() < 12 || body.password.len() > 1024 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "use a password with at least 12 characters and at most 1024 bytes",
        ));
    }
    let permit = state
        .login_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "password checks are busy; try again in a moment",
            )
        })?;
    let root = state.cfg.bots_dir.clone();
    let active_state = state.clone();
    let token = tokio::task::spawn_blocking(move || {
        // Keep the write, activation and locks together even if the tab closes
        // while the worker hashes. A completed disk write must activate in memory.
        let (_setup, _permit) = (setup, permit);
        let hash = auth::hash_password(&body.password)?;
        crate::panel::credentials::save(&root, &hash)?;
        let mut active = active_state
            .migrated_password
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *active = Some(hash);
        active_state.sessions.revoke_all();
        Ok::<_, anyhow::Error>(active_state.sessions.create())
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the panel password could not be saved",
        )
    })?
    .map_err(|e| ApiError::internal(&e))?;
    tracing::info!("panel password saved; legacy identity login disabled");
    Ok((
        [(header::SET_COOKIE, session_cookie_header(&token))],
        Json(session_body(&state, Some(&Identity::Password))),
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
    async fn untrusted_proxy_headers_cannot_bypass_a_password_session() {
        let mut h = with_password("untrusted-proxy", "a separate panel password");
        let mut cfg = (*h.state.cfg).clone();
        // Even a retained allowlist must not bypass the deployment's trust flag.
        cfg.auth = AuthMode::Either {
            allowed_users: vec!["alice@example.com".into()],
            hash: cfg.auth.password_hash().unwrap().into(),
        };
        cfg.trust_identity_header = false;
        h.state.cfg = Arc::new(cfg);
        h.app = super::super::router(h.state.clone());

        let request = |cookie: Option<&str>| {
            let mut builder = Request::builder()
                .uri("/api/bots")
                .header("Tailscale-User-Login", "alice@example.com");
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            builder.body(Body::empty()).unwrap()
        };
        let response = h.app.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let (status, _, cookie) = post(
            &h,
            "/api/login",
            r#"{"password":"a separate panel password"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let cookie = cookie.unwrap();
        let cookie = cookie.split(';').next().unwrap();
        let response = h.app.clone().oneshot(request(Some(cookie))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        h.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/logout")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let response = h.app.clone().oneshot(request(Some(cookie))).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
        assert_eq!(v["runtime"], "docker");
        assert_eq!(v["rfqDefault"], true);
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
