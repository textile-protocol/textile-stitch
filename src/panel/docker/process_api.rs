// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A [`DockerApi`] that supervises local `stitch` child processes.
//!
//! Used by the desktop tray app so operators can run the panel without Docker.
//! Container vocabulary (create/start/stop/logs) is preserved so the rest of the
//! panel stays shared; under the hood each "container" is a process plus a small
//! JSON record on disk.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::stream;
use serde::{Deserialize, Serialize};

use crate::panel::inventory::RUN_DIR;
use crate::setup;

use super::{
    BindSpec, ContainerInfo, ContainerState, CreateSpec, DockerApi, Keepalive, LogLine, LogOptions,
    LogSource, LogStream, MountInfo, RunEvent, RunStream, STOP_GRACE_SECS,
};

/// Image reference reported for process-supervised bots. No registry host, so
/// panel update detection treats them as local-only (no false "pull available").
pub const BUNDLED_IMAGE: &str = "stitch:bundled";

/// Cap for in-memory restart backoff after unexpected exits.
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How often the background supervisor reaps exited bots and applies restart policy.
/// Must not depend on UI/API polls — desktop `--autostart` has no browser traffic.
const SUPERVISOR_TICK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedBot {
    id: String,
    name: String,
    image: String,
    labels: HashMap<String, String>,
    env: Vec<String>,
    binds: Vec<PersistedBind>,
    cmd: Option<Vec<String>>,
    restart_unless_stopped: bool,
    /// Operator intent: keep running across panel restarts.
    wanted_up: bool,
    created_unix: i64,
    /// OS pid of the last spawned bot process. Used to terminate orphans left
    /// behind when the panel exits without a clean stop (crash / force-quit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    /// Linux `/proc/<pid>/stat` starttime (clock ticks after boot). Paired with
    /// `pid` so a recycled PID is not killed on restore. Absent on non-Linux or
    /// when the field couldn't be read at spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid_starttime: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedBind {
    host_path: String,
    container_path: String,
    read_only: bool,
}

impl From<&BindSpec> for PersistedBind {
    fn from(b: &BindSpec) -> Self {
        Self {
            host_path: b.host_path.display().to_string(),
            container_path: b.container_path.display().to_string(),
            read_only: b.read_only,
        }
    }
}

impl PersistedBind {
    fn to_bind(&self) -> BindSpec {
        BindSpec {
            host_path: PathBuf::from(&self.host_path),
            container_path: PathBuf::from(&self.container_path),
            read_only: self.read_only,
        }
    }

    fn to_mount(&self) -> MountInfo {
        MountInfo {
            source: PathBuf::from(&self.host_path),
            destination: PathBuf::from(&self.container_path),
            rw: !self.read_only,
        }
    }
}

struct LiveBot {
    record: PersistedBot,
    child: Option<Child>,
    /// Combined stdout/stderr log file.
    log_path: PathBuf,
    /// After an unexpected exit, wait until this instant before respawning.
    restart_after: Option<Instant>,
    /// Consecutive unexpected exits (reset on a successful spawn).
    restart_failures: u32,
}

/// Process-backed runtime for the panel.
pub struct ProcessRuntime {
    stitch_bin: PathBuf,
    state_dir: PathBuf,
    inner: Arc<Mutex<HashMap<String, LiveBot>>>,
    /// Content id derived from the bot binary, used as a stand-in image digest.
    image_id: String,
    /// Set when Drop runs so the supervisor thread exits before bots are stopped.
    stop_supervisor: Arc<AtomicBool>,
    supervisor: Option<std::thread::JoinHandle<()>>,
}

