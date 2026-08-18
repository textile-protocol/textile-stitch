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

    /// MPCVault is still experimental (its live client-signer callback flow has
    /// not been validated against a paid vault), so the UI keeps warning on it.
    /// Turnkey is production-ready. The local hotwallet was never experimental.
    pub fn experimental(self) -> bool {
        matches!(self, SignerKind::Mpcvault)
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

/// Owner-only file holding the venue maker API key. Never written into TOML.
/// The bot also accepts `STITCH_RFQ_API_KEY` / `STITCH_RFQ_API_KEY_FILE`; this
/// sibling is what the panel writes so a Docker restart picks it up without
/// recreating the container (the run dir is already mounted).
pub const RFQ_API_KEY_FILE: &str = "rfq-api.key";

/// Env var that points at [`RFQ_API_KEY_FILE`]. Kept in `stitch.env` so a
/// process-mode bot that sources the file finds the key the same way it finds
/// the wallet. The raw key itself is never inlined here.
pub const RFQ_API_KEY_FILE_ENV: &str = "STITCH_RFQ_API_KEY_FILE";

/// Write the maker API key to [`RFQ_API_KEY_FILE`] (owner-only) and point
/// `stitch.env` at it. Empty input is refused so a blank paste can't wipe a
/// working key. The caller's string should be dropped after this returns.
pub fn write_rfq_api_key(dir: impl AsRef<Path>, key_raw: &str) -> Result<()> {
    let key = key_raw.trim();
    anyhow::ensure!(!key.is_empty(), "the RFQ API key can't be empty");
    let paths = config_paths(dir.as_ref());
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;
    let path = paths.dir.join(RFQ_API_KEY_FILE);
    let mut line = format!("{key}\n");
    write_key_file_atomic(&path, line.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    line.zeroize();
    point_env_at_rfq_key(&paths)
}

/// True when [`RFQ_API_KEY_FILE`] is sitting next to the config. Used by the
/// panel GET so the form can say "a key is saved" without ever reading it.
pub fn rfq_api_key_is_set(dir: impl AsRef<Path>) -> bool {
    dir.as_ref().join(RFQ_API_KEY_FILE).is_file()
}

/// Keep `STITCH_RFQ_API_KEY_FILE` in `stitch.env` after a signer rewrite, which
/// otherwise replaces the whole file from the signer template.
fn point_env_at_rfq_key(paths: &ConfigPaths) -> Result<()> {
    let key_path = paths.dir.join(RFQ_API_KEY_FILE);
    if !key_path.is_file() {
        return Ok(());
    }
    upsert_env_assignment(
        &paths.env,
        RFQ_API_KEY_FILE_ENV,
        &key_path.display().to_string(),
    )
}

/// Replace or append one `KEY='value'` line in an env file. Preserves every
/// other line (signer paths, comments, RUST_LOG). Creates the file when absent.
fn upsert_env_assignment(env_path: &Path, key: &str, value: &str) -> Result<()> {
    let assignment = format!("{key}={}\n", shell_single_quote(value));
    let existing = match std::fs::read_to_string(env_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("reading {}", env_path.display()))
        }
    };
    let mut out = String::new();
    let mut replaced = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if let Some((k, _)) = trimmed.split_once('=') {
            if k.trim() == key {
                if !replaced {
                    out.push_str(&assignment);
                    replaced = true;
                }
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        out.push_str(&assignment);
    }
    write_toml_atomic(env_path, &out)?;
    restrict_to_owner(env_path)?;
    Ok(())
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
    // Last line of defence, checked before anything touches disk. The CLI picker
    // and the panel's create handler both filter pending corridors already, but
    // they're two of several callers and the failure is silent: a config written
    // from a pending template points at the zero reactor, so the bot starts,
    // quotes, and is never fillable. Refusing at the single write point means a
    // new caller can't reintroduce that.
    if corridor.pending_deploy {
        anyhow::bail!(
            "the {} corridor on {} isn't deployed yet, so a bot can't quote it",
            corridor.display_name,
            corridor.network_label
        );
    }
    write_config_signer_from_toml(dir, corridor.toml_template, signer)
}

/// Write a config from an already-rendered `stitch.toml` body rather than a
/// catalog corridor. Same file-writing guarantees as [`write_config_signer`];
/// used by the panel's custom-corridor path, where the toml is built from
/// operator input instead of shipped as a preset.
///
/// The caller is responsible for the body being a valid config — the custom
/// renderer parses it before handing it here — because there is no catalog entry
/// to fall back on. There is no pending-deploy notion for a custom corridor: its
/// reactor is whatever the operator gave, validated non-zero at render time.
pub fn write_config_signer_from_toml(
    dir: impl AsRef<Path>,
    toml_template: &str,
    signer: &SignerSetup,
) -> Result<ConfigPaths> {
    validate_signer(signer)?;

    let paths = config_paths(dir.as_ref());
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;

    // Hot wallet keeps the template byte-for-byte; MPC backends get a [signer]
    // section appended (comments elsewhere preserved via toml_edit).
    let toml = match signer {
        SignerSetup::Local { .. } => toml_template.to_string(),
        _ => render_toml_with_signer(toml_template, signer)?,
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
    let paths = config_paths(dir.as_ref());
    let updated = prepared_signer_toml(&paths, signer)?;

    // Snapshot the whole set before touching any of it. Each write is atomic on its own,
    // but the set isn't: a same-backend credential rotation overwrites the existing secret
    // *in place*, so a later failure (the env chmod, the toml write) would leave a
    // half-applied signer — e.g. a new private key paired with the old public-key env —
    // with no way back. And `change_signer` has already removed the old container by the
    // time it calls this, so a partial write strands the operator with no container and no
    // intact signer. On any failure, roll the whole set back to the old signer.
    let backup = SignerBackup::capture(&[
        secret_path(&paths, signer),
        paths.env.clone(),
        paths.toml.clone(),
    ])?;

    // Stage the secret and env first, then commit the toml (which selects the signer)
    // last — all atomic replaces.
    let write = (|| -> Result<()> {
        write_signer_secrets(&paths, signer)?;
        write_toml_atomic(&paths.env, &render_env_for(&paths, signer))?;
        restrict_to_owner(&paths.env)?;
        // Signer rewrite replaces stitch.env wholesale. Put the RFQ key pointer
        // back so a later signer change doesn't silently drop RFQ auth.
        point_env_at_rfq_key(&paths)?;
        write_toml_atomic(&paths.toml, &updated)?;
        Ok(())
    })();

    if let Err(e) = write {
        backup.restore();
        return Err(e);
    }
    // Everything committed. Drop the old signer's now-unreferenced secrets.
    remove_other_secrets(&paths, signer);
    Ok(())
}

/// A snapshot of the files [`apply_signer`] is about to replace, so a failure partway
/// through can put the old signer back intact. `None` content means the file didn't
/// exist (a fresh bot), so restoring it means removing whatever was staged.
struct SignerBackup {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl SignerBackup {
    fn capture(paths: &[PathBuf]) -> Result<Self> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let content = match std::fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(anyhow::Error::new(e))
                        .with_context(|| format!("snapshotting {}", path.display()))
                }
            };
            files.push((path.clone(), content));
        }
        Ok(Self { files })
    }

    /// Best-effort: put each file back to its captured content, or remove one that didn't
    /// exist. Logs and keeps going on error — there's nothing better to do mid-rollback,
    /// and a clear log beats aborting the rollback half done.
    fn restore(&self) {
        for (path, content) in &self.files {
            let res = match content {
                Some(bytes) => write_key_file_atomic(path, bytes),
                None => std::fs::remove_file(path).or_else(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => Ok(()),
                    _ => Err(anyhow::Error::new(e)),
                }),
            };
            if let Err(e) = res {
                tracing::error!(
                    "couldn't roll {} back after a failed signer change: {e:#}",
                    path.display()
                );
            }
        }
    }
}

