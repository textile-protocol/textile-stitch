// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The real [`DockerApi`]: bollard over the local Docker unix socket.
//!
//! This is the only file in the panel that knows bollard exists. It translates
//! between the daemon's wire types and the panel's domain types, and does not
//! make policy decisions — those live in the inventory and API layers, which are
//! testable against the fake.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, ContainerSummary, HostConfig, HostConfigLogConfig, MountPoint,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder,
    ListContainersOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RestartContainerOptionsBuilder, StopContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use bollard::Docker;
use futures_util::{stream, StreamExt};

use super::{
    ContainerInfo, ContainerState, CreateSpec, DockerApi, Keepalive, LogLine, LogOptions,
    LogSource, LogStream, MountInfo, RunEvent, RunStream, STOP_GRACE_SECS,
};

/// Connection timeout for daemon calls, in seconds. Generous because pulling an
/// image on first bot creation goes through the same socket.
const TIMEOUT_SECS: u64 = 120;

/// How many bytes of a single log line we keep. Docker frames arbitrarily, and a
/// bot that logs a huge payload should not be able to exhaust the panel's memory
/// through an SSE stream.
const MAX_LOG_LINE: usize = 16 * 1024;

/// Log rotation for bot containers, matching the shipped compose files so an
/// adopted fleet and a panel-created one behave identically on disk.
const LOG_MAX_SIZE: &str = "10m";
const LOG_MAX_FILE: &str = "5";

/// Cap on a container archive read into memory. The run directory holds a config,
/// a key and a small JSON ledger; anything approaching this means the operator put
/// something unexpected there, and streaming it into the panel's heap is not the
/// right response.
const MAX_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;

pub struct BollardDocker {
    docker: Docker,
}

impl BollardDocker {
    /// Connect to the daemon over a unix socket.
    pub fn connect(socket: &Path) -> Result<Self> {
        let path = socket
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("docker socket path is not valid UTF-8"))?;
        // Docker's API is backwards compatible; bollard's default version is
        // negotiated down by the daemon if it's older.
        let docker = Docker::connect_with_unix(path, TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
            .with_context(|| format!("connecting to the Docker socket at {path}"))?;
        Ok(Self { docker })
    }

    /// Confirm the daemon is actually reachable, so a bad socket mount is a
    /// startup error with a clear message rather than a broken fleet list.
    pub async fn ping(&self) -> Result<()> {
        self.docker
            .ping()
            .await
            .context("the Docker daemon did not respond")?;
        Ok(())
    }

    /// Pull an image and confirm it actually landed.
    ///
    /// Separate from [`DockerApi::ensure_image`] so the fast path and the
    /// fall-back-to-local decision live there and this only does the network work.
    async fn pull_image(&self, image: &str) -> Result<()> {
        let (name, tag) = split_image_ref(image);
        let mut options = CreateImageOptionsBuilder::default().from_image(name);
        if let Some(tag) = tag {
            options = options.tag(tag);
        }

        let mut progress = self.docker.create_image(Some(options.build()), None, None);
        while let Some(update) = progress.next().await {
            update.with_context(|| {
                format!(
                    "pulling {image}. If it lives in a private registry, run \
                     `docker pull {image}` on the host first so the daemon has \
                     credentials for it."
                )
            })?;
        }

        // The pull stream can finish without the image landing (a manifest for
        // another platform, say). Confirm rather than assume, so the caller isn't
        // told to expect an image that create will then reject.
        self.docker
            .inspect_image(image)
            .await
            .with_context(|| format!("{image} is still not present after pulling it"))?;
        Ok(())
    }
}

#[async_trait]
impl DockerApi for BollardDocker {
    async fn list_all(&self) -> Result<Vec<ContainerInfo>> {
        let options = ListContainersOptionsBuilder::default().all(true).build();
        let summaries = self
            .docker
            .list_containers(Some(options))
            .await
            .context("listing containers")?;
        Ok(summaries.iter().map(to_container_info).collect())
    }

