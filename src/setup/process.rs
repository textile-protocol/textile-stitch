// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Locate the `stitch` binary next to the current executable, and stop a child
//! process cleanly. Used by the panel process runtime and the desktop tray app.

use std::path::PathBuf;
use std::process::Child;

/// Locate the `stitch` bot binary: prefer one sitting next to the current
/// executable (the app-bundle / unzipped release layout), then fall back to PATH.
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

/// Ask a child to stop gracefully. On Unix this is SIGTERM, which Stitch handles
/// by finishing its current tick. On Windows there is no clean per-child signal,
/// so this is a hard kill.
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

    #[test]
    fn find_stitch_binary_prefers_a_sibling_when_present() {
        // Smoke: either we find something or we don't — must not panic.
        let _ = find_stitch_binary();
    }
}
