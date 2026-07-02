// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Write the three operator files (stitch.toml, stitch.env, stitch.key) for a
//! chosen corridor, with the key file locked down to the current user.

use std::path::{Path, PathBuf};

use alloy_primitives::{hex, Address};
use anyhow::{Context, Result};
use k256::ecdsa::SigningKey;
use zeroize::Zeroize;

use crate::config::Config;
use crate::setup::catalog::Corridor;
use crate::setup::paths::{config_paths, ConfigPaths};
use crate::signer::{address_from_signing_key, parse_address, parse_mnemonic, parse_private_key};

/// The signer backend the operator picked. Drives the dropdown in the GUI and
/// which fields/secrets the wizard collects. `Local` is the hotwallet default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignerKind {
    #[default]
    Local,
    Turnkey,
    Mpcvault,
}

impl SignerKind {
    /// Human label for the dropdown.
    pub fn label(self) -> &'static str {
        match self {
            SignerKind::Local => "Hot wallet (local key)",
            SignerKind::Turnkey => "MPC — Turnkey",
            SignerKind::Mpcvault => "MPC — MPCVault",
        }
    }

    /// The MPC backends are new; flag them as experimental so the UI can warn.
    pub fn experimental(self) -> bool {
        matches!(self, SignerKind::Turnkey | SignerKind::Mpcvault)
    }

    /// Label with an `· Experimental` marker appended for experimental backends,
    /// for the dropdown and the current-signer summary.
    pub fn display_label(self) -> String {
        if self.experimental() {
            format!("{}  ·  Experimental", self.label())
        } else {
            self.label().to_string()
        }
    }

    pub const ALL: [SignerKind; 3] = [SignerKind::Local, SignerKind::Turnkey, SignerKind::Mpcvault];
}

/// How the operator supplied their hot-wallet key. Either a raw private key or a
/// BIP-39 seed phrase we derive the account-0 key from — either way only the
/// resulting private key is written to `stitch.key`; the phrase is never persisted
/// and the runtime signer only ever sees a raw key.
#[derive(Debug, Clone)]
pub enum LocalKeyMaterial {
    /// A raw secp256k1 private key, `0x…` hex.
    PrivateKey(String),
    /// A BIP-39 seed phrase; account 0 is derived at [`crate::signer::DEFAULT_DERIVATION_PATH`].
    SeedPhrase(String),
}

impl LocalKeyMaterial {
    /// The signing key this material resolves to. Validates as a side effect: a bad
    /// hex key or an invalid/mis-typed seed phrase fails here rather than deriving a
    /// garbage key.
    fn signing_key(&self) -> Result<SigningKey> {
        match self {
            LocalKeyMaterial::PrivateKey(raw) => parse_private_key(raw),
            LocalKeyMaterial::SeedPhrase(phrase) => parse_mnemonic(phrase),
        }
    }

    /// The operator address this material controls, so the setup UI can confirm the
    /// wallet (especially the derived one) before anything is saved.
    pub fn operator_address(&self) -> Result<Address> {
        Ok(address_from_signing_key(&self.signing_key()?))
    }

    /// The `0x`-prefixed private key to persist to `stitch.key`. Derives from the
    /// seed phrase when needed, so what lands on disk is always a single raw key.
    /// The returned string is secret — the caller zeroizes it after the write.
    fn private_key_hex(&self) -> Result<String> {
        let key = self.signing_key()?;
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&key.to_bytes());
        let out = format!("0x{}", hex::encode(bytes));
        bytes.zeroize();
        Ok(out)
    }
}

/// Everything needed to write a signer: the non-secret fields that go into the
/// `[signer]` TOML section, plus the secret material that goes to an owner-only
/// file referenced by `stitch.env` (never into the TOML).
#[derive(Debug, Clone)]
pub enum SignerSetup {
    /// Hot wallet: the operator's key material (raw key or seed phrase). The
    /// derived private key goes to stitch.key; a seed phrase is never persisted.
    Local { material: LocalKeyMaterial },
    /// Turnkey MPC. The API public key is not secret (→ env inline); the API
    /// private key is (→ turnkey-api.key).
    Turnkey {
        organization_id: String,
        sign_with: String,
        operator_address: String,
        api_base_url: Option<String>,
        api_public_key: String,
        api_private_key: String,
    },
    /// MPCVault MPC. The API token is secret (→ mpcvault-api.token); the vault
    /// needs the client-signer sidecar running (documented, not written here).
    Mpcvault {
        vault_uuid: String,
        client_signer_pubkey: String,
        operator_address: String,
        api_base_url: Option<String>,
        callback_listen_addr: Option<String>,
        api_token: String,
    },
}

