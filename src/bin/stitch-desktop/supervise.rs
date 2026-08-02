// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Start / stop the bundled `stitch-panel` child process.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::paths::DesktopPaths;

pub struct PanelSupervisor {
    paths: DesktopPaths,
    panel_bin: PathBuf,
    stitch_bin: PathBuf,
    child: Option<Child>,
}

impl PanelSupervisor {
    pub fn new(paths: DesktopPaths) -> Result<Self> {
        let panel_bin = find_next_to_exe("stitch-panel")
            .context("stitch-panel binary not found next to stitch-desktop")?;
        let stitch_bin = find_next_to_exe("stitch")
            .or_else(|| find_next_to_exe("stitch.exe"))
            .context("stitch binary not found next to stitch-desktop")?;
        Ok(Self {
            paths,
            panel_bin,
            stitch_bin,
            child: None,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        self.reap();
        // A previous desktop crash can leave stitch-panel (+ bots) on 8420. Stop
        // that leftover before we spawn, or we'd "succeed" on a port we don't own.
        self.stop_leftover_panel()?;

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.panel_log)
            .with_context(|| format!("opening {}", self.paths.panel_log.display()))?;
        let log_err = log.try_clone()?;

        let mut cmd = Command::new(&self.panel_bin);
        cmd.current_dir(&self.paths.root);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::from(log));
        cmd.stderr(Stdio::from(log_err));
        // Load panel.env into the child environment.
        load_env_file(&mut cmd, &self.paths.env_file)?;
        cmd.env("STITCH_PANEL_STITCH_BIN", &self.stitch_bin);
        cmd.env("STITCH_PANEL_RUNTIME", "process");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", self.panel_bin.display()))?;
        write_panel_pid(&self.paths, child.id());
        self.child = Some(child);

        // Wait briefly for the listen port so Open Stitch isn't a connection refused.
        // Success requires *our* child to still be alive — a pre-existing listener on
        // 8420 must not count as ready while the new panel has already exited.
        for _ in 0..50 {
            if !self.is_running() {
                clear_panel_pid(&self.paths);
                bail!(
                    "stitch-panel exited during startup — is port 8420 already in use? see {}",
                    self.paths.panel_log.display()
                );
            }
            if port_open(8420) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Still starting; don't fail — Open can retry. Child is alive.
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            // No live handle — still try to stop a leftover from panel.pid.
            self.stop_leftover_panel()?;
            return Ok(());
        };
        let pid = child.id();
        stop_panel_process_tree(pid, &mut child, panel_stop_grace_secs())?;
        clear_panel_pid(&self.paths);
        Ok(())
    }

    /// Stop a stitch-panel recorded in `panel.pid` from a previous session.
    fn stop_leftover_panel(&mut self) -> Result<()> {
        let Some(pid) = read_panel_pid(&self.paths) else {
            return Ok(());
        };
        if !pid_looks_like_panel(pid, &self.panel_bin) {
            clear_panel_pid(&self.paths);
            return Ok(());
        }
        eprintln!("stitch-desktop: stopping leftover stitch-panel (pid {pid})");
        // Prefer a graceful panel SIGTERM so ProcessRuntime::drop can stop bots
        // with the normal STOP_GRACE_SECS (tree-kill after 300ms would cut mid-tick).
        stop_external_panel(pid, panel_stop_grace_secs())?;
        clear_panel_pid(&self.paths);
        // Brief wait so 8420 is released before we bind again.
        for _ in 0..30 {
            if !port_open(8420) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// True while the supervised panel child is still alive. Reaps a dead child
    /// (and clears `panel.pid`) so callers see a consistent stopped state.
    pub(crate) fn is_running(&mut self) -> bool {
        self.reap();
        self.child.is_some()
    }

    fn reap(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                self.child = None;
                clear_panel_pid(&self.paths);
            }
        }
    }
}

