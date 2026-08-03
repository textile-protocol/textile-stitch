// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Hide the console window when spawning Windows CLI tools (`reg`, `cmd`,
//! `tasklist`, …). Without this, each spawn flashes a black console — operators
//! often mistake that for the panel browser failing to open.

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
