// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use super::testkit::{harness, Harness};
use super::{router, AppState};
use crate::panel::auth::{verify_password, SESSION_COOKIE};
use crate::panel::config::{startup_auth, AuthMode};
use crate::panel::credentials::{load, PASSWORD_FILE};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const PASSWORD: &str = "a separate panel password";

fn legacy(tag: &str) -> Harness {
    let mut h = harness(tag);
    let mut cfg = (*h.state.cfg).clone();
    cfg.auth = AuthMode::Tailnet {
        allowed_users: vec!["alice@example.com".into()],
    };
    cfg.trust_identity_header = true;
    h.state.cfg = Arc::new(cfg);
    h.app = router(h.state.clone());
    h
}

fn setup(login: Option<&str>, password: &str, confirmation: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/panel/password")
        .header(header::CONTENT_TYPE, "application/json")
        .header("Sec-Fetch-Site", "same-origin");
    if let Some(login) = login {
        req = req.header("Tailscale-User-Login", login);
    }
    req.body(Body::from(
        json!({"password":password,"confirmation":confirmation}).to_string(),
    ))
    .unwrap()
}

async fn body(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(h: &Harness, path: &str, cookie: Option<&str>) -> Response {
    let mut req = Request::builder()
        .uri(path)
        .header("Tailscale-User-Login", "alice@example.com");
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    h.app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn ui_migration_keeps_the_operator_signed_in_and_disables_legacy_access() {
    let mut h = legacy("password-migration");
    let status = body(get(&h, "/api/session", None).await).await;
    assert_eq!(status["authenticated"], true);
    assert_eq!(status["passwordSetupRequired"], true);
    assert_eq!(get(&h, "/api/bots", None).await.status(), StatusCode::OK);

    let response = h
        .app
        .clone()
        .oneshot(setup(Some("alice@example.com"), PASSWORD, PASSWORD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let status = body(response).await;
    assert_eq!(status["authenticated"], true);
    assert_eq!(status["passwordLogin"], true);
    assert_eq!(status["tailnetLogin"], false);
    assert_eq!(status["passwordSetupRequired"], false);
    assert!(verify_password(&load(&h.root).unwrap().unwrap(), PASSWORD).unwrap());
    assert!(!std::fs::read_to_string(h.root.join(PASSWORD_FILE))
        .unwrap()
        .contains(PASSWORD));
    assert_eq!(
        get(&h, "/api/bots", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let old_cookie = format!("{SESSION_COOKIE}={}", h.token);
    assert_eq!(
        get(&h, "/api/bots", Some(&old_cookie)).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&h, "/api/bots", Some(&cookie)).await.status(),
        StatusCode::OK
    );

    // Restart with the same mounted directory and old container env. Startup
    // must select the saved password before considering the identity flags.
    let (auth, trust) = startup_auth(&h.root).unwrap();
    assert!(matches!(auth, AuthMode::Password { .. }));
    assert!(!trust);
    let mut cfg = (*h.state.cfg).clone();
    cfg.auth = auth;
    cfg.trust_identity_header = trust;
    h.state = AppState::new(cfg, h.docker.clone());
    h.app = router(h.state.clone());
    assert_eq!(
        get(&h, "/api/bots", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"password":PASSWORD}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["passwordSetupRequired"], false);
    assert!(
        h.docker.calls().is_empty(),
        "migration must not stop or recreate any bot"
    );
}

#[tokio::test]
async fn anonymous_unlisted_and_cross_site_callers_cannot_claim_a_panel() {
    let h = legacy("password-unauthorized");
    for (login, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("bob@example.com"), StatusCode::FORBIDDEN),
    ] {
        let response = h
            .app
            .clone()
            .oneshot(setup(login, PASSWORD, PASSWORD))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    let mut req = setup(Some("alice@example.com"), PASSWORD, PASSWORD);
    req.headers_mut()
        .insert("Sec-Fetch-Site", "cross-site".parse().unwrap());
    let response = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!h.root.join(PASSWORD_FILE).exists());
}

#[tokio::test]
async fn invalid_password_or_failed_save_leaves_existing_access_working() {
    let h = legacy("password-failed-save");
    let large = "x".repeat(1025);
    for (password, confirmation) in [
        ("short", "short"),
        (PASSWORD, "different"),
        (&large, &large),
    ] {
        let response = h
            .app
            .clone()
            .oneshot(setup(Some("alice@example.com"), password, confirmation))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!h.root.join(PASSWORD_FILE).exists());
    }
    std::fs::create_dir(h.root.join(PASSWORD_FILE)).unwrap();
    let response = h
        .app
        .clone()
        .oneshot(setup(Some("alice@example.com"), PASSWORD, PASSWORD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let status = body(get(&h, "/api/session", None).await).await;
    assert_eq!(status["authenticated"], true);
    assert_eq!(status["passwordSetupRequired"], true);
    assert_eq!(
        std::fs::read_dir(&h.root).unwrap().count(),
        1,
        "failed writes must clean their staging file"
    );
    assert!(
        startup_auth(&h.root).is_err(),
        "invalid saved state must not restore identity login on boot"
    );
}

#[tokio::test]
async fn only_one_operator_can_set_the_password_and_it_cannot_be_overwritten() {
    let h = legacy("password-conflict");
    let lock = h.state.password_setup_lock.lock().await;
    let response = h
        .app
        .clone()
        .oneshot(setup(Some("alice@example.com"), PASSWORD, PASSWORD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    drop(lock);
    let response = h
        .app
        .clone()
        .oneshot(setup(Some("alice@example.com"), PASSWORD, PASSWORD))
        .await
        .unwrap();
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let mut req = setup(None, "a different password", "a different password");
    req.headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let response = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(verify_password(&load(&h.root).unwrap().unwrap(), PASSWORD).unwrap());
}

#[tokio::test]
async fn fallback_password_still_requires_migration_and_is_replaced() {
    let mut h = legacy("password-fallback");
    let mut cfg = (*h.state.cfg).clone();
    cfg.auth = AuthMode::Either {
        allowed_users: vec!["alice@example.com".into()],
        hash: crate::panel::auth::hash_password("old fallback password").unwrap(),
    };
    h.state.cfg = Arc::new(cfg);
    h.app = router(h.state.clone());
    let status = body(get(&h, "/api/session", None).await).await;
    assert_eq!(status["passwordLogin"], true);
    assert_eq!(status["passwordSetupRequired"], true);

    let response = h
        .app
        .clone()
        .oneshot(setup(Some("alice@example.com"), PASSWORD, PASSWORD))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"password":"old fallback password"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn closing_the_tab_after_persistence_cannot_leave_identity_login_active() {
    let h = legacy("password-cancelled");
    // Stop activation after the file is published, then drop the HTTP future.
    // The blocking worker must still finish the authentication transition.
    let activation_gate = h.state.migrated_password.read().unwrap();
    let app = h.app.clone();
    let request = tokio::spawn(async move {
        app.oneshot(setup(Some("alice@example.com"), PASSWORD, PASSWORD))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !h.root.join(PASSWORD_FILE).is_file() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    drop(activation_gate);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let active = h
                .state
                .migrated_password
                .try_read()
                .map(|value| value.is_some())
                .unwrap_or(false);
            if active {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        get(&h, "/api/bots", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
}
