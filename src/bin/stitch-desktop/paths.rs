// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! On-disk layout for the desktop app (panel env, password, bots root).

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct DesktopPaths {
    pub root: PathBuf,
    pub bots_dir: PathBuf,
    pub env_file: PathBuf,
    pub password_file: PathBuf,
    pub panel_log: PathBuf,
}

impl DesktopPaths {
    pub fn resolve() -> Result<Self> {
        let root = data_root()?;
        Ok(Self {
            bots_dir: root.join("bots"),
            env_file: root.join("panel.env"),
            password_file: root.join("panel.password"),
            panel_log: root.join("panel.log"),
            root,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        std::fs::create_dir_all(&self.bots_dir)
            .with_context(|| format!("creating {}", self.bots_dir.display()))?;
        Ok(())
    }
}

fn data_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("STITCH_DESKTOP_HOME") {
        return Ok(PathBuf::from(p));
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Stitch"));
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(base).join("Stitch"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(xdg).join("stitch"));
        }
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("stitch"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        anyhow::bail!("unsupported platform for stitch-desktop")
    }
}
