// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Fixed-policy runtime broker for the hosted panel profile.
//!
//! The broker is the only process on a hosted host that holds the Docker
//! socket. It listens on a Unix socket and answers a fixed set of calls about a
//! fixed set of containers, named in `STITCH_BROKER_BOTS` by the host's
//! Terraform. There is no Docker API passthrough: no create, exec, remove,
//! inspect, pull of arbitrary images, or anything that takes a mount, env or
//! policy from the caller.
//!
//! What a caller can do, per enrolled container:
//!
//! - list it (`GET /v1/bots`): the panel's own container summary, which has no
//!   env, so no secret crosses the socket
//! - start, stop, restart it, with the panel's fixed grace period
//! - read a bounded log tail, optionally followed
//! - read the labels and repo digests of the image it runs
//! - recreate it on another image, only as `<repository>@sha256:<digest>` in
//!   the one repository `STITCH_BROKER_IMAGE_REPOSITORY` names, and only with a
//!   build provenance attestation for that digest from the mirror's bot
//!   container workflow, verified here ([`super::attestation::verify_swap`])
//!
//! The socket is `0660 root:65532`; the panel container runs as a member of
//! group 65532 and mounts the socket directory read-only.
use super::docker::swap::{swap_outcome, SwapOutcome};
use super::docker::{
    ContainerInfo, ContainerState, DockerApi, LogOptions, LogSource, MountInfo, STOP_GRACE_SECS,
};
use super::http::ApiError;
use anyhow::{bail, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::Arc,
};

/// Where the broker listens, inside both the broker and the panel containers.
pub const SOCKET_PATH: &str = "/run/stitch-broker/control.sock";

/// The group that may connect to the socket. The hosted panel runs in it.
pub const SOCKET_GID: u32 = 65532;

/// Repository hosted bots may be moved onto when nothing else is configured.
pub const DEFAULT_IMAGE_REPOSITORY: &str = "ghcr.io/textile-protocol/textile-stitch";

/// Upper bound on a log tail request. Followed streams start from this too.
pub const MAX_LOG_TAIL: usize = 2000;

/// The Compose project of the hosted panel stack (`name:` in
/// `docker-compose.panel.hosted.yml`). Compose labels every container it starts
/// for that project with it, and names a service without `container_name`
/// `<project>-<service>-<n>`.
pub const PANEL_STACK_PROJECT: &str = "stitch-panel";

/// The label Compose puts the project name in.
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";

/// The panel stack's own containers, by the names the hosted compose file gives
/// them, plus the bare service names. Never enrollable: stopping or swapping one
/// would cut off the panel, its tailnet and break-glass access, or the broker.
/// A test checks every service in the compose file is covered here.
const RESERVED_NAMES: &[&str] = &[
    "stitch-panel",
    "stitch-panel-tailscale",
    "stitch-runtime-broker",
    "tailscale",
    "panel",
    "runtime-broker",
];

/// Whether `name` is one of the panel stack's containers: a reserved name, or a
/// name Compose would generate for a service of the panel project.
fn is_panel_stack_name(name: &str) -> bool {
    RESERVED_NAMES.contains(&name)
        || name
            .strip_prefix(PANEL_STACK_PROJECT)
            .is_some_and(|rest| rest.starts_with(['-', '_']))
}

/// Whether a running container belongs to the panel stack, whatever its name.
fn is_panel_stack_container(c: &ContainerInfo) -> bool {
    is_panel_stack_name(&c.name)
        || c.labels.get(COMPOSE_PROJECT_LABEL).map(String::as_str) == Some(PANEL_STACK_PROJECT)
}

#[derive(Clone)]
pub struct Broker {
    docker: Arc<dyn DockerApi>,
    policy: Arc<Policy>,
    actions: Arc<tokio::sync::Semaphore>,
}

/// What the broker will act on. Built once at startup from the environment and
/// never changed by a request.
#[derive(Debug)]
pub struct Policy {
    names: BTreeSet<String>,
    image_repository: String,
}

