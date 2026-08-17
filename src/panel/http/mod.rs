// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The panel's HTTP surface.
//!
//! Handlers are thin: they resolve a bot out of the live inventory, call into
//! `setup` or the Docker layer, and translate the result to JSON. Everything that
//! decides what a valid config is lives in [`crate::setup`], so the panel and the
//! desktop app can't disagree.
//!
//! Errors are returned as `{"error": "…"}` with the full context chain. The panel
//! is an admin tool behind a tailnet, and an operator debugging a bot that won't
//! start is better served by the real message than by a sanitised one. Secrets
//! never reach a response body: nothing here reads key material, and the wizard
//! takes it write-only.

pub mod assets;
pub mod bots;
pub mod enroll;
pub mod logs;
pub mod origin;
pub mod session;
pub mod settings;
pub mod updates;
pub mod wizard;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Serialize;

use super::auth::{self, AuthError, Identity, Sessions, IDENTITY_HEADER, SESSION_COOKIE};
use super::docker::DockerApi;
use super::inventory::{self, Bot, Fleet};
use super::migrate::ContainerFiles;
use super::PanelConfig;

/// Everything a handler needs. Cheap to clone: one `Arc` per field.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<PanelConfig>,
    pub docker: Arc<dyn DockerApi>,
    /// Reading files out of containers, used only by the layout migration. An
    /// `Option` because it isn't part of [`DockerApi`], and a backend that can't
    /// do it should still serve every other route.
    pub files: Option<Arc<dyn ContainerFiles>>,
    pub sessions: Arc<Sessions>,
    /// Exclusive claims on operator wallets, held across any action that puts a
    /// signer on one — an approval run, a bot launch, a settings restart. One lock
    /// per wallet, so the paths can't slip past each other's checks. See
    /// [`logs::WalletLocks`].
    pub wallet_locks: Arc<logs::WalletLocks>,
    /// Permits for password verification. `/api/login` answers before the caller has
    /// a credential, and each check costs 19 MiB and a blocking thread, so the one
    /// public route that does real work is the one that needs a ceiling.
    pub login_permits: Arc<tokio::sync::Semaphore>,
    /// One lock per bot config, so two saves can't each read the same file and write
    /// a complete one back. See [`settings::ConfigLocks`].
    pub config_locks: Arc<settings::ConfigLocks>,
}

impl AppState {
    pub fn new(cfg: PanelConfig, docker: Arc<dyn DockerApi>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            docker,
            files: None,
            sessions: Arc::new(Sessions::new()),
            wallet_locks: Arc::new(logs::WalletLocks::new()),
            login_permits: Arc::new(tokio::sync::Semaphore::new(
                auth::MAX_CONCURRENT_VERIFICATIONS,
            )),
            config_locks: Arc::new(settings::ConfigLocks::new()),
        }
    }

    pub fn with_container_files(mut self, files: Arc<dyn ContainerFiles>) -> Self {
        self.files = Some(files);
        self
    }

    /// Rebuild the fleet from the live container list. Every request that needs
    /// bot state does this: the container list plus the config dirs *are* the
    /// state, so there is nothing to cache and no way to serve a stale view.
    pub async fn fleet(&self) -> Result<Fleet, ApiError> {
        let containers = self.docker.list_all().await.map_err(|e| {
            let detail = match self.cfg.runtime {
                crate::panel::PanelRuntime::Docker => format!(
                    "couldn't reach the Docker daemon at {}: {e:#}",
                    self.cfg.docker_socket.display()
                ),
                crate::panel::PanelRuntime::Process => {
                    format!("couldn't list local bots: {e:#}")
                }
            };
            ApiError::new(StatusCode::BAD_GATEWAY, detail)
        })?;
        Ok(inventory::discover(&containers, &self.cfg))
    }

    /// Resolve one bot by name, 404ing when it isn't there.
    pub async fn bot(&self, name: &str) -> Result<Bot, ApiError> {
        Ok(self.bot_and_fleet(name).await?.0)
    }

    /// The same lookup, keeping the fleet it came out of.
    ///
    /// Resolving a bot builds the whole fleet anyway, so this costs nothing extra.
    /// Anything that has to reason about a bot's *neighbours* needs it — an
    /// operator wallet shared between two config directories is one nonce sequence,
    /// and only the fleet shows that.
    pub async fn bot_and_fleet(&self, name: &str) -> Result<(Bot, Fleet), ApiError> {
        let fleet = self.fleet().await?;
        let bot = fleet
            .get(name)
            .cloned()
            .map_err(|e| ApiError::new(StatusCode::NOT_FOUND, format!("{e:#}")))?;
        Ok((bot, fleet))
    }
}

