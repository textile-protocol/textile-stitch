// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! An in-memory [`DockerApi`] for tests.
//!
//! Every layer above the Docker client — inventory, the wizard, settings, the
//! compose export, layout migration — is exercised against this rather than a
//! real daemon, so the test suite runs anywhere and covers the failure paths
//! (name conflicts, missing containers) that are awkward to provoke for real.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Result};
use async_trait::async_trait;
use futures_util::{stream, StreamExt};

use super::{
    BindSpec, ContainerInfo, ContainerState, CreateSpec, DockerApi, Keepalive, LogLine, LogOptions,
    LogSource, LogStream, MountInfo, RunEvent, RunStream,
};

/// One recorded call, so tests can assert on ordering and grace periods rather
/// than only on end state. Restart-on-save, for instance, is only correct if the
/// stop happened after the config write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// The image pre-flight. `refresh` is recorded because Recreate must ask the
    /// registry even when the tag is already cached, and migrate/one-shots must not.
    EnsureImage {
        image: String,
        refresh: bool,
    },
    LocalImageDigests(String),
    ScheduleImageSwap {
        name: String,
        new_image: String,
        docker_socket: String,
    },
    Create(String),
    Start(String),
    Stop {
        name: String,
        grace_secs: i64,
    },
    Restart {
        name: String,
        grace_secs: i64,
    },
    Remove {
        name: String,
        force: bool,
    },
    /// The tail is recorded because clamping an absurd request is a real
    /// behaviour worth asserting, not just an internal constant.
    Logs {
        name: String,
        tail: usize,
    },
    OneShot {
        name: String,
        cmd: Vec<String>,
    },
    /// Reading files out of a container. Recorded alongside the lifecycle calls
    /// because *when* it happens is the whole point: a nonce ledger snapshot is
    /// only trustworthy once the bot has been stopped.
    ReadFiles {
        name: String,
        dir: String,
    },
}

/// A fake Docker daemon holding containers in memory.
#[derive(Default)]
pub struct FakeDocker {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    containers: Vec<ContainerInfo>,
    calls: Vec<Call>,
    /// Lines every `logs` call replays.
    log_lines: Vec<LogLine>,
    /// Exit code the next one-shot reports.
    one_shot_exit: i64,
    /// Specs the one-shots ran with, for asserting mounts and image.
    one_shots: Vec<CreateSpec>,
    /// Specs passed to `create`, for asserting env and mounts on the replacement
    /// a migration or recreate builds.
    creates: Vec<CreateSpec>,
    /// When set, the next mutating call fails with this message. Used to test
    /// that a failed restart is reported honestly rather than swallowed.
    fail_next: Option<String>,
    /// When set, the next `remove` fails with this message. Separate from
    /// `fail_next` because migrate stops before it removes, and a single armed
    /// failure would fire on the stop instead.
    fail_remove: Option<String>,
    /// When set, `run_one_shot` parks the caller's keepalive here instead of
    /// letting it go with the stream, so a test can inspect what is held while a
    /// run is still notionally in flight.
    hold_keepalive: bool,
    held_keepalive: Option<Keepalive>,
    /// When set, every `start` fails with this. Separate from `fail_next` for the
    /// same reason as `fail_remove`, and sticky rather than one-shot because the
    /// case worth testing is a daemon that has gone away mid-migration — the
    /// rollback's restart then fails too, and the operator has to be told.
    start_error: Option<String>,
    /// When set, every `stop` fails with this. Sticky, so a settle after an ambiguous
    /// start can't confirm the container is gone — the case where the wallet claim has
    /// to be held rather than released.
    stop_error: Option<String>,
    /// Files `read_dir` serves, as if they were inside a container.
    container_files: Vec<(String, Vec<u8>)>,
    /// When set, every `ensure_image` fails with this — an unreachable registry
    /// or an image that doesn't exist. Sticky rather than one-shot, because the
    /// point of these tests is that nothing downstream of it runs at all.
    image_error: Option<String>,
    /// Allow this many `list_all` calls, then fail every one after — a daemon that
    /// goes unreachable partway through a handler. A count rather than a flag because
    /// a handler that re-reads the fleet after acting needs its first read to succeed
    /// and a later one to fail. `None` never fails.
    list_calls_left: Option<usize>,
    /// Digests `local_image_digests` returns for a given image reference.
    image_digests: HashMap<String, Vec<String>>,
    /// When set, `schedule_image_swap` fails with this message.
    swap_error: Option<String>,
}