/// The container summary that crosses the socket. Mirrors [`ContainerInfo`],
/// which carries no env: labels and mounts are what the panel's inventory needs
/// to find each bot's config directory.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct WireContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub state: String,
    pub status: String,
    pub created_unix: i64,
    pub labels: HashMap<String, String>,
    pub mounts: Vec<WireMount>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct WireMount {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub rw: bool,
}

/// One log line as newline-delimited JSON.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct WireLogLine {
    pub stderr: bool,
    pub text: String,
}

/// What the broker says about the image an enrolled container runs.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct WireImage {
    pub labels: HashMap<String, String>,
    pub repo_digests: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ImageRequest {
    pub image: String,
    /// Sigstore bundles for `image`, as GHCR serves them. The panel fetches them
    /// (the broker has no network) and the broker verifies them; at least one
    /// must pass or the swap is refused.
    #[serde(default)]
    pub attestations: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub follow: bool,
}

#[derive(Debug, Deserialize)]
pub struct ImageQuery {
    pub image: String,
}

impl From<ContainerInfo> for WireContainer {
    fn from(c: ContainerInfo) -> Self {
        Self {
            id: c.id,
            name: c.name,
            image: c.image,
            image_id: c.image_id,
            state: c.state.as_str().into(),
            status: c.status,
            created_unix: c.created_unix,
            labels: c.labels,
            mounts: c
                .mounts
                .into_iter()
                .map(|m| WireMount {
                    source: m.source,
                    destination: m.destination,
                    rw: m.rw,
                })
                .collect(),
        }
    }
}

impl From<WireContainer> for ContainerInfo {
    fn from(c: WireContainer) -> Self {
        Self {
            id: c.id,
            name: c.name,
            image: c.image,
            image_id: c.image_id,
            state: ContainerState::parse(&c.state),
            status: c.status,
            created_unix: c.created_unix,
            labels: c.labels,
            mounts: c
                .mounts
                .into_iter()
                .map(|m| MountInfo {
                    source: m.source,
                    destination: m.destination,
                    rw: m.rw,
                })
                .collect(),
        }
    }
}

/// A container name Docker would accept, and not one of the panel stack's own.
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric());
    first_ok
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && !is_panel_stack_name(name)
}

/// `<repository>@sha256:<64 lowercase hex>`, with nothing else: no tag, no
/// other algorithm, no other repository. Tags (including `latest`) are refused
/// because they can move after review; a digest names exactly one image.
pub fn check_digest_ref(image: &str, repository: &str) -> Result<(), String> {
    let Some((repo, digest)) = image.split_once('@') else {
        return Err(format!(
            "{image} is not a digest reference. Use {repository}@sha256:<digest>; tags such as \
             :latest can move after review and are refused."
        ));
    };
    if repo != repository {
        return Err(format!(
            "{repo} is not the bot image repository. This host only runs {repository}@sha256:…"
        ));
    }
    let hex = digest.strip_prefix("sha256:").unwrap_or("");
    let hex_ok = hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex_ok {
        return Err(format!(
            "{digest} is not a sha256 digest. Expected sha256: followed by 64 lowercase hex characters."
        ));
    }
    Ok(())
}

impl Policy {
    /// Parse the enrolled names and the allowed image repository.
    pub fn new(names: &str, image_repository: &str) -> Result<Self> {
        let names: BTreeSet<String> = names.split(',').map(str::trim).map(str::to_owned).collect();
        if names.is_empty() || names.len() > 64 || names.iter().any(|n| !valid_name(n)) {
            bail!(
                "STITCH_BROKER_BOTS must list 1 to 64 exact bot container names, comma-separated \
                 (e.g. stitch-luno-a). Empty entries, wildcards and the panel's own containers \
                 are refused."
            );
        }
        let image_repository = image_repository.trim().to_string();
        if image_repository.is_empty()
            || image_repository.contains(['@', ' '])
            || image_repository
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains(':'))
        {
            bail!("STITCH_BROKER_IMAGE_REPOSITORY must be a bare repository such as {DEFAULT_IMAGE_REPOSITORY}");
        }
        Ok(Self {
            names,
            image_repository,
        })
    }

    pub fn allows(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn image_repository(&self) -> &str {
        &self.image_repository
    }
}

