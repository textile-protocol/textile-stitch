// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Where operator config lives and how to recognise an already-set-up folder.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The three files setup manages, plus the log file the GUI tees to.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub dir: PathBuf,
    pub toml: PathBuf,
    pub env: PathBuf,
    pub key: PathBuf,
    pub log: PathBuf,
}

/// Resolve the standard file names inside a config directory.
pub fn config_paths(dir: impl AsRef<Path>) -> ConfigPaths {
    let dir = dir.as_ref().to_path_buf();
    ConfigPaths {
        toml: dir.join("stitch.toml"),
        env: dir.join("stitch.env"),
        key: dir.join("stitch.key"),
        log: dir.join("stitch.log"),
        dir,
    }
}

/// Default config directory: the folder containing the running executable, so
/// config lands next to the unzipped release. Falls back to the current working
/// directory if the executable path can't be resolved.
pub fn default_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The current user's home directory. On Windows the profile is `USERPROFILE`;
/// `HOME` is only set by shells like Git Bash / MSYS and can point elsewhere, so
/// it must not win there — otherwise `~/Stitch` would resolve to two different
/// folders depending on how the app was launched. Elsewhere it's `HOME`.
pub fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }
    .map(PathBuf::from)
}

/// Per-user directory for Stitch's own app state — currently just the pointer to
/// the operator's chosen config folder. This is deliberately independent of where
/// the config itself lives (which the operator can put anywhere via Browse), and
/// stable across launches: Windows `%APPDATA%\Stitch`, macOS
/// `~/Library/Application Support/Stitch`, otherwise `$XDG_CONFIG_HOME/stitch` or
/// `~/.config/stitch`. `None` only if the platform's base dir can't be resolved.
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
/// the operator's chosen config folder.
const LOCATION_FILE: &str = "config-location";

/// Remember `dir` as the config folder to reopen on the next launch. Best-effort:
/// a write failure just means the app falls back to its default folder next time,
/// which is no worse than before this existed. Only the non-secret folder path is
/// stored here — never any config or key contents.
pub fn remember_config_dir(dir: impl AsRef<Path>) {
    if let Some(state) = app_state_dir() {
        let _ = write_location_to(&state, dir.as_ref());
    }
}

/// The config folder remembered from a previous setup, if one was saved. Returns
/// the raw stored path without checking it still holds a config — the caller
/// decides whether to fall back (see the GUI's startup, which drops back to the
/// default folder when this isn't configured).
pub fn remembered_config_dir() -> Option<PathBuf> {
    read_location_from(&app_state_dir()?)
}

/// Write the pointer file under `state_dir`, creating the directory if needed.
/// Split out from [`remember_config_dir`] so the file format is testable without
/// depending on the process's real `APPDATA`/`HOME`.
fn write_location_to(state_dir: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(
        state_dir.join(LOCATION_FILE),
        format!("{}\n", dir.display()),
    )
}