fn stop_panel_process_tree(pid: u32, child: &mut Child, grace_secs: u64) -> Result<()> {
    #[cfg(windows)]
    {
        // Prefer graceful taskkill (no /F) so the panel can Drop-stop bots;
        // force the tree only if it outlives the grace window.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = std::time::Instant::now() + Duration::from_secs(grace_secs);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(None) => {
                    signal_process_tree(pid);
                    let _ = child.wait();
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    #[cfg(not(windows))]
    {
        // SIGTERM → panel graceful_shutdown → ProcessRuntime::drop stops bots
        // in parallel (one STOP_GRACE_SECS window, not N×). Wait that + slack
        // before tree-killing stuck SSE / hung shutdown.
        stitch_bot::setup::terminate(child).context("stopping stitch-panel")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(grace_secs);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(None) => {
                    // Graceful shutdown stuck (e.g. long-lived log SSE). Don't
                    // SIGKILL the panel alone — that skips Drop and leaves bots
                    // trading. Tear down the process tree first.
                    signal_process_tree(pid);
                    let _ = child.wait();
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Graceful stop for a panel we don't own a `Child` handle for (leftover pid).
fn stop_external_panel(pid: u32, grace_secs: u64) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        wait_for_pid_exit(pid, grace_secs);
        if pid_alive(pid) {
            signal_process_tree(pid);
            wait_for_pid_exit(pid, 2);
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        wait_for_pid_exit(pid, grace_secs);
        if pid_alive(pid) {
            signal_process_tree(pid);
            wait_for_pid_exit(pid, 2);
        }
        Ok(())
    }
}

fn wait_for_pid_exit(pid: u32, grace_secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(grace_secs);
    while pid_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // signal 0: existence check, no delivery
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

/// Desktop stop budget after SIGTERM: ProcessRuntime::drop stops bots in
/// parallel, so one STOP_GRACE_SECS (+ slack) covers any fleet size.
fn panel_stop_grace_secs() -> u64 {
    stitch_bot::panel::STOP_GRACE_SECS.max(0) as u64 + 5
}

fn signal_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // Direct children first (bot processes), then the panel itself.
        let _ = Command::new("pkill")
            .args(["-TERM", "-P", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(300));
        let _ = Command::new("pkill")
            .args(["-KILL", "-P", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn panel_pid_path(paths: &DesktopPaths) -> PathBuf {
    paths.root.join("panel.pid")
}

fn write_panel_pid(paths: &DesktopPaths, pid: u32) {
    let _ = std::fs::write(panel_pid_path(paths), format!("{pid}\n"));
}

fn clear_panel_pid(paths: &DesktopPaths) {
    let _ = std::fs::remove_file(panel_pid_path(paths));
}

fn read_panel_pid(paths: &DesktopPaths) -> Option<u32> {
    let text = std::fs::read_to_string(panel_pid_path(paths)).ok()?;
    text.trim().parse().ok()
}

fn pid_looks_like_panel(pid: u32, panel_bin: &Path) -> bool {
    let want = panel_bin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let Some(want) = want else {
        return false;
    };
    #[cfg(target_os = "linux")]
    {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return false;
        }
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            let name = exe
                .file_name()
                .map(|n| n.to_string_lossy().replace(" (deleted)", ""));
            if name.as_deref() == Some(want.as_str()) {
                return true;
            }
        }
        if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            let first = cmdline.split(|b| *b == 0).next().unwrap_or(&[]);
            if let Ok(arg0) = std::str::from_utf8(first) {
                if Path::new(arg0).file_name().map(|n| n.to_string_lossy())
                    == Some(std::borrow::Cow::Borrowed(want.as_str()))
                {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(target_os = "macos")]
    {
        let Ok(output) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        else {
            return false;
        };
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == want.as_str()
    }
    #[cfg(windows)]
    {
        let Ok(output) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        text.contains(&want.to_ascii_lowercase()) && text.contains(&pid.to_string())
    }
    #[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
    {
        let Ok(output) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        else {
            return false;
        };
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == want.as_str()
    }
}

fn find_next_to_exe(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(name);
    if candidate.is_file() {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        let with_ext = dir.join(format!("{name}.exe"));
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }
    None
}

fn load_env_file(cmd: &mut Command, path: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for line in text.lines() {
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
        let value = unquote(raw.trim());
        cmd.env(key, value);
    }
    Ok(())
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("'\\''", "'")
    } else {
        s.to_string()
    }
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}