    async fn ensure_image(&self, image: &str, refresh: bool) -> Result<()> {
        // Present already and nobody asked for a refresh: don't touch the network.
        // A host that pulled once keeps working offline, and a pinned `sha-*` tag
        // can't have changed anyway. Recreate passes `refresh` so a mutable tag
        // like `:latest` actually picks up a new release.
        let present = self.docker.inspect_image(image).await.is_ok();
        if present && !refresh {
            return Ok(());
        }

        match self.pull_image(image).await {
            Ok(()) => Ok(()),
            // A refresh that can't pull but already has the image locally runs the
            // local copy. Recreate sets `refresh` so a mutable tag picks up a release,
            // but an operator may have pulled a private image by hand with credentials
            // the daemon can't supply on its own unauthenticated pull — the suggested
            // `docker pull` workaround can't help, because the next attempt just repeats
            // that same anonymous pull. Failing here would strand a bot on an image
            // that is right there on the host, so warn and use it. With nothing local
            // there is nothing to fall back to, so the pull error stands.
            //
            // Panel self-update must NOT use this fallback — see
            // [`DockerApi::require_fresh_image`].
            Err(e) if present => {
                tracing::warn!(
                    "couldn't refresh {image}, using the copy already on the host: {e:#}"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn require_fresh_image(&self, image: &str) -> Result<()> {
        self.pull_image(image).await.with_context(|| {
            format!("pulling {image} — refusing to continue on a possibly stale local copy")
        })
    }

    async fn local_image_digests(&self, image: &str) -> Result<Vec<String>> {
        match self.docker.inspect_image(image).await {
            Ok(info) => Ok(info.repo_digests.unwrap_or_default()),
            // Not on the host yet — an empty list means "nothing to match against",
            // which the update check treats as unknown rather than "behind".
            Err(_) => Ok(Vec::new()),
        }
    }

    async fn schedule_image_swap(
        &self,
        name: &str,
        new_image: &str,
        docker_socket: &Path,
    ) -> Result<()> {
        // Strict pull — never fall back to a cached local tag. ensure_image's
        // refresh path tolerates pull failure when a local copy exists (bot
        // Recreate), but self-update must not restart the panel onto a stale
        // `:latest` after GHCR/auth/rate-limit failures.
        self.require_fresh_image(new_image).await?;

        let inspect = self
            .docker
            .inspect_container(name, None)
            .await
            .with_context(|| format!("inspecting {name} for a self-update"))?;
        let config = inspect
            .config
            .context("the container has no Config — cannot clone it for an update")?;
        let host_config = inspect.host_config.unwrap_or_default();

        let next = format!("{name}-next");
        // A leftover from a previous failed swap must not block this one.
        let _ = self
            .docker
            .remove_container(
                &next,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await;

        let body = ContainerCreateBody {
            image: Some(new_image.to_string()),
            env: config.env.clone(),
            cmd: config.cmd.clone(),
            entrypoint: config.entrypoint.clone(),
            working_dir: config.working_dir.clone(),
            user: config.user.clone(),
            labels: config.labels.clone(),
            // HostConfig.port_bindings alone is not enough — Docker only publishes
            // a host port when the create Config also exposes the container port.
            // The password-only install snippet binds 127.0.0.1:8420:8420; dropping
            // exposed_ports here would leave the UI unreachable after self-update.
            exposed_ports: config.exposed_ports.clone(),
            host_config: Some(host_config),
            stop_timeout: config.stop_timeout.or(Some(STOP_GRACE_SECS)),
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::default().name(&next).build();
        self.docker
            .create_container(Some(options), body)
            .await
            .with_context(|| format!("creating the replacement container {next}"))?;

        // The panel process lives inside `name`. Stopping it from here would kill
        // the task mid-swap, so a short-lived helper on the docker socket finishes
        // the rename after we return. CreateContainer bind sources are host paths:
        // resolve STITCH_PANEL_DOCKER_SOCKET (in-container) through this container's
        // mounts to the host source, then mount that onto the helper's default path.
        let mounts: Vec<crate::panel::docker::MountInfo> = inspect
            .mounts
            .as_ref()
            .map(|m| m.iter().map(to_mount_info).collect())
            .unwrap_or_default();
        let host_socket = crate::panel::docker::host_docker_socket_bind(&mounts, docker_socket);
        let socket = host_socket
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("docker socket path is not valid UTF-8"))?;
        anyhow::ensure!(
            !socket.contains(':'),
            "docker socket path {socket} contains a colon, which Docker cannot express in a bind mount"
        );
        const HELPER_IMAGE: &str = "docker:27-cli";
        self.ensure_image(HELPER_IMAGE, false).await?;
        let script = format!(
            "sleep 2 && docker stop -t 30 {name} && docker rm -f {name} && \
             docker rename {next} {name} && docker start {name}"
        );
        let helper_name = format!("{name}-updater");
        let _ = self
            .docker
            .remove_container(
                &helper_name,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await;
        let helper_body = ContainerCreateBody {
            image: Some(HELPER_IMAGE.into()),
            cmd: Some(vec!["sh".into(), "-c".into(), script]),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{socket}:/var/run/docker.sock")]),
                auto_remove: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let helper_opts = CreateContainerOptionsBuilder::default()
            .name(&helper_name)
            .build();
        let helper = self
            .docker
            .create_container(Some(helper_opts), helper_body)
            .await
            .context("creating the panel self-update helper")?;
        self.docker
            .start_container(&helper.id, None)
            .await
            .context("starting the panel self-update helper")?;
        tracing::info!(
            container = %name,
            new_image,
            "armed panel image swap; helper will recreate the container shortly"
        );
        Ok(())
    }

    async fn create(&self, spec: &CreateSpec) -> Result<String> {
        let binds = spec
            .binds
            .iter()
            .map(|b| b.to_bind_string())
            .collect::<Result<Vec<_>>>()?;

        let host_config = HostConfig {
            binds: Some(binds),
            restart_policy: spec.restart_unless_stopped.then_some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".to_string()),
                config: Some(HashMap::from([
                    ("max-size".to_string(), LOG_MAX_SIZE.to_string()),
                    ("max-file".to_string(), LOG_MAX_FILE.to_string()),
                ])),
            }),
            ..Default::default()
        };

        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            cmd: spec.cmd.clone(),
            env: Some(spec.env.clone()),
            labels: Some(spec.labels.clone()),
            host_config: Some(host_config),
            // Baked into the container, not just passed on the stops the panel
            // itself issues: a `docker stop` from the shell, or the daemon going
            // down with the host, otherwise falls back to Docker's 10s default and
            // kills the bot mid-tick.
            stop_timeout: Some(STOP_GRACE_SECS),
            ..Default::default()
        };

        let options = CreateContainerOptionsBuilder::default()
            .name(&spec.name)
            .build();
        let created = self
            .docker
            .create_container(Some(options), body)
            .await
            .with_context(|| format!("creating container {}", spec.name))?;
        Ok(created.id)
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.docker
            .start_container(name, None)
            .await
            .with_context(|| format!("starting {name}"))
    }

    async fn stop(&self, name: &str, grace_secs: i64) -> Result<()> {
        let options = StopContainerOptionsBuilder::default()
            .t(grace_secs as i32)
            .build();
        self.docker
            .stop_container(name, Some(options))
            .await
            .with_context(|| format!("stopping {name}"))
    }

    async fn restart(&self, name: &str, grace_secs: i64) -> Result<()> {
        let options = RestartContainerOptionsBuilder::default()
            .t(grace_secs as i32)
            .build();
        self.docker
            .restart_container(name, Some(options))
            .await
            .with_context(|| format!("restarting {name}"))
    }

    async fn remove(&self, name: &str, force: bool) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::default()
            .force(force)
            .build();
        self.docker
            .remove_container(name, Some(options))
            .await
            .with_context(|| format!("removing {name}"))
    }

    fn logs(&self, name: &str, opts: LogOptions) -> LogStream {
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .follow(opts.follow)
            .tail(&opts.tail.to_string())
            .build();
        let raw = self.docker.logs(name, Some(options));
        let name = name.to_string();
        Box::pin(raw.map(move |item| {
            item.map(to_log_line)
                .with_context(|| format!("reading logs for {name}"))
        }))
    }

    fn run_one_shot(
        &self,
        spec: CreateSpec,
        keepalive: Option<Keepalive>,
        hold_until_started: Option<Keepalive>,
    ) -> RunStream {
        // The daemon handle wraps a connection pool and is cheap to clone, so
        // the returned stream can own one and outlive this call.
        let this = Self {
            docker: self.docker.clone(),
        };
        Box::pin(
            stream::once(async move { this.one_shot(spec, keepalive, hold_until_started).await })
                .flat_map(|result| {
                    match result {
                        Ok(s) => s,
                        // Setup failed before any output: surface the reason as the single
                        // item so the caller sees it instead of an empty stream.
                        Err(e) => Box::pin(stream::once(async move { Err(e) })) as RunStream,
                    }
                }),
        )
    }
}

impl BollardDocker {
    /// Create and start a throwaway container, returning a stream of its output
    /// terminated by its exit code. The container is reaped once it exits.
    async fn one_shot(
        &self,
        spec: CreateSpec,
        keepalive: Option<Keepalive>,
        hold_until_started: Option<Keepalive>,
    ) -> Result<RunStream> {
        let name = spec.name.clone();
        // An approve or dry run can be the first thing an operator does on a host,
        // before any bot container exists, so the image may not be cached yet.
        self.ensure_image(&spec.image, false).await?;
        // A leftover container from an interrupted previous run would make create
        // fail with a name conflict, so clear it first. Best-effort: "not found"
        // is the normal case.
        let _ = self.remove(&name, true).await;

        self.create(&spec).await?;
        // Armed the instant the container exists, before anything that can fail or
        // be cancelled. A container that was created but never started is invisible
        // in the fleet and never exits on its own, so it has to be covered from
        // here, not from after the start.
        // The keepalive rides along inside the guard, not inside the stream: the
        // stream ends when the browser stops listening, which is earlier than the
        // container going away. Whichever path reaps it drops the keepalive after
        // the removal, so a caller holding a resource against "this container is
        // still running" gets exactly that.
        // The config hold rides in the guard too, not as a bare local: if the start below
        // is cancelled (the SSE request is dropped) after Docker has actually started the
        // container, dropping the local would release the config lock while the container
        // is live and about to load its config — a save could move it in between. In the
        // guard, the reap path releases it only once the container is confirmed gone.
        let guard = Arc::new(ReapOnDrop {
            docker: self.docker.clone(),
            name: Mutex::new(Some(name.clone())),
            keepalive: Mutex::new(keepalive),
            config_hold: Mutex::new(hold_until_started),
        });

        // Attach to logs before starting, so nothing emitted between create and
        // start is missed.
        let logs = self.logs(
            &name,
            LogOptions {
                follow: true,
                tail: 0,
            },
        );
        // A failure here returns through `?`, which drops the guard and takes the
        // created container with it.
        self.start(&name).await?;
        // Started: the container has loaded its config from the mounted file, so a save
        // that moves it now can't affect this run. Release the config lock the caller
        // parked here — holding it past the start would block saves for the whole run.
        guard.release_config();

        let docker = self.docker.clone();
        // Both halves of the stream hold the guard, so the container is reaped
        // whichever way the run ends: normally through the exit event below, or by
        // the guard's `Drop` when the browser closes the SSE connection.
        let exit_guard = Arc::clone(&guard);

        // The exit event resolves the status and reaps the container before
        // yielding, so the stream ends with exactly one Exited and leaves nothing
        // behind on the host.
        let exit = stream::once(async move {
            let code = wait_for_exit(&docker, &name).await?;
            // Reaping is untidy to fail but must not turn a completed run into a
            // reported failure — the operator already has the output and the code.
            //
            // Disarm only when the container is actually gone. A failure leaves it up,
            // so `Drop` still owes it another attempt — and until one succeeds the
            // keepalive stays held rather than freeing an operator's wallet while the
            // process it guards is still running.
            if let Reaped::Gone = reap_once(&docker, &name).await {
                exit_guard.disarm();
            }
            Ok(RunEvent::Exited { code })
        });

        Ok(Box::pin(
            logs.map(move |l| {
                // Keeps the guard alive for as long as anyone is reading.
                let _ = &guard;
                l.map(RunEvent::Line)
            })
            .chain(exit),
        ))
    }
}

/// How long to wait before the first reap retry, and the ceiling the backoff grows
/// to. Generous because the failures this rides out — a daemon restarting, a host
/// under load — resolve on the scale of seconds to minutes, not milliseconds, and a
/// tight loop would just spin.
const REAP_RETRY_MIN: Duration = Duration::from_secs(1);
const REAP_RETRY_MAX: Duration = Duration::from_secs(30);

/// The outcome of one attempt to reap a one-shot container.
enum Reaped {
    /// Removed, or already gone. Either way nothing is running under that name.
    Gone,
    /// The daemon refused or was unreachable. The container may still be up.
    Failed(bollard::errors::Error),
}

/// Force-remove a one-shot container once, classifying the result.
///
/// A 404 is [`Reaped::Gone`], not a failure: the container is already gone, whether a
/// previous attempt removed it before the transport error or an operator did it by
/// hand. Treating it as success is what lets a manual cleanup release a held wallet.
async fn reap_once(docker: &Docker, name: &str) -> Reaped {
    let options = RemoveContainerOptionsBuilder::default().force(true).build();
    match docker.remove_container(name, Some(options)).await {
        Ok(()) => Reaped::Gone,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Reaped::Gone,
        Err(e) => Reaped::Failed(e),
    }
}

/// Force-removes a one-shot container if nobody is left listening to its output.
///
/// The normal path reaps the container when it exits. This covers the other one:
/// the operator navigates away or reloads, axum drops the SSE response, and the
/// run is dropped mid-flight. Without this, every abandoned click leaves a live
/// container behind — and `stitch --dry-run` never exits on its own, so an
/// abandoned dry run would keep polling the RPC until someone noticed.
struct ReapOnDrop {
    docker: Docker,
    /// `None` once the run reaped itself, which makes [`Drop`] a no-op.
    name: Mutex<Option<String>>,
    /// Whatever the caller needs held until the container is gone. Released by
    /// [`Self::disarm`] on the normal path, or by the reap task in [`Drop`] — in
    /// both cases only once a removal has succeeded.
    keepalive: Mutex<Option<Keepalive>>,
    /// Held only until the container has *started* (and so loaded its config), then
    /// released by [`Self::release_config`] — the approve route parks the config lock
    /// here so a save can't move the mounted config before the container reads it. If
    /// the run is abandoned before the start is confirmed it's released by the reap
    /// path instead, only once the container is gone, so a cancelled-mid-start container
    /// that Docker did bring up is never left unguarded.
    config_hold: Mutex<Option<Keepalive>>,
}

impl ReapOnDrop {
    /// The container is gone: stop tracking it and let everything held go.
    fn disarm(&self) {
        if let Ok(mut name) = self.name.lock() {
            *name = None;
        }
        self.release();
    }

    fn release(&self) {
        if let Ok(mut held) = self.keepalive.lock() {
            *held = None;
        }
        self.release_config();
    }

    /// The container has started (or is gone): drop the config lock.
    fn release_config(&self) {
        if let Ok(mut held) = self.config_hold.lock() {
            *held = None;
        }
    }
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let Some(name) = self.name.lock().ok().and_then(|mut n| n.take()) else {
            return;
        };
        // Removal is async and `drop` is not, so it has to outlive this scope. No
        // runtime means the process is going down anyway, and the daemon reaps
        // nothing for us — say so rather than failing silently.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no runtime to reap the abandoned one-shot {name} on");
            return;
        };
        let docker = self.docker.clone();
        // Moved into the task, so both are dropped when the removal finishes rather
        // than when this `drop` returns. That difference is the whole point: the
        // stream is already gone by now, but the container is not. The config hold
        // is usually already released (the start confirmed), but if the run was
        // cancelled mid-start it's still held here and must not be freed until the
        // container — which Docker may have started — is confirmed gone.
        let keepalive = self.keepalive.lock().ok().and_then(|mut h| h.take());
        let config_hold = self.config_hold.lock().ok().and_then(|mut h| h.take());
        handle.spawn(async move {
            // Retry with backoff until the container is confirmed gone, and release
            // the keepalive — an operator's wallet claim, for an abandoned approval —
            // only then. A one attempt that fails leaves the container up: dropping the
            // wallet there is exactly the race this guards against, letting a second
            // approval or a bot launch pick the same nonce while the first is still
            // signing. So hold the wallet and keep trying. A daemon down for hours
            // keeps the wallet blocked for hours, which is the deliberate trade — a
            // blocked wallet is loud (every approve and launch on it refuses and says
            // why) while a dropped nonce is silent. A 404 counts as gone, so an operator
            // who removes the container by hand unblocks the wallet without a restart.
            // The name carries a per-run suffix, so a retry can never reap a newer run.
            let mut backoff = REAP_RETRY_MIN;
            loop {
                match reap_once(&docker, &name).await {
                    Reaped::Gone => {
                        tracing::info!("reaped {name}: nobody was listening to it any more");
                        break;
                    }
                    Reaped::Failed(e) => {
                        tracing::warn!(
                            "couldn't reap the abandoned one-shot {name}, retrying in {backoff:?}: {e}"
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(REAP_RETRY_MAX);
                    }
                }
            }
            drop(keepalive);
            drop(config_hold);
        });
    }
}

#[async_trait]
impl crate::panel::migrate::ContainerFiles for BollardDocker {
    /// Read a directory out of a container through the archive endpoint, which
    /// returns it as a tar stream. Used by the layout migration to rescue the
    /// slot-nonce ledger that the flat layout keeps inside the container.
    async fn read_dir(&self, container: &str, dir: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let options = DownloadFromContainerOptionsBuilder::default()
            .path(dir)
            .build();
        let mut stream = self
            .docker
            .download_from_container(container, Some(options));

        let mut archive = Vec::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.with_context(|| format!("reading {dir} out of {container}"))?;
            archive.extend_from_slice(&bytes);
            anyhow::ensure!(
                archive.len() <= MAX_ARCHIVE_BYTES,
                "{dir} in {container} is larger than {MAX_ARCHIVE_BYTES} bytes; refusing to \
                 read it into memory"
            );
        }
        extract_files(&archive)
    }
}

/// Split an image reference into the name and tag `/images/create` wants as
/// separate query parameters.
///
/// A digest (`repo@sha256:…`) splits on the `@`. Otherwise the tag is whatever
/// follows the last colon — but only if that colon comes after the last slash,
/// or a registry port (`registry:5000/repo`) would be mistaken for a tag.
fn split_image_ref(image: &str) -> (&str, Option<&str>) {
    if let Some((name, digest)) = image.split_once('@') {
        return (name, Some(digest));
    }
    match image.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, Some(tag)),
        _ => (image, None),
    }
}