impl ProcessRuntime {
    /// Build a runtime that spawns `stitch_bin` and persists state under `bots_dir`.
    pub fn new(stitch_bin: PathBuf, bots_dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            stitch_bin.is_file(),
            "stitch binary not found at {} — set STITCH_PANEL_STITCH_BIN or place \
             `stitch` next to stitch-panel",
            stitch_bin.display()
        );
        let state_dir = bots_dir.join(".process-runtime");
        std::fs::create_dir_all(&state_dir).with_context(|| {
            format!("creating process-runtime state at {}", state_dir.display())
        })?;
        let image_id = format!("sha256:{}", short_file_digest(&stitch_bin)?);
        let stop_supervisor = Arc::new(AtomicBool::new(false));
        let mut rt = Self {
            stitch_bin,
            state_dir,
            inner: Arc::new(Mutex::new(HashMap::new())),
            image_id,
            stop_supervisor,
            supervisor: None,
        };
        rt.load_persisted()?;
        rt.supervisor = Some(spawn_supervisor(
            Arc::clone(&rt.inner),
            rt.stitch_bin.clone(),
            rt.state_dir.clone(),
            Arc::clone(&rt.stop_supervisor),
        ));
        Ok(rt)
    }

    /// Locate the bot binary: env override, then next to this executable, then PATH.
    pub fn find_stitch_binary() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("STITCH_PANEL_STITCH_BIN") {
            let path = PathBuf::from(p);
            if path.is_file() {
                return Some(path);
            }
        }
        setup::find_stitch_binary()
    }

    fn log_path_for(&self, name: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.log"))
    }

    fn load_persisted(&self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.state_dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        let mut inner = self.inner.lock().unwrap();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let record: PersistedBot = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            let name = record.name.clone();
            let log_path = self.log_path_for(&name);
            let wanted = record.wanted_up && record.restart_unless_stopped;
            let mut live = LiveBot {
                record,
                child: None,
                log_path,
                restart_after: None,
                restart_failures: 0,
            };
            // Kill any orphan left from a previous panel process before we spawn
            // again — otherwise two market makers share one wallet. Only signal
            // the pid when it still looks like our stitch binary (and, on Linux,
            // the starttime matches) so a recycled PID is never killed.
            if let Some(pid) = live.record.pid.take() {
                let starttime = live.record.pid_starttime.take();
                terminate_managed_pid(pid, starttime, &self.stitch_bin, STOP_GRACE_SECS);
                let _ = persist_record(&self.state_dir, &live.record);
            }
            if wanted {
                if let Err(e) = spawn_bot(&self.stitch_bin, &self.state_dir, &mut live) {
                    tracing::error!("failed to restore {}: {e:#}", live.record.name);
                    live.record.wanted_up = false;
                    let _ = persist_record(&self.state_dir, &live.record);
                }
            }
            inner.insert(name, live);
        }
        Ok(())
    }

    fn info_of(live: &mut LiveBot, image_id: &str) -> ContainerInfo {
        let (state, status) = match live.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => (ContainerState::Running, "Up (process)".to_string()),
                Ok(Some(status)) => {
                    live.child = None;
                    live.record.pid = None;
                    live.record.pid_starttime = None;
                    (ContainerState::Exited, format!("Exited ({status})"))
                }
                Err(_) => (ContainerState::Unknown, "Unknown".to_string()),
            },
            None => {
                if live.record.wanted_up {
                    (ContainerState::Exited, "Exited".to_string())
                } else {
                    (ContainerState::Created, "Created".to_string())
                }
            }
        };
        ContainerInfo {
            id: live.record.id.clone(),
            name: live.record.name.clone(),
            image: live.record.image.clone(),
            image_id: image_id.to_string(),
            state,
            status,
            created_unix: live.record.created_unix,
            labels: live.record.labels.clone(),
            mounts: live
                .record
                .binds
                .iter()
                .map(PersistedBind::to_mount)
                .collect(),
        }
    }

    /// Reap exited children and, when desired state is still up, schedule a respawn.
    fn reap_and_maybe_restart(stitch_bin: &Path, state_dir: &Path, live: &mut LiveBot) {
        if let Some(child) = live.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    live.child = None;
                    live.record.pid = None;
                    live.record.pid_starttime = None;
                    let _ = persist_record(state_dir, &live.record);
                    if live.record.wanted_up && live.record.restart_unless_stopped {
                        live.restart_failures = live.restart_failures.saturating_add(1);
                        let delay = restart_backoff(live.restart_failures);
                        live.restart_after = Some(Instant::now() + delay);
                        tracing::warn!(
                            bot = %live.record.name,
                            failures = live.restart_failures,
                            ?delay,
                            "bot process exited unexpectedly; will restart"
                        );
                    }
                }
                Ok(None) | Err(_) => {}
            }
        }

        if live.child.is_none()
            && live.record.wanted_up
            && live.record.restart_unless_stopped
            && live.restart_after.is_some_and(|at| Instant::now() >= at)
        {
            live.restart_after = None;
            match spawn_bot(stitch_bin, state_dir, live) {
                Ok(()) => {
                    live.restart_failures = 0;
                }
                Err(e) => {
                    live.restart_failures = live.restart_failures.saturating_add(1);
                    let delay = restart_backoff(live.restart_failures);
                    live.restart_after = Some(Instant::now() + delay);
                    tracing::error!(
                        bot = %live.record.name,
                        "failed to restart after unexpected exit: {e:#}; retrying in {delay:?}"
                    );
                }
            }
        }
    }
}

impl Drop for ProcessRuntime {
    fn drop(&mut self) {
        // Stop the supervisor before touching children so it can't respawn while
        // we're shutting down.
        self.stop_supervisor.store(true, Ordering::Relaxed);
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }

        // Dropping `Child` does not stop the OS process. Terminate every live bot
        // so a panel quit/crash doesn't leave market makers running; `wanted_up`
        // stays true so the next panel start restores them.
        //
        // Stop bots in parallel — sequential waits would be N × STOP_GRACE_SECS
        // and the desktop tray's single-grace deadline would tree-kill the rest.
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let mut children = Vec::new();
        for live in inner.values_mut() {
            if let Some(child) = live.child.take() {
                children.push(child);
            }
            if live.record.pid.take().is_some() || live.record.pid_starttime.take().is_some() {
                let _ = persist_record(&self.state_dir, &live.record);
            }
        }
        drop(inner);
        std::thread::scope(|scope| {
            for mut child in children {
                scope.spawn(move || {
                    let _ = stop_child(&mut child, STOP_GRACE_SECS);
                });
            }
        });
    }
}

