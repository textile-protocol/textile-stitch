// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! How a Stitch bot container is specified: mounts, environment, labels.
//!
//! One definition, used by three callers that must agree — the wizard when it
//! creates a bot, the layout migration when it recreates one, and the compose
//! export when it writes the recovery file. If they disagreed, an operator who
//! fell back to the exported compose file would get a subtly different bot than
//! the panel was running.
//!
//! The layout is the one from the production compose file: the per-bot directory
//! mounted read-write at the run dir, with the config and the signer secret
//! re-mounted read-only on top. The directory has to be writable because the bot
//! writes its slot-nonce ledger next to the config, and that ledger has to reach
//! the host — a bot that loses it comes back with fresh nonces and cannot replace
//! its still-live orders until they expire.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::panel::config::PanelConfig;
use crate::panel::docker::{BindSpec, CreateSpec};
use crate::panel::inventory::{Bot, Layout, RUN_DIR};
use crate::panel::naming::{
    container_name, LABEL_BOT, LABEL_CORRIDOR, LABEL_LAYOUT, LABEL_ONE_SHOT,
};
use crate::setup::{self, SignerView, RFQ_API_KEY_FILE, RFQ_API_KEY_FILE_ENV};

/// Value of the layout label on containers the panel creates.
pub const LAYOUT_DIRECTORY: &str = "directory";

/// The secret file each signer backend reads, as named by
/// [`crate::setup::write_config_signer`].
const LOCAL_SECRET: &str = "stitch.key";
const TURNKEY_SECRET: &str = "turnkey-api.key";
const MPCVAULT_SECRET: &str = "mpcvault-api.token";

/// What the panel knows about a bot's on-disk config, as far as provisioning
/// cares: which signer it selects and therefore which secret file to mount and
/// which environment variables to set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerRuntime {
    /// Secret file name inside the bot directory.
    pub secret_file: String,
    /// Extra environment variables, already in `KEY=value` form.
    pub env: Vec<String>,
}

/// Work out the signer runtime for a bot directory by reading its config.
///
/// The environment is set explicitly rather than by sourcing the `stitch.env` the
/// writer produced: that file holds absolute *host* paths, which don't exist
/// inside the container. Only the non-secret Turnkey public key is lifted out of
/// it, because it has nowhere else to live.
pub fn signer_runtime(dir: &Path) -> Result<SignerRuntime> {
    signer_runtime_at(&setup::config_paths(dir).toml)
}

/// Work out the signer runtime from a bot's config file, whatever it is called.
///
/// The flat layout names files per bot (`stitch.bot1.toml`), so a bot adopted
/// from a compose file can't be read through its directory: there may be no
/// `stitch.toml` there, and if there is it belongs to a different bot.
/// A signer this can't determine is an error, never a guess at the hot wallet.
/// Everything downstream — the mounts a container gets, the env it starts with, the
/// service block written into a compose export — is wrong for a Turnkey or MPCVault
/// bot silently read as local, and wrong in a way that surfaces as "the bot won't
/// start" long after the decision was made.
pub fn signer_runtime_at(config: &Path) -> Result<SignerRuntime> {
    let toml =
        std::fs::read_to_string(config).with_context(|| format!("reading {}", config.display()))?;
    let signer = setup::try_read_signer(&toml)
        .with_context(|| format!("working out which signer {} uses", config.display()))?;
    Ok(with_rfq_key_env(
        signer_runtime_for(&signer, config),
        config,
    ))
}

/// Pure core of [`signer_runtime`], so the mapping from signer to mounts and env
/// is testable without a config on disk.
pub fn signer_runtime_from(signer: &SignerView, dir: &Path) -> SignerRuntime {
    with_rfq_key_env(
        signer_runtime_for(signer, &setup::config_paths(dir).toml),
        &setup::config_paths(dir).toml,
    )
}

/// As [`signer_runtime_from`], keyed on the config file rather than its
/// directory so per-bot names resolve to their own siblings.
pub fn signer_runtime_for(signer: &SignerView, config: &Path) -> SignerRuntime {
    // Paths in the environment are container paths: the bot dir is mounted at the
    // run dir, so a secret written beside stitch.toml lands there too.
    let in_container = |name: &str| format!("{RUN_DIR}/{name}");
    match signer {
        SignerView::Local => SignerRuntime {
            secret_file: LOCAL_SECRET.to_string(),
            env: vec![format!(
                "STITCH_PRIVATE_KEY_FILE={}",
                in_container(LOCAL_SECRET)
            )],
        },
        SignerView::Turnkey { .. } => {
            let mut env = vec![format!(
                "TURNKEY_API_PRIVATE_KEY_FILE={}",
                in_container(TURNKEY_SECRET)
            )];
            // The API public key isn't secret and the TOML has no home for it, so
            // the writer put it in stitch.env. Lifting it from there keeps one
            // source of truth; a missing value means the bot will fail to start
            // and say so, which beats guessing.
            if let Some(pubkey) = env_value(config, "TURNKEY_API_PUBLIC_KEY") {
                env.push(format!("TURNKEY_API_PUBLIC_KEY={pubkey}"));
            }
            SignerRuntime {
                secret_file: TURNKEY_SECRET.to_string(),
                env,
            }
        }
        SignerView::Mpcvault { .. } => SignerRuntime {
            secret_file: MPCVAULT_SECRET.to_string(),
            env: vec![format!(
                "MPCVAULT_API_TOKEN_FILE={}",
                in_container(MPCVAULT_SECRET)
            )],
        },
    }
}