impl FakeDocker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a container as if it already existed on the host.
    pub fn with_container(self, info: ContainerInfo) -> Self {
        self.add_container(info);
        self
    }

    /// Seed a container after the fake is already shared behind an `Arc`, which is
    /// how the HTTP tests set up a fleet.
    pub fn add_container(&self, info: ContainerInfo) {
        self.state.lock().unwrap().containers.push(info);
    }

    /// Seed the lines that `logs` will replay.
    pub fn with_log_lines(self, lines: Vec<LogLine>) -> Self {
        self.set_log_lines(lines);
        self
    }

    pub fn set_log_lines(&self, lines: Vec<LogLine>) {
        self.state.lock().unwrap().log_lines = lines;
    }

    /// Point a seeded container at another image, as a pinned or forked bot would
    /// report.
    pub fn set_container_image(&self, name: &str, image: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(c) = st.containers.iter_mut().find(|c| c.name == name) {
            c.image = image.to_string();
        }
    }

    /// Seed the files `read_dir` reports from inside a container.
    pub fn set_container_files(&self, files: Vec<(String, Vec<u8>)>) {
        self.state.lock().unwrap().container_files = files;
    }

    /// Make the next mutating call fail.
    pub fn fail_next(&self, message: &str) {
        self.state.lock().unwrap().fail_next = Some(message.to_string());
    }

    /// Arm a failure on the next `remove` only.
    pub fn fail_remove(&self, message: &str) {
        self.state.lock().unwrap().fail_remove = Some(message.to_string());
    }

    /// Make every `start` fail, as a daemon that died mid-migration would.
    pub fn fail_start(&self, message: &str) {
        self.state.lock().unwrap().start_error = Some(message.to_string());
    }

    /// Make every `stop` fail, so a settle can't confirm a container is gone.
    pub fn fail_stop(&self, message: &str) {
        self.state.lock().unwrap().stop_error = Some(message.to_string());
    }

    /// Make the image pre-flight fail, as an unreachable registry would.
    pub fn fail_image(&self, message: &str) {
        self.state.lock().unwrap().image_error = Some(message.to_string());
    }

    /// Seed the digests a local image reports, for update-detection tests.
    pub fn set_image_digests(&self, image: &str, digests: Vec<String>) {
        self.state
            .lock()
            .unwrap()
            .image_digests
            .insert(image.to_string(), digests);
    }

    /// Make `schedule_image_swap` fail.
    pub fn fail_swap(&self, message: &str) {
        self.state.lock().unwrap().swap_error = Some(message.to_string());
    }

    /// Let the next `allowed` calls to `list_all` succeed, then fail every one after.
    /// Stands in for the daemon going unreachable partway through a handler.
    pub fn fail_list_after(&self, allowed: usize) {
        self.state.lock().unwrap().list_calls_left = Some(allowed);
    }

    /// Park the next one-shot's keepalive instead of letting it go when the stream
    /// ends, standing in for a container that is still running.
    pub fn hold_one_shot_keepalive(&self) {
        self.state.lock().unwrap().hold_keepalive = true;
    }

    /// Let a parked keepalive go, standing in for the container finally being
    /// removed.
    pub fn release_one_shot_keepalive(&self) {
        let mut st = self.state.lock().unwrap();
        st.hold_keepalive = false;
        st.held_keepalive = None;
    }

    /// Exit code the next one-shot run reports.
    pub fn set_one_shot_exit(&self, code: i64) {
        self.state.lock().unwrap().one_shot_exit = code;
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Full specs of the one-shots that ran, in order.
    pub fn one_shot_specs(&self) -> Vec<CreateSpec> {
        self.state.lock().unwrap().one_shots.clone()
    }

    /// Specs every successful `create` was given, in order.
    pub fn create_specs(&self) -> Vec<CreateSpec> {
        self.state.lock().unwrap().creates.clone()
    }

    /// The current state of a container by name, if it exists.
    pub fn state_of(&self, name: &str) -> Option<ContainerState> {
        self.state
            .lock()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.state)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.state_of(name).is_some()
    }

    /// Take the armed failure, if any.
    fn check_failure(st: &mut FakeState) -> Result<()> {
        match st.fail_next.take() {
            Some(msg) => bail!(msg),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl DockerApi for FakeDocker {
    async fn list_all(&self) -> Result<Vec<ContainerInfo>> {
        let mut st = self.state.lock().unwrap();
        if let Some(left) = st.list_calls_left {
            if left == 0 {
                bail!("the Docker daemon is not reachable");
            }
            st.list_calls_left = Some(left - 1);
        }
        Ok(st.containers.clone())
    }

    async fn ensure_image(&self, image: &str, refresh: bool) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::EnsureImage {
            image: image.to_string(),
            refresh,
        });
        match st.image_error.clone() {
            Some(msg) => bail!(msg),
            None => Ok(()),
        }
    }

    async fn require_fresh_image(&self, image: &str) -> Result<()> {
        // Same failure surface as a hard pull — the fake has no local-fallback
        // path to model separately.
        self.ensure_image(image, true).await
    }

    async fn local_image_digests(&self, image: &str) -> Result<Vec<String>> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::LocalImageDigests(image.to_string()));
        Ok(st.image_digests.get(image).cloned().unwrap_or_default())
    }

    async fn schedule_image_swap(
        &self,
        name: &str,
        new_image: &str,
        docker_socket: &std::path::Path,
    ) -> Result<()> {
        // Mirror production: strict pull before arming the swap.
        self.require_fresh_image(new_image).await?;
        let mut st = self.state.lock().unwrap();
        let host_socket = st
            .containers
            .iter()
            .find(|c| c.name == name)
            .map(|c| crate::panel::docker::host_docker_socket_bind(&c.mounts, docker_socket))
            .unwrap_or_else(|| docker_socket.to_path_buf());
        st.calls.push(Call::ScheduleImageSwap {
            name: name.to_string(),
            new_image: new_image.to_string(),
            docker_socket: host_socket.display().to_string(),
        });
        if let Some(msg) = st.swap_error.clone() {
            bail!(msg);
        }
        // Stand in for a successful swap: the container is now on the new image.
        if let Some(c) = st.containers.iter_mut().find(|c| c.name == name) {
            c.image = new_image.to_string();
            c.image_id = format!("sha256:swapped-{new_image}");
        } else {
            bail!("No such container: {name}");
        }
        Ok(())
    }

    async fn create(&self, spec: &CreateSpec) -> Result<String> {
        let mut st = self.state.lock().unwrap();
        Self::check_failure(&mut st)?;
        if st.containers.iter().any(|c| c.name == spec.name) {
            // Same error shape the daemon produces, so callers that map it to a
            // friendly "that name is taken" are exercised.
            bail!(
                "Conflict. The container name \"/{}\" is already in use",
                spec.name
            );
        }
        st.calls.push(Call::Create(spec.name.clone()));
        st.creates.push(spec.clone());
        let id = format!("fake-{}", spec.name);
        st.containers.push(ContainerInfo {
            id: id.clone(),
            name: spec.name.clone(),
            image: spec.image.clone(),
            image_id: format!("sha256:fake-{}", spec.name),
            state: ContainerState::Created,
            status: "Created".to_string(),
            created_unix: 0,
            labels: spec.labels.clone(),
            mounts: spec.binds.iter().map(bind_to_mount).collect(),
        });
        Ok(id)
    }

    async fn start(&self, name: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        Self::check_failure(&mut st)?;
        st.calls.push(Call::Start(name.to_string()));
        if let Some(msg) = st.start_error.clone() {
            bail!(msg);
        }
        set_state(&mut st, name, ContainerState::Running, "Up 1 second")
    }

    async fn stop(&self, name: &str, grace_secs: i64) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        Self::check_failure(&mut st)?;
        st.calls.push(Call::Stop {
            name: name.to_string(),
            grace_secs,
        });
        if let Some(msg) = st.stop_error.clone() {
            bail!(msg);
        }
        set_state(&mut st, name, ContainerState::Exited, "Exited (0)")
    }

    async fn restart(&self, name: &str, grace_secs: i64) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        Self::check_failure(&mut st)?;
        st.calls.push(Call::Restart {
            name: name.to_string(),
            grace_secs,
        });
        set_state(&mut st, name, ContainerState::Running, "Up 1 second")
    }

    async fn remove(&self, name: &str, force: bool) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        Self::check_failure(&mut st)?;
        if let Some(msg) = st.fail_remove.take() {
            bail!(msg);
        }
        st.calls.push(Call::Remove {
            name: name.to_string(),
            force,
        });
        let before = st.containers.len();
        st.containers.retain(|c| c.name != name);
        if st.containers.len() == before {
            bail!("No such container: {name}");
        }
        Ok(())
    }

    fn logs(&self, name: &str, opts: LogOptions) -> LogStream {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::Logs {
            name: name.to_string(),
            tail: opts.tail,
        });
        let lines = st.log_lines.clone();
        Box::pin(stream::iter(lines.into_iter().map(Ok)))
    }

    fn run_one_shot(
        &self,
        spec: CreateSpec,
        keepalive: Option<Keepalive>,
        hold_until_started: Option<Keepalive>,
    ) -> RunStream {
        // The fake starts the container synchronously, so the caller's "hold until
        // started" claim is released the moment this returns — it isn't captured into
        // the stream the way `keepalive` is.
        drop(hold_until_started);
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::OneShot {
            name: spec.name.clone(),
            cmd: spec.cmd.clone().unwrap_or_default(),
        });
        // Kept whole, separately from the call log: what a one-shot mounts and
        // which image it runs are as much of its behaviour as its command, and
        // the call log is compared exhaustively elsewhere.
        st.one_shots.push(spec.clone());
        let code = st.one_shot_exit;
        let lines = st.log_lines.clone();
        // Held for as long as the caller can hold this fake, so a test can assert
        // that a claim is still taken while a run is "in flight" and released once
        // the stream is dropped. The real one drops it after the container is
        // removed; here the stream ending is the closest analogue.
        if st.hold_keepalive {
            st.held_keepalive = keepalive.clone();
        }
        Box::pin(
            stream::iter(
                lines
                    .into_iter()
                    .map(|l| Ok(RunEvent::Line(l)))
                    .chain(std::iter::once(Ok(RunEvent::Exited { code }))),
            )
            .map(move |item| {
                let _holding = &keepalive;
                item
            }),
        )
    }
}