/// Pull the regular files out of a tar archive, flattening to base names and
/// skipping directories.
///
/// Entry paths come from the container, so they are untrusted: only the final
/// component is kept, which means a crafted `../../etc/passwd` entry can't escape
/// wherever the caller writes the results.
fn extract_files(archive: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut tar = tar::Archive::new(std::io::Cursor::new(archive));
    for entry in tar.entries().context("reading the container archive")? {
        let mut entry = entry.context("reading an entry from the container archive")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("an archive entry has no path")?;
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {name} from the container archive"))?;
        out.push((name, bytes));
    }
    Ok(out)
}

/// Block until a container exits and return its status code. A non-zero exit is
/// reported by bollard as an error carrying the status; for a dry run or an
/// approve that is the answer the operator wants, not a transport failure, so it
/// is unwrapped back into a code.
async fn wait_for_exit(docker: &Docker, name: &str) -> Result<i64> {
    let mut waiter =
        docker.wait_container(name, Some(WaitContainerOptionsBuilder::default().build()));
    match waiter.next().await {
        Some(Ok(r)) => Ok(r.status_code),
        Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => Ok(code),
        Some(Err(e)) => Err(anyhow::Error::new(e).context(format!("waiting for {name} to exit"))),
        None => anyhow::bail!("{name} exited without reporting a status"),
    }
}