/// If the panel has written `rfq-api.key` beside the config, point the
/// container at it. Existing containers created before the key was saved
/// still see the file via the run-dir mount; the bot also falls back to
/// that sibling path so a Settings save doesn't need a recreate.
fn with_rfq_key_env(mut runtime: SignerRuntime, config: &Path) -> SignerRuntime {
    let present = config
        .parent()
        .is_some_and(|dir| dir.join(RFQ_API_KEY_FILE).is_file());
    if present {
        runtime.env.push(format!(
            "{RFQ_API_KEY_FILE_ENV}={RUN_DIR}/{RFQ_API_KEY_FILE}"
        ));
    }
    runtime
}

/// Read one value out of the `stitch.env` beside a bot's config, undoing the
/// writer's POSIX single-quoting. Returns `None` when the file or key is absent.
fn env_value(config: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(find_beside(config, "stitch.env")?).ok()?;
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| unquote(v.trim()))
        .filter(|v| !v.is_empty())
}

/// Undo `'…'` POSIX single-quoting, including the `'\''` escape the writer emits.
fn unquote(value: &str) -> String {
    match value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        Some(inner) => inner.replace("'\\''", "'"),
        None => value.to_string(),
    }
}

/// The mounts for a bot, in the layout that keeps the nonce ledger on the host.
///
/// Ordering matters: Docker applies binds in order, so the directory must come
/// before the files re-mounted on top of it.
pub fn bot_mounts(host_dir: &Path, secret_file: &str) -> Vec<BindSpec> {
    vec![
        // Read-write: this is what makes the slot-nonce ledger survive recreation.
        BindSpec::rw(host_dir, RUN_DIR),
        BindSpec::ro(
            host_dir.join("stitch.toml"),
            format!("{RUN_DIR}/stitch.toml"),
        ),
        BindSpec::ro(
            host_dir.join(secret_file),
            format!("{RUN_DIR}/{secret_file}"),
        ),
    ]
}

/// The mounts for a bot still on the flat layout: the config and the secret as
/// two read-only file mounts, no writable run directory.
///
/// Reproducing this shape is only for describing a bot we adopted — it's the
/// layout that loses the nonce ledger, which is what [`bot_mounts`] fixes. The
/// sources keep their per-bot names (`stitch.bot1.toml`) while the destinations
/// are canonical, because that's what the bot inside the container expects.
pub fn flat_bot_mounts(config: &Path, secret: &Path, secret_file: &str) -> Vec<BindSpec> {
    vec![
        BindSpec::ro(config, format!("{RUN_DIR}/stitch.toml")),
        BindSpec::ro(secret, format!("{RUN_DIR}/{secret_file}")),
    ]
}

/// Give a bot's config directory, and everything in it, to the uid the bot image
/// runs as.
///
/// The panel needs the Docker socket, so it normally runs as root and everything
/// it writes lands root-owned. The bot runs as `stitch`: its entrypoint starts
/// with `chmod 700` on the run directory, which a non-owner cannot do, so it exits
/// before reading any config — and even without that it could neither read the
/// `0600` key nor write its nonce ledger. Handing ownership over is what makes a
/// panel-created bot able to start at all.
///
/// Files already owned by that uid are left alone, so a panel running as the same
/// user as the bot needs no privileges. Anything else is a hard error with the
/// command to fix it: a bot that can't start is worse than a create that refuses.
#[cfg(unix)]
pub fn hand_over_to_bot(dir: &Path, uid: u32) -> Result<()> {
    let mut paths = vec![dir.to_path_buf()];
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    paths.extend(entries.filter_map(|e| e.ok()).map(|e| e.path()));

    for path in paths {
        chown_to(&path, uid).with_context(|| {
            format!(
                "giving {} to uid {uid}, which is what the bot image runs as. The panel has to \
                 run as root to do this, or you can hand it over yourself with \
                 `chown -R {uid} {}` and retry.",
                path.display(),
                dir.display()
            )
        })?;
    }
    Ok(())
}