impl Drop for SignerBackup {
    fn drop(&mut self) {
        // The secret file's captured bytes are sensitive — don't leave them in freed heap.
        for (_, content) in &mut self.files {
            if let Some(bytes) = content {
                bytes.zeroize();
            }
        }
    }
}

/// Confirm a signer change would succeed, without writing anything: validate the
/// credentials and check that applying them to the config on disk still parses. The
/// panel's Change signer flow calls this *before* it removes the live container, so a
/// bad key — or a config that's invalid on disk — is caught while the bot is still up
/// rather than after it's been destroyed.
pub fn validate_signer_change(dir: impl AsRef<Path>, signer: &SignerSetup) -> Result<()> {
    prepared_signer_toml(&config_paths(dir.as_ref()), signer).map(|_| ())
}

/// The validated TOML a signer change would write: validate the credentials, apply the
/// signer to the config on disk, and confirm the result parses. No side effects.
fn prepared_signer_toml(paths: &ConfigPaths, signer: &SignerSetup) -> Result<String> {
    validate_signer(signer)?;
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
    Ok(updated)
}

/// Write a new corridor template into `dir/stitch.toml` while preserving the
/// existing `[signer]` section, so switching corridor on an MPC config doesn't
/// silently drop the signer — which would leave stitch.env pointing at MPC
/// credentials while the config falls back to the hot wallet. The secret file
/// and stitch.env are unchanged and stay correct. A hot-wallet config (no
/// `[signer]`) gets the template byte-for-byte, exactly as before.
///
/// Refuses when the current file is missing, unreadable, or not valid TOML —
/// swallowing those failures would look like "no `[signer]`" and overwrite an
/// MPC/Turnkey config with the bare hot-wallet template.
///
/// The desktop setup GUI always uses the standard filename. The panel also has
/// flat-layout bots whose mounted file is `stitch.<bot>.toml` — those must call
/// [`switch_corridor_file`] with the actual path.
pub fn switch_corridor_preserving_signer(dir: impl AsRef<Path>, template: &str) -> Result<()> {
    switch_corridor_file(&config_paths(dir.as_ref()).toml, template)
}