impl SignerSetup {
    pub fn kind(&self) -> SignerKind {
        match self {
            SignerSetup::Local { .. } => SignerKind::Local,
            SignerSetup::Turnkey { .. } => SignerKind::Turnkey,
            SignerSetup::Mpcvault { .. } => SignerKind::Mpcvault,
        }
    }
}

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

/// Hot-wallet convenience: write a config whose signer is the local key. Kept for
/// the CLI `stitch init` and existing callers; delegates to [`write_config_signer`].
pub fn write_config(
    dir: impl AsRef<Path>,
    corridor: &Corridor,
    key_raw: &str,
) -> Result<ConfigPaths> {
    write_config_signer(
        dir,
        corridor,
        &SignerSetup::Local {
            material: LocalKeyMaterial::PrivateKey(key_raw.to_string()),
        },
    )
}

/// Validate the signer, then write stitch.toml (the corridor template, plus a
/// `[signer]` section for MPC backends), stitch.env (pointing at the secret
/// file(s)), and the secret file itself — all owner-only. Nothing is written if
/// validation fails, so a bad input never leaves half a config.
pub fn write_config_signer(
    dir: impl AsRef<Path>,
    corridor: &Corridor,
    signer: &SignerSetup,
) -> Result<ConfigPaths> {
    validate_signer(signer)?;

    let paths = config_paths(dir.as_ref());
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;

    // Hot wallet keeps the template byte-for-byte; MPC backends get a [signer]
    // section appended (comments elsewhere preserved via toml_edit).
    let toml = match signer {
        SignerSetup::Local { .. } => corridor.toml_template.to_string(),
        _ => render_toml_with_signer(corridor.toml_template, signer)?,
    };

    // Stage the secret and env first, then commit the toml (which selects the
    // signer) last — all through atomic replaces. A failure on any earlier write
    // leaves the old toml still selecting the old, untouched signer, so the config
    // stays consistent. Drop the old signer's secrets only after everything commits.
    write_signer_secrets(&paths, signer)?;
    write_toml_atomic(&paths.env, &render_env_for(&paths, signer))?;
    restrict_to_owner(&paths.env)?;
    write_toml_atomic(&paths.toml, &toml)?;
    remove_other_secrets(&paths, signer);

    Ok(paths)
}

/// Change only the signer of an already-set-up folder: rewrite the `[signer]`
/// section (or remove it for the hot wallet), rewrite stitch.env, and write the
/// new secret file. Leaves corridor, spreads, and endpoints untouched. Used by
/// the Settings screen. Re-validates the whole config before touching disk.
pub fn apply_signer(dir: impl AsRef<Path>, signer: &SignerSetup) -> Result<()> {
    validate_signer(signer)?;
    let paths = config_paths(dir.as_ref());

    let current = std::fs::read_to_string(&paths.toml)
        .with_context(|| format!("reading {}", paths.toml.display()))?;
    let mut doc: toml_edit::DocumentMut = current
        .parse()
        .with_context(|| format!("{} is not valid TOML", paths.toml.display()))?;
    match signer {
        SignerSetup::Local { .. } => {
            doc.as_table_mut().remove("signer");
        }
        _ => {
            doc["signer"] = toml_edit::Item::Table(signer_table(signer));
        }
    }
    let updated = doc.to_string();
    Config::from_toml(&updated).context("the updated config is not valid")?;

    // Stage the secret and env first, then commit the toml (which selects the
    // signer) last — all atomic replaces — so a failure on the secret/env write
    // leaves the old toml still selecting the old, untouched signer. Drop the old
    // signer's secrets only after everything commits.
    write_signer_secrets(&paths, signer)?;
    write_toml_atomic(&paths.env, &render_env_for(&paths, signer))?;
    restrict_to_owner(&paths.env)?;
    write_toml_atomic(&paths.toml, &updated)?;
    remove_other_secrets(&paths, signer);
    Ok(())
}

