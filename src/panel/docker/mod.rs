// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The Docker control surface, as its own domain types behind a trait.
//!
//! Nothing above this module mentions bollard. That buys two things: the
//! inventory, wizard and settings logic is testable against an in-memory fake
//! with no daemon, and swapping the transport later (a remote daemon over TLS,
//! say) touches one file.
//!
//! On "pause": Docker's own `pause` sends SIGSTOP, which freezes the bot
//! mid-tick while its already-signed limit orders stay live and fillable on the
//! book, with nothing left running to replace or expire them. That is strictly
//! worse than stopping, which lets the bot finish its tick and shut down
//! cleanly. So the panel's pause action is a graceful stop, and `pause` is not
//! exposed at all.

pub mod fake;

#[cfg(feature = "panel")]
mod bollard_api;

#[cfg(feature = "panel")]
pub use bollard_api::BollardDocker;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::Stream;

/// How long a bot gets to finish its current tick after SIGTERM before Docker
/// kills it. Matches `stop_grace_period: 30s` in the shipped compose files; the
/// bot only checks for shutdown between ticks, so a shorter grace risks killing
/// it mid-tick.
pub const STOP_GRACE_SECS: i64 = 30;

/// Container lifecycle state, normalised from the daemon's string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Restarting,
    /// Includes Docker's `paused`. The panel never pauses anything itself, but an
    /// operator could have, and a paused bot is not running.
    Paused,
    Exited,
    Removing,
    Dead,
    Unknown,
}

impl ContainerState {
    /// Parse the daemon's state string. Unknown values map to
    /// [`ContainerState::Unknown`] rather than being guessed at, so a future
    /// Docker state can't be silently reported as running.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "created" => ContainerState::Created,
            "running" => ContainerState::Running,
            "restarting" => ContainerState::Restarting,
            "paused" => ContainerState::Paused,
            "exited" | "stopping" => ContainerState::Exited,
            "removing" => ContainerState::Removing,
            "dead" => ContainerState::Dead,
            _ => ContainerState::Unknown,
        }
    }

    /// Whether the bot is actively quoting. Only `running` counts: a paused or
    /// restarting container is not posting orders.
    pub fn is_running(self) -> bool {
        matches!(self, ContainerState::Running)
    }

    /// Whether the process is gone for good, so a failed `docker stop` is noise
    /// rather than a reason to refuse destruction.
    ///
    /// Narrower than `!is_running()`: a paused container is frozen mid-tick, and a
    /// restarting one can still execute between attempts — force-removing either
    /// is the same risk as killing a running bot.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ContainerState::Created | ContainerState::Exited | ContainerState::Dead
        )
    }

    /// Whether the operator means this bot to be up, so an action that replaces the
    /// container starts the replacement instead of leaving it in `created`.
    ///
    /// A third question, distinct from both of the above, and the one to reach for
    /// when deciding whether to start something. [`Self::is_running`] answers "is it
    /// quoting right now" and says no for `restarting` — but a restart policy
    /// relaunching a crashing bot is the daemon carrying out an intent, and Recreate
    /// is often exactly how an operator installs the image that fixes the crash loop.
    /// Reading that as "wasn't running, leave it stopped" strands the bot in the one
    /// case where the action was meant to rescue it.
    ///
    /// Everything else stays down, for two different reasons. Terminal states are a
    /// deliberate stop and it has to survive. `paused`, `removing` and `unknown`
    /// can't be reproduced in a fresh container and none of them says the bot was
    /// meant to be on the book, so the replacement is left stopped and the operator
    /// is told — a guess in that direction puts orders back out there.
    pub fn wants_to_be_up(self) -> bool {
        matches!(self, ContainerState::Running | ContainerState::Restarting)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ContainerState::Created => "created",
            ContainerState::Running => "running",
            ContainerState::Restarting => "restarting",
            ContainerState::Paused => "paused",
            ContainerState::Exited => "exited",
            ContainerState::Removing => "removing",
            ContainerState::Dead => "dead",
            ContainerState::Unknown => "unknown",
        }
    }
}

/// One bind mount on a container, as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    /// Path on the Docker host.
    pub source: PathBuf,
    /// Path inside the container.
    pub destination: PathBuf,
    /// Whether the container can write through this mount. The bot needs a
    /// writable mount for its slot-nonce ledger, so this is what tells a correct
    /// layout from a ledger-losing one.
    pub rw: bool,
}