/// Turn a Docker log frame into a line, tagging which stream it came from so the
/// UI can colour errors. Frames are byte slices that may split or join lines;
/// the trailing newline is trimmed and over-long frames are truncated.
fn to_log_line(out: LogOutput) -> LogLine {
    let (source, bytes) = match out {
        LogOutput::StdErr { message } => (LogSource::Stderr, message),
        LogOutput::StdOut { message } => (LogSource::Stdout, message),
        // stdin echo and TTY console output are both the bot's own output as far
        // as an operator reading the panel is concerned.
        LogOutput::StdIn { message } | LogOutput::Console { message } => {
            (LogSource::Stdout, message)
        }
    };
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if text.len() > MAX_LOG_LINE {
        // Truncate on a char boundary so the result stays valid UTF-8.
        let cut = (0..=MAX_LOG_LINE)
            .rev()
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str(" …[truncated]");
    }
    let text = text.trim_end_matches(['\n', '\r']).to_string();
    LogLine { source, text }
}

fn to_container_info(s: &ContainerSummary) -> ContainerInfo {
    ContainerInfo {
        id: s.id.clone().unwrap_or_default(),
        name: s
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default(),
        image: s.image.clone().unwrap_or_default(),
        image_id: s.image_id.clone().unwrap_or_default(),
        state: s
            .state
            .as_ref()
            .map(|st| ContainerState::parse(st.as_ref()))
            .unwrap_or(ContainerState::Unknown),
        status: s.status.clone().unwrap_or_default(),
        created_unix: s.created.unwrap_or_default(),
        labels: s.labels.clone().unwrap_or_default(),
        mounts: s
            .mounts
            .as_ref()
            .map(|m| m.iter().map(to_mount_info).collect())
            .unwrap_or_default(),
    }
}