/// A JSON error response.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Bad input from the operator: a name that isn't allowed, a spread that
    /// doesn't parse, a corridor that doesn't exist.
    pub fn bad_request(e: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_REQUEST, e.to_string())
    }

    /// Nothing here. Used for a bot that isn't in the fleet and for an API path
    /// that isn't routed.
    pub fn not_found(e: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::NOT_FOUND, e.to_string())
    }

    /// The request is fine but the fleet's current state refuses it: a duplicate
    /// bot name, a bot whose config the panel can't edit.
    pub fn conflict(e: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::CONFLICT, e.to_string())
    }

    /// Something the panel tried and couldn't finish. The message carries the
    /// whole context chain, because the operator is the one who has to fix it.
    pub fn internal(e: &anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(&e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Log server-side faults; a 4xx is the client's problem and would just be
        // noise in the panel's log.
        if self.status.is_server_error() {
            tracing::warn!(status = %self.status, "{}", self.message);
        }
        (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response()
    }
}

/// Turn a bot-not-editable situation into a refusal that says why.
pub fn require_editable(bot: &Bot) -> Result<(), ApiError> {
    if bot.is_editable() {
        return Ok(());
    }
    let reason = bot
        .warnings
        .iter()
        .find(|w| w.blocks_editing())
        .map(|w| w.message())
        .unwrap_or_else(|| {
            format!(
                "the panel can't read {}'s config, so it can't edit it",
                bot.name
            )
        });
    Err(ApiError::conflict(reason))
}

/// Refuse a lifecycle action the panel can't aim at one container.
///
/// Only duplicate names get here: see [`Warning::blocks_actions`]. Editing is a
/// separate, broader check — a bot can be unstoppable and still editable, or the
/// other way round.
pub fn require_actionable(bot: &Bot) -> Result<(), ApiError> {
    match bot.warnings.iter().find(|w| w.blocks_actions()) {
        Some(w) => Err(ApiError::conflict(w.message())),
        None => Ok(()),
    }
}

/// Build the whole app: public routes, authenticated API, and the embedded SPA.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(public_routes())
        .merge(protected_routes(&state))
        // Anything not an API route is the SPA, so a deep link like /bots/bot-a
        // reloads instead of 404ing. The assets are public: the shell is a login
        // form until an API call succeeds.
        .fallback(assets::serve)
        // Outermost, so it covers login and logout as well as the authenticated
        // routes. Identity-header auth is attached by the proxy independently of
        // cookies, so `SameSite` doesn't cover it and this does.
        .layer(axum::middleware::from_fn(same_origin_only))
        .with_state(state)
}

/// Reject state-changing requests made from another site. See [`origin`].
async fn same_origin_only(req: Request, next: Next) -> Result<Response, ApiError> {
    origin::check(req.method(), req.uri().path(), req.headers())
        .map_err(|reason| ApiError::new(StatusCode::FORBIDDEN, reason))?;
    Ok(next.run(req).await)
}

/// Routes reachable without a credential, because they're how you get one.
fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/session", get(session::current))
        .route("/api/login", post(session::login))
        .route("/api/logout", post(session::logout))
        .route("/api/health", get(health))
}

fn protected_routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/api/corridors", get(wizard::corridors))
        .route("/api/wallets/generate", post(wizard::generate_wallet))
        .route("/api/signer/check", post(wizard::check_signer))
        .route("/api/bots", get(bots::list).post(wizard::create))
        .route("/api/bots/{name}", get(bots::show).delete(bots::remove))
        .route("/api/bots/{name}/start", post(bots::start))
        .route("/api/bots/{name}/stop", post(bots::stop))
        .route("/api/bots/{name}/restart", post(bots::restart))
        .route("/api/bots/{name}/recreate", post(bots::recreate))
        .route("/api/bots/{name}/update", post(bots::update))
        .route("/api/bots/{name}/versions", get(bots::versions))
        .route("/api/bots/{name}/rollback", post(bots::rollback))
        .route("/api/bots/{name}/migrate", post(bots::migrate_layout))
        .route("/api/bots/{name}/settings", get(settings::show))
        .route("/api/bots/{name}/settings", patch(settings::update))
        .route("/api/bots/{name}/rfq/enroll", post(enroll::enroll))
        .route("/api/bots/{name}/config", get(settings::raw))
        .route("/api/bots/{name}/config", put(settings::save_raw))
        .route("/api/bots/{name}/signer", put(bots::change_signer))
        .route("/api/bots/{name}/corridor", post(bots::switch_corridor))
        .route("/api/bots/{name}/logs", get(logs::tail))
        .route("/api/bots/{name}/approve", post(logs::approve))
        .route("/api/bots/{name}/dry-run", post(logs::dry_run))
        .route("/api/compose-export", get(bots::compose_export))
        .route("/api/updates", get(updates::status))
        .route("/api/panel/update", post(updates::update_panel))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
}

/// Liveness only. Says nothing about the fleet and needs no credential, so a
/// container healthcheck doesn't need one either.
async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Reject unauthenticated requests, and hand the identity to the handler.
async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let identity = identify(&state, &req).map_err(|e| {
        ApiError::new(
            match e {
                AuthError::Missing => StatusCode::UNAUTHORIZED,
                AuthError::Rejected => StatusCode::FORBIDDEN,
            },
            e.message(),
        )
    })?;
    req.extensions_mut().insert(identity);
    Ok(next.run(req).await)
}