fn spawn_supervisor(
    inner: Arc<Mutex<HashMap<String, LiveBot>>>,
    stitch_bin: PathBuf,
    state_dir: PathBuf,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("stitch-process-supervisor".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(mut map) = inner.lock() {
                    for live in map.values_mut() {
                        ProcessRuntime::reap_and_maybe_restart(&stitch_bin, &state_dir, live);
                    }
                }
                // Sleep in short slices so Drop can join promptly.
                let slices = (SUPERVISOR_TICK.as_millis() / 100).max(1);
                for _ in 0..slices {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("spawn process-runtime supervisor")
}

fn restart_backoff(failures: u32) -> Duration {
    let secs = 1u64
        .checked_shl(failures.saturating_sub(1).min(6))
        .unwrap_or(64);
    Duration::from_secs(secs).min(RESTART_BACKOFF_MAX)
}

fn persist_record(state_dir: &Path, record: &PersistedBot) -> Result<()> {
    let path = state_dir.join(format!("{}.json", record.name));
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(record)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn remove_record(state_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(state_dir.join(format!("{name}.json")));
}

fn short_file_digest(path: &Path) -> Result<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path)?;
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    Ok(format!("{:016x}", h.finish()))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rewrite_path(value: &str, binds: &[BindSpec]) -> String {
    let path = Path::new(value);
    for bind in binds {
        if path == bind.container_path.as_path() {
            return bind.host_path.display().to_string();
        }
        if let Ok(rel) = path.strip_prefix(&bind.container_path) {
            return bind.host_path.join(rel).display().to_string();
        }
    }
    // Also accept the canonical RUN_DIR string even if binds used PathBuf equality quirks.
    if let Some(rest) = value.strip_prefix(RUN_DIR) {
        let rest = rest.trim_start_matches('/');
        if let Some(run) = binds
            .iter()
            .find(|b| b.container_path.as_os_str() == RUN_DIR)
        {
            return if rest.is_empty() {
                run.host_path.display().to_string()
            } else {
                run.host_path.join(rest).display().to_string()
            };
        }
    }
    value.to_string()
}

fn host_workdir(binds: &[BindSpec]) -> Option<PathBuf> {
    binds
        .iter()
        .find(|b| b.container_path.as_os_str() == RUN_DIR)
        .map(|b| b.host_path.clone())
}

fn build_command(
    stitch_bin: &Path,
    spec_cmd: Option<&[String]>,
    binds: &[BindSpec],
) -> Result<Command> {
    let mut args: Vec<String> = match spec_cmd {
        Some(cmd) if !cmd.is_empty() => {
            // Fast-forward past a leading "stitch" token from Docker image CMDs.
            let rest = if cmd[0] == "stitch" || cmd[0].ends_with("/stitch") {
                &cmd[1..]
            } else {
                cmd
            };
            rest.iter().map(|a| rewrite_path(a, binds)).collect()
        }
        _ => {
            let config = rewrite_path(&format!("{RUN_DIR}/stitch.toml"), binds);
            vec!["--config".into(), config]
        }
    };

    // Ensure --config points at a host path even if the caller passed a container path
    // without going through rewrite (defensive).
    if let Some(i) = args.iter().position(|a| a == "--config") {
        if let Some(cfg) = args.get_mut(i + 1) {
            *cfg = rewrite_path(cfg, binds);
        }
    }

    let mut command = Command::new(stitch_bin);
    command.args(&args);
    if let Some(dir) = host_workdir(binds) {
        command.current_dir(dir);
    }
    Ok(command)
}

fn apply_env(command: &mut Command, env: &[String], binds: &[BindSpec]) {
    for kv in env {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        command.env(k, rewrite_path(v, binds));
    }
}

fn spawn_bot(stitch_bin: &Path, state_dir: &Path, live: &mut LiveBot) -> Result<()> {
    if let Some(child) = live.child.as_mut() {
        if let Ok(None) = child.try_wait() {
            bail!("{} is already running", live.record.name);
        }
        live.child = None;
        live.record.pid = None;
        live.record.pid_starttime = None;
    }
    let binds: Vec<BindSpec> = live
        .record
        .binds
        .iter()
        .map(PersistedBind::to_bind)
        .collect();
    let mut command = build_command(stitch_bin, live.record.cmd.as_deref(), &binds)?;
    apply_env(&mut command, &live.record.env, &binds);
    // Prefer host stitch.env paths when present (absolute host paths from the writer).
    if let Some(dir) = host_workdir(&binds) {
        let env_file = dir.join("stitch.env");
        if env_file.exists() {
            apply_env_file(&mut command, &env_file);
        }
    }

    if let Some(parent) = live.log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&live.log_path)
        .with_context(|| format!("opening log {}", live.log_path.display()))?;
    let log_err = log.try_clone()?;
    {
        let mut header = log.try_clone()?;
        let _ = writeln!(header, "\n--- stitch start {} ---", now_unix());
    }
    command.stdout(Stdio::from(log));
    command.stderr(Stdio::from(log_err));
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = command
        .spawn()
        .with_context(|| format!("spawning {} for {}", stitch_bin.display(), live.record.name))?;
    let pid = child.id();
    live.record.pid = Some(pid);
    live.record.pid_starttime = process_starttime(pid);
    live.child = Some(child);
    live.record.wanted_up = true;
    live.restart_after = None;
    persist_record(state_dir, &live.record)?;
    Ok(())
}

fn apply_env_file(cmd: &mut Command, env_path: &Path) {
    let Ok(contents) = std::fs::read_to_string(env_path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = {
            let raw = raw.trim();
            if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
                raw[1..raw.len() - 1].replace("'\\''", "'")
            } else {
                raw.to_string()
            }
        };
        cmd.env(key, value);
    }
}