fn to_mount_info(m: &MountPoint) -> MountInfo {
    MountInfo {
        source: m.source.clone().map(PathBuf::from).unwrap_or_default(),
        destination: m.destination.clone().map(PathBuf::from).unwrap_or_default(),
        // Docker omits RW for some mount types; absent means not writable, which
        // is the conservative reading for the ledger-layout check.
        rw: m.rw.unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn an_image_reference_splits_into_the_name_and_tag_the_pull_api_wants() {
        assert_eq!(
            split_image_ref("ghcr.io/textile-protocol/textile-stitch:sha-abc123"),
            (
                "ghcr.io/textile-protocol/textile-stitch",
                Some("sha-abc123")
            )
        );
        // No tag: the daemon defaults to `latest`.
        assert_eq!(split_image_ref("textile-stitch"), ("textile-stitch", None));
        // A registry port is not a tag. Splitting on the last colon regardless
        // would ask the daemon to pull `5000/textile-stitch`, which doesn't exist.
        assert_eq!(
            split_image_ref("registry.internal:5000/textile-stitch"),
            ("registry.internal:5000/textile-stitch", None)
        );
        assert_eq!(
            split_image_ref("registry.internal:5000/textile-stitch:v2"),
            ("registry.internal:5000/textile-stitch", Some("v2"))
        );
        // A pinned digest goes over as the tag parameter too.
        assert_eq!(
            split_image_ref("ghcr.io/textile-protocol/textile-stitch@sha256:beef"),
            (
                "ghcr.io/textile-protocol/textile-stitch",
                Some("sha256:beef")
            )
        );
    }

    #[test]
    fn log_frames_are_tagged_by_stream_and_stripped_of_newlines() {
        let out = LogOutput::StdErr {
            message: Bytes::from_static(b"boom\n"),
        };
        let line = to_log_line(out);
        assert_eq!(line.source, LogSource::Stderr);
        assert_eq!(line.text, "boom");

        let out = LogOutput::StdOut {
            message: Bytes::from_static(b"posted order\r\n"),
        };
        assert_eq!(to_log_line(out).text, "posted order");
    }

    #[test]
    fn an_over_long_frame_is_truncated_on_a_char_boundary() {
        // A bot logging a huge payload must not be able to grow the SSE buffer
        // without bound, and truncation must not split a multi-byte char.
        let big = "é".repeat(MAX_LOG_LINE);
        let out = LogOutput::StdOut {
            message: Bytes::from(big.into_bytes()),
        };
        let line = to_log_line(out);
        assert!(line.text.len() <= MAX_LOG_LINE + 32);
        assert!(line.text.ends_with("…[truncated]"));
    }

    #[test]
    fn invalid_utf8_in_a_frame_does_not_panic() {
        let out = LogOutput::StdOut {
            message: Bytes::from_static(&[0xff, 0xfe, b'o', b'k']),
        };
        assert!(to_log_line(out).text.contains("ok"));
    }

    #[test]
    fn a_summary_without_optional_fields_maps_to_unknown_not_running() {
        // The daemon omits fields in some API versions; a bot with no reported
        // state must never render as running.
        let info = to_container_info(&ContainerSummary::default());
        assert_eq!(info.state, ContainerState::Unknown);
        assert!(!info.state.is_running());
        assert!(info.labels.is_empty());
        assert!(info.mounts.is_empty());
    }

    #[test]
    fn a_mount_with_no_rw_flag_is_treated_as_read_only() {
        // The ledger-layout check keys off `rw`, so guessing "writable" here
        // would hide a layout that silently loses the nonce ledger.
        let m = MountPoint {
            source: Some("/host/bot-a".into()),
            destination: Some("/home/stitch/run".into()),
            rw: None,
            ..Default::default()
        };
        let info = to_mount_info(&m);
        assert!(!info.rw);
        assert_eq!(info.source, PathBuf::from("/host/bot-a"));
    }

    #[test]
    fn container_names_lose_the_apis_leading_slash() {
        let s = ContainerSummary {
            names: Some(vec!["/stitch-bot-a".to_string()]),
            ..Default::default()
        };
        assert_eq!(to_container_info(&s).name, "stitch-bot-a");
    }

    /// Build a tar archive the way the Docker archive endpoint would. The name is
    /// written straight into the header rather than through `append_data`, because
    /// the builder rejects the hostile paths these tests need to cover.
    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, body) in entries {
            let mut header = tar::Header::new_gnu();
            let name = &mut header.as_gnu_mut().unwrap().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(if path.ends_with('/') {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            header.set_cksum();
            builder.append(&header, *body).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn archive_extraction_keeps_files_and_drops_directories() {
        let archive = tar_of(&[
            ("run/", b""),
            ("run/stitch-slot-nonces-42161.json", b"{\"nonce\":7}"),
        ]);
        let files = extract_files(&archive).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "stitch-slot-nonces-42161.json");
        assert_eq!(files[0].1, b"{\"nonce\":7}");
    }

    #[test]
    fn a_traversing_entry_path_is_flattened_to_its_base_name() {
        // Entry paths come from a container the panel does not control. Keeping
        // only the base name is what stops a crafted archive from writing
        // outside the bot's directory.
        let archive = tar_of(&[("../../../etc/passwd", b"root:x:0:0")]);
        let files = extract_files(&archive).unwrap();
        assert_eq!(files[0].0, "passwd");
    }
}
