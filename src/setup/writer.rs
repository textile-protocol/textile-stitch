// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Write the three operator files (stitch.toml, stitch.env, stitch.key) for a
//! chosen corridor, with the key file locked down to the current user.

use std::path::Path;

use anyhow::{Context, Result};
use zeroize::Zeroize;

use crate::setup::catalog::Corridor;
use crate::setup::paths::{config_paths, ConfigPaths};
use crate::signer::parse_private_key;

/// The `stitch.env` body: point the bot at the key file and set a sane log level.
/// The path is shell-single-quoted because the install guides `source` this file,
/// so a directory with spaces (e.g. `/Users/First Last`) must not be word-split.
pub fn render_env(paths: &ConfigPaths) -> String {
    format!(
        "STITCH_PRIVATE_KEY_FILE={}\nRUST_LOG=info\n",
        shell_single_quote(&paths.key.display().to_string())
    )
}

/// POSIX shell single-quoting: wrap in single quotes and turn any embedded single
/// quote into the `'\''` escape sequence, so the value survives `. stitch.env`.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Validate the key, then write stitch.toml (the corridor template), stitch.env,
/// and stitch.key into `dir`. The key file and env file are restricted to the
/// owner. Returns the paths written. The caller's key string should be zeroized
/// after this returns; the copy taken here is wiped before returning.
pub fn write_config(
    dir: impl AsRef<Path>,
    corridor: &Corridor,
    key_raw: &str,
) -> Result<ConfigPaths> {
    // Validate before writing anything, so a bad key never leaves half a config.
    parse_private_key(key_raw).context("the private key is not valid")?;

    let paths = config_paths(dir.as_ref());
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;

    std::fs::write(&paths.toml, corridor.toml_template)
        .with_context(|| format!("writing {}", paths.toml.display()))?;

    std::fs::write(&paths.env, render_env(&paths))
        .with_context(|| format!("writing {}", paths.env.display()))?;

    // Create the key file owner-only from the start so the raw key is never
    // momentarily world-readable between write and chmod.
    let mut key_line = format!("{}\n", key_raw.trim());
    write_key_file(&paths.key, key_line.as_bytes())
        .with_context(|| format!("writing {}", paths.key.display()))?;
    key_line.zeroize();

    // write_key_file already creates the key owner-only on both platforms; only
    // the env file (no secret) still needs locking down here.
    restrict_to_owner(&paths.env)?;

    Ok(paths)
}

/// Lock a file down so only its owner can read or write it.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 600 {}", path.display()))
}

/// Windows: drop inherited ACEs and grant only the current user, via icacls.
#[cfg(windows)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|u| !u.is_empty())
        .context("USERNAME env var not set; cannot set file ACL")?;
    // /inheritance:r removes inherited permissions; /grant:r USER:F grants
    // full control to the current user only.
    let status = std::process::Command::new("icacls")
        .args([&p, "/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .status()
        .with_context(|| format!("running icacls on {p}"))?;
    if !status.success() {
        anyhow::bail!("icacls failed to restrict {p}");
    }
    Ok(())
}

/// Write the key file with owner-only permissions from creation (Unix), so the
/// secret is never briefly world-readable.
#[cfg(unix)]
fn write_key_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // `mode(0o600)` below only applies when the file is created. If a key file
    // (or placeholder) already exists, tighten it to 0600 BEFORE we truncate and
    // write, so an old group/world-readable file can't expose the new key during
    // the write window.
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

/// Windows has no umask. Lock the key file to the current user with icacls BEFORE
/// the secret is written, so the key never lands under inherited or pre-existing
/// ACLs during the write.
#[cfg(windows)]
fn write_key_file(path: &Path, bytes: &[u8]) -> Result<()> {
    // Start from a clean ACL. A reused key file can carry explicit ACEs for other
    // principals (e.g. Everyone) that `icacls /grant:r` does NOT drop, and
    // truncating an existing file preserves its DACL. Deleting it first means the
    // fresh file only inherits from its parent, which `/inheritance:r` then
    // strips, leaving the owner grant as the only ACE.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, b"")?;
    restrict_to_owner(path)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::catalog::find_corridor;

    const KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("stitch-writer-{}-{}", std::process::id(), tag));
        d
    }

    #[test]
    fn render_env_points_at_the_key_file() {
        let p = config_paths("/tmp/x");
        let env = render_env(&p);
        assert!(env.contains("STITCH_PRIVATE_KEY_FILE='/tmp/x/stitch.key'"));
        assert!(env.contains("RUST_LOG=info"));
    }

    #[test]
    fn render_env_quotes_paths_with_spaces() {
        // A `source`d env file must keep a spaced path as one shell word.
        let p = config_paths("/Users/First Last/Stitch");
        let env = render_env(&p);
        assert!(env.contains("STITCH_PRIVATE_KEY_FILE='/Users/First Last/Stitch/stitch.key'"));
    }

    #[test]
    fn write_config_writes_all_three_files() {
        let dir = unique_dir("ok");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let paths = write_config(&dir, corridor, KEY).unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.toml).unwrap(),
            corridor.toml_template
        );
        assert!(std::fs::read_to_string(&paths.env)
            .unwrap()
            .contains("stitch.key"));
        assert_eq!(std::fs::read_to_string(&paths.key).unwrap().trim(), KEY);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_config_rejects_a_bad_key_before_writing() {
        let dir = unique_dir("badkey");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let err = write_config(&dir, corridor, "not-a-key").unwrap_err();
        assert!(err.to_string().contains("private key"));
        assert!(
            !config_paths(&dir).toml.exists(),
            "nothing written on bad key"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_dir("perms");
        let corridor = find_corridor("brla-usdt-celo").unwrap();
        let paths = write_config(&dir, corridor, KEY).unwrap();
        let mode = std::fs::metadata(&paths.key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