impl Broker {
    pub fn new(docker: Arc<dyn DockerApi>, policy: Policy) -> Self {
        Self {
            docker,
            policy: Arc::new(policy),
            actions: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/v1/bots", get(list))
            .route("/v1/bots/{name}/logs", get(logs))
            .route("/v1/bots/{name}/image", get(image_info).post(replace_image))
            .route("/v1/bots/{name}/{action}", post(act))
            .with_state(self)
    }

    async fn enrolled(&self) -> Result<Vec<ContainerInfo>, ApiError> {
        let containers = self.docker.list_all().await.map_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "broker inventory failed");
            ApiError::new(StatusCode::BAD_GATEWAY, "runtime inventory unavailable")
        })?;
        Ok(containers
            .into_iter()
            // The names were checked at startup; the label catches a panel
            // stack container that a host renamed onto an enrolled name.
            .filter(|c| self.policy.allows(&c.name) && !is_panel_stack_container(c))
            .collect())
    }

    /// The enrolled container called `name`, or a refusal that says why not.
    async fn find(&self, name: &str) -> Result<ContainerInfo, ApiError> {
        if !self.policy.allows(name) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "container is not enrolled with the runtime broker",
            ));
        }
        self.enrolled()
            .await?
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| ApiError::not_found("enrolled container is not on this host"))
    }

    fn permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
        self.actions.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "another lifecycle action is in progress",
            )
        })
    }
}

async fn list(State(broker): State<Broker>) -> Result<Json<Vec<WireContainer>>, ApiError> {
    Ok(Json(
        broker
            .enrolled()
            .await?
            .into_iter()
            .map(WireContainer::from)
            .collect(),
    ))
}

async fn act(
    State(broker): State<Broker>,
    Path((name, action)): Path<(String, Action)>,
) -> Result<StatusCode, ApiError> {
    let _permit = broker.permit()?;
    let container = broker.find(&name).await?;
    // Docker restart also starts stopped containers. Preserve an intentional stop;
    // bringing a bot back on the book requires the explicit Start action.
    if matches!(&action, Action::Restart) && container.state.is_terminal() {
        return Err(ApiError::conflict(
            "container is stopped; use Start to bring it back up",
        ));
    }
    let result = match action {
        Action::Start => broker.docker.start(&name).await,
        Action::Stop => broker.docker.stop(&name, STOP_GRACE_SECS).await,
        Action::Restart => broker.docker.restart(&name, STOP_GRACE_SECS).await,
    };
    result.map_err(|e| {
        tracing::warn!(container = %name, ?action, error = %format!("{e:#}"), "broker action failed");
        ApiError::new(StatusCode::BAD_GATEWAY, "runtime action failed")
    })?;
    tracing::info!(container = %name, ?action, "broker lifecycle action");
    Ok(StatusCode::NO_CONTENT)
}