#[async_trait]
impl crate::panel::migrate::ContainerFiles for FakeDocker {
    async fn read_dir(&self, name: &str, dir: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(Call::ReadFiles {
            name: name.to_string(),
            dir: dir.to_string(),
        });
        if !st.containers.iter().any(|c| c.name == name) {
            bail!("No such container: {name}");
        }
        Ok(st.container_files.clone())
    }
}

fn set_state(st: &mut FakeState, name: &str, state: ContainerState, status: &str) -> Result<()> {
    match st.containers.iter_mut().find(|c| c.name == name) {
        Some(c) => {
            c.state = state;
            c.status = status.to_string();
            Ok(())
        }
        None => bail!("No such container: {name}"),
    }
}

fn bind_to_mount(b: &BindSpec) -> MountInfo {
    MountInfo {
        source: b.host_path.clone(),
        destination: b.container_path.clone(),
        rw: !b.read_only,
    }
}

/// Build a `ContainerInfo` for tests without spelling out every field.
pub fn container(name: &str, state: ContainerState) -> ContainerInfo {
    ContainerInfo {
        id: format!("id-{name}"),
        name: name.to_string(),
        image: "ghcr.io/textile-protocol/textile-stitch:latest".to_string(),
        image_id: format!("sha256:id-{name}"),
        state,
        status: state.as_str().to_string(),
        created_unix: 1_700_000_000,
        labels: HashMap::new(),
        mounts: Vec::new(),
    }
}