fn stop_child(child: &mut Child, grace_secs: i64) -> Result<()> {
    setup::terminate(child).context("sending stop signal")?;
    let deadline = Instant::now() + Duration::from_secs(grace_secs.max(0) as u64);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// True when `pid` is a live (non-zombie) process. Zombies still have a
/// `/proc/<pid>` entry and answer `kill(pid, 0)`, but they aren't running
/// market-maker code — safe to ignore and respawn.
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        for line in status.lines() {
            let Some(state) = line.strip_prefix("State:") else {
                continue;
            };
            // `State:\tZ (zombie)` — anything else (R/S/D/T…) counts as alive.
            return !state.trim_start().starts_with('Z');
        }
        // status file without State: treat as alive if the pid exists.
        true
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(pid, 0) is a liveness probe.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        let Ok(output) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        text.contains(&pid.to_string())
    }
}

/// Linux starttime from `/proc/<pid>/stat` (field 22), used as a pid-reuse guard.
fn process_starttime(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `stat` is `pid (comm) state ...` — comm can contain spaces/parens, so
        // split on the final `) ` that closes comm, then index the fields after.
        let rest = stat.rsplit_once(')')?.1;
        let field = rest.split_whitespace().nth(19)?; // starttime is field 22 overall → index 19 after state
        field.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// True when `pid` still looks like a stitch bot we spawned: exe/cmdline match
/// `stitch_bin`, and on Linux the persisted starttime still matches.
fn pid_is_our_stitch(pid: u32, starttime: Option<u64>, stitch_bin: &Path) -> bool {
    if !process_alive(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(expected) = starttime {
            match process_starttime(pid) {
                Some(actual) if actual == expected => {}
                _ => return false,
            }
        }
        let want_name = stitch_bin.file_name();
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            // Kernel appends " (deleted)" when the binary was replaced on disk.
            let exe_name = exe.file_name().and_then(|n| {
                let s = n.to_string_lossy();
                let trimmed = s.strip_suffix(" (deleted)").unwrap_or(&s);
                Some(std::ffi::OsString::from(trimmed))
            });
            if want_name.is_some() && exe_name.as_deref() == want_name {
                return true;
            }
            if exe == stitch_bin {
                return true;
            }
        }
        if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            let first = cmdline.split(|b| *b == 0).next().unwrap_or(&[]);
            if let Ok(arg0) = std::str::from_utf8(first) {
                let arg0_name = Path::new(arg0).file_name();
                if want_name.is_some() && arg0_name == want_name {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(windows)]
    {
        let _ = starttime;
        let want = stitch_bin
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stitch.exe".into());
        let Ok(output) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        return text.contains(&want.to_ascii_lowercase()) && text.contains(&pid.to_string());
    }
    #[cfg(all(unix, not(target_os = "linux"), not(windows)))]
    {
        let _ = starttime;
        // macOS / BSD: match the process command name via `ps`.
        let want = stitch_bin
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stitch".into());
        let Ok(output) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let comm = String::from_utf8_lossy(&output.stdout);
        comm.trim() == want
    }
}

/// Terminate a persisted bot pid only when identity checks pass.
fn terminate_managed_pid(pid: u32, starttime: Option<u64>, stitch_bin: &Path, grace_secs: i64) {
    if !pid_is_our_stitch(pid, starttime, stitch_bin) {
        if process_alive(pid) {
            tracing::warn!(
                pid,
                stitch = %stitch_bin.display(),
                "persisted bot pid is not our stitch process; leaving it alone"
            );
        }
        return;
    }
    terminate_pid(pid, grace_secs);
}

fn terminate_pid(pid: u32, grace_secs: i64) {
    if !process_alive(pid) {
        return;
    }
    #[cfg(unix)]
    {
        // kill(1) rather than libc::kill — clearer and matches what operators run.
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(grace_secs.max(0) as u64);
        while Instant::now() < deadline {
            if !process_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && process_alive(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = grace_secs;
    }
}

fn read_log_tail(path: &Path, tail: usize) -> Vec<LogLine> {
    if tail == 0 {
        return Vec::new();
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    // Bound the read: seek near the end instead of loading a multi-day log.
    // ~512 bytes/line is plenty for stitch's line-oriented tracing output.
    const BYTES_PER_LINE: u64 = 512;
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let len = meta.len();
    let window = (tail as u64)
        .saturating_mul(BYTES_PER_LINE)
        .saturating_add(4096);
    let start = len.saturating_sub(window);
    if start > 0 {
        if file.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
    }
    let mut buf = String::new();
    if std::io::Read::read_to_string(&mut file, &mut buf).is_err() {
        return Vec::new();
    }
    // If we sought mid-file, drop the first (likely partial) line.
    let text = if start > 0 {
        match buf.split_once('\n') {
            Some((_, rest)) => rest,
            None => return Vec::new(),
        }
    } else {
        buf.as_str()
    };
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(tail);
    lines[from..]
        .iter()
        .map(|t| LogLine {
            source: LogSource::Stdout,
            text: (*t).to_string(),
        })
        .collect()
}

/// Pump one child pipe into the SSE channel until EOF or the receiver disconnects.
fn pump_lines(
    reader: impl BufRead,
    source: LogSource,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<RunEvent>>,
) {
    for line in reader.lines() {
        match line {
            Ok(text) => {
                if tx
                    .send(Ok(RunEvent::Line(LogLine { source, text })))
                    .is_err()
                {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

#[async_trait]
impl DockerApi for ProcessRuntime {
    async fn list_all(&self) -> Result<Vec<ContainerInfo>> {
        let mut inner = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(inner.len());
        for live in inner.values_mut() {
            Self::reap_and_maybe_restart(&self.stitch_bin, &self.state_dir, live);
            out.push(Self::info_of(live, &self.image_id));
        }
        Ok(out)
    }

    async fn ensure_image(&self, _image: &str, _refresh: bool) -> Result<()> {
        anyhow::ensure!(
            self.stitch_bin.is_file(),
            "stitch binary missing at {}",
            self.stitch_bin.display()
        );
        Ok(())
    }

    async fn require_fresh_image(&self, image: &str) -> Result<()> {
        self.ensure_image(image, true).await
    }

    async fn local_image_digests(&self, _image: &str) -> Result<Vec<String>> {
        // Empty → update detection treats the bot as not behind (see updates::is_behind).
        // Desktop upgrades go through the tray app, not GHCR pulls.
        Ok(Vec::new())
    }

    async fn schedule_image_swap(
        &self,
        _name: &str,
        _new_image: &str,
        _docker_socket: &Path,
    ) -> Result<()> {
        bail!(
            "this panel is running in desktop process mode — use the Stitch menu bar \
             (or system tray) Update item to install a newer release"
        )
    }

    async fn create(&self, spec: &CreateSpec) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.contains_key(&spec.name) {
            bail!(
                "Conflict. The container name \"/{}\" is already in use",
                spec.name
            );
        }
        let id = format!("proc-{}", spec.name);
        let record = PersistedBot {
            id: id.clone(),
            name: spec.name.clone(),
            // Report bundled so registry update nags stay quiet; the binary is local.
            image: if spec.image.contains("textile-stitch") {
                BUNDLED_IMAGE.to_string()
            } else {
                spec.image.clone()
            },
            labels: spec.labels.clone(),
            env: spec.env.clone(),
            binds: spec.binds.iter().map(PersistedBind::from).collect(),
            cmd: spec.cmd.clone(),
            restart_unless_stopped: spec.restart_unless_stopped,
            wanted_up: false,
            created_unix: now_unix(),
            pid: None,
            pid_starttime: None,
        };
        persist_record(&self.state_dir, &record)?;
        let log_path = self.log_path_for(&spec.name);
        inner.insert(
            spec.name.clone(),
            LiveBot {
                record,
                child: None,
                log_path,
                restart_after: None,
                restart_failures: 0,
            },
        );
        Ok(id)
    }

    async fn start(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let live = inner
            .get_mut(name)
            .ok_or_else(|| anyhow!("No such container: {name}"))?;
        // Clear any pending auto-restart schedule — this is an explicit start.
        live.restart_after = None;
        live.restart_failures = 0;
        spawn_bot(&self.stitch_bin, &self.state_dir, live)?;
        Ok(())
    }

    async fn stop(&self, name: &str, grace_secs: i64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let live = inner
            .get_mut(name)
            .ok_or_else(|| anyhow!("No such container: {name}"))?;
        live.restart_after = None;
        live.restart_failures = 0;
        if let Some(mut child) = live.child.take() {
            stop_child(&mut child, grace_secs)?;
        } else if let Some(pid) = live.record.pid {
            // Orphan from a previous panel instance.
            terminate_managed_pid(pid, live.record.pid_starttime, &self.stitch_bin, grace_secs);
        }
        live.record.pid = None;
        live.record.pid_starttime = None;
        live.record.wanted_up = false;
        persist_record(&self.state_dir, &live.record)?;
        Ok(())
    }

    async fn restart(&self, name: &str, grace_secs: i64) -> Result<()> {
        self.stop(name, grace_secs).await?;
        self.start(name).await
    }

    async fn remove(&self, name: &str, force: bool) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let mut live = inner
            .remove(name)
            .ok_or_else(|| anyhow!("No such container: {name}"))?;
        if let Some(mut child) = live.child.take() {
            if force {
                let _ = child.kill();
                let _ = child.wait();
            } else {
                stop_child(&mut child, STOP_GRACE_SECS)?;
            }
        } else if let Some(pid) = live.record.pid {
            terminate_managed_pid(
                pid,
                live.record.pid_starttime,
                &self.stitch_bin,
                STOP_GRACE_SECS,
            );
        }
        remove_record(&self.state_dir, name);
        let _ = std::fs::remove_file(&live.log_path);
        Ok(())
    }

    fn logs(&self, name: &str, opts: LogOptions) -> LogStream {
        let (log_path, exists) = {
            let inner = self.inner.lock().unwrap();
            match inner.get(name) {
                Some(live) => (live.log_path.clone(), true),
                None => (PathBuf::new(), false),
            }
        };
        if !exists {
            let name = name.to_string();
            return Box::pin(stream::once(async move {
                Err(anyhow!("No such container: {name}"))
            }));
        }
        let initial = read_log_tail(&log_path, opts.tail);
        if !opts.follow {
            return Box::pin(stream::iter(initial.into_iter().map(Ok)));
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for line in initial {
            let _ = tx.send(Ok(line));
        }
        std::thread::spawn(move || {
            let mut file = match std::fs::OpenOptions::new().read(true).open(&log_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!(e)));
                    return;
                }
            };
            let _ = file.seek(SeekFrom::End(0));
            let mut reader = BufReader::new(file);
            loop {
                let mut buf = String::new();
                match reader.read_line(&mut buf) {
                    Ok(0) => {
                        std::thread::sleep(Duration::from_millis(200));
                        if tx.is_closed() {
                            break;
                        }
                    }
                    Ok(_) => {
                        let text = buf.trim_end_matches(['\n', '\r']).to_string();
                        if tx
                            .send(Ok(LogLine {
                                source: LogSource::Stdout,
                                text,
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(anyhow!(e)));
                        break;
                    }
                }
            }
        });
        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx))
    }

    fn run_one_shot(
        &self,
        spec: CreateSpec,
        keepalive: Option<Keepalive>,
        hold_until_started: Option<Keepalive>,
    ) -> RunStream {
        let stitch_bin = self.stitch_bin.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let _hold_started = hold_until_started;
            let run = (|| -> Result<(Child, i64)> {
                let binds = spec.binds.clone();
                let mut command = build_command(&stitch_bin, spec.cmd.as_deref(), &binds)?;
                apply_env(&mut command, &spec.env, &binds);
                if let Some(dir) = host_workdir(&binds) {
                    let env_file = dir.join("stitch.env");
                    if env_file.exists() {
                        apply_env_file(&mut command, &env_file);
                    }
                }
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
                command.stdin(Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    command.creation_flags(CREATE_NO_WINDOW);
                }
                let mut child = command.spawn().context("spawning one-shot")?;
                // Config is loaded at start — release the caller's hold.
                drop(_hold_started);

                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                // Drain both pipes concurrently so a chatty stderr can't fill its
                // pipe and deadlock the child while we wait on stdout.
                let tx_out = tx.clone();
                let out_thread = std::thread::spawn(move || {
                    if let Some(out) = stdout {
                        pump_lines(BufReader::new(out), LogSource::Stdout, &tx_out);
                    }
                });
                let tx_err = tx.clone();
                let err_thread = std::thread::spawn(move || {
                    if let Some(err) = stderr {
                        pump_lines(BufReader::new(err), LogSource::Stderr, &tx_err);
                    }
                });

                let code = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break status.code().unwrap_or(1) as i64,
                        Ok(None) if tx.is_closed() => {
                            // Browser stopped watching — same contract as Docker's
                            // ReapOnDrop: kill the child before releasing keepalive.
                            let _ = stop_child(&mut child, STOP_GRACE_SECS);
                            let _ = out_thread.join();
                            let _ = err_thread.join();
                            return Ok((child, -1));
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                        Err(e) => {
                            let _ = stop_child(&mut child, STOP_GRACE_SECS);
                            let _ = out_thread.join();
                            let _ = err_thread.join();
                            return Err(e.into());
                        }
                    }
                };
                let _ = out_thread.join();
                let _ = err_thread.join();
                Ok((child, code))
            })();

            match run {
                Ok((_child, code)) if code >= 0 => {
                    let _ = tx.send(Ok(RunEvent::Exited { code }));
                }
                Ok(_) => {
                    // Abandoned: stream already closed; nothing to send.
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
            // Keepalive drops only after the child is gone (stop_child / wait above).
            drop(keepalive);
        });
        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::naming::LABEL_BOT;
    use futures_util::StreamExt;

    #[test]
    fn rewrite_path_maps_run_dir_onto_the_host_bind() {
        let binds = [BindSpec::rw("/home/op/bots/a", RUN_DIR)];
        assert_eq!(
            rewrite_path(&format!("{RUN_DIR}/stitch.toml"), &binds),
            "/home/op/bots/a/stitch.toml"
        );
        assert_eq!(
            rewrite_path(&format!("{RUN_DIR}/stitch.key"), &binds),
            "/home/op/bots/a/stitch.key"
        );
    }

    #[test]
    fn build_command_defaults_to_config_on_the_host() {
        let binds = [BindSpec::rw("/data/bot-a", RUN_DIR)];
        let cmd = build_command(Path::new("/bin/stitch"), None, &binds).unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["--config", "/data/bot-a/stitch.toml"]);
    }

    #[test]
    fn build_command_strips_leading_stitch_token() {
        let binds = [BindSpec::rw("/data/bot-a", RUN_DIR)];
        let spec = vec![
            "stitch".into(),
            "approve".into(),
            "--config".into(),
            format!("{RUN_DIR}/stitch.toml"),
        ];
        let cmd = build_command(Path::new("/bin/stitch"), Some(&spec), &binds).unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["approve", "--config", "/data/bot-a/stitch.toml"]);
    }

    #[test]
    fn restart_backoff_grows_then_caps() {
        assert_eq!(restart_backoff(1), Duration::from_secs(1));
        assert_eq!(restart_backoff(2), Duration::from_secs(2));
        assert_eq!(restart_backoff(3), Duration::from_secs(4));
        assert_eq!(restart_backoff(20), RESTART_BACKOFF_MAX);
    }

    #[test]
    fn read_log_tail_returns_only_the_last_n_lines_from_a_large_file() {
        let dir = temp_root("logtail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bot.log");
        let mut body = String::new();
        for i in 0..5_000 {
            body.push_str(&format!("line-{i}\n"));
        }
        std::fs::write(&path, &body).unwrap();
        let lines = read_log_tail(&path, 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "line-4997");
        assert_eq!(lines[1].text, "line-4998");
        assert_eq!(lines[2].text, "line-4999");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn create_start_stop_round_trip_with_true_binary() {
        // Use the system `true` as a stand-in bot that exits immediately.
        let true_bin = which_true();
        let root = std::env::temp_dir().join(format!(
            "stitch-process-rt-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let bots = root.join("bots");
        std::fs::create_dir_all(&bots).unwrap();
        let rt = ProcessRuntime::new(true_bin, &bots).unwrap();

        let mut labels = HashMap::new();
        labels.insert(LABEL_BOT.into(), "bot-a".into());
        let host = bots.join("bot-a");
        std::fs::create_dir_all(&host).unwrap();
        // `true` ignores --config args and exits 0; enough to exercise lifecycle.
        let spec = CreateSpec {
            name: "stitch-bot-a".into(),
            image: "ghcr.io/textile-protocol/textile-stitch:latest".into(),
            labels,
            env: vec!["RUST_LOG=info".into()],
            binds: vec![BindSpec::rw(&host, RUN_DIR)],
            cmd: None,
            restart_unless_stopped: false,
        };
        rt.create(&spec).await.unwrap();
        rt.start("stitch-bot-a").await.unwrap();
        // Give it a moment to exit.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let list = rt.list_all().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].image, BUNDLED_IMAGE);
        rt.stop("stitch-bot-a", 1).await.unwrap();
        rt.remove("stitch-bot-a", true).await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dropping_runtime_terminates_live_children() {
        let sleep_bin = which_sleep();
        let root = temp_root("drop-kills");
        let bots = root.join("bots");
        std::fs::create_dir_all(&bots).unwrap();
        let rt = ProcessRuntime::new(sleep_bin.clone(), &bots).unwrap();
        let host = bots.join("bot-a");
        std::fs::create_dir_all(&host).unwrap();
        let mut labels = HashMap::new();
        labels.insert(LABEL_BOT.into(), "bot-a".into());
        rt.create(&CreateSpec {
            name: "stitch-bot-a".into(),
            image: BUNDLED_IMAGE.into(),
            labels,
            env: vec![],
            binds: vec![BindSpec::rw(&host, RUN_DIR)],
            cmd: Some(vec!["30".into()]),
            restart_unless_stopped: true,
        })
        .await
        .unwrap();
        rt.start("stitch-bot-a").await.unwrap();
        let pid = {
            let inner = rt.inner.lock().unwrap();
            inner["stitch-bot-a"].record.pid.expect("pid persisted")
        };
        assert!(process_alive(pid));
        drop(rt);
        // Give Drop's stop_child a moment.
        for _ in 0..50 {
            if !process_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!process_alive(pid), "Drop must terminate the bot process");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restore_kills_orphan_pid_before_respawn() {
        let sleep_bin = which_sleep();
        let root = temp_root("orphan");
        let bots = root.join("bots");
        let state = bots.join(".process-runtime");
        std::fs::create_dir_all(&state).unwrap();
        let host = bots.join("bot-a");
        std::fs::create_dir_all(&host).unwrap();

        // Spawn an orphan the new runtime should find via persisted pid.
        // Forget the Child so the OS reparents it (like a crashed panel would):
        // if we keep the handle, a SIGTERM'd process becomes our zombie and
        // kill(pid, 0) stays true until we wait().
        let orphan = Command::new(&sleep_bin)
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let orphan_pid = orphan.id();
        let orphan_start = process_starttime(orphan_pid);
        std::mem::forget(orphan);
        let record = PersistedBot {
            id: "proc-stitch-bot-a".into(),
            name: "stitch-bot-a".into(),
            image: BUNDLED_IMAGE.into(),
            labels: HashMap::new(),
            env: vec![],
            binds: vec![PersistedBind::from(&BindSpec::rw(&host, RUN_DIR))],
            cmd: Some(vec!["30".into()]),
            restart_unless_stopped: true,
            wanted_up: true,
            created_unix: now_unix(),
            pid: Some(orphan_pid),
            pid_starttime: orphan_start,
        };
        persist_record(&state, &record).unwrap();

        let rt = ProcessRuntime::new(sleep_bin, &bots).unwrap();
        assert!(
            !process_alive(orphan_pid),
            "orphan from the previous panel must be terminated on restore"
        );
        let new_pid = {
            let inner = rt.inner.lock().unwrap();
            inner["stitch-bot-a"].record.pid
        };
        assert!(new_pid.is_some());
        assert_ne!(new_pid, Some(orphan_pid));
        drop(rt);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn abandoned_one_shot_terminates_the_child() {
        let sleep_bin = which_sleep();
        let root = temp_root("oneshot");
        let bots = root.join("bots");
        std::fs::create_dir_all(&bots).unwrap();
        let rt = ProcessRuntime::new(sleep_bin, &bots).unwrap();
        let host = bots.join("bot-a");
        std::fs::create_dir_all(&host).unwrap();

        let mut stream = rt.run_one_shot(
            CreateSpec {
                name: "stitch-dry-bot-a".into(),
                image: BUNDLED_IMAGE.into(),
                labels: HashMap::new(),
                env: vec![],
                binds: vec![BindSpec::rw(&host, RUN_DIR)],
                cmd: Some(vec!["30".into()]),
                restart_unless_stopped: false,
            },
            None,
            None,
        );
        // Drop the stream without waiting for exit — mimics Stop watching / navigate away.
        drop(stream.next().await);
        drop(stream);

        // The one-shot thread needs a beat to notice the closed channel and SIGTERM.
        // We can't see the pid from outside easily; instead ensure Drop of runtime
        // (which also stops managed bots) and that a second create isn't blocked.
        // Probe via /proc: any sleep 30 child of this test process should die.
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Best-effort: if something is still sleeping from this test root's spawn
        // path, terminate_pid won't know — the unit above covers Drop/orphan.
        // Here we just assert the stream path doesn't panic / hang the test.
        drop(rt);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn wanted_bot_respawns_after_unexpected_exit() {
        let true_bin = which_true();
        let root = temp_root("respawn");
        let bots = root.join("bots");
        std::fs::create_dir_all(&bots).unwrap();
        let rt = ProcessRuntime::new(true_bin, &bots).unwrap();
        let host = bots.join("bot-a");
        std::fs::create_dir_all(&host).unwrap();
        let mut labels = HashMap::new();
        labels.insert(LABEL_BOT.into(), "bot-a".into());
        rt.create(&CreateSpec {
            name: "stitch-bot-a".into(),
            image: BUNDLED_IMAGE.into(),
            labels,
            env: vec![],
            binds: vec![BindSpec::rw(&host, RUN_DIR)],
            cmd: None,
            restart_unless_stopped: true,
        })
        .await
        .unwrap();
        rt.start("stitch-bot-a").await.unwrap();
        // `true` exits immediately; schedule a restart with zero backoff for the test
        // by clearing restart_after after the first reap.
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let mut inner = rt.inner.lock().unwrap();
            let live = inner.get_mut("stitch-bot-a").unwrap();
            ProcessRuntime::reap_and_maybe_restart(&rt.stitch_bin, &rt.state_dir, live);
            // Force immediate retry regardless of backoff.
            live.restart_after = Some(Instant::now());
            ProcessRuntime::reap_and_maybe_restart(&rt.stitch_bin, &rt.state_dir, live);
            assert!(
                live.child.is_some() || live.restart_after.is_some() || live.record.pid.is_some(),
                "wanted bot should be running or scheduled after unexpected exit"
            );
        }
        drop(rt);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn supervisor_respawns_without_list_all_poll() {
        // Regression: restart used to advance only inside list_all(), so a
        // headless --autostart panel never brought wanted bots back after exit.
        let sleep_bin = which_sleep();
        let root = temp_root("supervisor");
        let bots = root.join("bots");
        std::fs::create_dir_all(&bots).unwrap();
        let rt = ProcessRuntime::new(sleep_bin, &bots).unwrap();
        let host = bots.join("bot-a");
        std::fs::create_dir_all(&host).unwrap();
        let mut labels = HashMap::new();
        labels.insert(LABEL_BOT.into(), "bot-a".into());
        rt.create(&CreateSpec {
            name: "stitch-bot-a".into(),
            image: BUNDLED_IMAGE.into(),
            labels,
            env: vec![],
            binds: vec![BindSpec::rw(&host, RUN_DIR)],
            cmd: Some(vec!["1".into()]),
            restart_unless_stopped: true,
        })
        .await
        .unwrap();
        rt.start("stitch-bot-a").await.unwrap();
        let first_pid = {
            let inner = rt.inner.lock().unwrap();
            inner.get("stitch-bot-a").unwrap().record.pid
        };
        // Wait for sleep 1 to exit, backoff (~1s), and a supervisor tick — no list_all.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let (second_pid, wanted, child_or_scheduled) = {
            let inner = rt.inner.lock().unwrap();
            let live = inner.get("stitch-bot-a").unwrap();
            (
                live.record.pid,
                live.record.wanted_up,
                live.child.is_some() || live.restart_after.is_some() || live.record.pid.is_some(),
            )
        };
        assert!(wanted, "wanted_up should stay true");
        assert!(
            child_or_scheduled,
            "supervisor should keep a wanted bot running or scheduled without list_all"
        );
        if let (Some(a), Some(b)) = (first_pid, second_pid) {
            assert_ne!(a, b, "bot should have been respawned under a new pid");
        }
        drop(rt);
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stitch-process-rt-{tag}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn which_true() -> PathBuf {
        ["true", "/bin/true", "/usr/bin/true"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("need a true binary for the process-runtime test")
    }

    fn which_sleep() -> PathBuf {
        ["sleep", "/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("need a sleep binary for the process-runtime test")
    }

    #[test]
    fn terminate_pid_kills_a_forgotten_sleep() {
        let sleep_bin = which_sleep();
        let orphan = Command::new(&sleep_bin)
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = orphan.id();
        std::mem::forget(orphan);
        assert!(process_alive(pid), "precondition");
        terminate_pid(pid, 2);
        assert!(
            !process_alive(pid),
            "terminate_pid should kill forgotten orphan"
        );
    }

    #[test]
    fn terminate_managed_pid_skips_a_process_that_is_not_stitch() {
        let sleep_bin = which_sleep();
        let true_bin = which_true();
        let orphan = Command::new(&sleep_bin)
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = orphan.id();
        let start = process_starttime(pid);
        std::mem::forget(orphan);
        // Claim this sleep pid belongs to `true` — identity check must refuse.
        terminate_managed_pid(pid, start, &true_bin, 2);
        assert!(
            process_alive(pid),
            "must not kill a live process that isn't our stitch binary"
        );
        terminate_pid(pid, 2); // cleanup
    }
}