/// Pull the credentials out of a request and ask [`auth::authorize`] about them.
pub fn identify(state: &AppState, req: &Request) -> Result<Identity, AuthError> {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let token =
        header(header::COOKIE.as_str()).and_then(|c| auth::cookie_value(&c, SESSION_COOKIE));
    auth::authorize(
        &state.cfg,
        &state.sessions,
        header(IDENTITY_HEADER).as_deref(),
        token.as_deref(),
    )
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Wiring shared by the handler tests: a fake-Docker app, a temp bots root,
    //! and enough of a config on disk to exercise the settings and log routes.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use tower::ServiceExt;

    use super::{router, AppState};
    use crate::panel::auth::SESSION_COOKIE;
    use crate::panel::docker::fake::FakeDocker;
    use crate::panel::PanelConfig;

    /// A hardhat test key. Public, funded on nobody's mainnet.
    pub const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A private temp directory for one test.
    pub fn temp_root(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "stitch-panel-http-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the test bots root");
        dir
    }

    pub struct Harness {
        pub app: Router,
        pub state: AppState,
        pub docker: Arc<FakeDocker>,
        pub root: PathBuf,
        pub token: String,
    }

    /// An app with a fake Docker, a fresh bots root, and a live session.
    pub fn harness(tag: &str) -> Harness {
        harness_with_bot_image(tag, "ghcr.io/textile-protocol/textile-stitch:test")
    }

    /// Same as [`harness`], with a caller-chosen `STITCH_PANEL_BOT_IMAGE`.
    pub fn harness_with_bot_image(tag: &str, bot_image: &str) -> Harness {
        harness_with(tag, bot_image, None)
    }

    /// Same as [`harness`], with optional overrides for bot image and docker socket.
    pub fn harness_with(tag: &str, bot_image: &str, docker_socket: Option<&str>) -> Harness {
        let root = temp_root(tag);
        let docker = Arc::new(FakeDocker::new());
        let mut cfg = PanelConfig::for_test(root.clone(), root.clone());
        cfg.bot_image = bot_image.into();
        if let Some(sock) = docker_socket {
            cfg.docker_socket = PathBuf::from(sock);
        }
        let state = AppState::new(cfg, docker.clone()).with_container_files(docker.clone());
        let token = state.sessions.create();
        Harness {
            app: router(state.clone()),
            state,
            docker,
            root,
            token,
        }
    }

    impl Harness {
        /// Send a request carrying the harness's session cookie.
        pub async fn send(&self, req: Request<Body>) -> (StatusCode, String) {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                header::COOKIE,
                format!("{SESSION_COOKIE}={}", self.token).parse().unwrap(),
            );
            let res = self
                .app
                .clone()
                .oneshot(Request::from_parts(parts, body))
                .await
                .expect("the router must not fail to respond");
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .expect("reading the response body");
            (status, String::from_utf8_lossy(&bytes).to_string())
        }

        pub async fn get(&self, path: &str) -> (StatusCode, String) {
            self.send(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
        }

        pub async fn post_json(&self, path: &str, body: serde_json::Value) -> (StatusCode, String) {
            self.json("POST", path, body).await
        }

        pub async fn patch_json(
            &self,
            path: &str,
            body: serde_json::Value,
        ) -> (StatusCode, String) {
            self.json("PATCH", path, body).await
        }

        pub async fn put_json(&self, path: &str, body: serde_json::Value) -> (StatusCode, String) {
            self.json("PUT", path, body).await
        }

        async fn json(
            &self,
            method: &str,
            path: &str,
            body: serde_json::Value,
        ) -> (StatusCode, String) {
            self.send(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
        }

        /// Parse a JSON response, failing loudly with the body when it isn't JSON.
        pub fn parse(body: &str) -> serde_json::Value {
            serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::harness;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn an_api_call_without_a_credential_is_unauthorized() {
        let h = harness("noauth");
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/bots")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_bad_credential_is_forbidden_not_unauthorized() {
        // The distinction matters to the UI: 401 means "show the login form",
        // 403 means "your credential was seen and refused".
        let h = harness("badauth");
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/bots")
                    .header("cookie", "stitch_panel_session=deadbeef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn health_needs_no_credential() {
        let h = harness("health");
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_authenticated_call_reaches_the_handler() {
        let h = harness("ok");
        let (status, body) = h.get("/api/bots").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn a_cross_site_form_post_is_refused_even_with_a_valid_credential() {
        // The attack this closes: the operator has a page from any other site
        // open on a tailnet device, that page submits a plain HTML form at the
        // panel, and `tailscale serve` attaches their identity on the way in. The
        // credential is genuine, so only the request's provenance can stop it.
        let h = harness("csrf");
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/bots/bot-a/stop")
                    .header("tailscale-user-login", "operator@example.com")
                    .header("sec-fetch-site", "cross-site")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // The panel's own page, same request, still works — it gets as far as the
        // handler, which is where "no such bot" comes from.
        let (status, body) = h
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/api/bots/bot-a/stop")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test]
    async fn an_unknown_bot_is_a_404_with_a_readable_message() {
        let h = harness("missing");
        let (status, body) = h.get("/api/bots/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("no bot called"), "{body}");
    }
}