/// A stdout log line, for seeding the fake.
pub fn out(text: &str) -> LogLine {
    LogLine {
        source: LogSource::Stdout,
        text: text.to_string(),
    }
}

/// The per-bot directory layout: the config dir mounted read-write with the
/// config and key re-mounted read-only on top. This is what the panel creates
/// and what the production compose file uses.
pub fn dir_layout_mounts(host_dir: &str) -> Vec<MountInfo> {
    vec![
        MountInfo {
            source: PathBuf::from(host_dir),
            destination: PathBuf::from("/home/stitch/run"),
            rw: true,
        },
        MountInfo {
            source: PathBuf::from(format!("{host_dir}/stitch.toml")),
            destination: PathBuf::from("/home/stitch/run/stitch.toml"),
            rw: false,
        },
        MountInfo {
            source: PathBuf::from(format!("{host_dir}/stitch.key")),
            destination: PathBuf::from("/home/stitch/run/stitch.key"),
            rw: false,
        },
    ]
}

/// The flat-file layout from `docker-compose.example.yml`: only the two files are
/// mounted, so `/home/stitch/run` is container-local and the slot-nonce ledger
/// written beside the config is lost whenever the container is recreated.
pub fn flat_layout_mounts(host_dir: &str, bot: &str) -> Vec<MountInfo> {
    vec![
        MountInfo {
            source: PathBuf::from(format!("{host_dir}/stitch.{bot}.toml")),
            destination: PathBuf::from("/home/stitch/run/stitch.toml"),
            rw: false,
        },
        MountInfo {
            source: PathBuf::from(format!("{host_dir}/stitch.{bot}.key")),
            destination: PathBuf::from("/home/stitch/run/stitch.key"),
            rw: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn spec(name: &str) -> CreateSpec {
        CreateSpec {
            name: name.to_string(),
            image: "img".to_string(),
            labels: HashMap::new(),
            env: Vec::new(),
            binds: Vec::new(),
            cmd: None,
            restart_unless_stopped: true,
        }
    }

    #[tokio::test]
    async fn create_start_stop_moves_through_the_expected_states() {
        let d = FakeDocker::new();
        d.create(&spec("stitch-bot-a")).await.unwrap();
        assert_eq!(d.state_of("stitch-bot-a"), Some(ContainerState::Created));
        d.start("stitch-bot-a").await.unwrap();
        assert_eq!(d.state_of("stitch-bot-a"), Some(ContainerState::Running));
        d.stop("stitch-bot-a", 30).await.unwrap();
        assert_eq!(d.state_of("stitch-bot-a"), Some(ContainerState::Exited));
        assert_eq!(
            d.calls(),
            vec![
                Call::Create("stitch-bot-a".into()),
                Call::Start("stitch-bot-a".into()),
                Call::Stop {
                    name: "stitch-bot-a".into(),
                    grace_secs: 30
                },
            ]
        );
    }

    #[tokio::test]
    async fn creating_a_duplicate_name_conflicts_like_the_daemon() {
        let d = FakeDocker::new();
        d.create(&spec("stitch-bot-a")).await.unwrap();
        let err = d.create(&spec("stitch-bot-a")).await.unwrap_err();
        assert!(err.to_string().contains("already in use"));
    }

    #[tokio::test]
    async fn acting_on_a_missing_container_errors() {
        let d = FakeDocker::new();
        assert!(d.start("nope").await.is_err());
        assert!(d.stop("nope", 30).await.is_err());
        assert!(d.remove("nope", false).await.is_err());
    }

    #[tokio::test]
    async fn an_armed_failure_fires_once_then_clears() {
        let d = FakeDocker::new();
        d.create(&spec("stitch-bot-a")).await.unwrap();
        d.fail_next("daemon is busy");
        let err = d.restart("stitch-bot-a", 30).await.unwrap_err();
        assert!(err.to_string().contains("daemon is busy"));
        // The next call succeeds, so a test can assert recovery.
        d.restart("stitch-bot-a", 30).await.unwrap();
    }

    #[tokio::test]
    async fn one_shot_streams_lines_then_the_exit_code() {
        let d = FakeDocker::new().with_log_lines(vec![out("approving"), out("done")]);
        d.set_one_shot_exit(3);
        let events: Vec<_> = d
            .run_one_shot(spec("stitch-approve-bot-a"), None, None)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], RunEvent::Line(_)));
        assert_eq!(events[2], RunEvent::Exited { code: 3 });
    }

    #[tokio::test]
    async fn created_containers_record_their_binds_as_mounts() {
        // Inventory reads mounts to find a bot's config dir, so the fake has to
        // reflect binds back the way the daemon does.
        let d = FakeDocker::new();
        let mut s = spec("stitch-bot-a");
        s.binds = vec![
            BindSpec::rw("/host/bot-a", "/home/stitch/run"),
            BindSpec::ro("/host/bot-a/stitch.toml", "/home/stitch/run/stitch.toml"),
        ];
        d.create(&s).await.unwrap();
        let listed = d.list_all().await.unwrap();
        let mounts = &listed[0].mounts;
        assert_eq!(mounts.len(), 2);
        assert!(mounts[0].rw, "the dir mount must be writable");
        assert!(!mounts[1].rw, "the config mount must be read-only");
    }
}
