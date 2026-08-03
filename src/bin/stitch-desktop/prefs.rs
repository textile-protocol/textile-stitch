// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Persisted desktop UI preferences.

use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::DesktopPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopPrefs {
    /// When true, macOS uses ActivationPolicy::Accessory (no Dock icon).
    /// Default is false — Dock icon is shown.
    pub hide_dock_icon: bool,
    /// When true, hold an OS power assertion so the machine does not idle-sleep
    /// (Amphetamine-style). Restored on launch from this file.
    pub keep_awake: bool,
}

impl Default for DesktopPrefs {
    fn default() -> Self {
        Self {
            hide_dock_icon: false,
            keep_awake: false,
        }
    }
}

impl DesktopPrefs {
    pub fn load(paths: &DesktopPaths) -> Self {
        let path = prefs_path(paths);
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, paths: &DesktopPaths) -> Result<()> {
        let path = prefs_path(paths);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).context("serializing desktop prefs")?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

fn prefs_path(paths: &DesktopPaths) -> std::path::PathBuf {
    paths.root.join("desktop-prefs.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_paths(root: PathBuf) -> DesktopPaths {
        DesktopPaths {
            bots_dir: root.join("bots"),
            env_file: root.join("panel.env"),
            password_file: root.join("panel.password"),
            panel_log: root.join("panel.log"),
            root,
        }
    }

    #[test]
    fn default_hides_dock_off() {
        assert!(!DesktopPrefs::default().hide_dock_icon);
        assert!(!DesktopPrefs::default().keep_awake);
    }

    #[test]
    fn round_trip_hide_dock() {
        let dir = std::env::temp_dir().join(format!("stitch-desktop-prefs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = tmp_paths(dir.clone());
        let prefs = DesktopPrefs {
            hide_dock_icon: true,
            keep_awake: true,
        };
        prefs.save(&paths).unwrap();
        let loaded = DesktopPrefs::load(&paths);
        assert!(loaded.hide_dock_icon);
        assert!(loaded.keep_awake);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "stitch-desktop-prefs-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = tmp_paths(dir);
        let loaded = DesktopPrefs::load(&paths);
        assert!(!loaded.hide_dock_icon);
        assert!(!loaded.keep_awake);
    }

    #[test]
    fn legacy_prefs_without_keep_awake_default_off() {
        let dir = std::env::temp_dir().join(format!(
            "stitch-desktop-prefs-legacy-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = tmp_paths(dir.clone());
        fs::write(
            paths.root.join("desktop-prefs.json"),
            r#"{ "hide_dock_icon": true }"#,
        )
        .unwrap();
        let loaded = DesktopPrefs::load(&paths);
        assert!(loaded.hide_dock_icon);
        assert!(!loaded.keep_awake);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefs_path_under_root() {
        let root = PathBuf::from("/tmp/stitch-test");
        let paths = tmp_paths(root.clone());
        assert_eq!(prefs_path(&paths), root.join("desktop-prefs.json"));
    }
}