/// Write a corridor template into an existing config file, keeping `[signer]`.
///
/// Same rules as [`switch_corridor_preserving_signer`], but the caller names the
/// file — required for flat-layout panel bots that mount `stitch.<bot>.toml`
/// rather than `stitch.toml`.
pub fn switch_corridor_file(toml_path: &Path, template: &str) -> Result<()> {
    let current = std::fs::read_to_string(toml_path)
        .with_context(|| format!("reading {}", toml_path.display()))?;
    let existing: toml_edit::DocumentMut = current.parse().with_context(|| {
        format!(
            "{} is not valid TOML; fix or replace the config before switching corridor",
            toml_path.display()
        )
    })?;
    match existing.get("signer").cloned() {
        None => write_toml_atomic(toml_path, template),
        Some(signer) => {
            let mut doc: toml_edit::DocumentMut = template
                .parse()
                .context("corridor template is not valid TOML")?;
            doc["signer"] = signer;
            let updated = doc.to_string();
            Config::from_toml(&updated).context("the switched config is not valid")?;
            write_toml_atomic(toml_path, &updated)
        }
    }
}

/// Apply [`crate::setup::apply_rfq_default_preset`] to a file just written
/// from a corridor template (create or switch).
pub fn stamp_rfq_default_preset(toml_path: &Path) -> Result<()> {
    let current = std::fs::read_to_string(toml_path)
        .with_context(|| format!("reading {}", toml_path.display()))?;
    let next = crate::setup::apply_rfq_default_preset(&current)?;
    write_toml_atomic(toml_path, &next)
}

/// Path of the owner-only secret file for a signer, next to stitch.toml.
fn secret_path(paths: &ConfigPaths, signer: &SignerSetup) -> PathBuf {
    match signer {
        SignerSetup::Local { .. } => paths.key.clone(),
        SignerSetup::Turnkey { .. } => paths.dir.join("turnkey-api.key"),
        SignerSetup::Mpcvault { .. } => paths.dir.join("mpcvault-api.token"),
    }
}