/// Write a new corridor template into stitch.toml while preserving the existing
/// `[signer]` section, so switching corridor on an MPC config doesn't silently
/// drop the signer — which would leave stitch.env pointing at MPC credentials
/// while the config falls back to the hot wallet. The secret file and stitch.env
/// are unchanged and stay correct. A hot-wallet config (no `[signer]`) gets the
/// template byte-for-byte, exactly as before.
pub fn switch_corridor_preserving_signer(dir: impl AsRef<Path>, template: &str) -> Result<()> {
    let paths = config_paths(dir.as_ref());
    let existing_signer = std::fs::read_to_string(&paths.toml)
        .ok()
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|d| d.get("signer").cloned());
    match existing_signer {
        None => write_toml_atomic(&paths.toml, template),
        Some(signer) => {
            let mut doc: toml_edit::DocumentMut = template
                .parse()
                .context("corridor template is not valid TOML")?;
            doc["signer"] = signer;
            let updated = doc.to_string();
            Config::from_toml(&updated).context("the switched config is not valid")?;
            write_toml_atomic(&paths.toml, &updated)
        }
    }
}

/// Path of the owner-only secret file for a signer, next to stitch.toml.
fn secret_path(paths: &ConfigPaths, signer: &SignerSetup) -> PathBuf {
    match signer {
        SignerSetup::Local { .. } => paths.key.clone(),
        SignerSetup::Turnkey { .. } => paths.dir.join("turnkey-api.key"),
        SignerSetup::Mpcvault { .. } => paths.dir.join("mpcvault-api.token"),
    }
}

/// Delete the secret files that don't belong to `keep`, so switching signer never
/// leaves a stale hot-wallet key (or an old MPC token) sitting on disk. Runs after
/// the new secret is written, so it can't remove the one just created.
/// Best-effort: a missing file is fine.
fn remove_other_secrets(paths: &ConfigPaths, keep: &SignerSetup) {
    let kept = secret_path(paths, keep);
    for candidate in [
        paths.key.clone(),
        paths.dir.join("turnkey-api.key"),
        paths.dir.join("mpcvault-api.token"),
    ] {
        if candidate != kept {
            let _ = std::fs::remove_file(&candidate);
        }
    }
}

/// stitch.env for a signer: point the bot at the secret file(s) and set the log
/// level. Turnkey's API public key is not secret, so it goes inline.
fn render_env_for(paths: &ConfigPaths, signer: &SignerSetup) -> String {
    let q = |s: &str| shell_single_quote(s);
    let secret = q(&secret_path(paths, signer).display().to_string());
    let head = match signer {
        SignerSetup::Local { .. } => format!("STITCH_PRIVATE_KEY_FILE={secret}\n"),
        SignerSetup::Turnkey { api_public_key, .. } => format!(
            "TURNKEY_API_PUBLIC_KEY={}\nTURNKEY_API_PRIVATE_KEY_FILE={secret}\n",
            q(api_public_key.trim())
        ),
        SignerSetup::Mpcvault { .. } => format!("MPCVAULT_API_TOKEN_FILE={secret}\n"),
    };
    format!("{head}RUST_LOG=info\n")
}

/// Write the signer's secret to its owner-only file, atomically (stage owner-only,
/// then rename over the target). On a mid-write failure the previously working
/// secret is left intact rather than truncated — losing it locks the operator out
/// of signing, which matters most on the Settings rotation path.
fn write_signer_secrets(paths: &ConfigPaths, signer: &SignerSetup) -> Result<()> {
    // The hot wallet persists the derived/parsed private key (never the seed
    // phrase); the MPC backends persist their API secret verbatim.
    let mut line = match signer {
        SignerSetup::Local { material } => format!("{}\n", material.private_key_hex()?),
        SignerSetup::Turnkey {
            api_private_key, ..
        } => format!("{}\n", api_private_key.trim()),
        SignerSetup::Mpcvault { api_token, .. } => format!("{}\n", api_token.trim()),
    };
    let path = secret_path(paths, signer);
    let res = write_key_file_atomic(&path, line.as_bytes())
        .with_context(|| format!("writing {}", path.display()));
    line.zeroize();
    res
}

