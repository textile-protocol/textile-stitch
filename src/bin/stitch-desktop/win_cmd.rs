// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Hide the console window when spawning Windows CLI tools (`tasklist`,
//! `taskkill`, …). Without this, each spawn flashes a black console.
//! Registry access goes through [`crate::win_reg`] (no `reg.exe`).

use std::process::Command;

/// Apply `CREATE_NO_WINDOW` on Windows; no-op elsewhere.
pub fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
