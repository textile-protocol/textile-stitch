// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Where operator config lives and how to recognise an already-set-up folder.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The three files setup manages under an operator config directory.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub dir: PathBuf,
    pub toml: PathBuf,
    pub env: PathBuf,
    pub key: PathBuf,
}

/// Resolve the standard file names inside a config directory.
pub fn config_paths(dir: impl AsRef<Path>) -> ConfigPaths {
    let dir = dir.as_ref().to_path_buf();
    ConfigPaths {
        toml: dir.join("stitch.toml"),
        env: dir.join("stitch.env"),
        key: dir.join("stitch.key"),
        dir,
    }
}

/// Default config directory for the old desktop/setup apps: the folder containing
/// the running executable (unzipped release). Falls back to cwd.
pub fn default_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The current user's home directory. On Windows the profile is `USERPROFILE`;
/// `HOME` is only set by shells like Git Bash / MSYS and can point elsewhere, so
/// it must not win there. Elsewhere it's `HOME`.
pub fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }
    .map(PathBuf::from)
}

/// Per-user directory for Stitch's own app state — historically the pointer to
/// the operator's chosen config folder (`config-location`). Windows
/// `%APPDATA%\Stitch`, macOS `~/Library/Application Support/Stitch`, otherwise
/// `$XDG_CONFIG_HOME/stitch` or `~/.config/stitch`.
pub fn app_state_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("Stitch"))
    } else if cfg!(target_os = "macos") {
        home_dir().map(|h| h.join("Library/Application Support/Stitch"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".config")))
            .map(|c| c.join("stitch"))
    }
}

/// Name of the pointer file inside [`app_state_dir`] holding the absolute path of
/// the operator's chosen config folder (old stitch-setup GUI).
const LOCATION_FILE: &str = "config-location";

/// Remember `dir` as the config folder to reopen on the next launch. Best-effort.
pub fn remember_config_dir(dir: impl AsRef<Path>) {
    if let Some(state) = app_state_dir() {
        let _ = write_location_to(&state, dir.as_ref());
    }
}

/// The config folder remembered from a previous setup, if one was saved.
pub fn remembered_config_dir() -> Option<PathBuf> {
    read_location_from(&app_state_dir()?)
}

/// Write the pointer file under `state_dir`, creating the directory if needed.
fn write_location_to(state_dir: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(
        state_dir.join(LOCATION_FILE),
        format!("{}\n", dir.display()),
    )
}