/// Render the corridor template with a `[signer]` section appended for an MPC
/// backend (Local is handled by the caller and never reaches here).
fn render_toml_with_signer(template: &str, signer: &SignerSetup) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = template
        .parse()
        .context("corridor template is not valid TOML")?;
    doc["signer"] = toml_edit::Item::Table(signer_table(signer));
    Ok(doc.to_string())
}

/// The `[signer]` table for an MPC backend. Only non-secret fields; secrets live
/// in the env/secret file. Optional fields are omitted when blank so the bot
/// falls back to its defaults.
fn signer_table(signer: &SignerSetup) -> toml_edit::Table {
    use toml_edit::value;
    let mut t = toml_edit::Table::new();
    let set_opt = |t: &mut toml_edit::Table, k: &str, v: &Option<String>| {
        if let Some(s) = v {
            let s = s.trim();
            if !s.is_empty() {
                t[k] = value(s);
            }
        }
    };
    match signer {
        SignerSetup::Turnkey {
            organization_id,
            sign_with,
            operator_address,
            api_base_url,
            ..
        } => {
            t["provider"] = value("turnkey");
            t["organization_id"] = value(organization_id.trim());
            t["sign_with"] = value(sign_with.trim());
            t["operator_address"] = value(operator_address.trim());
            set_opt(&mut t, "api_base_url", api_base_url);
        }
        SignerSetup::Mpcvault {
            vault_uuid,
            client_signer_pubkey,
            operator_address,
            api_base_url,
            callback_listen_addr,
            ..
        } => {
            t["provider"] = value("mpcvault");
            t["vault_uuid"] = value(vault_uuid.trim());
            t["client_signer_pubkey"] = value(client_signer_pubkey.trim());
            t["operator_address"] = value(operator_address.trim());
            set_opt(&mut t, "api_base_url", api_base_url);
            set_opt(&mut t, "callback_listen_addr", callback_listen_addr);
        }
        SignerSetup::Local { .. } => {}
    }
    t
}

/// Validate a signer's inputs before any file is touched. MPC backends need their
/// required non-secret fields plus a valid operator address and a non-empty secret.
fn validate_signer(signer: &SignerSetup) -> Result<()> {
    let need = |ok: bool, msg: &str| -> Result<()> {
        if ok {
            Ok(())
        } else {
            anyhow::bail!(msg.to_string())
        }
    };
    match signer {
        SignerSetup::Local { material } => {
            // Parses the key or derives from the seed phrase; either failing here
            // means nothing is written. The underlying error already names which.
            material.signing_key()?;
        }
        SignerSetup::Turnkey {
            organization_id,
            sign_with,
            operator_address,
            api_public_key,
            api_private_key,
            ..
        } => {
            need(
                !organization_id.trim().is_empty(),
                "organization id is required",
            )?;
            need(!sign_with.trim().is_empty(), "sign-with is required")?;
            parse_address(operator_address)
                .context("operator address is not a valid EVM address")?;
            need(
                !api_public_key.trim().is_empty(),
                "Turnkey API public key is required",
            )?;
            need(
                !api_private_key.trim().is_empty(),
                "Turnkey API private key is required",
            )?;
        }
        SignerSetup::Mpcvault {
            vault_uuid,
            client_signer_pubkey,
            operator_address,
            api_token,
            ..
        } => {
            need(!vault_uuid.trim().is_empty(), "vault UUID is required")?;
            need(
                !client_signer_pubkey.trim().is_empty(),
                "client-signer public key is required",
            )?;
            parse_address(operator_address)
                .context("operator address is not a valid EVM address")?;
            need(
                !api_token.trim().is_empty(),
                "MPCVault API token is required",
            )?;
        }
    }
    Ok(())
}