async fn logs(
    State(broker): State<Broker>,
    Path(name): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Response, ApiError> {
    broker.find(&name).await?;
    let opts = LogOptions {
        follow: query.follow,
        tail: query.tail.unwrap_or(500).min(MAX_LOG_TAIL),
    };
    let lines = broker.docker.logs(&name, opts).map(|line| {
        let wire = match line {
            Ok(line) => WireLogLine {
                stderr: line.source == LogSource::Stderr,
                text: line.text,
            },
            Err(e) => WireLogLine {
                stderr: true,
                text: format!("log stream ended: {e:#}"),
            },
        };
        let mut bytes = serde_json::to_vec(&wire).unwrap_or_default();
        bytes.push(b'\n');
        Ok::<_, std::convert::Infallible>(bytes)
    });
    Ok((
        [("content-type", "application/x-ndjson")],
        Body::from_stream(lines),
    )
        .into_response())
}

/// Labels and digests of an image an enrolled container runs. `image` must be
/// that container's image reference or image id, so the broker never answers
/// about images unrelated to an enrolled bot.
async fn image_info(
    State(broker): State<Broker>,
    Path(name): Path<String>,
    Query(query): Query<ImageQuery>,
) -> Result<Json<WireImage>, ApiError> {
    let container = broker.find(&name).await?;
    if query.image != container.image && query.image != container.image_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "the broker only describes the image an enrolled container runs",
        ));
    }
    let unavailable = |e: anyhow::Error| {
        tracing::warn!(error = %format!("{e:#}"), "broker image read failed");
        ApiError::new(StatusCode::BAD_GATEWAY, "image details unavailable")
    };
    Ok(Json(WireImage {
        labels: broker
            .docker
            .local_image_labels(&query.image)
            .await
            .map_err(unavailable)?,
        repo_digests: broker
            .docker
            .local_image_digests(&query.image)
            .await
            .map_err(unavailable)?,
    }))
}

/// A failed swap's reply. `outcome` says what the swap left behind (see
/// [`super::docker::swap`]); `null` means it isn't known, and the panel must
/// treat the bot as possibly live.
#[derive(Debug, Deserialize, Serialize)]
pub struct SwapErrorBody {
    pub error: String,
    pub outcome: Option<SwapOutcome>,
}

async fn replace_image(
    State(broker): State<Broker>,
    Path(name): Path<String>,
    Json(body): Json<ImageRequest>,
) -> Result<StatusCode, Response> {
    // Checked here as well as in the panel: the broker is the boundary, and a
    // compromised panel process must not be able to pick a tag or a repository.
    check_digest_ref(&body.image, broker.policy.image_repository())
        .map_err(|e| ApiError::bad_request(e).into_response())?;
    // Before anything about the container is touched. A refusal is a definite
    // outcome: nothing was stopped.
    let attested = super::attestation::verify_swap(
        broker.policy.image_repository(),
        &body.image,
        &body.attestations,
    )
    .map_err(|error| {
        tracing::warn!(container = %name, image = %body.image, %error, "broker refused an unattested image");
        let reply = SwapErrorBody {
            error,
            outcome: Some(SwapOutcome::Unchanged),
        };
        (StatusCode::FORBIDDEN, Json(reply)).into_response()
    })?;
    tracing::info!(
        container = %name,
        digest = %attested.digest,
        workflow = %attested.workflow,
        commit = %attested.commit,
        signed_at = %attested.signed_at,
        "broker verified the image's build attestation"
    );
    let _permit = broker.permit().map_err(IntoResponse::into_response)?;
    let container = broker
        .find(&name)
        .await
        .map_err(IntoResponse::into_response)?;
    let previous = container.image.clone();
    broker
        .docker
        .replace_image(&name, &body.image)
        .await
        .map_err(|e| {
            let outcome = swap_outcome(&e);
            tracing::warn!(container = %name, image = %body.image, ?outcome, error = %format!("{e:#}"), "broker image swap failed");
            let reply = SwapErrorBody {
                error: format!("image swap failed: {e:#}"),
                outcome,
            };
            (StatusCode::BAD_GATEWAY, Json(reply)).into_response()
        })?;
    tracing::info!(container = %name, from = %previous, to = %body.image, commit = %attested.commit, "broker image swap");
    Ok(StatusCode::NO_CONTENT)
}

