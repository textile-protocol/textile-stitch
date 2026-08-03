// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Amphetamine-style keep-awake for the desktop tray app.
//!
//! Holds an OS power assertion while enabled so idle sleep does not stop the
//! local panel / bots. Display sleep is still allowed.

use anyhow::{Context, Result};

/// Owns the platform keep-awake assertion. Dropping (or clearing) releases it.
pub struct KeepAwakeController {
    guard: Option<keepawake::KeepAwake>,
}

impl KeepAwakeController {
    pub fn new() -> Self {
        Self { guard: None }
    }

    pub fn is_active(&self) -> bool {
        self.guard.is_some()
    }

    /// Enable or disable the assertion. Idempotent.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        if enabled {
            if self.guard.is_none() {
                let awake = keepawake::Builder::default()
                    // Match Amphetamine's default: block idle/system sleep, allow
                    // the display to dim/sleep.
                    .display(false)
                    .idle(true)
                    .sleep(true)
                    .reason("Stitch keep awake")
                    .app_name("Stitch")
                    .app_reverse_domain("com.textile.stitch")
                    .create()
                    .context("enabling keep awake")?;
                self.guard = Some(awake);
            }
        } else {
            self.guard = None;
        }
        Ok(())
    }
}

/// Tray / Settings label — Mac-specific wording on Apple, generic elsewhere.
pub fn label() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Keep Mac awake"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Keep computer awake"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_nonempty() {
        assert!(!label().is_empty());
    }

    #[test]
    fn starts_inactive() {
        assert!(!KeepAwakeController::new().is_active());
    }

    #[test]
    fn disable_when_inactive_is_ok() {
        let mut c = KeepAwakeController::new();
        c.set_enabled(false).unwrap();
        assert!(!c.is_active());
    }
}
