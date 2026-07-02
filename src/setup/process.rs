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

/// `stitch --config <toml> [--dry-run]`, with the signer's env wired through.
pub fn run_command(stitch_bin: &Path, paths: &ConfigPaths, dry_run: bool) -> Command {
    let mut cmd = Command::new(stitch_bin);
    cmd.arg("--config").arg(&paths.toml);
    if dry_run {
        cmd.arg("--dry-run");
    }
    apply_signer_env(&mut cmd, paths);
    cmd
}

/// `stitch approve --config <toml> [--dry-run]` for the Permit2 button.
pub fn approve_command(stitch_bin: &Path, paths: &ConfigPaths, dry_run: bool) -> Command {
    let mut cmd = Command::new(stitch_bin);
    cmd.arg("approve").arg("--config").arg(&paths.toml);
    if dry_run {
        cmd.arg("--dry-run");
    }
    apply_signer_env(&mut cmd, paths);
    cmd
}

/// Wire the signer's credentials into the command's environment. The setup writer
/// tailors `stitch.env` to the chosen signer (STITCH_PRIVATE_KEY_FILE for the hot
/// wallet, TURNKEY_*/MPCVAULT_* for MPC), so sourcing that file works for every
/// backend. Falls back to the local key path if the env file isn't there yet.
fn apply_signer_env(cmd: &mut Command, paths: &ConfigPaths) {
    if paths.env.exists() {
        apply_env_file(cmd, &paths.env);
    } else {
        cmd.env("STITCH_PRIVATE_KEY_FILE", &paths.key);
    }
}

/// Parse a `stitch.env` (`KEY='value'` shell-single-quoted, or `KEY=value`) and
/// set each pair on the command. Best-effort: an unreadable file is a no-op.
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
        if !key.is_empty() {
            cmd.env(key, unquote_shell(raw.trim()));
        }
    }
}

/// Undo POSIX shell single-quoting (`'a'\''b'` → `a'b`); leave unquoted values as-is.
fn unquote_shell(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("'\\''", "'")
    } else {
        s.to_string()
    }
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

    fn envs_of(cmd: &Command) -> std::collections::HashMap<String, String> {
        cmd.get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn run_command_sources_mpc_env_from_stitch_env() {
        let dir = std::env::temp_dir().join(format!("stitch-proc-mpc-{}", std::process::id()));
        let corridor = crate::setup::catalog::find_corridor("cngn-usdt-bsc").unwrap();
        let signer = crate::setup::writer::SignerSetup::Mpcvault {
            vault_uuid: "v".into(),
            client_signer_pubkey: "k".into(),
            operator_address: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".into(),
            api_base_url: None,
            callback_listen_addr: None,
            api_token: "tok".into(),
        };
        let paths = crate::setup::writer::write_config_signer(&dir, corridor, &signer).unwrap();
        let cmd = run_command(Path::new("/bin/stitch"), &paths, false);
        let envs = envs_of(&cmd);
        assert!(envs.contains_key("MPCVAULT_API_TOKEN_FILE"));
        assert!(!envs.contains_key("STITCH_PRIVATE_KEY_FILE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_command_falls_back_to_key_env_without_a_stitch_env() {
        let p = config_paths("/tmp/stitch-proc-missing-env");
        let cmd = run_command(Path::new("/bin/stitch"), &p, false);
        assert!(envs_of(&cmd).contains_key("STITCH_PRIVATE_KEY_FILE"));
    }
}