/// Read the pointer file under `state_dir`. `None` if it's absent or blank, so a
/// stray empty file never resolves to the current directory.
fn read_location_from(state_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(state_dir.join(LOCATION_FILE)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Config folders that builds before the `config-location` pointer existed may
/// have used, but the current [`home_dir`] no longer resolves to. Only Windows is
/// affected: the old default resolved `~` via `HOME` before `USERPROFILE`, so a
/// setup made while launching from Git Bash/MSYS (which sets `HOME`) can live
/// under `HOME/Stitch` even though the new default is `USERPROFILE/Stitch`.
/// Startup checks these for an existing config before falling back to the wizard,
/// so an upgrading operator isn't sent back through setup. Empty off Windows, and
/// empty when `HOME` is unset or already equals the current home.
pub fn legacy_gui_dirs() -> Vec<PathBuf> {
    let current = home_dir();
    legacy_gui_dirs_from(
        cfg!(windows),
        std::env::var_os("HOME").map(PathBuf::from),
        current.as_deref(),
    )
}

/// Pure core of [`legacy_gui_dirs`], split out so the migration logic is testable
/// on any host regardless of the real platform or environment.
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
/// MPC setups have no stitch.key (their secret is turnkey-api.key /
/// mpcvault-api.token), so requiring stitch.key would wrongly send a valid MPC
/// operator back through the wizard on reopen.
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
/// prompts: unlike `is_configured` (which needs a complete, runnable setup), this
/// trips on a lone secret or a partial setup so we never clobber one silently.
pub fn has_operator_files(dir: impl AsRef<Path>) -> bool {
    let p = config_paths(dir);
    p.toml.exists() || p.env.exists() || has_signer_secret(&p)
}

/// Operator address controlled by the key file in this folder.
pub fn operator_address(dir: impl AsRef<Path>) -> anyhow::Result<alloy_primitives::Address> {
    use zeroize::Zeroize;
    let p = config_paths(dir);
    let mut raw =
        std::fs::read_to_string(&p.key).with_context(|| format!("reading {}", p.key.display()))?;
    // Wipe the on-heap key copy after deriving the address. The GUI may stay open
    // supervising the bot, so this string must not linger for a process dump.
    let key = crate::signer::parse_private_key(&raw);
    raw.zeroize();
    let key = key?;
    Ok(crate::signer::address_from_signing_key(&key))
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
    fn remembered_config_dir_round_trips_through_the_pointer_file() {
        let state = unique_dir("state");
        // A fresh state dir has no pointer yet.
        assert!(read_location_from(&state).is_none());
        // Writing then reading returns the exact folder, even one with spaces.
        let chosen = PathBuf::from("/Users/First Last/My Stitch");
        write_location_to(&state, &chosen).unwrap();
        assert_eq!(read_location_from(&state), Some(chosen));
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn legacy_gui_dirs_finds_the_old_home_location_on_windows() {
        // Windows, with HOME (set by Git Bash) pointing somewhere other than the
        // USERPROFILE-based current home: the old HOME/Stitch is a legacy candidate.
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
        // Off Windows there was never a HOME-vs-USERPROFILE split to migrate.
        assert!(legacy_gui_dirs_from(
            false,
            Some(PathBuf::from("/home/op")),
            Some(Path::new("/home/op"))
        )
        .is_empty());
        // Windows but HOME already equals the current home: no divergence.
        assert!(legacy_gui_dirs_from(
            true,
            Some(PathBuf::from("C:\\Users\\op")),
            Some(Path::new("C:\\Users\\op"))
        )
        .is_empty());
        // Windows with HOME unset: nothing to migrate.
        assert!(legacy_gui_dirs_from(true, None, Some(Path::new("C:\\Users\\op"))).is_empty());
    }

    #[test]
    fn read_location_ignores_a_blank_pointer() {
        // A blank/whitespace file must not resolve to "." — it means "no memory".
        let state = unique_dir("blank");
        write_location_to(&state, Path::new("   ")).unwrap();
        assert!(read_location_from(&state).is_none());
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn config_paths_uses_standard_names() {
        let p = config_paths("/tmp/x");
        assert!(p.toml.ends_with("stitch.toml"));
        assert!(p.env.ends_with("stitch.env"));
        assert!(p.key.ends_with("stitch.key"));
        assert!(p.log.ends_with("stitch.log"));
    }

    #[test]
    fn is_configured_requires_both_toml_and_key() {
        let dir = unique_dir("cfg");
        assert!(!is_configured(&dir));
        std::fs::write(dir.join("stitch.toml"), "x").unwrap();
        assert!(!is_configured(&dir), "toml alone is not configured");
        std::fs::write(dir.join("stitch.key"), "x").unwrap();
        assert!(is_configured(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_configured_recognizes_an_mpc_only_folder() {
        let dir = unique_dir("mpc-cfg");
        std::fs::write(dir.join("stitch.toml"), "x").unwrap();
        assert!(!is_configured(&dir), "toml alone is not configured");
        // An MPC setup has no stitch.key; its secret is the api token/key.
        std::fs::write(dir.join("mpcvault-api.token"), "x").unwrap();
        assert!(is_configured(&dir), "toml + MPC secret is configured");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn has_operator_files_trips_on_a_lone_key() {
        let dir = unique_dir("lone");
        assert!(!has_operator_files(&dir));
        // A hand-placed key with no toml is NOT "configured" but must still gate
        // overwrite, so the key is never truncated silently.
        std::fs::write(dir.join("stitch.key"), "x").unwrap();
        assert!(!is_configured(&dir));
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
        std::fs::remove_dir_all(&dir).ok();
    }
}