/// Host path to bind into the panel self-update helper for the Docker socket.
///
/// `STITCH_PANEL_DOCKER_SOCKET` is where the panel *opens* the socket inside its
/// own container. `docker create` bind sources are resolved on the Docker host,
/// so a remap like `/var/run/docker.sock:/docker.sock` must mount the host
/// source (`/var/run/docker.sock`), not the in-container destination. Directory
/// binds are the same story: `/var/run:/docker-run` with the socket at
/// `/docker-run/docker.sock` must become `/var/run/docker.sock` on the host.
/// When no mount matches (panel running on the host), the configured path is
/// already a host path and is returned unchanged.
pub fn host_docker_socket_bind(
    mounts: &[MountInfo],
    in_container_socket: &std::path::Path,
) -> PathBuf {
    if let Some(source) = mounts
        .iter()
        .find(|m| m.destination.as_path() == in_container_socket)
        .map(|m| m.source.clone())
        .filter(|s| !s.as_os_str().is_empty())
    {
        return source;
    }
    // Socket lives under a directory mount (e.g. /var/run → /docker-run).
    // Prefer the longest destination prefix when several mounts could match.
    mounts
        .iter()
        .filter(|m| !m.source.as_os_str().is_empty())
        .filter_map(|m| {
            in_container_socket
                .strip_prefix(&m.destination)
                .ok()
                .filter(|rel| !rel.as_os_str().is_empty())
                .map(|rel| (m.destination.as_os_str().len(), m.source.join(rel)))
        })
        .max_by_key(|(dest_len, _)| *dest_len)
        .map(|(_, host)| host)
        .unwrap_or_else(|| in_container_socket.to_path_buf())
}

/// A container as the panel needs to see it.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    /// Name without the API's leading slash.
    pub name: String,
    pub image: String,
    /// Content-addressed image id (`sha256:…`) when the daemon reported one.
    /// Used to compare a running container against a registry digest for updates.
    pub image_id: String,
    pub state: ContainerState,
    /// The daemon's human-readable status, e.g. "Up 3 hours".
    pub status: String,
    /// Creation time as a unix timestamp.
    pub created_unix: i64,
    pub labels: HashMap<String, String>,
    pub mounts: Vec<MountInfo>,
}

impl ContainerInfo {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

/// A bind mount to create. Rendered into Docker's `src:dst[:ro]` bind syntax,
/// the same form the hand-written compose files use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindSpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

impl BindSpec {
    pub fn rw(host_path: impl Into<PathBuf>, container_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: container_path.into(),
            read_only: false,
        }
    }

    pub fn ro(host_path: impl Into<PathBuf>, container_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: container_path.into(),
            read_only: true,
        }
    }

    /// Docker's short bind string (`src:dst[:ro]`). Paths with a colon would
    /// produce a mount spec that silently means something else, so they're
    /// rejected rather than escaped — Docker has no escape for this form.
    ///
    /// Prefer [`Self::to_compose_volume_yaml`] for compose export and the
    /// structured Mount API for container create: both accept Windows drive
    /// paths (`C:/Users/...`) that this short form cannot express.
    pub fn to_bind_string(&self) -> Result<String> {
        let host = path_str(&self.host_path)?;
        let container = path_str(&self.container_path)?;
        for (label, p) in [("host", &host), ("container", &container)] {
            anyhow::ensure!(
                !p.contains(':'),
                "{label} path {p} contains a colon, which Docker cannot express in a bind mount"
            );
        }
        Ok(if self.read_only {
            format!("{host}:{container}:ro")
        } else {
            format!("{host}:{container}")
        })
    }

    /// Host and container paths as UTF-8 strings, for long-form compose mounts
    /// when [`Self::to_bind_string`] cannot express them (Windows drive letters).
    pub fn path_strings(&self) -> Result<(String, String)> {
        Ok((path_str(&self.host_path)?, path_str(&self.container_path)?))
    }
}

fn path_str(p: &std::path::Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("path {} is not valid UTF-8", p.display()))
}