/// Rewrite ONLY the key file for an already-set-up folder, owner-only, and return
/// the operator address the new key controls. Leaves stitch.toml and stitch.env
/// untouched — the Settings screen uses this to swap the wallet in isolation.
/// The caller's key string should be zeroized after this returns.
pub fn write_key(dir: impl AsRef<Path>, key_raw: &str) -> Result<alloy_primitives::Address> {
    // Validate before touching disk, so a bad paste never truncates a good key.
    let key = parse_private_key(key_raw).context("the private key is not valid")?;
    let paths = config_paths(dir.as_ref());
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;
    let mut key_line = format!("{}\n", key_raw.trim());
    // Stage-then-rename: an interrupted write must never truncate the operator's
    // existing, working key — losing it locks them out of signing.
    write_key_file_atomic(&paths.key, key_line.as_bytes())
        .with_context(|| format!("writing {}", paths.key.display()))?;
    key_line.zeroize();
    Ok(crate::signer::address_from_signing_key(&key))
}

/// Write the key file atomically: stage the secret in an owner-only sibling temp
/// file, then rename it over the target. If the write fails, the existing key is
/// left intact rather than truncated or removed. `write_key_file` already creates
/// the temp owner-only on both platforms, so the secret is never world-readable.
fn write_key_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = key_tmp_path(path);
    write_key_file(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    replace_file(&tmp, path).map_err(|e| {
        // Best-effort cleanup so a failed rename doesn't strand the staged key.
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("replacing {}", path.display()))
    })?;
    Ok(())
}

/// The owner-only staging path next to a secret file, derived from its name (e.g.
/// `.turnkey-api.key.tmp`) so each secret stages to its own temp without collision.
fn key_tmp_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("stitch.key");
    dir.join(format!(".{name}.tmp"))
}

/// Replace a text file atomically: write a sibling temp file, then rename it over
/// the target so a crash mid-write can't leave a half-written config behind.
pub fn write_toml_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("stitch.toml");
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    replace_file(&tmp, path).with_context(|| {
        // Best-effort cleanup so a failed rename doesn't strand the temp file.
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })?;
    Ok(())
}

