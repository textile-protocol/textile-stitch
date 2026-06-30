// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Build the `stitch` child-process invocations the GUI drives (run, approve,
//! update) and locate the bot binary next to this one. The actual spawning and
//! output streaming is done by the GUI; these builders are the testable seam.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use crate::setup::paths::ConfigPaths;

/// What the supervised bot process is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Running,
    DryRun,
}

/// Locate the `stitch` bot binary: prefer one sitting next to the current
/// executable (the unzipped release layout), then fall back to PATH.
pub fn find_stitch_binary() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "stitch.exe"
    } else {
        "stitch"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    which_on_path(exe_name)
}

/// Minimal PATH lookup so we don't add a `which` dependency.
fn which_on_path(exe_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe_name))
        .find(|c| c.exists())
}

/// `stitch --config <toml> [--dry-run]`, with the key file wired through the env.
pub fn run_command(stitch_bin: &Path, paths: &ConfigPaths, dry_run: bool) -> Command {
    let mut cmd = Command::new(stitch_bin);
    cmd.arg("--config").arg(&paths.toml);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.env("STITCH_PRIVATE_KEY_FILE", &paths.key);
    cmd
}

/// `stitch approve --config <toml> [--dry-run]` for the Permit2 button.
pub fn approve_command(stitch_bin: &Path, paths: &ConfigPaths, dry_run: bool) -> Command {
    let mut cmd = Command::new(stitch_bin);
    cmd.arg("approve").arg("--config").arg(&paths.toml);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.env("STITCH_PRIVATE_KEY_FILE", &paths.key);
    cmd
}

/// `stitch --update` for the update button.
pub fn update_command(stitch_bin: &Path) -> Command {
    let mut cmd = Command::new(stitch_bin);
    cmd.arg("--update");
    cmd
}

/// Ask a child to stop gracefully. On Unix this is SIGTERM, which Stitch handles
/// by finishing its current tick. On Windows there is no clean per-child signal,
/// so this is a hard kill (the GUI surfaces that caveat).
pub fn terminate(child: &mut Child) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: pid is from a live Child we own and have not wait()ed on, so
        // the PID cannot have been recycled. pid_t is i32 on all supported
        // platforms and process IDs fit within i32.
        if let Ok(pid) = i32::try_from(child.id()) {
            let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
            if rc == 0 {
                return Ok(());
            }
        }
        // Fall through to a hard kill if SIGTERM failed or the pid didn't fit.
    }
    child.kill()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::paths::config_paths;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn run_command_omits_dry_run_when_false() {
        let p = config_paths("/tmp/x");
        let cmd = run_command(Path::new("/bin/stitch"), &p, false);
        let args = args_of(&cmd);
        assert!(args.contains(&"--config".to_string()));
        assert!(!args.contains(&"--dry-run".to_string()));
    }

    #[test]
    fn run_command_adds_dry_run_when_true() {
        let p = config_paths("/tmp/x");
        let cmd = run_command(Path::new("/bin/stitch"), &p, true);
        assert!(args_of(&cmd).contains(&"--dry-run".to_string()));
    }

    #[test]
    fn approve_command_uses_the_approve_verb() {
        let p = config_paths("/tmp/x");
        let cmd = approve_command(Path::new("/bin/stitch"), &p, true);
        let args = args_of(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("approve"));
        assert!(args.contains(&"--dry-run".to_string()));
    }

    #[test]
    fn update_command_passes_update_flag() {
        let cmd = update_command(Path::new("/bin/stitch"));
        assert_eq!(args_of(&cmd), vec!["--update".to_string()]);
    }
}