/// Read the pointer file under `state_dir`. `None` if absent or blank.
fn read_location_from(state_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(state_dir.join(LOCATION_FILE)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Config folders that builds before the `config-location` pointer existed may
/// have used. Only Windows is affected: the old default resolved `~` via `HOME`
/// before `USERPROFILE`.
pub fn legacy_gui_dirs() -> Vec<PathBuf> {
    let current = home_dir();
    legacy_gui_dirs_from(
        cfg!(windows),
        std::env::var_os("HOME").map(PathBuf::from),
        current.as_deref(),
    )
}

/// Pure core of [`legacy_gui_dirs`], split out for tests.
fn legacy_gui_dirs_from(
    is_windows: bool,
    home_env: Option<PathBuf>,
    current_home: Option<&Path>,
) -> Vec<PathBuf> {
    if !is_windows {
        return Vec::new();
    }
    match home_env {
        Some(home) if current_home != Some(home.as_path()) => vec![home.join("Stitch")],
        _ => Vec::new(),
    }
}

/// A folder counts as configured once the config and a signer secret both exist.
pub fn is_configured(dir: impl AsRef<Path>) -> bool {
    let p = config_paths(dir);
    p.toml.exists() && has_signer_secret(&p)
}

/// True if any signer's secret file is present: the hot-wallet stitch.key, or an
/// MPC api key/token.
fn has_signer_secret(p: &ConfigPaths) -> bool {
    p.key.exists()
        || p.dir.join("turnkey-api.key").exists()
        || p.dir.join("mpcvault-api.token").exists()
}

/// True if writing a config into this folder would replace any existing operator
/// file (stitch.toml, stitch.env, or any signer secret). Used to gate overwrite
/// prompts so we never clobber a partial setup silently.
pub fn has_operator_files(dir: impl AsRef<Path>) -> bool {
    let p = config_paths(dir);
    p.toml.exists() || p.env.exists() || has_signer_secret(&p)
}

/// Operator address controlled by the key file in this folder.
pub fn operator_address(dir: impl AsRef<Path>) -> anyhow::Result<alloy_primitives::Address> {
    operator_address_from_key(&config_paths(dir).key)
}

/// Operator address derived from an explicit key file path.
///
/// The flat layout names keys per bot (`stitch.bot1.key`), so callers that know
/// the config path resolve the sibling key and pass it here rather than assuming
/// the canonical `stitch.key` in the parent directory.
pub fn operator_address_from_key(
    key: impl AsRef<Path>,
) -> anyhow::Result<alloy_primitives::Address> {
    use zeroize::Zeroize;
    let key = key.as_ref();
    let mut raw =
        std::fs::read_to_string(key).with_context(|| format!("reading {}", key.display()))?;
    // Wipe the on-heap key copy after deriving the address.
    let parsed = crate::signer::parse_private_key(&raw);
    raw.zeroize();
    let parsed = parsed?;
    Ok(crate::signer::address_from_signing_key(&parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("stitch-paths-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn config_paths_uses_standard_names() {
        let p = config_paths("/tmp/x");
        assert!(p.toml.ends_with("stitch.toml"));
        assert!(p.env.ends_with("stitch.env"));
        assert!(p.key.ends_with("stitch.key"));
        assert_eq!(p.dir, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn has_operator_files_trips_on_a_lone_key() {
        let dir = unique_dir("lone");
        assert!(!has_operator_files(&dir));
        // A hand-placed key with no toml must still gate overwrite.
        std::fs::write(dir.join("stitch.key"), "x").unwrap();
        assert!(has_operator_files(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn has_operator_files_trips_on_mpc_secret() {
        let dir = unique_dir("mpc");
        std::fs::write(dir.join("mpcvault-api.token"), "x").unwrap();
        assert!(has_operator_files(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn operator_address_derives_from_key_file() {
        // Anvil/Hardhat account #0 — known address.
        let dir = unique_dir("addr");
        std::fs::write(
            dir.join("stitch.key"),
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80\n",
        )
        .unwrap();
        let addr = operator_address(&dir).unwrap();
        assert_eq!(
            format!("{addr:?}").to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
        let from_path = operator_address_from_key(dir.join("stitch.key")).unwrap();
        assert_eq!(addr, from_path);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remembered_config_dir_round_trips_through_the_pointer_file() {
        let state = unique_dir("state");
        assert!(read_location_from(&state).is_none());
        let chosen = PathBuf::from("/Users/First Last/My Stitch");
        write_location_to(&state, &chosen).unwrap();
        assert_eq!(read_location_from(&state), Some(chosen));
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn legacy_gui_dirs_finds_the_old_home_location_on_windows() {
        let dirs = legacy_gui_dirs_from(
            true,
            Some(PathBuf::from("C:\\msys\\home\\op")),
            Some(Path::new("C:\\Users\\op")),
        );
        assert_eq!(
            dirs,
            vec![PathBuf::from("C:\\msys\\home\\op").join("Stitch")]
        );
    }

    #[test]
    fn legacy_gui_dirs_empty_when_home_matches_or_off_windows() {
        assert!(legacy_gui_dirs_from(
            false,
            Some(PathBuf::from("/home/op")),
            Some(Path::new("/home/op"))
        )
        .is_empty());
        assert!(legacy_gui_dirs_from(
            true,
            Some(PathBuf::from("C:\\Users\\op")),
            Some(Path::new("C:\\Users\\op"))
        )
        .is_empty());
    }

    #[test]
    fn is_configured_needs_toml_and_secret() {
        let dir = unique_dir("cfg");
        assert!(!is_configured(&dir));
        std::fs::write(dir.join("stitch.toml"), "[bot]\n").unwrap();
        assert!(!is_configured(&dir));
        std::fs::write(dir.join("stitch.key"), "x").unwrap();
        assert!(is_configured(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