/// Everything needed to create a bot container.
#[derive(Debug, Clone)]
pub struct CreateSpec {
    pub name: String,
    pub image: String,
    pub labels: HashMap<String, String>,
    /// `KEY=value` pairs.
    pub env: Vec<String>,
    pub binds: Vec<BindSpec>,
    /// Overrides the image's CMD. `None` keeps the image default, which is the
    /// bot itself; the one-shot paths set this to `approve` or a dry run.
    pub cmd: Option<Vec<String>>,
    /// `unless-stopped`, so a host reboot brings bots back but an operator's
    /// deliberate stop sticks. One-shots set this to `false`.
    pub restart_unless_stopped: bool,
}

/// Which stream a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Stdout,
    Stderr,
}

/// One line of container output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub source: LogSource,
    pub text: String,
}

/// An event from a one-shot container run (`stitch approve`, a dry run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    Line(LogLine),
    /// The process exited. Always the last event of a successful run.
    Exited {
        code: i64,
    },
}

pub type LogStream = Pin<Box<dyn Stream<Item = Result<LogLine>> + Send>>;
pub type RunStream = Pin<Box<dyn Stream<Item = Result<RunEvent>> + Send>>;

/// Something a one-shot's caller needs kept alive until the container is gone.
///
/// Deliberately opaque — see [`DockerApi::run_one_shot`]. It exists because "the
/// stream ended" and "the container is gone" are different moments, and a resource
/// scoped to the wrong one is a bug the type system can otherwise not see.
pub type Keepalive = std::sync::Arc<dyn Send + Sync>;

/// Options for reading container logs.
#[derive(Debug, Clone, Copy)]
pub struct LogOptions {
    /// Keep the stream open and emit new lines as they arrive.
    pub follow: bool,
    /// How many historical lines to replay first.
    pub tail: usize,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            follow: true,
            tail: 500,
        }
    }
}

/// The container operations the panel needs. Deliberately small: the panel is a
/// control plane over config files, not a general Docker UI.
#[async_trait]
pub trait DockerApi: Send + Sync {
    /// Every container on the host, running or not. Filtering to Stitch bots is
    /// the inventory layer's job, because "is this a bot" depends on labels,
    /// image and mounts together.
    async fn list_all(&self) -> Result<Vec<ContainerInfo>>;

    /// Make sure `image` is on the host.
    ///
    /// When `refresh` is set, always ask the registry — used by Recreate so a
    /// mutable tag like `:latest` actually picks up a new release. Without it, a
    /// present local copy is enough: migrate and one-shots must keep the binary
    /// they already trust.
    ///
    /// Docker's create endpoint does not pull: it fails with `No such image`. So
    /// this has to be called before [`Self::create`] — and, on a path that
    /// replaces a container, *before* the old one is removed, so a registry that
    /// can't be reached leaves the bot standing instead of deleted.
    async fn ensure_image(&self, image: &str, refresh: bool) -> Result<()>;

    /// Pull `image` from the registry and require the pull to succeed.
    ///
    /// Unlike [`Self::ensure_image`] with `refresh: true`, a failed pull is
    /// always an error — never fall back to a pre-existing local copy. Panel
    /// self-update uses this so a GHCR outage after `/api/updates` reported a
    /// newer digest cannot arm a swap onto a stale cached `:latest`.
    async fn require_fresh_image(&self, image: &str) -> Result<()>;

    /// Repo digests (`repo@sha256:…`) for a local image, empty when it isn't on
    /// the host. Used to compare a running container against a registry tag
    /// without pulling on every status poll.
    async fn local_image_digests(&self, image: &str) -> Result<Vec<String>>;

    /// Schedule replacing a live container with `new_image`, preserving its
    /// create config (env, binds, network mode, restart policy).
    ///
    /// Used for panel self-update: the panel process lives inside the container
    /// being replaced, so the swap is armed here and finished by a short-lived
    /// helper after this method returns. Callers should answer the HTTP client
    /// before the helper stops them.
    ///
    /// `docker_socket` is the path the panel opens inside its own namespace
    /// (`STITCH_PANEL_DOCKER_SOCKET`). Implementations must resolve that to a
    /// *host* bind source via the panel container's mounts before creating the
    /// helper — a remap like `/var/run/docker.sock:/docker.sock` would otherwise
    /// mount a non-existent host path. Hardcoding `/var/run/docker.sock` would
    /// miss installs that set a custom socket path.
    async fn schedule_image_swap(
        &self,
        name: &str,
        new_image: &str,
        docker_socket: &std::path::Path,
    ) -> Result<()>;

    async fn create(&self, spec: &CreateSpec) -> Result<String>;

    async fn start(&self, name: &str) -> Result<()>;

