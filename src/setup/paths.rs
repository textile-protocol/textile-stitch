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
