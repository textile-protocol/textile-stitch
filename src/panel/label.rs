// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A bot's display name, separate from its id.
//!
//! The id (`0x5cffb39d`) is the config directory and the container name and
//! cannot change. The display name is what the operator wants to read on the
//! page, and can. It lives in `panel.json` beside the config so it travels
//! with the bot's files (backup, migration, a copied directory) and never
//! touches `stitch.toml`, which is the bot's, not the panel's.
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const LABEL_FILE: &str = "panel.json";
/// Long enough for "Lagos desk · cNGN main", short enough for a table cell.
pub const MAX_DISPLAY_NAME: usize = 40;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PanelMeta {
    display_name: Option<String>,
}

fn read(dir: &Path) -> PanelMeta {
    std::fs::read_to_string(dir.join(LABEL_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// The operator's name for the bot, if they set one.
pub fn read_display_name(dir: &Path) -> Option<String> {
    read(dir)
        .display_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A name the operator typed, or the reason it is refused. Trimmed, bounded,
/// one line, no control characters: it is printed into a title, a tab and a
/// list, not parsed by anything.
pub fn validate_display_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        bail!("the name can't be empty; clear it to go back to the wallet id");
    }
    if name.chars().count() > MAX_DISPLAY_NAME {
        bail!("the name can't be longer than {MAX_DISPLAY_NAME} characters");
    }
    if name.chars().any(|c| c.is_control()) {
        bail!("the name has to be a single line");
    }
    Ok(name.to_string())
}

/// Set (or, with `None`, clear) the display name. Atomic write, so a crash
/// mid-way leaves the old name rather than an empty file.
pub fn write_display_name(dir: &Path, name: Option<&str>) -> Result<()> {
    let path = dir.join(LABEL_FILE);
    let mut meta = read(dir);
    meta.display_name = match name {
        Some(n) => Some(validate_display_name(n)?),
        None => None,
    };
    if meta.display_name.is_none() {
        // Nothing else lives in the file yet; an empty one is just clutter.
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    crate::setup::write_file_atomic(&path, serde_json::to_vec_pretty(&meta)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("stitch-label-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_name_round_trips_and_clearing_removes_the_file() {
        let d = dir("roundtrip");
        assert_eq!(read_display_name(&d), None);
        write_display_name(&d, Some("  Lagos desk  ")).unwrap();
        assert_eq!(read_display_name(&d).as_deref(), Some("Lagos desk"));
        write_display_name(&d, None).unwrap();
        assert_eq!(read_display_name(&d), None);
        assert!(!d.join(LABEL_FILE).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_name_is_refused_with_the_reason() {
        assert!(validate_display_name("   ")
            .unwrap_err()
            .to_string()
            .contains("empty"));
        assert!(validate_display_name(&"x".repeat(41))
            .unwrap_err()
            .to_string()
            .contains("40"));
        assert!(validate_display_name("two\nlines")
            .unwrap_err()
            .to_string()
            .contains("single line"));
        assert_eq!(
            validate_display_name("cNGN · Lagos").unwrap(),
            "cNGN · Lagos"
        );
    }
}