/// Unix socket only. The broker owns its directory; the panel mounts it read-only.
#[cfg(unix)]
pub async fn serve() -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let docker = Arc::new(super::docker::BollardDocker::connect(
        std::path::Path::new("/var/run/docker.sock"),
    )?);
    docker.ping().await?;
    let policy = Policy::new(
        &std::env::var("STITCH_BROKER_BOTS").unwrap_or_default(),
        &std::env::var("STITCH_BROKER_IMAGE_REPOSITORY")
            .unwrap_or_else(|_| DEFAULT_IMAGE_REPOSITORY.to_string()),
    )?;
    tracing::info!(bots = ?policy.names, repository = %policy.image_repository, "runtime broker policy");
    let broker = Broker::new(docker, policy);
    let socket = std::path::Path::new(SOCKET_PATH);
    std::fs::create_dir_all(socket.parent().unwrap())?;
    match std::fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(socket)?,
        Ok(_) => bail!("broker socket path is not a socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = tokio::net::UnixListener::bind(socket)?;
    let path = std::ffi::CString::new(socket.as_os_str().as_encoded_bytes())?;
    // Safe: the path is NUL-terminated and uid/gid are fixed deployment values.
    if unsafe { libc::chown(path.as_ptr(), 0, SOCKET_GID) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    axum::serve(listener, broker.router()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::docker::{
        fake::{Call, FakeDocker},
        ContainerInfo, ContainerState, LogLine,
    };
    use super::*;
    use crate::panel::attestation::tests::{BOT_BUNDLE, BOT_DIGEST, PANEL_BUNDLE, PANEL_DIGEST};
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use std::collections::HashMap;
    use tower::ServiceExt;

    const REPO: &str = DEFAULT_IMAGE_REPOSITORY;

    fn digest(c: char) -> String {
        format!("{REPO}@sha256:{}", c.to_string().repeat(64))
    }

    fn container(id: char, name: &str) -> ContainerInfo {
        ContainerInfo {
            id: id.to_string().repeat(64),
            name: name.into(),
            image: digest('1'),
            image_id: "sha256:reviewed".into(),
            state: ContainerState::Running,
            status: "Up".into(),
            created_unix: 0,
            labels: HashMap::from([("com.docker.compose.service".into(), name.into())]),
            mounts: vec![],
        }
    }

    fn app(docker: Arc<FakeDocker>, names: &str) -> Router {
        Broker::new(docker, Policy::new(names, REPO).unwrap()).router()
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<String>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let response = app
            .clone()
            .oneshot(
                req.body(body.map(Body::from).unwrap_or_else(Body::empty))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Every service in the hosted compose file, by its `container_name` and by
    /// the name Compose would give it without one. Fails when the file gains a
    /// service the reserved set doesn't cover, or changes its project name.
    #[test]
    fn every_container_of_the_hosted_panel_stack_is_refused() {
        let compose = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docker-compose.panel.hosted.yml"),
        )
        .unwrap();
        let project = compose
            .lines()
            .find_map(|l| l.strip_prefix("name:"))
            .map(str::trim)
            .expect("the hosted compose file sets a project name");
        assert_eq!(project, PANEL_STACK_PROJECT);
        let mut in_services = false;
        let mut services = Vec::new();
        let mut container_names = Vec::new();
        for line in compose.lines() {
            if !line.starts_with(' ') && !line.starts_with('#') && !line.is_empty() {
                in_services = line == "services:";
                continue;
            }
            if !in_services {
                continue;
            }
            if let Some(key) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':')) {
                if !key.starts_with([' ', '#']) {
                    services.push(key.to_string());
                }
            }
            if let Some(v) = line.trim().strip_prefix("container_name:") {
                container_names.push(v.trim().to_string());
            }
        }
        assert!(services.len() >= 3, "{services:?}");
        assert_eq!(container_names.len(), services.len(), "{container_names:?}");
        for name in container_names
            .iter()
            .cloned()
            .chain(services.iter().cloned())
            .chain(services.iter().map(|s| format!("{project}-{s}-1")))
        {
            assert!(!valid_name(&name), "{name} could be enrolled");
            assert!(Policy::new(&name, REPO).is_err(), "{name}");
        }
    }

    #[tokio::test]
    async fn a_panel_stack_container_is_hidden_even_under_an_enrolled_name() {
        let docker = Arc::new(FakeDocker::new());
        let mut sidecar = container('t', "stitch-luno-a");
        sidecar
            .labels
            .insert(COMPOSE_PROJECT_LABEL.into(), PANEL_STACK_PROJECT.into());
        docker.add_container(sidecar);
        let broker = Broker::new(docker.clone(), Policy::new("stitch-luno-a", REPO).unwrap());
        assert!(broker.enrolled().await.unwrap().is_empty());
        assert!(broker.find("stitch-luno-a").await.is_err());
    }

    #[test]
    fn policy_refuses_empty_wildcard_reserved_and_malformed_names() {
        for names in [
            "",
            "*",
            "stitch-a,",
            "stitch-panel",
            "stitch-panel-tailscale",
            "stitch-runtime-broker",
            "tailscale",
            "stitch-panel-panel-1",
            "stitch-luno-a,stitch-panel-tailscale",
            "-leading-dash",
            "has space",
            "a/b",
        ] {
            assert!(Policy::new(names, REPO).is_err(), "{names:?}");
        }
        assert!(Policy::new("stitch-luno-a, stitch-luno-b", REPO).is_ok());
        for repo in ["", "ghcr.io/x/y:latest", "ghcr.io/x/y@sha256:abc"] {
            assert!(Policy::new("stitch-a", repo).is_err(), "{repo:?}");
        }
    }

    #[test]
    fn only_full_sha256_digests_in_the_configured_repository_pass() {
        assert!(check_digest_ref(&digest('a'), REPO).is_ok());
        for image in [
            format!("{REPO}:latest"),
            format!("{REPO}:sha-abc123"),
            format!("{REPO}:latest@sha256:{}", "a".repeat(64)),
            format!("{REPO}@sha256:{}", "A".repeat(64)),
            format!("{REPO}@sha256:{}", "a".repeat(63)),
            format!("{REPO}@sha512:{}", "a".repeat(128)),
            format!("ghcr.io/attacker/stitch@sha256:{}", "a".repeat(64)),
            REPO.to_string(),
        ] {
            assert!(check_digest_ref(&image, REPO).is_err(), "{image}");
        }
    }

    #[tokio::test]
    async fn inventory_lists_only_enrolled_names() {
        let docker = Arc::new(FakeDocker::new());
        docker.add_container(container('a', "stitch-luno-a"));
        docker.add_container(container('b', "unrelated"));
        let app = app(docker, "stitch-luno-a");
        let (status, body) = send(&app, "GET", "/v1/bots", None).await;
        assert_eq!(status, StatusCode::OK);
        let list: Vec<WireContainer> = serde_json::from_str(&body).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "stitch-luno-a");
        assert!(!body.contains("unrelated"));
    }

    #[tokio::test]
    async fn arbitrary_docker_operations_and_unenrolled_names_are_refused() {
        let docker = Arc::new(FakeDocker::new());
        docker.add_container(container('a', "stitch-luno-a"));
        docker.add_container(container('b', "other"));
        let app = app(docker.clone(), "stitch-luno-a");
        let payload = r#"{"Privileged":true,"Binds":["/:/host"],"image":"x"}"#.to_string();
        for (method, path) in [
            ("POST", "/containers/create"),
            ("POST", "/v1/bots/create"),
            ("POST", "/v1/bots/stitch-luno-a/exec"),
            ("POST", "/v1/bots/stitch-luno-a/remove"),
            ("POST", "/v1/bots/other/start"),
            ("GET", "/v1/bots/other/logs"),
            ("POST", "/v1/bots/other/image"),
            ("POST", "/v1/policy"),
            ("DELETE", "/v1/bots/stitch-luno-a"),
        ] {
            let (status, _) = send(&app, method, path, Some(payload.clone())).await;
            assert!(status.is_client_error(), "{method} {path}: {status}");
        }
        assert!(
            docker.calls().iter().all(|c| !matches!(
                c,
                Call::Start(_)
                    | Call::Stop { .. }
                    | Call::Remove { .. }
                    | Call::Create(_)
                    | Call::ReplaceImage { .. }
            )),
            "{:?}",
            docker.calls()
        );
    }

    #[tokio::test]
    async fn lifecycle_actions_use_the_fixed_grace_period() {
        let docker = Arc::new(FakeDocker::new());
        docker.add_container(container('a', "stitch-luno-a"));
        let app = app(docker.clone(), "stitch-luno-a");
        for action in ["stop", "start", "restart"] {
            let (status, _) = send(
                &app,
                "POST",
                &format!("/v1/bots/stitch-luno-a/{action}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{action}");
        }
        assert!(docker.calls().iter().any(|call| matches!(call, Call::Stop { name, grace_secs } if name == "stitch-luno-a" && *grace_secs == STOP_GRACE_SECS)));
    }

    #[tokio::test]
    async fn an_enrolled_name_that_is_gone_is_not_found() {
        let docker = Arc::new(FakeDocker::new());
        let app = app(docker.clone(), "stitch-luno-a");
        let (status, _) = send(&app, "POST", "/v1/bots/stitch-luno-a/start", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(docker.calls().iter().all(|c| !matches!(c, Call::Start(_))));
    }

    #[tokio::test]
    async fn restart_preserves_terminal_states_until_explicit_start() {
        for state in [
            ContainerState::Created,
            ContainerState::Exited,
            ContainerState::Dead,
        ] {
            let docker = Arc::new(FakeDocker::new());
            let mut bot = container('a', "stitch-luno-a");
            bot.state = state;
            docker.add_container(bot);
            let app = app(docker.clone(), "stitch-luno-a");
            let (status, _) = send(&app, "POST", "/v1/bots/stitch-luno-a/restart", None).await;
            assert_eq!(status, StatusCode::CONFLICT, "{state:?}");
            assert_eq!(docker.state_of("stitch-luno-a"), Some(state));
            let (status, _) = send(&app, "POST", "/v1/bots/stitch-luno-a/start", None).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{state:?}");
            assert_eq!(
                docker.state_of("stitch-luno-a"),
                Some(ContainerState::Running)
            );
        }
    }

    #[tokio::test]
    async fn image_swaps_take_only_digests_in_the_configured_repository() {
        let docker = Arc::new(FakeDocker::new());
        docker.add_container(container('a', "stitch-luno-a"));
        let app = app(docker.clone(), "stitch-luno-a");
        for image in [
            format!("{REPO}:latest"),
            format!("{REPO}:sha-abc"),
            format!("ghcr.io/attacker/x@sha256:{}", "b".repeat(64)),
        ] {
            let body = serde_json::to_string(&ImageRequest {
                image: image.clone(),
                attestations: vec![BOT_BUNDLE.to_string()],
            })
            .unwrap();
            let (status, _) = send(&app, "POST", "/v1/bots/stitch-luno-a/image", Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{image}");
        }
        assert!(docker
            .calls()
            .iter()
            .all(|c| !matches!(c, Call::ReplaceImage { .. })));

        let image = format!("{REPO}@{BOT_DIGEST}");
        let (status, _) = swap(&app, &image, &[BOT_BUNDLE]).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(docker.calls().iter().any(|c| matches!(c, Call::ReplaceImage { name, new_image } if name == "stitch-luno-a" && *new_image == image)));
    }

    async fn swap(app: &Router, image: &str, bundles: &[&str]) -> (StatusCode, String) {
        let body = serde_json::to_string(&ImageRequest {
            image: image.to_string(),
            attestations: bundles.iter().map(|b| b.to_string()).collect(),
        })
        .unwrap();
        send(app, "POST", "/v1/bots/stitch-luno-a/image", Some(body)).await
    }

    /// The broker is the gate: whatever the panel sends (or leaves out), a digest
    /// without a valid bot-workflow attestation never reaches the runtime, and the
    /// refusal says nothing was touched so the panel releases the wallet cleanly.
    #[tokio::test]
    async fn image_swaps_need_a_valid_bot_attestation_for_the_exact_digest() {
        let docker = Arc::new(FakeDocker::new());
        docker.add_container(container('a', "stitch-luno-a"));
        let app = app(docker.clone(), "stitch-luno-a");
        let bot = format!("{REPO}@{BOT_DIGEST}");
        let other = digest('b');
        let cases: [(&str, &str, &[&str]); 5] = [
            ("no attestation", &bot, &[]),
            ("the panel image's attestation", &bot, &[PANEL_BUNDLE]),
            ("another digest's attestation", &other, &[BOT_BUNDLE]),
            ("a garbage bundle", &bot, &["{}"]),
            (
                "the panel image under the bot repository",
                &format!("{REPO}@{PANEL_DIGEST}"),
                &[PANEL_BUNDLE],
            ),
        ];
        for (what, image, bundles) in cases {
            let (status, body) = swap(&app, image, bundles).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{what}: {body}");
            let reply: SwapErrorBody = serde_json::from_str(&body).unwrap();
            assert_eq!(reply.outcome, Some(SwapOutcome::Unchanged), "{what}");
        }
        // The fixture bundle is real, so a mixed list still passes on the good one.
        let (status, _) = swap(&app, &bot, &[PANEL_BUNDLE, BOT_BUNDLE]).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let swaps = docker
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::ReplaceImage { .. }))
            .count();
        assert_eq!(swaps, 1);
    }

    #[tokio::test]
    async fn a_broker_for_another_repository_refuses_every_swap() {
        let docker = Arc::new(FakeDocker::new());
        let mut c = container('a', "stitch-luno-a");
        let panel_repo = "ghcr.io/textile-protocol/textile-stitch-panel";
        c.image = format!("{panel_repo}@sha256:{}", "1".repeat(64));
        docker.add_container(c);
        let app = Broker::new(
            docker.clone(),
            Policy::new("stitch-luno-a", panel_repo).unwrap(),
        )
        .router();
        let (status, body) = swap(
            &app,
            &format!("{panel_repo}@{PANEL_DIGEST}"),
            &[PANEL_BUNDLE],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("no attestation policy"), "{body}");
        assert!(docker
            .calls()
            .iter()
            .all(|c| !matches!(c, Call::ReplaceImage { .. })));
    }

    #[tokio::test]
    async fn logs_are_bounded_and_limited_to_enrolled_containers() {
        let docker = Arc::new(FakeDocker::new().with_log_lines(vec![LogLine {
            source: LogSource::Stdout,
            text: "quoted".into(),
        }]));
        docker.add_container(container('a', "stitch-luno-a"));
        let app = app(docker.clone(), "stitch-luno-a");
        let (status, body) =
            send(&app, "GET", "/v1/bots/stitch-luno-a/logs?tail=999999", None).await;
        assert_eq!(status, StatusCode::OK);
        let line: WireLogLine = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(line.text, "quoted");
        assert!(docker
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Logs { tail, .. } if *tail == MAX_LOG_TAIL)));
    }

    #[tokio::test]
    async fn image_details_are_only_given_for_the_image_a_bot_runs() {
        let docker = Arc::new(FakeDocker::new());
        docker.add_container(container('a', "stitch-luno-a"));
        let app = app(docker, "stitch-luno-a");
        let own = urlencode(&digest('1'));
        let (status, _) = send(
            &app,
            "GET",
            &format!("/v1/bots/stitch-luno-a/image?image={own}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let other = urlencode("alpine:latest");
        let (status, _) = send(
            &app,
            "GET",
            &format!("/v1/bots/stitch-luno-a/image?image={other}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    fn urlencode(s: &str) -> String {
        s.replace(':', "%3A")
            .replace('/', "%2F")
            .replace('@', "%40")
    }
}