    /// SIGTERM, then SIGKILL after the grace period. The bot handles SIGTERM
    /// between ticks, so this is a clean shutdown, not a kill.
    async fn stop(&self, name: &str, grace_secs: i64) -> Result<()>;

    async fn restart(&self, name: &str, grace_secs: i64) -> Result<()>;

    /// Remove the container. Does not touch the bot's config directory — that's
    /// a separate, explicitly confirmed step.
    async fn remove(&self, name: &str, force: bool) -> Result<()>;

    fn logs(&self, name: &str, opts: LogOptions) -> LogStream;

    /// Create, start, stream and then remove a throwaway container. Used for
    /// `stitch approve` and dry runs, so an operator can validate a config
    /// without putting orders on the book.
    ///
    /// `keepalive` is held until the container is actually gone — after a normal
    /// exit *and* its removal, or after the removal that follows an abandoned
    /// stream. The caller uses it for anything that must outlive the process rather
    /// than the connection watching it; the approve route parks its operator-wallet
    /// claim there, because a claim released when the browser disconnects would let
    /// a second approval start signing while the first is still broadcasting.
    ///
    /// Opaque on purpose: the Docker layer has no business knowing what it's
    /// holding, only that it drops it last.
    ///
    /// `hold_until_started` is the mirror image: held only until the container has
    /// *started*, then dropped. The approve route parks the config lock there, because
    /// the container loads its config from the mounted file at start — so the file must
    /// not be moved by a settings save between the caller's wallet claim and the start,
    /// or the container could sign from a wallet nothing claimed. Once started, the
    /// config is loaded and the lock can go.
    fn run_one_shot(
        &self,
        spec: CreateSpec,
        keepalive: Option<Keepalive>,
        hold_until_started: Option<Keepalive>,
    ) -> RunStream;
}