/// The files [`apply_signer`] writes, as names relative to `dir`: `stitch.toml`,
/// `stitch.env`, and the backend's secret. Derived from the same mapping the writer
/// uses, so it can't drift. A signer change hands *exactly* these to the bot's uid — not
/// the whole directory — so a migrated bot's hand-placed backup or other retained file
/// keeps its ownership instead of being swept into the bot's reach.
pub fn signer_files(dir: impl AsRef<Path>, signer: &SignerSetup) -> Vec<String> {
    let paths = config_paths(dir.as_ref());
    [
        paths.toml.clone(),
        paths.env.clone(),
        secret_path(&paths, signer),
    ]
    .iter()
    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    .collect()
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
    fn write_rfq_api_key_is_owner_only_and_never_echoed_into_toml() {
        let dir = unique_dir("rfq-key");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stitch.env"),
            "STITCH_PRIVATE_KEY_FILE='x'\nRUST_LOG=info\n",
        )
        .unwrap();

        write_rfq_api_key(&dir, "  tx_live_secret  ").unwrap();
        assert!(rfq_api_key_is_set(&dir));
        let stored = std::fs::read_to_string(dir.join(RFQ_API_KEY_FILE)).unwrap();
        assert_eq!(stored.trim(), "tx_live_secret");
        let env = std::fs::read_to_string(dir.join("stitch.env")).unwrap();
        assert!(env.contains("STITCH_PRIVATE_KEY_FILE='x'"));
        assert!(env.contains(&format!(
            "{RFQ_API_KEY_FILE_ENV}='{}/{}'",
            dir.display(),
            RFQ_API_KEY_FILE
        )));
        assert!(
            !env.contains("tx_live_secret"),
            "the raw key must not land in stitch.env"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(RFQ_API_KEY_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "rfq-api.key must be owner-only");
        }

        assert!(write_rfq_api_key(&dir, "   ").is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(RFQ_API_KEY_FILE))
                .unwrap()
                .trim(),
            "tx_live_secret",
            "a blank paste must not wipe a working key"
        );
        std::fs::remove_dir_all(&dir).ok();
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

    /// The picker filters pending corridors, but the write point is the only
    /// place every caller funnels through — so it refuses too, and refuses
    /// before touching disk. A half-written config pointing at a zero reactor
    /// is worse than no config: the bot would start and quote into nothing.
    ///
    /// Built from a synthetic corridor rather than a catalog entry on purpose:
    /// pending is a temporary state, so pinning this to whichever corridor
    /// happens to be awaiting a deploy would silently stop testing the guard
    /// the moment that corridor went live.
    #[test]
    fn write_config_rejects_a_pending_corridor_before_writing() {
        let dir = unique_dir("pending");
        let live = find_corridor("cngn-usdt-bsc").unwrap();
        let pending = Corridor {
            pending_deploy: true,
            ..*live
        };

        let err = write_config(&dir, &pending, KEY).unwrap_err();
        assert!(
            err.to_string().contains("isn't deployed yet"),
            "unexpected error: {err}"
        );
        assert!(
            !config_paths(&dir).toml.exists(),
            "nothing written for a pending corridor"
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
    fn signer_files_lists_exactly_the_written_files_per_backend() {
        // The signer change hands over only these — never the whole directory — so the
        // set must match what `apply_signer` writes for each backend and nothing else.
        let dir = std::path::Path::new("/tmp/whatever");
        let local = signer_files(
            dir,
            &SignerSetup::Local {
                material: LocalKeyMaterial::PrivateKey(KEY.into()),
            },
        );
        assert_eq!(local, vec!["stitch.toml", "stitch.env", "stitch.key"]);

        let turnkey = signer_files(
            dir,
            &SignerSetup::Turnkey {
                organization_id: "org".into(),
                sign_with: OPERATOR.into(),
                operator_address: OPERATOR.into(),
                api_public_key: "pub".into(),
                api_private_key: "priv".into(),
                api_base_url: None,
            },
        );
        assert_eq!(
            turnkey,
            vec!["stitch.toml", "stitch.env", "turnkey-api.key"]
        );

        let mpc = signer_files(
            dir,
            &SignerSetup::Mpcvault {
                vault_uuid: "v".into(),
                client_signer_pubkey: "k".into(),
                operator_address: OPERATOR.into(),
                api_base_url: None,
                callback_listen_addr: None,
                api_token: "t".into(),
            },
        );
        assert_eq!(mpc, vec!["stitch.toml", "stitch.env", "mpcvault-api.token"]);
    }

    #[test]
    fn apply_signer_rolls_back_the_whole_set_when_a_write_fails() {
        // Same-backend credential rotation overwrites the secret in place, so a failure
        // after that write must not leave a half-applied signer (e.g. new private key,
        // old public-key env). Force the final toml write to fail and assert the secret,
        // env, and toml are all back to the previous signer.
        let dir = unique_dir("rollback");
        let corridor = find_corridor("cngn-usdt-bsc").unwrap();
        let turnkey = |org: &str, priv_key: &str, pub_key: &str| SignerSetup::Turnkey {
            organization_id: org.into(),
            sign_with: OPERATOR.into(),
            operator_address: OPERATOR.into(),
            api_base_url: None,
            api_public_key: pub_key.into(),
            api_private_key: priv_key.into(),
        };
        write_config_signer(&dir, corridor, &turnkey("ORG_A", "PRIV_A", "PUB_A")).unwrap();
        let paths = config_paths(&dir);
        let secret = dir.join("turnkey-api.key");
        let old_secret = std::fs::read(&secret).unwrap();
        let old_env = std::fs::read(&paths.env).unwrap();
        let old_toml = std::fs::read(&paths.toml).unwrap();

        // Occupy the toml write's temp path with a directory: the secret and env writes
        // land, but the final toml commit can't stage, so the write fails partway.
        std::fs::create_dir(dir.join(".stitch.toml.tmp")).unwrap();

        let err = apply_signer(&dir, &turnkey("ORG_B", "PRIV_B", "PUB_B")).unwrap_err();
        assert!(format!("{err:#}").contains("stitch.toml"), "{err:#}");

        // The whole set is back to the old signer — no half-applied credentials.
        assert_eq!(
            std::fs::read(&secret).unwrap(),
            old_secret,
            "secret rolled back"
        );
        assert_eq!(
            std::fs::read(&paths.env).unwrap(),
            old_env,
            "env rolled back"
        );
        assert_eq!(
            std::fs::read(&paths.toml).unwrap(),
            old_toml,
            "toml rolled back"
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
        let celo = find_corridor("wbrl-usdt-celo").unwrap();
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
        let celo = find_corridor("wbrl-usdt-celo").unwrap();
        switch_corridor_preserving_signer(&dir, celo.toml_template).unwrap();
        assert_eq!(
            std::fs::read_to_string(config_paths(&dir).toml).unwrap(),
            celo.toml_template
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switch_corridor_refuses_invalid_toml() {
        let dir = unique_dir("switch-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let toml = config_paths(&dir).toml;
        std::fs::write(&toml, "this is not [[[ valid toml").unwrap();
        let before = std::fs::read_to_string(&toml).unwrap();
        let celo = find_corridor("wbrl-usdt-celo").unwrap();
        let err = switch_corridor_preserving_signer(&dir, celo.toml_template).unwrap_err();
        assert!(
            err.to_string().contains("not valid TOML"),
            "expected parse refusal, got: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&toml).unwrap(),
            before,
            "invalid config must not be overwritten"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switch_corridor_refuses_missing_file() {
        let dir = unique_dir("switch-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let celo = find_corridor("wbrl-usdt-celo").unwrap();
        let err = switch_corridor_preserving_signer(&dir, celo.toml_template).unwrap_err();
        assert!(
            err.to_string().contains("reading"),
            "expected read refusal, got: {err:#}"
        );
        assert!(
            !config_paths(&dir).toml.exists(),
            "must not create a config when the prior file was missing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_dir("perms");
        let corridor = find_corridor("wbrl-usdt-celo").unwrap();
        let paths = write_config(&dir, corridor, KEY).unwrap();
        let mode = std::fs::metadata(&paths.key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