/// Hand the directory and exactly these entries to the bot's uid.
///
/// [`hand_over_to_bot`] sweeps the whole directory, which is right when the panel
/// created everything in it — the wizard's case. The layout migration is different:
/// `check` deliberately tolerates files an operator put in the target by hand (a
/// README, a backup, a ledger they recovered themselves), and chowning those hands
/// the bot's uid access to data the panel doesn't own — permanently, because a
/// rollback has no record of who owned them before.
///
/// The directory itself is unavoidable: the bot writes its ledger there, so it has
/// to own it. The caller is expected to remember the previous owner and put it back
/// if the migration rolls back.
#[cfg(unix)]
pub fn hand_over_paths_to_bot(dir: &Path, names: &[String], uid: u32) -> Result<()> {
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(names.iter().map(|n| dir.join(n)));
    for path in paths {
        // A staged name can be absent when an earlier step failed halfway; that's
        // the rollback's problem, not a reason to fail the handover.
        if !path.exists() {
            continue;
        }
        chown_to(&path, uid).with_context(|| {
            format!(
                "giving {} to uid {uid}, which is what the bot image runs as. The panel has to \
                 run as root to do this, or you can hand it over yourself with \
                 `chown -R {uid} {}` and retry.",
                path.display(),
                dir.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn hand_over_paths_to_bot(_dir: &Path, _names: &[String], _uid: u32) -> Result<()> {
    Ok(())
}

/// The uid owning a path, when that can be read at all.
#[cfg(unix)]
pub fn owner_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
pub fn owner_uid(_path: &Path) -> Option<u32> {
    None
}

/// Give a path back to its previous owner during a rollback.
///
/// Best-effort and silent on failure by design: it runs while another error is
/// already on its way to the operator, and a cleanup hiccup must not replace the
/// failure that actually matters. Logged so it isn't invisible.
#[cfg(unix)]
pub fn restore_owner(path: &Path, uid: u32) {
    if let Err(e) = chown_to(path, uid) {
        tracing::warn!(
            "couldn't give {} back to uid {uid} after a failed migration: {e:#}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
pub fn restore_owner(_path: &Path, _uid: u32) {}

#[cfg(unix)]
fn chown_to(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // `symlink_metadata` and `lchown`, never `metadata` and `chown`: those two follow
    // symlinks, so a link sitting in a bot directory would hand its *target* to the
    // bot's uid — a file anywhere on the host, chosen by whoever could write that
    // directory, chowned by a panel running as root. Nothing here ever wants to
    // follow a link: the only paths worth touching are the ones the panel wrote, and
    // it writes regular files.
    let meta = std::fs::symlink_metadata(path)?;
    // Already theirs: nothing to do, and no privileges needed.
    if meta.uid() == uid {
        return Ok(());
    }
    // Owner only. The run directory is 0700 and the secret 0600, so the group has
    // no access to leave alone or grant — and changing it would need the panel to
    // be a member of a group it has no reason to be in.
    std::os::unix::fs::lchown(path, Some(uid), None)?;
    Ok(())
}

/// Ownership isn't the same problem off Unix: Docker Desktop maps it for bind
/// mounts, so there's nothing to hand over.
#[cfg(not(unix))]
pub fn hand_over_to_bot(_dir: &Path, _uid: u32) -> Result<()> {
    Ok(())
}

/// One of a bot's companion files — its signer secret, its `stitch.env` —
/// sitting next to its config.
///
/// The name is derived from the config's own: `stitch.bot1.toml` has its key at
/// `stitch.bot1.key`, so an arbitrary naming scheme still resolves. The canonical
/// name is the fallback, and for a canonically named config the two are the same
/// thing.
///
/// Derived first, because the flat layout puts every bot's files in one
/// directory: a `stitch.key` sitting next to `stitch.bot1.toml` belongs to some
/// other bot, and preferring it would sign with the wrong wallet.
pub fn find_beside(config: &Path, canonical: &str) -> Option<PathBuf> {
    if let Some(path) = find_beside_derived(config, canonical) {
        return Some(path);
    }
    let dir = config.parent()?;
    let direct = dir.join(canonical);
    direct.exists().then_some(direct)
}

/// Like [`find_beside`], but never falls back to the bare canonical name.
///
/// Deleting a flat-layout bot must use this: after the derived key is gone,
/// [`find_beside`]'s fallback would pick up a neighbour's `stitch.key` /
/// `turnkey-api.key` in the shared bots directory and wipe the wrong secret.
pub fn find_beside_derived(config: &Path, canonical: &str) -> Option<PathBuf> {
    let dir = config.parent()?;
    let name = derived_name(config, canonical)?;
    let path = dir.join(name);
    path.exists().then_some(path)
}

/// `stitch.bot1.toml` + `stitch.key` -> `stitch.bot1.key`.
fn derived_name(config: &Path, canonical: &str) -> Option<String> {
    let stem = config.file_stem()?.to_str()?;
    let ext = Path::new(canonical).extension()?.to_str()?;
    Some(format!("{stem}.{ext}"))
}

/// The mounts a bot needs in whichever layout it currently uses, with every path
/// rooted at `base`: the directory holding its files as the *consumer* of the
/// spec sees them — the daemon's host view for a container, or `.` for a compose
/// file written beside them.
///
/// A flat-layout bot keeps the per-bot file names it was adopted with, so those
/// are read from the panel's view of its config rather than assumed canonical.
/// Assuming would mount a path that doesn't exist, or — when an unrelated
/// `stitch.toml` happens to share the directory — a different bot's config and
/// key.
pub fn mounts_for(bot: &Bot, base: &Path, signer: &SignerRuntime) -> Result<Vec<BindSpec>, String> {
    if bot.layout != Layout::FlatFiles {
        return Ok(bot_mounts(base, &signer.secret_file));
    }

    let config = bot
        .config_panel_path
        .as_ref()
        .ok_or_else(|| "the panel can't read its config file".to_string())?;
    let secret = find_beside(config, &signer.secret_file).ok_or_else(|| {
        format!(
            "its signer secret isn't next to {}, so the mount can't be named",
            file_name(config)
        )
    })?;
    Ok(flat_bot_mounts(
        &base.join(file_name(config)),
        &base.join(file_name(&secret)),
        &signer.secret_file,
    ))
}

/// Refuse a container whose file mounts aren't backed by real files, before Docker
/// invents them.
///
/// A missing bind-mount source is not an error to the daemon: it creates it, as a
/// **directory**. So a spec naming `<dir>/stitch.toml` on a host where that file
/// isn't there doesn't fail — it puts a folder where the operator's config should
/// be, the bot starts against nothing, and every later read fails with
/// "Is a directory (os error 21)". Nothing in the panel noticed, because from its
/// side the create succeeded.
///
/// The panel can see the same files through its own view of the bots root, so it
/// looks there and refuses first. Two things it will not do:
///
/// - Judge a path outside the mounted root. An adopted bot whose config lives
///   elsewhere is already reported as uneditable, and blocking on a path the panel
///   simply cannot see would refuse work that does succeed.
/// - Touch the run-directory mount. Docker creating *that* one is the intended
///   behaviour — it's a directory either way.
///
/// What it cannot catch: a `STITCH_PANEL_HOST_BOTS_DIR` that names the wrong
/// directory. [`PanelConfig::to_panel_path`] is defined as the inverse of that
/// mapping, so a wrong value maps straight back to a file the panel *can* see and
/// every check passes. Catching that needs the daemon's own answer — the panel
/// reading its own container's mount table — not the filesystem in front of it.
pub fn check_file_mounts(binds: &[BindSpec], cfg: &PanelConfig) -> Result<(), String> {
    let run_dir = Path::new(RUN_DIR);
    for bind in binds {
        if bind.container_path == run_dir || !bind.host_path.starts_with(&cfg.host_bots_dir) {
            continue;
        }
        let seen = cfg.to_panel_path(&bind.host_path);
        let host = bind.host_path.display();
        if seen.is_dir() {
            return Err(format!(
                "{host} is a directory on the host, not a file. That is what Docker leaves \
                 behind when a bind mount's source doesn't exist, so a container was created \
                 here while the file was missing. Remove the empty directory and put the real \
                 file back before trying again."
            ));
        }
        if !seen.exists() {
            return Err(format!(
                "{host} isn't there. Creating a container that mounts it would not fail — Docker \
                 would silently create it as a directory, and the bot would come up against an \
                 empty file it can't parse. Put the file back first."
            ));
        }
    }
    Ok(())
}

/// A path's final component, falling back to the whole path so a caller building
/// a message or a mount never silently loses it.
fn file_name(path: &Path) -> String {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.to_string(),
        None => path.display().to_string(),
    }
}

/// The image a bot runs today, falling back to the panel's configured image when
/// there's no container to read it from.
///
/// Anything that recreates an existing container has to preserve this. An adopted
/// bot can be pinned to a digest or built from a fork, and swapping the trading
/// binary underneath an operator who asked for something else — a layout
/// migration, say — is not a change they can see coming.
pub fn image_of(bot: &Bot, cfg: &PanelConfig) -> String {
    bot.image.clone().unwrap_or_else(|| cfg.bot_image.clone())
}

/// The full container spec for a bot. `image` is explicit because the caller
/// decides between the bot's own image and the panel's configured one; see
/// [`image_of`].
pub fn bot_container_spec(
    cfg: &PanelConfig,
    name: &str,
    image: &str,
    signer: &SignerRuntime,
    corridor_id: Option<&str>,
) -> CreateSpec {
    let host_dir = cfg.host_bot_dir(name);
    let mut labels = HashMap::from([
        (LABEL_BOT.to_string(), name.to_string()),
        (LABEL_LAYOUT.to_string(), LAYOUT_DIRECTORY.to_string()),
    ]);
    if let Some(id) = corridor_id {
        labels.insert(LABEL_CORRIDOR.to_string(), id.to_string());
    }

    let mut env = vec!["RUST_LOG=info".to_string()];
    env.extend(signer.env.iter().cloned());

    CreateSpec {
        name: container_name(name),
        image: image.to_string(),
        labels,
        env,
        binds: bot_mounts(&host_dir, &signer.secret_file),
        // The image's own CMD points the bot at the mounted config.
        cmd: None,
        // A host reboot brings bots back; an operator's deliberate stop sticks.
        restart_unless_stopped: true,
    }
}

/// What a one-shot run does. Both are read-mostly operations an operator wants to
/// run before putting orders on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneShot {
    /// Grant the Permit2 allowances the bot needs to trade. Sends transactions.
    Approve,
    /// Load the config, price a tick and print what it would post, without
    /// signing or submitting anything.
    DryRun,
}

impl OneShot {
    fn command(self) -> Vec<String> {
        let config = format!("{RUN_DIR}/stitch.toml");
        match self {
            OneShot::Approve => vec!["stitch".into(), "approve".into(), "--config".into(), config],
            OneShot::DryRun => vec![
                "stitch".into(),
                "--config".into(),
                config,
                "--dry-run".into(),
            ],
        }
    }

    /// Container name prefix, so a leftover one-shot is obvious in `docker ps -a`
    /// and can't collide with the bot itself.
    fn name_prefix(self) -> &'static str {
        match self {
            OneShot::Approve => "stitch-approve",
            OneShot::DryRun => "stitch-dryrun",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OneShot::Approve => "approve",
            OneShot::DryRun => "dry-run",
        }
    }
}

/// A throwaway container that runs one command against a bot's config.
///
/// It gets the same mounts as the bot, because approve has to sign with the same
/// key and a dry run has to read the same config. It carries a one-shot label
/// instead of a bot label and no restart policy, so discovery skips it and the
/// daemon never resurrects it.
///
/// The binds and the image are passed in rather than derived from `cfg`, because
/// an adopted bot's files can be anywhere on the host in either layout, and what
/// a dry run tells you is only true of the binary the bot actually runs.
pub fn one_shot_spec(
    image: &str,
    binds: Vec<BindSpec>,
    name: &str,
    signer: &SignerRuntime,
    what: OneShot,
) -> CreateSpec {
    let mut env = vec!["RUST_LOG=info".to_string()];
    env.extend(signer.env.iter().cloned());
    CreateSpec {
        // The suffix keeps a leftover container from a crashed run — or a second
        // operator clicking at the same moment — from colliding on the name.
        name: format!("{}-{name}-{}", what.name_prefix(), run_suffix()),
        image: image.to_string(),
        labels: HashMap::from([(
            LABEL_ONE_SHOT.to_string(),
            format!("{name}:{}", what.as_str()),
        )]),
        env,
        binds,
        cmd: Some(what.command()),
        restart_unless_stopped: false,
    }
}

/// A short, non-secret uniquifier for one-shot container names.
fn run_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!("{nanos:08x}")
}

/// The bot directory's signer secret path, for permission checks and migration.
pub fn secret_path(dir: &Path, signer: &SignerRuntime) -> PathBuf {
    dir.join(&signer.secret_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::MountInfo;
    use crate::panel::inventory::{layout_of, Layout};

    const IMAGE: &str = "ghcr.io/textile-protocol/textile-stitch:sha-abc";

    fn cfg() -> PanelConfig {
        let mut cfg = PanelConfig::for_test("/data/bots", "/home/ec2-user/stitch");
        cfg.bot_image = IMAGE.into();
        cfg
    }

    fn local() -> SignerRuntime {
        signer_runtime_from(&SignerView::Local, Path::new("/data/bots/bot-a"))
    }

    /// Mounts as the daemon would report them back, so the spec can be checked
    /// against the same layout detector the inventory uses.
    fn as_reported(binds: &[BindSpec]) -> Vec<MountInfo> {
        binds
            .iter()
            .map(|b| MountInfo {
                source: b.host_path.clone(),
                destination: b.container_path.clone(),
                rw: !b.read_only,
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn handing_a_directory_to_a_uid_we_cannot_become_fails_with_the_fix() {
        // The panel normally runs as root and this is a no-op-or-succeed. When it
        // doesn't have the privilege, a bot created anyway would exit on startup
        // against files it can't read — so this has to fail loudly, and say how to
        // unblock it.
        let dir = std::env::temp_dir().join(format!("stitch-chown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stitch.toml"), "x").unwrap();

        // Already ours: nothing to do, no privileges needed.
        let ours = crate::panel::config::current_uid();
        assert!(hand_over_to_bot(&dir, ours).is_ok());

        if ours != 0 {
            let err = hand_over_to_bot(&dir, ours + 1).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("what the bot image runs as"), "{msg}");
            assert!(msg.contains("chown -R"), "{msg}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_handover_never_follows_a_symlink_out_of_the_directory() {
        // `metadata`/`chown` follow links, so a link in a bot directory would have
        // handed its *target* to the bot's uid — any file on the host, picked by
        // whoever could write that directory, chowned by a panel running as root.
        let dir = std::env::temp_dir().join(format!("stitch-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "not the bot's").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let before = owner_uid(&outside);
        // Hand over the link itself. Whatever happens to the link, the target's
        // ownership must not move.
        let _ = chown_to(&link, crate::panel::config::current_uid() + 1);
        assert_eq!(
            owner_uid(&outside),
            before,
            "the link's target must never change hands"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_config_that_docker_turned_into_a_directory_is_refused() {
        // The exact damage: `stitch.toml` exists as a directory because a container was
        // once created with that path as a bind source while the file was missing.
        // Mounting it again would keep the bot broken and tell the operator nothing.
        let root = std::env::temp_dir().join(format!("stitch-cfm-dir-{}", std::process::id()));
        let dir = root.join("bot-a");
        std::fs::create_dir_all(dir.join("stitch.toml")).unwrap();
        std::fs::write(dir.join("stitch.key"), "k").unwrap();
        let cfg = PanelConfig::for_test(&root, &root);

        let binds = bot_mounts(&cfg.host_bot_dir("bot-a"), "stitch.key");
        let err = check_file_mounts(&binds, &cfg).unwrap_err();
        assert!(err.contains("is a directory"), "{err}");
        assert!(err.contains("Remove the empty directory"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_mount_source_the_host_hasnt_got_is_refused_before_docker_invents_it() {
        // Docker would create this as a directory rather than fail, which is how the
        // case above happens in the first place.
        let root = std::env::temp_dir().join(format!("stitch-cfm-missing-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bot-a")).unwrap();
        let cfg = PanelConfig::for_test(&root, &root);

        let binds = bot_mounts(&cfg.host_bot_dir("bot-a"), "stitch.key");
        let err = check_file_mounts(&binds, &cfg).unwrap_err();
        assert!(err.contains("isn't there"), "{err}");
        assert!(err.contains("silently create it as a directory"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn real_files_pass_and_paths_the_panel_cannot_see_are_left_alone() {
        let root = std::env::temp_dir().join(format!("stitch-cfm-ok-{}", std::process::id()));
        let dir = root.join("bot-a");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stitch.toml"), "x").unwrap();
        std::fs::write(dir.join("stitch.key"), "k").unwrap();
        let cfg = PanelConfig::for_test(&root, &root);
        assert!(
            check_file_mounts(&bot_mounts(&cfg.host_bot_dir("bot-a"), "stitch.key"), &cfg).is_ok()
        );

        // An adopted bot outside the mounted root: the panel can't judge it, and
        // refusing would block work that does succeed.
        let outside = bot_mounts(Path::new("/somewhere/else/bot-x"), "stitch.key");
        assert!(check_file_mounts(&outside, &cfg).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_secret_is_found_under_the_flat_layouts_per_bot_name() {
        let dir = std::env::temp_dir().join(format!("stitch-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("stitch.bot1.toml");
        std::fs::write(&config, "x").unwrap();
        std::fs::write(dir.join("stitch.bot1.key"), "x").unwrap();
        assert_eq!(
            find_beside(&config, "stitch.key"),
            Some(dir.join("stitch.bot1.key"))
        );

        // A canonical key sharing the directory belongs to another bot, so the
        // one named after this config still wins. Signing with the neighbour's
        // wallet would post orders from the wrong address.
        std::fs::write(dir.join("stitch.key"), "x").unwrap();
        assert_eq!(
            find_beside(&config, "stitch.key"),
            Some(dir.join("stitch.bot1.key"))
        );

        // With nothing derived to find, the canonical name is the fallback.
        std::fs::remove_file(dir.join("stitch.bot1.key")).unwrap();
        assert_eq!(
            find_beside(&config, "stitch.key"),
            Some(dir.join("stitch.key"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_flat_layout_mounts_are_the_shape_that_loses_the_ledger() {
        // Reproducing the adopted layout faithfully means reproducing its flaw,
        // so assert it through the same detector the inventory uses.
        let mounts = flat_bot_mounts(
            Path::new("./stitch.bot1.toml"),
            Path::new("./stitch.bot1.key"),
            "stitch.key",
        );
        assert_eq!(layout_of(&as_reported(&mounts)), Layout::FlatFiles);
        // Sources keep the per-bot names; destinations are what the bot expects.
        assert_eq!(
            mounts[0].to_bind_string().unwrap(),
            "./stitch.bot1.toml:/home/stitch/run/stitch.toml:ro"
        );
        assert_eq!(
            mounts[1].to_bind_string().unwrap(),
            "./stitch.bot1.key:/home/stitch/run/stitch.key:ro"
        );
    }

    #[test]
    fn the_spec_produces_the_layout_that_persists_the_ledger() {
        // This is the whole point of the layout, so assert it through the same
        // detector the inventory uses rather than by eyeballing the binds.
        let spec = bot_container_spec(&cfg(), "bot-a", IMAGE, &local(), Some("cngn-usdt-bsc"));
        assert_eq!(layout_of(&as_reported(&spec.binds)), Layout::Directory);
    }

    #[test]
    fn mounts_use_the_host_view_of_the_bot_directory() {
        // A path that's correct inside the panel container is wrong in a mount
        // spec: the daemon resolves it on the host.
        let spec = bot_container_spec(&cfg(), "bot-a", IMAGE, &local(), None);
        assert_eq!(
            spec.binds[0].host_path,
            PathBuf::from("/home/ec2-user/stitch/bot-a")
        );
        assert!(
            !spec
                .binds
                .iter()
                .any(|b| b.host_path.starts_with("/data/bots")),
            "no panel-internal path may reach a mount spec"
        );
    }

    #[test]
    fn the_directory_mount_comes_before_the_files_layered_on_it() {
        let spec = bot_container_spec(&cfg(), "bot-a", IMAGE, &local(), None);
        assert_eq!(spec.binds[0].container_path, PathBuf::from(RUN_DIR));
        assert!(!spec.binds[0].read_only);
        assert!(spec.binds[1..].iter().all(|b| b.read_only));
    }

    #[test]
    fn a_created_bot_carries_the_labels_that_make_it_discoverable() {
        let spec = bot_container_spec(&cfg(), "bot-a", IMAGE, &local(), Some("cngn-usdt-bsc"));
        assert_eq!(spec.name, "stitch-bot-a");
        assert_eq!(
            spec.labels.get(LABEL_BOT).map(String::as_str),
            Some("bot-a")
        );
        assert_eq!(
            spec.labels.get(LABEL_CORRIDOR).map(String::as_str),
            Some("cngn-usdt-bsc")
        );
        assert_eq!(
            spec.labels.get(LABEL_LAYOUT).map(String::as_str),
            Some(LAYOUT_DIRECTORY)
        );
        // No compose project label, so `docker compose down --remove-orphans` in
        // the operator's old project can't sweep a panel-created bot away.
        assert!(!spec.labels.contains_key("com.docker.compose.project"));
        assert!(spec.restart_unless_stopped);
    }

    #[test]
    fn each_signer_backend_mounts_its_own_secret_and_points_the_env_at_it() {
        let dir = Path::new("/data/bots/bot-a");

        let l = signer_runtime_from(&SignerView::Local, dir);
        assert_eq!(l.secret_file, "stitch.key");
        assert_eq!(
            l.env,
            vec!["STITCH_PRIVATE_KEY_FILE=/home/stitch/run/stitch.key"]
        );

        let m = signer_runtime_from(
            &SignerView::Mpcvault {
                vault_uuid: "v".into(),
                client_signer_pubkey: "k".into(),
                operator_address: "0x0".into(),
                api_base_url: String::new(),
                callback_listen_addr: String::new(),
            },
            dir,
        );
        assert_eq!(m.secret_file, "mpcvault-api.token");
        assert_eq!(
            m.env,
            vec!["MPCVAULT_API_TOKEN_FILE=/home/stitch/run/mpcvault-api.token"]
        );

        // Every backend's env points inside the container, never at a host path.
        for rt in [l, m] {
            for var in &rt.env {
                assert!(
                    var.contains(RUN_DIR),
                    "{var} must reference the container run dir"
                );
            }
        }
    }

    #[test]
    fn turnkeys_public_key_is_lifted_out_of_stitch_env() {
        let dir = std::env::temp_dir().join(format!("stitch-prov-tk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The writer emits this POSIX-quoted.
        std::fs::write(
            dir.join("stitch.env"),
            "TURNKEY_API_PUBLIC_KEY='02abc'\nRUST_LOG=info\n",
        )
        .unwrap();

        let rt = signer_runtime_from(
            &SignerView::Turnkey {
                organization_id: "org".into(),
                sign_with: "0x0".into(),
                operator_address: "0x0".into(),
                api_base_url: String::new(),
            },
            &dir,
        );
        assert_eq!(rt.secret_file, "turnkey-api.key");
        assert!(rt.env.contains(&"TURNKEY_API_PUBLIC_KEY=02abc".to_string()));
        assert!(rt.env.contains(
            &"TURNKEY_API_PRIVATE_KEY_FILE=/home/stitch/run/turnkey-api.key".to_string()
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_turnkey_public_key_is_omitted_rather_than_guessed() {
        // Better that the bot fails at startup with its own clear error than the
        // panel invents a value.
        let rt = signer_runtime_from(
            &SignerView::Turnkey {
                organization_id: "org".into(),
                sign_with: "0x0".into(),
                operator_address: "0x0".into(),
                api_base_url: String::new(),
            },
            Path::new("/definitely/not/here"),
        );
        assert!(!rt
            .env
            .iter()
            .any(|e| e.starts_with("TURNKEY_API_PUBLIC_KEY")));
    }

    #[test]
    fn env_values_survive_the_writers_quoting() {
        assert_eq!(unquote("'plain'"), "plain");
        assert_eq!(unquote("'with space'"), "with space");
        assert_eq!(unquote("'it'\\''s'"), "it's");
        // Unquoted values pass through, which is how RUST_LOG is written.
        assert_eq!(unquote("info"), "info");
    }

    #[test]
    fn a_one_shot_shares_the_bots_mounts_but_not_its_identity() {
        let cfg = cfg();
        let bot = bot_container_spec(&cfg, "bot-a", IMAGE, &local(), None);
        let approve = one_shot_spec(
            IMAGE,
            bot.binds.clone(),
            "bot-a",
            &local(),
            OneShot::Approve,
        );

        // Same mounts: approve has to sign with the same key.
        assert_eq!(approve.binds, bot.binds);
        // Different container name, so it can't collide with the bot.
        assert_ne!(approve.name, bot.name);
        assert!(approve.name.contains("bot-a"));
        // Never resurrected by the daemon.
        assert!(!approve.restart_unless_stopped);
        // And it is not mistaken for the bot by discovery.
        assert_ne!(
            approve.labels.get(LABEL_BOT).map(String::as_str),
            Some("bot-a")
        );
    }

    #[test]
    fn the_one_shot_commands_point_at_the_mounted_config() {
        let cfg = cfg();
        let binds = bot_mounts(&cfg.host_bot_dir("bot-a"), "stitch.key");
        let approve = one_shot_spec(IMAGE, binds.clone(), "bot-a", &local(), OneShot::Approve)
            .cmd
            .unwrap();
        assert_eq!(approve[0], "stitch");
        assert!(approve.contains(&"approve".to_string()));
        assert!(approve.contains(&format!("{RUN_DIR}/stitch.toml")));

        let dry = one_shot_spec(IMAGE, binds, "bot-a", &local(), OneShot::DryRun)
            .cmd
            .unwrap();
        assert!(dry.contains(&"--dry-run".to_string()));
        assert!(
            !dry.contains(&"approve".to_string()),
            "a dry run must never send an approval transaction"
        );
    }

    #[test]
    fn rfq_api_key_file_is_pointed_at_inside_the_container() {
        let dir =
            std::env::temp_dir().join(format!("stitch-prov-rfq-{}-{}", std::process::id(), "key"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stitch.toml"), "rpc_url = \"http://x\"\n").unwrap();
        std::fs::write(dir.join(RFQ_API_KEY_FILE), "tx_live_x\n").unwrap();
        let rt = signer_runtime_from(&SignerView::Local, &dir);
        assert!(rt.env.contains(&format!(
            "{RFQ_API_KEY_FILE_ENV}={RUN_DIR}/{RFQ_API_KEY_FILE}"
        )));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn signer_runtime_reads_a_real_config_directory() {
        let dir = std::env::temp_dir().join(format!("stitch-prov-real-{}", std::process::id()));
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(
            &dir,
            corridor,
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let rt = signer_runtime(&dir).unwrap();
        assert_eq!(rt.secret_file, "stitch.key");
        assert!(secret_path(&dir, &rt).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn signer_runtime_fails_loudly_when_there_is_no_config() {
        assert!(signer_runtime(Path::new("/definitely/not/here")).is_err());
    }
}