/// Rename `tmp` over `path`, replacing any existing file. `std::fs::rename`
/// replaces atomically on Unix; on Windows it can refuse to overwrite an existing
/// destination, surfacing an "already exists" error. Only in that specific case do
/// we remove the destination and retry — the staged content stays safe in `tmp`
/// throughout. A lock, permission, or other failure is propagated untouched, so we
/// never delete a working config or key when the retry couldn't have succeeded.
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(path)?;
            std::fs::rename(tmp, path)
        }
        Err(e) => Err(e),
    }
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

    // Hardhat/Anvil default mnemonic; its account 0 is exactly KEY above.
    const MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn write_config_signer_persists_the_key_derived_from_a_seed_phrase() {
        let dir = unique_dir("seed");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let signer = SignerSetup::Local {
            material: LocalKeyMaterial::SeedPhrase(MNEMONIC.into()),
        };
        let paths = write_config_signer(&dir, corridor, &signer).unwrap();
        // stitch.key holds the derived private key, never the phrase itself.
        let stored = std::fs::read_to_string(&paths.key).unwrap();
        assert_eq!(stored.trim().to_lowercase(), KEY.to_lowercase());
        assert!(
            !stored.contains("test"),
            "the seed phrase must not be written"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_config_signer_rejects_a_bad_seed_phrase_before_writing() {
        let dir = unique_dir("seed-bad");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let signer = SignerSetup::Local {
            material: LocalKeyMaterial::SeedPhrase("not a valid seed phrase".into()),
        };
        assert!(write_config_signer(&dir, corridor, &signer).is_err());
        assert!(
            !config_paths(&dir).toml.exists(),
            "nothing written on a bad phrase"
        );
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

    #[test]
    fn write_key_rewrites_only_the_key_and_returns_the_address() {
        let dir = unique_dir("rekey");
        // Seed a full config with a different key first.
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        write_config(&dir, corridor, KEY).unwrap();
        let toml_before = std::fs::read_to_string(config_paths(&dir).toml).unwrap();
        let env_before = std::fs::read_to_string(config_paths(&dir).env).unwrap();

        // Anvil/Hardhat account #1.
        const KEY2: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        let addr = write_key(&dir, KEY2).unwrap();
        assert_eq!(
            format!("{addr:?}").to_lowercase(),
            "0x70997970c51812dc3a010c7d01b50e0d17dc79c8"
        );
        assert_eq!(
            std::fs::read_to_string(config_paths(&dir).key)
                .unwrap()
                .trim(),
            KEY2
        );
        // The other two files are untouched.
        assert_eq!(
            std::fs::read_to_string(config_paths(&dir).toml).unwrap(),
            toml_before
        );
        assert_eq!(
            std::fs::read_to_string(config_paths(&dir).env).unwrap(),
            env_before
        );
        // The atomic staging file is renamed away, never stranded.
        assert!(!dir.join(".stitch.key.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_key_rejects_a_bad_key_without_touching_the_file() {
        let dir = unique_dir("rekey-bad");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        write_config(&dir, corridor, KEY).unwrap();
        assert!(write_key(&dir, "not-a-key").is_err());
        // Original key survives a rejected replacement.
        assert_eq!(
            std::fs::read_to_string(config_paths(&dir).key)
                .unwrap()
                .trim(),
            KEY
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_toml_atomic_replaces_contents_and_leaves_no_temp_file() {
        let dir = unique_dir("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stitch.toml");
        std::fs::write(&path, "old = 1\n").unwrap();
        write_toml_atomic(&path, "new = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 2\n");
        assert!(
            !dir.join(".stitch.toml.tmp").exists(),
            "temp file must be gone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // A valid checksummed EVM address (Anvil account #0) for MPC operator fields.
    const OPERATOR: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn mpcvault_config_emits_signer_section_and_token_file() {
        let dir = unique_dir("mpcv");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let signer = SignerSetup::Mpcvault {
            vault_uuid: "vault-123".into(),
            client_signer_pubkey: "ssh-ed25519 AAAA".into(),
            operator_address: OPERATOR.into(),
            api_base_url: None,
            callback_listen_addr: None,
            api_token: "tok-abc".into(),
        };
        let paths = write_config_signer(&dir, corridor, &signer).unwrap();
        let toml = std::fs::read_to_string(&paths.toml).unwrap();
        assert!(toml.contains("[signer]"));
        assert!(toml.contains("provider = \"mpcvault\""));
        assert!(toml.contains("vault_uuid = \"vault-123\""));
        // The whole config still parses through the real loader.
        Config::from_toml(&toml).unwrap();
        // The secret never lands in the TOML; it has its own owner-only file.
        assert!(!toml.contains("tok-abc"));
        assert_eq!(
            std::fs::read_to_string(dir.join("mpcvault-api.token"))
                .unwrap()
                .trim(),
            "tok-abc"
        );
        // The secret is staged then renamed; no per-target temp is left behind.
        assert!(!dir.join(".mpcvault-api.token.tmp").exists());
        let env = std::fs::read_to_string(&paths.env).unwrap();
        assert!(env.contains("MPCVAULT_API_TOKEN_FILE="));
        assert!(!env.contains("STITCH_PRIVATE_KEY_FILE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn turnkey_config_puts_public_key_in_env_and_private_in_a_file() {
        let dir = unique_dir("tk");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let signer = SignerSetup::Turnkey {
            organization_id: "org-1".into(),
            sign_with: OPERATOR.into(),
            operator_address: OPERATOR.into(),
            api_base_url: None,
            api_public_key: "PUBKEY".into(),
            api_private_key: "PRIVKEY".into(),
        };
        let paths = write_config_signer(&dir, corridor, &signer).unwrap();
        let toml = std::fs::read_to_string(&paths.toml).unwrap();
        assert!(toml.contains("provider = \"turnkey\""));
        assert!(!toml.contains("PRIVKEY") && !toml.contains("PUBKEY"));
        let env = std::fs::read_to_string(&paths.env).unwrap();
        assert!(env.contains("TURNKEY_API_PUBLIC_KEY='PUBKEY'"));
        assert!(env.contains("TURNKEY_API_PRIVATE_KEY_FILE="));
        assert_eq!(
            std::fs::read_to_string(dir.join("turnkey-api.key"))
                .unwrap()
                .trim(),
            "PRIVKEY"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn signer_config_rejects_a_bad_operator_address_before_writing() {
        let dir = unique_dir("badop");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let signer = SignerSetup::Mpcvault {
            vault_uuid: "v".into(),
            client_signer_pubkey: "k".into(),
            operator_address: "not-an-address".into(),
            api_base_url: None,
            callback_listen_addr: None,
            api_token: "t".into(),
        };
        assert!(write_config_signer(&dir, corridor, &signer).is_err());
        assert!(
            !config_paths(&dir).toml.exists(),
            "nothing written on bad input"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_signer_swaps_local_to_mpc_and_back_preserving_corridor() {
        let dir = unique_dir("swap");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        write_config(&dir, corridor, KEY).unwrap();

        apply_signer(
            &dir,
            &SignerSetup::Mpcvault {
                vault_uuid: "v1".into(),
                client_signer_pubkey: "k1".into(),
                operator_address: OPERATOR.into(),
                api_base_url: None,
                callback_listen_addr: None,
                api_token: "tok".into(),
            },
        )
        .unwrap();
        let toml = std::fs::read_to_string(config_paths(&dir).toml).unwrap();
        assert!(toml.contains("provider = \"mpcvault\""));
        assert!(toml.contains("chain_id"), "corridor fields preserved");
        assert!(std::fs::read_to_string(config_paths(&dir).env)
            .unwrap()
            .contains("MPCVAULT_API_TOKEN_FILE="));
        // Switching to MPC removes the stale hot-wallet key.
        assert!(
            !config_paths(&dir).key.exists(),
            "stale stitch.key removed after switching to MPC"
        );
        assert!(dir.join("mpcvault-api.token").exists());

        apply_signer(
            &dir,
            &SignerSetup::Local {
                material: LocalKeyMaterial::PrivateKey(KEY.into()),
            },
        )
        .unwrap();
        let toml2 = std::fs::read_to_string(config_paths(&dir).toml).unwrap();
        assert!(!toml2.contains("[signer]"), "signer removed for hot wallet");
        assert!(std::fs::read_to_string(config_paths(&dir).env)
            .unwrap()
            .contains("STITCH_PRIVATE_KEY_FILE="));
        // Switching back to the hot wallet removes the MPC token.
        assert!(config_paths(&dir).key.exists());
        assert!(
            !dir.join("mpcvault-api.token").exists(),
            "stale MPC token removed after switching back"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switch_corridor_keeps_the_mpc_signer() {
        let dir = unique_dir("switch-sig");
        let bsc = find_corridor("cngn-usdt-bsc").unwrap();
        write_config_signer(
            &dir,
            bsc,
            &SignerSetup::Mpcvault {
                vault_uuid: "v".into(),
                client_signer_pubkey: "k".into(),
                operator_address: OPERATOR.into(),
                api_base_url: None,
                callback_listen_addr: None,
                api_token: "tok".into(),
            },
        )
        .unwrap();
        let celo = find_corridor("brla-usdt-celo").unwrap();
        switch_corridor_preserving_signer(&dir, celo.toml_template).unwrap();
        let toml = std::fs::read_to_string(config_paths(&dir).toml).unwrap();
        // New corridor took effect (Celo chain id)...
        assert!(toml.contains("42220"), "switched to the Celo corridor");
        // ...and the MPC signer survived the switch.
        assert!(toml.contains("provider = \"mpcvault\""));
        assert!(toml.contains("vault_uuid = \"v\""));
        Config::from_toml(&toml).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switch_corridor_writes_template_verbatim_for_hot_wallet() {
        let dir = unique_dir("switch-hot");
        let bsc = find_corridor("cngn-usdt-bsc").unwrap();
        write_config(&dir, bsc, KEY).unwrap();
        let celo = find_corridor("brla-usdt-celo").unwrap();
        switch_corridor_preserving_signer(&dir, celo.toml_template).unwrap();
        assert_eq!(
            std::fs::read_to_string(config_paths(&dir).toml).unwrap(),
            celo.toml_template
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