/// Hold `held` until `container` is confirmed stopped or gone, in a background task.
///
/// The safety valve for an *ambiguous* Docker response: a launch, restart, or live-change
/// whose start/restart returned an error the connection may have dropped *after* Docker
/// acted, so the container could be running its allowance preflight on a wallet the caller
/// claimed. Releasing that claim then would let a sibling collide on the pending nonce. So
/// the claim (whatever `held` is — a wallet claim, or a pair of them) is handed here and
/// kept alive until the container is terminal or absent, retrying the stop with backoff.
///
/// Keys off the container's liveness via `list_all`, not a successful `stop`: Docker
/// reports an already-stopped or removed container as an error, so an operator who cleans
/// up by hand still releases the claim. Indefinite by design — a daemon that never returns
/// keeps the wallet blocked, which is loud and safe over a silent nonce race.
pub fn hold_until_stopped<H: Send + 'static>(
    docker: std::sync::Arc<dyn DockerApi>,
    container: String,
    held: H,
) {
    tokio::spawn(async move {
        let _held = held;
        let mut backoff = std::time::Duration::from_secs(1);
        loop {
            if let Ok(containers) = docker.list_all().await {
                if !containers
                    .iter()
                    .any(|c| c.name == container && !c.state.is_terminal())
                {
                    tracing::info!(
                        "{container} is no longer live; releasing the claim held for it"
                    );
                    break;
                }
            }
            match docker.stop(&container, STOP_GRACE_SECS).await {
                Ok(()) => {
                    tracing::info!("stopped {container}; releasing the claim held for it");
                    break;
                }
                Err(e) => {
                    tracing::error!(
                        "still can't stop {container} to release a held claim, retrying in {backoff:?}: {e:#}"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_parsing_covers_dockers_vocabulary() {
        assert_eq!(ContainerState::parse("running"), ContainerState::Running);
        assert_eq!(ContainerState::parse("exited"), ContainerState::Exited);
        assert_eq!(ContainerState::parse("paused"), ContainerState::Paused);
        assert_eq!(ContainerState::parse("dead"), ContainerState::Dead);
    }

    #[test]
    fn an_unrecognised_state_is_never_reported_as_running() {
        // A future Docker state must not be optimistically treated as healthy.
        let s = ContainerState::parse("teleporting");
        assert_eq!(s, ContainerState::Unknown);
        assert!(!s.is_running());
    }

    #[test]
    fn only_running_counts_as_quoting() {
        assert!(ContainerState::Running.is_running());
        for s in [
            ContainerState::Paused,
            ContainerState::Restarting,
            ContainerState::Created,
            ContainerState::Exited,
            ContainerState::Dead,
        ] {
            assert!(!s.is_running(), "{s:?} must not count as running");
        }
    }

    #[test]
    fn only_terminal_states_are_safe_to_destroy_after_a_failed_stop() {
        for s in [
            ContainerState::Created,
            ContainerState::Exited,
            ContainerState::Dead,
        ] {
            assert!(s.is_terminal(), "{s:?}");
        }
        for s in [
            ContainerState::Running,
            ContainerState::Paused,
            ContainerState::Restarting,
            ContainerState::Removing,
            ContainerState::Unknown,
        ] {
            assert!(!s.is_terminal(), "{s:?} can still be mid-tick");
        }
    }

    #[test]
    fn bind_strings_match_the_compose_convention() {
        assert_eq!(
            BindSpec::rw("/host/bot-a", "/home/stitch/run")
                .to_bind_string()
                .unwrap(),
            "/host/bot-a:/home/stitch/run"
        );
        assert_eq!(
            BindSpec::ro("/host/bot-a/stitch.toml", "/home/stitch/run/stitch.toml")
                .to_bind_string()
                .unwrap(),
            "/host/bot-a/stitch.toml:/home/stitch/run/stitch.toml:ro"
        );
    }

    #[test]
    fn a_colon_in_a_path_is_refused_rather_than_mangled() {
        // Docker's bind syntax is colon-delimited with no escape, so a path
        // containing one would mount something other than what was asked for.
        let spec = BindSpec::rw("/host/bot:a", "/home/stitch/run");
        let err = spec.to_bind_string().unwrap_err();
        assert!(err.to_string().contains("colon"));
    }

    #[test]
    fn windows_drive_paths_cannot_use_short_bind_strings() {
        // Docker Desktop on Windows passes host paths like C:/Users/… into
        // STITCH_PANEL_HOST_BOTS_DIR. Short bind syntax can't express the
        // drive letter; callers must use long-form mounts / the Mount API.
        let spec = BindSpec::ro(
            "C:/Users/op/stitch-bots/bot-a/stitch.toml",
            "/home/stitch/run/stitch.toml",
        );
        assert!(spec.to_bind_string().is_err());
        let (host, container) = spec.path_strings().unwrap();
        assert_eq!(host, "C:/Users/op/stitch-bots/bot-a/stitch.toml");
        assert_eq!(container, "/home/stitch/run/stitch.toml");
    }

    #[test]
    fn helper_socket_bind_uses_the_host_source_of_a_remapped_mount() {
        let mounts = [MountInfo {
            source: PathBuf::from("/var/run/docker.sock"),
            destination: PathBuf::from("/docker.sock"),
            rw: true,
        }];
        assert_eq!(
            host_docker_socket_bind(&mounts, Path::new("/docker.sock")),
            PathBuf::from("/var/run/docker.sock")
        );
        // No matching mount (panel on the host): configured path is already host-side.
        assert_eq!(
            host_docker_socket_bind(&[], Path::new("/var/run/docker.sock")),
            PathBuf::from("/var/run/docker.sock")
        );
    }

    #[test]
    fn helper_socket_bind_resolves_sockets_under_a_directory_mount() {
        let mounts = [MountInfo {
            source: PathBuf::from("/var/run"),
            destination: PathBuf::from("/docker-run"),
            rw: true,
        }];
        assert_eq!(
            host_docker_socket_bind(&mounts, Path::new("/docker-run/docker.sock")),
            PathBuf::from("/var/run/docker.sock")
        );
        // Exact file mount still wins over a parent directory mount.
        let nested = [
            MountInfo {
                source: PathBuf::from("/var/run"),
                destination: PathBuf::from("/docker-run"),
                rw: true,
            },
            MountInfo {
                source: PathBuf::from("/run/docker.sock"),
                destination: PathBuf::from("/docker-run/docker.sock"),
                rw: true,
            },
        ];
        assert_eq!(
            host_docker_socket_bind(&nested, Path::new("/docker-run/docker.sock")),
            PathBuf::from("/run/docker.sock")
        );
    }

    #[test]
    fn default_log_options_follow_with_a_bounded_replay() {
        let opts = LogOptions::default();
        assert!(opts.follow);
        assert!(opts.tail > 0, "an unbounded replay would flood the client");
    }
}
