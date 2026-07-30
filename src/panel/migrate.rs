// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Move a bot from the flat-file mount layout to the per-bot directory layout.
//!
//! The flat layout — mounting only `stitch.toml` and the key, as
//! `docker-compose.example.yml` does — leaves `/home/stitch/run` inside the
//! container. The bot writes its slot-nonce ledger next to its config, so in that
//! layout the ledger never reaches the host and is destroyed every time the
//! container is recreated. A bot that comes back without its ledger mints fresh
//! nonces and cannot replace the orders it already has live, so those orders sit
//! unmanaged on the book until they expire.
//!
//! Migration therefore has to do more than move two files. The ledger is live
//! state that exists only inside the running container, so it is copied out
//! through the Docker archive API before the container is replaced. When that
//! isn't possible, the caller is told plainly rather than the loss being papered
//! over.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::panel::config::PanelConfig;
use crate::panel::docker::{ContainerState, CreateSpec, DockerApi, STOP_GRACE_SECS};
use crate::panel::inventory::{Bot, Layout, RUN_DIR};
use crate::panel::provision::{self, bot_container_spec, find_beside, image_of, signer_runtime_at};
use crate::setup;

/// Suffix of the on-disk slot-nonce ledger, as written by [`crate::slots`]:
/// `stitch.<chain_id>.<maker_address>.slot-nonces.json`.
const LEDGER_SUFFIX: &str = ".slot-nonces.json";

/// What a migration did, so the operator gets told rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub name: String,
    /// Directory the bot's files now live in, as the panel sees it.
    pub dir: PathBuf,
    /// Files moved from the old flat location.
    pub moved: Vec<String>,
    /// Ledger files recovered from the old container's filesystem.
    pub ledgers_recovered: Vec<String>,
    /// Set when the ledger could not be recovered, with the reason. The bot will
    /// come up with fresh nonces and can't replace already-live orders until they
    /// expire.
    ///
    /// Only ever populated under [`OnLedgerLoss::Accept`] — the default aborts
    /// instead, because until the old container is removed the ledger is still
    /// there to retry for.
    pub ledger_loss: Option<String>,
    /// Whether the new container was started. It is, unless the old one was in a
    /// terminal state — a bot the operator had stopped stays stopped.
    pub started: bool,
}

impl MigrationReport {
    /// A short operator-facing summary.
    pub fn message(&self) -> String {
        let mut parts = vec![format!(
            "{} now uses the per-bot directory layout at {}, so its nonce ledger survives \
             container recreation.",
            self.name,
            self.dir.display()
        )];
        if !self.ledgers_recovered.is_empty() {
            parts.push(format!(
                "Recovered {} ledger file(s) from the old container.",
                self.ledgers_recovered.len()
            ));
        }
        if let Some(reason) = &self.ledger_loss {
            parts.push(format!(
                "The existing nonce ledger could not be recovered ({reason}). Any orders that \
                 were live before the migration will stay on the book until they expire; the \
                 bot can't replace them."
            ));
        }
        if !self.started {
            parts.push("The bot was already stopped, so it was left stopped.".to_string());
        }
        parts.join(" ")
    }
}

/// What to do when the old container's nonce ledger can't be read.
///
/// In the flat layout that container holds the only current copy, and step 6 of
/// [`migrate`] destroys it. So the read failing is not a detail to note in the
/// report — until the remove happens it is still recoverable, and after it never
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnLedgerLoss {
    /// Roll back and let the operator retry. The default, because a transient
    /// daemon error must not be the reason a bot loses the nonces for orders that
    /// are live on chain.
    #[default]
    Abort,
    /// Migrate without the ledger and report what it costs. The operator's
    /// explicit choice, for a ledger that genuinely can't be read however many
    /// times they retry — an adopted bot on a custom image with no [`RUN_DIR`], or
    /// a run directory too large to pull through the archive API.
    Accept,
}

/// Reading a file out of a stopped-or-running container. Separate from
/// [`DockerApi`] because only the migration needs it, and because the tar
/// unpacking it implies is worth keeping out of the main Docker surface.
#[async_trait::async_trait]
pub trait ContainerFiles: Send + Sync {
    /// Files directly inside `dir` in the container, as `(file name, contents)`.
    /// Subdirectories are skipped. An unreadable path is an error, not an empty
    /// list, so a migration can report the loss honestly.
    async fn read_dir(&self, container: &str, dir: &str) -> Result<Vec<(String, Vec<u8>)>>;
}

/// Whether the migration can get this container to a state where copying the
/// ledger is sound.
///
/// The ledger is live state inside the container, so the copy only means anything
/// once nothing can write it. Two shapes qualify. A terminal container (created /
/// exited / dead) has no process at all, so it's already quiet. A `running` or
/// `restarting` one can be stopped gracefully, and the grace period is what lets
/// the current tick finish and flush the nonces it just used.
///
/// Note what `restarting` is doing on the safe side and *not* on the "already
/// stopped" side: between restart attempts there's no live process, so
/// [`ContainerState::is_running`] is false, but the daemon will start another one
/// the moment the backoff elapses. Skipping the stop for it — which is what
/// keying off `is_running` did — means a tick can advance the ledger while it's
/// being copied, and the remove that follows can land during an exited interval,
/// leaving a replacement holding a ledger that's already stale.
///
/// The rest are refused rather than guessed at:
///
/// - `paused` is frozen mid-tick and cannot handle SIGTERM, so a graceful stop
///   degenerates into SIGKILL after the grace period and the in-flight post's
///   nonce may never reach the ledger.
/// - `removing` is on its way out; there is nothing left to recover from.
/// - `unknown` is a daemon state this build doesn't recognise, and the entire
///   point of [`ContainerState::Unknown`] is to not assume it's idle.
fn quiesceable(state: ContainerState) -> bool {
    state.is_terminal() || state.wants_to_be_up()
}

/// Check whether a bot can be migrated, returning why not if it can't.
///
/// Split out from [`migrate`] so the UI can offer the action only when it will
/// work, and explain the blockage when it won't.
pub fn check(bot: &Bot, cfg: &PanelConfig) -> Result<()> {
    if bot.layout == Layout::Directory {
        bail!("{} already uses the per-bot directory layout", bot.name);
    }
    if bot.layout == Layout::Unknown {
        bail!(
            "{} has no config file mounted, so there is nothing to move. Its config is \
             probably baked into a custom image.",
            bot.name
        );
    }
    let source = bot.config_panel_path.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "the panel can't read {}'s config, so it can't move it. Mount its directory into \
             the panel first.",
            bot.name
        )
    })?;
    if !source.exists() {
        bail!("{} is missing at {}", bot.name, source.display());
    }
    if !quiesceable(bot.state) {
        bail!(
            "{} is {}, and the panel can't get it to a state where copying its nonce ledger is \
             safe. Bring it to a plain running or stopped state first (`docker unpause` or \
             `docker stop {}`), then migrate.",
            bot.name,
            bot.state.as_str(),
            bot.container_name.as_deref().unwrap_or(&bot.name)
        );
    }
    // The name becomes a directory under the bots root, so it has to be safe
    // there — an adopted compose service could be called anything.
    crate::panel::naming::validate_bot_id(&bot.name).with_context(|| {
        format!(
            "\"{}\" can't be used as a directory name, so the panel can't migrate it \
             automatically",
            bot.name
        )
    })?;

    let target = cfg.bot_dir(&bot.name);
    if setup::has_operator_files(&target) {
        bail!(
            "{} already holds config files. Move or remove them first so the migration can't \
             overwrite a working setup.",
            target.display()
        );
    }
    Ok(())
}

/// Migrate one bot. Ordering is chosen so a failure at any step leaves the
/// operator no worse off than before:
///
/// 1. Validate, before anything is touched.
/// 2. Copy (not move) the config and secret into the new directory. The originals
///    stay put, so a failure here leaves the old container able to restart.
/// 3. Get the replacement's image onto the host, while the old bot still exists.
/// 4. Stop the old container, so nothing can write another nonce.
/// 5. Recover the ledger from the stopped container, while it still exists.
/// 6. Remove the old container.
/// 7. Create the new one, and start it unless the old one was already stopped.
///
/// Everything before a successful remove is undone on failure — the staged
/// directory is discarded and the bot is started again — because a half-populated
/// target makes [`check`] treat the bot as already migrated and refuse every
/// retry. Once the old container is gone there's no going back; from there the
/// errors say what state things are in.
pub async fn migrate(
    bot: &Bot,
    cfg: &PanelConfig,
    docker: &dyn DockerApi,
    files: Option<&dyn ContainerFiles>,
    on_ledger_loss: OnLedgerLoss,
) -> Result<MigrationReport> {
    check(bot, cfg)?;
    let container = bot.require_container()?.to_string();
    // Two different questions, kept separate even though `check` has narrowed the
    // states enough that they agree here. Anything non-terminal has a process that
    // can still execute — including a `restarting` container between attempts — so
    // it must be stopped before the ledger is read. Whether the *replacement* comes
    // back up is about intent, not liveness.
    let must_quiesce = !bot.state.is_terminal();
    let restart_after = bot.state.wants_to_be_up();
    let source_dir = bot
        .config_dir()
        .context("the bot's config directory could not be resolved")?;
    let source_toml = bot
        .config_panel_path
        .clone()
        .context("the bot's config path could not be resolved")?;

    let target = cfg.bot_dir(&bot.name);
    let mut staging = Staging::open(target.clone())?;

    // Everything before the stop is staging, and staging has to be undoable. A
    // half-populated target would make `check` treat the bot as already migrated
    // and refuse every retry, so the operator would have to clean up by hand.
    let staged = match stage(bot, cfg, docker, &source_toml, &mut staging).await {
        Ok(staged) => staged,
        Err(e) => {
            staging.discard();
            return Err(e);
        }
    };
    let Staged { moved, spec } = staged;

    // The stop is its own step, not folded into the ledger read, so that "the bot is
    // stopped" becomes a fact this function knows rather than one it infers. That's
    // what lets the rollbacks below tell the difference between "never stopped it" and
    // "stopped it and must put it back" — and a `start` aimed at a container that was
    // never stopped would fail on a still-running container and report a phantom
    // outage.
    //
    // It comes before the ledger read because the grace period deliberately lets the
    // current tick finish, and that tick can post orders and persist their nonces. A
    // snapshot taken while the bot is alive would miss them, and the remove that
    // follows throws away the only copy.
    if must_quiesce {
        if let Err(e) = docker.stop(&container, STOP_GRACE_SECS).await {
            // Nothing was stopped, so there is nothing to restart: the staging is the
            // only thing to undo.
            staging.discard();
            return Err(e).with_context(|| format!("stopping {container} before recreating it"));
        }
    }
    // Past here, if we stopped it then it is stopped because of us, and every failure
    // owes the operator a restart.
    let stopped_by_us = must_quiesce;

    // Still undoable: the old container exists either way, and the ledgers are
    // copied rather than moved, so the flat layout it runs from is untouched.
    let Ledgers {
        recovered: ledgers_recovered,
        loss: ledger_loss,
    } = match collect_ledgers(&container, &source_dir, &mut staging, files, on_ledger_loss).await {
        Ok(ledgers) => ledgers,
        Err(e) => {
            staging.discard();
            return Err(restore_after_failure(docker, &container, stopped_by_us, e).await);
        }
    };

    // Still undoable: a remove that fails leaves the old container in place
    // (stopped). If we kept the staged directory, `check` would refuse every
    // retry as "already holds config files".
    if let Err(e) = docker.remove(&container, false).await {
        staging.discard();
        let e = e.context(format!("removing {container} before recreating it"));
        return Err(restore_after_failure(docker, &container, stopped_by_us, e).await);
    }

    // Again, because the ledgers were written after the staging handover and the
    // bot has to own those too. Staging already proved the panel can do this, so
    // this can't newly fail here.
    staging.hand_over(cfg.bot_uid)?;

    docker.create(&spec).await.with_context(|| {
        format!(
            "recreating {container} with the new layout. Its config is already at {}, so you \
             can also bring it up from an exported compose file.",
            target.display()
        )
    })?;
    if restart_after {
        docker.start(&spec.name).await?;
    }

    Ok(MigrationReport {
        name: bot.name.clone(),
        dir: target,
        moved,
        ledgers_recovered,
        ledger_loss,
        started: restart_after,
    })
}

/// What staging produced: the files now in the target directory, and the spec the
/// replacement container will be created from.
struct Staged {
    moved: Vec<String>,
    spec: CreateSpec,
}

/// The new per-bot directory, and exactly which files this attempt put in it.
///
/// The rollback removes those and nothing else. The directory can legitimately
/// hold things the panel doesn't recognise — a README, a backup, a ledger from an
/// abandoned attempt — and `check` only refuses when it finds config files, so
/// "delete everything here" would quietly destroy an operator's data on a failure
/// they didn't cause.
struct Staging {
    dir: PathBuf,
    /// Whether the directory itself is ours to remove.
    created_dir: bool,
    created: Vec<String>,
    /// Every entry this attempt wrote — the ones it created, plus any it
    /// deliberately overwrote. This is the set the bot is given ownership of, and it
    /// is deliberately not "everything in the directory": the operator's own files
    /// can be in there, and a chown the rollback can't undo is a change to something
    /// the panel doesn't own.
    written: Vec<String>,
    /// The directory's owner before this attempt, when it already existed. Restored
    /// on rollback, because the bot needs to own the directory to write its ledger —
    /// so unlike the entries, this one has to be changed and then put back.
    dir_owner: Option<u32>,
}

impl Staging {
    fn open(dir: PathBuf) -> Result<Self> {
        let created_dir = !dir.exists();
        let dir_owner = if created_dir {
            None
        } else {
            provision::owner_uid(&dir)
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            created_dir,
            created: Vec::new(),
            written: Vec::new(),
            dir_owner,
        })
    }

    /// Hand the directory and only this attempt's own entries to the bot's uid.
    ///
    /// Not the whole directory: `check` deliberately tolerates files the operator
    /// put here by hand, and sweeping those into the chown would hand the bot's uid
    /// access to a README or a backup it has no business with — permanently, since
    /// the rollback has no record of what they were before.
    fn hand_over(&self, uid: u32) -> Result<()> {
        provision::hand_over_paths_to_bot(&self.dir, &self.written, uid)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Write a file into the directory, refusing to clobber whatever is already
    /// there and remembering it for the rollback.
    ///
    /// Remembered before the write, not after: a copy that fails halfway leaves a
    /// partial file, and that file is ours to clean up too.
    fn create<F>(&mut self, name: &str, write: F) -> Result<()>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        let to = self.path(name);
        // `create_new` rather than an `exists()` probe: the probe is a read, so two
        // migrations of the same bot could both pass it, both record the file as
        // theirs, and then one's rollback would delete the other's staged config while
        // that one is mid-flight — after it had already removed the old container.
        // Exclusive create makes exactly one of them the owner, decided by the kernel.
        //
        // The handle is dropped immediately: it exists to win the race, not to write.
        // `write` still gets the path, so a copy or a byte write works unchanged onto
        // the empty file this leaves behind.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&to)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("{} already exists", to.display())
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", to.display())),
        }
        self.created.push(name.to_string());
        self.remember_written(name);
        write(&to)
    }

    /// Record an entry this attempt wrote over rather than created, so it is handed
    /// to the bot along with the rest — the replacement has to be able to write the
    /// ledger it just got — without joining the list the rollback deletes.
    fn remember_written(&mut self, name: &str) {
        if !self.written.iter().any(|n| n == name) {
            self.written.push(name.to_string());
        }
    }

    /// Undo everything this attempt wrote, so the operator can retry.
    ///
    /// Best-effort: the migration already failed and that's the error worth
    /// reporting, not a cleanup hiccup. What matters is that `check` doesn't find
    /// leftover config files and refuse the retry.
    fn discard(&self) {
        for name in &self.created {
            let _ = std::fs::remove_file(self.path(name));
        }
        // Put the directory back in the operator's name. Best-effort like the rest
        // of the cleanup, but it matters: without it a failed migration silently
        // leaves a directory the operator no longer owns.
        if let Some(uid) = self.dir_owner {
            provision::restore_owner(&self.dir, uid);
        }
        if self.created_dir {
            // Only succeeds while empty, which is the point: anything unexpected
            // in there stays, and stays visible.
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

/// The nonce ledgers now in the target directory, and why any are missing.
struct Ledgers {
    recovered: Vec<String>,
    loss: Option<String>,
}

/// Undo the stop this migration performed, and say so if that fails.
///
/// The original failure is still the one worth reporting, so this can't replace it —
/// but "best effort" must not mean "silent". A restart that fails leaves the bot
/// stopped and not quoting, and an operator reading only the first error would take
/// "retry the migration" to mean nothing happened, while their bot is off the book.
/// So the two are reported together, with the manual command to fix it.
///
/// Only ever called with `stopped_by_us` true when the stop actually succeeded, which
/// is why [`migrate`] performs the stop itself. Aiming a `start` at a container that
/// was never stopped would fail against a still-running container and manufacture an
/// outage report out of nothing.
async fn restore_after_failure(
    docker: &dyn DockerApi,
    container: &str,
    stopped_by_us: bool,
    cause: anyhow::Error,
) -> anyhow::Error {
    if !stopped_by_us {
        return cause;
    }
    match docker.start(container).await {
        Ok(()) => cause,
        Err(restart) => anyhow::anyhow!(
            "{cause:#}\n\nThe migration stopped {container} and then could not start it again \
             ({restart:#}), so the bot is STOPPED and not quoting. Its config and key are \
             untouched — start it with `docker start {container}`, then retry the migration."
        ),
    }
}

/// Collect the bot's nonce ledgers into the new directory.
///
/// [`migrate`] has already stopped the container, which matters: the stop's grace
/// period deliberately lets the current tick finish, and that tick can post orders and
/// persist their nonces. A snapshot taken while the bot is still alive would miss
/// them, and the remove that follows throws away the only copy — so the replacement
/// would reuse nonces belonging to orders that are live on chain.
///
/// A read that *succeeds* and finds nothing is fine — that bot has no ledger, so
/// there is nothing to lose. A read that **fails** is not: the caller is about to
/// remove the only copy, so by default this errors and the caller's rollback puts the
/// container back, leaving the ledger where it is for a retry. Only
/// [`OnLedgerLoss::Accept`] downgrades it to a reported loss, and that has to come
/// from an operator who has decided the ledger is unreadable for good.
async fn collect_ledgers(
    container: &str,
    source_dir: &Path,
    staging: &mut Staging,
    files: Option<&dyn ContainerFiles>,
    on_ledger_loss: OnLedgerLoss,
) -> Result<Ledgers> {
    // Now that nothing is writing, collect the ledgers: whatever already sits on
    // the host (a partially-correct setup), then the container's own copy, which
    // exists only until the remove — and wins when both exist, because a flat
    // host copy can only be a stale manual snapshot.
    let mut recovered = copy_host_ledgers(source_dir, staging)?;
    let mut loss = None;
    match files {
        Some(reader) => match recover_ledgers(reader, container, staging).await {
            Ok(names) => {
                for name in names {
                    if !recovered.contains(&name) {
                        recovered.push(name);
                    }
                }
            }
            Err(e) => loss = Some(format!("{e:#}")),
        },
        // No reader at all: the panel would be removing the container without ever
        // looking inside it. A host copy is not a substitute — in the flat layout it
        // can only be a stale manual snapshot — so this counts as a failed read.
        None => loss = Some("reading files out of containers is not available".to_string()),
    }
    if let (Some(reason), OnLedgerLoss::Abort) = (&loss, on_ledger_loss) {
        // Says what couldn't be done and what it means, and nothing about the state
        // the bot ends up in: the restart happens in the caller and can itself fail.
        // Claiming "the bot is back the way it was found" here was a promise this
        // function had no way to keep.
        bail!(
            "couldn't read {container}'s nonce ledger ({reason}), and removing the container \
             would destroy the only copy. The migration was rolled back with the ledger still \
             in place, so retrying costs nothing. If it keeps failing — an adopted bot on a \
             custom image with no {RUN_DIR}, or a run directory too large to pull through the \
             archive API — migrate again with \"accept ledger loss\" to go ahead without it. Any \
             orders live at that point stay on the book until they expire."
        );
    }
    Ok(Ledgers { recovered, loss })
}

/// Populate the new per-bot directory and get everything ready to replace the
/// container, without touching the container itself.
///
/// Every failure in here is recoverable by the operator simply retrying, provided
/// the caller undoes the staging — which is why it's separated from the
/// replacement below.
async fn stage(
    bot: &Bot,
    cfg: &PanelConfig,
    docker: &dyn DockerApi,
    source_toml: &Path,
    staging: &mut Staging,
) -> Result<Staged> {
    let target = staging.dir.clone();
    let mut moved = vec!["stitch.toml".to_string()];
    staging.create("stitch.toml", |to| copy_into(source_toml, to))?;

    // Resolve the signer from the *source* config, not the staged target. Turnkey
    // keeps its API public key in the sibling stitch.env, which hasn't been copied
    // yet — reading the target would produce a container that starts and then
    // exits because the public key is missing.
    let signer = signer_runtime_at(source_toml)?;
    // The flat layout names files per bot (stitch.bot1.key), so the secret sits
    // next to the config under whatever name that mount pointed at, not
    // necessarily the canonical one.
    match find_beside(source_toml, &signer.secret_file) {
        Some(found) => {
            staging.create(&signer.secret_file, |to| copy_into(&found, to))?;
            moved.push(signer.secret_file.clone());
        }
        None => bail!(
            "couldn't find {}'s signer secret next to {}. Copy it into {} by hand, then \
             migrate again.",
            bot.name,
            source_toml.display(),
            target.display()
        ),
    }

    // Turnkey (and any future non-secret env the writer parks beside the config)
    // has to move with the bot. Without this the replacement's env is empty even
    // though signer_runtime_at already lifted the public key into the create spec
    // — a later recreate from the directory would lose it.
    if let Some(env) = find_beside(source_toml, "stitch.env") {
        staging.create("stitch.env", |to| copy_into(&env, to))?;
        moved.push("stitch.env".to_string());
    }

    // The bot runs as its own user and can't start on files it doesn't own. Doing
    // this during staging means a panel without the privileges to hand them over
    // fails while the old container is still running.
    staging.hand_over(cfg.bot_uid)?;

    // The bot comes back on the image it is running now. This action changes the
    // layout and nothing else, and an adopted bot can be pinned to a digest or
    // built from a fork — swapping its trading binary here would be an upgrade
    // nobody asked for. Recreate is where an image change is the point.
    let corridor = bot.config.as_ref().and_then(|c| c.corridor_id.clone());
    let image = image_of(bot, cfg);
    let spec = bot_container_spec(cfg, &bot.name, &image, &signer, corridor.as_deref());

    // Same window as the image pull, and for the same reason: a mount the daemon
    // can't resolve becomes a directory rather than an error, and finding that out
    // after the old container is gone is not recoverable.
    provision::check_file_mounts(&spec.binds, cfg).map_err(|e| anyhow::anyhow!(e))?;

    // The replacement's image has to be on the host before the old container is
    // touched: discovering an unreachable registry after the remove would leave
    // the operator with a migrated config and no bot.
    docker
        .ensure_image(&spec.image, false)
        .await
        .with_context(|| format!("getting {} onto the host before migrating", spec.image))?;

    Ok(Staged { moved, spec })
}

/// Copy a file. Copy rather than rename so a failure leaves the old container's
/// mounts intact.
fn copy_into(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    // Secrets must not become readable by anyone else in their new home.
    restrict_if_secret(to)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_if_secret(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".key") || n.ends_with(".token"));
    if is_secret {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_if_secret(_path: &Path) -> Result<()> {
    Ok(())
}

/// Copy ledger files that are already on the host into the new directory.
///
/// A copy, not a move: until the old container is gone the migration can still be
/// abandoned, and the flat layout it would come back on has to keep its ledger.
fn copy_host_ledgers(source_dir: &Path, staging: &mut Staging) -> Result<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(source_dir) else {
        return Ok(Vec::new());
    };
    let mut copied = Vec::new();
    for name in entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(LEDGER_SUFFIX))
    {
        if staging.path(&name).exists() {
            continue;
        }
        let from = source_dir.join(&name);
        staging.create(&name, |to| {
            std::fs::copy(&from, to)
                .map(|_| ())
                .with_context(|| format!("copying ledger {name}"))
        })?;
        copied.push(name);
    }
    Ok(copied)
}

/// Copy the ledger out of the container's own filesystem into the new directory.
///
/// The container's copy after the stop is definitive. A matching file already on
/// the host — whether copied earlier in this attempt or left in the target from
/// a previous try — can only be a stale snapshot: the live bot kept advancing
/// nonces inside the container. Overwriting here is what keeps the replacement
/// from reusing nonces that still belong to orders on the book.
async fn recover_ledgers(
    reader: &dyn ContainerFiles,
    container: &str,
    staging: &mut Staging,
) -> Result<Vec<String>> {
    let files = reader.read_dir(container, RUN_DIR).await?;
    let mut written = Vec::new();
    for (name, bytes) in files
        .into_iter()
        .filter(|(name, _)| name.ends_with(LEDGER_SUFFIX))
    {
        let to = staging.path(&name);
        if to.exists() {
            std::fs::write(&to, &bytes).with_context(|| {
                format!("replacing {} with the container's ledger", to.display())
            })?;
            // Written by us even though we didn't create it, so the bot has to own it
            // — but it isn't ours to delete on a rollback.
            staging.remember_written(&name);
        } else {
            staging.create(&name, |to| {
                std::fs::write(to, &bytes).with_context(|| format!("writing {}", to.display()))
            })?;
        }
        written.push(name);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::fake::{container, flat_layout_mounts, Call, FakeDocker};
    use crate::panel::docker::ContainerState;
    use crate::panel::inventory::discover;
    use crate::panel::naming::LABEL_COMPOSE_SERVICE;

    const KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// A `ContainerFiles` that serves a fixed set of files, or fails.
    struct FakeFiles {
        files: Vec<(String, Vec<u8>)>,
        error: Option<String>,
    }

    #[async_trait::async_trait]
    impl ContainerFiles for FakeFiles {
        async fn read_dir(&self, _c: &str, _d: &str) -> Result<Vec<(String, Vec<u8>)>> {
            match &self.error {
                Some(e) => bail!(e.clone()),
                None => Ok(self.files.clone()),
            }
        }
    }

    fn test_cfg(tag: &str) -> (PanelConfig, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "stitch-panel-mig-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // Same root either side, so the flat config the test writes is reachable
        // through the mount table the fake reports.
        let cfg = PanelConfig::for_test(root.clone(), root.clone());
        (cfg, root)
    }

    /// Seed the flat layout: per-bot config and key files loose in one directory,
    /// mounted individually — the shape `docker-compose.example.yml` produces.
    fn seed_flat(root: &Path, bot: &str) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(
            root.join(format!("stitch.{bot}.toml")),
            corridor.toml_template,
        )
        .unwrap();
        std::fs::write(root.join(format!("stitch.{bot}.key")), format!("{KEY}\n")).unwrap();
    }

    fn flat_bot(root: &Path, cfg: &PanelConfig, bot: &str, state: ContainerState) -> Bot {
        let mut c = container(&format!("stitch-{bot}"), state);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.into(), bot.to_string());
        c.mounts = flat_layout_mounts(root.to_str().unwrap(), bot);
        discover(&[c], cfg).get(bot).unwrap().clone()
    }

    #[tokio::test]
    async fn migrating_moves_the_files_and_recreates_with_the_good_layout() {
        let (cfg, root) = test_cfg("happy");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        assert_eq!(bot.layout, Layout::FlatFiles);

        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let files = FakeFiles {
            files: vec![(
                "stitch.56.0xf39fd.slot-nonces.json".into(),
                b"{\"slots\":[]}".to_vec(),
            )],
            error: None,
        };

        let report = migrate(&bot, &cfg, &docker, Some(&files), OnLedgerLoss::Abort)
            .await
            .unwrap();

        // Files landed in the per-bot directory under their canonical names.
        let dir = root.join("bot1");
        assert!(dir.join("stitch.toml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("stitch.key"))
                .unwrap()
                .trim(),
            KEY
        );
        assert_eq!(report.moved, vec!["stitch.toml", "stitch.key"]);
        // The live ledger was rescued, which is the point of the exercise.
        assert_eq!(report.ledgers_recovered.len(), 1);
        assert!(report.ledger_loss.is_none());
        assert!(dir.join("stitch.56.0xf39fd.slot-nonces.json").exists());
        assert!(report.started, "a running bot comes back running");

        // The old container was stopped gracefully, removed, and replaced.
        let calls = docker.calls();
        assert_eq!(
            calls,
            vec![
                Call::EnsureImage {
                    image: cfg.bot_image.clone(),
                    refresh: false
                },
                Call::Stop {
                    name: "stitch-bot1".into(),
                    grace_secs: STOP_GRACE_SECS
                },
                Call::Remove {
                    name: "stitch-bot1".into(),
                    force: false
                },
                Call::Create("stitch-bot1".into()),
                Call::Start("stitch-bot1".into()),
            ]
        );

        // The replacement really does persist the ledger now.
        let listed = docker.list_all().await.unwrap();
        let new = listed.iter().find(|c| c.name == "stitch-bot1").unwrap();
        assert_eq!(
            crate::panel::inventory::layout_of(&new.mounts),
            Layout::Directory
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn the_ledger_is_snapshotted_after_the_stop_and_before_the_remove() {
        // The stop's grace period lets the current tick finish, and that tick can
        // post orders and persist their nonces. Read the ledger any earlier and
        // those nonces die with the container, so the replacement reuses them for
        // orders that are live on chain.
        let (cfg, root) = test_cfg("order");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);

        // One fake plays both roles, so its call log interleaves the ledger read
        // with the lifecycle calls.
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.set_container_files(vec![(
            "stitch.56.0xf39fd.slot-nonces.json".into(),
            b"{\"slots\":[]}".to_vec(),
        )]);

        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(report.ledgers_recovered.len(), 1);
        assert!(report.ledger_loss.is_none());

        assert_eq!(
            docker.calls(),
            vec![
                // The image lands before anything destructive happens.
                Call::EnsureImage {
                    image: cfg.bot_image.clone(),
                    refresh: false
                },
                Call::Stop {
                    name: "stitch-bot1".into(),
                    grace_secs: STOP_GRACE_SECS
                },
                Call::ReadFiles {
                    name: "stitch-bot1".into(),
                    dir: RUN_DIR.into()
                },
                Call::Remove {
                    name: "stitch-bot1".into(),
                    force: false
                },
                Call::Create("stitch-bot1".into()),
                Call::Start("stitch-bot1".into()),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn an_unpullable_image_leaves_the_old_bot_running() {
        // Docker's create endpoint doesn't pull. If the image only turns out to be
        // missing after the old container is gone, the operator is left with a
        // migrated config and no bot — so the pull has to fail first.
        let (cfg, root) = test_cfg("nopull");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.fail_image("manifest unknown");

        let err = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("manifest unknown"));
        assert_eq!(
            docker.calls(),
            vec![Call::EnsureImage {
                image: cfg.bot_image.clone(),
                refresh: false
            }]
        );
        assert_eq!(
            docker.state_of("stitch-bot1"),
            Some(ContainerState::Running),
            "the bot must still be there to fall back on"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_failed_migration_leaves_nothing_staged_so_it_can_be_retried() {
        // `check` refuses to migrate into a directory that already holds operator
        // files, so a half-written target would turn one transient failure into a
        // permanent one that only a manual `rm -rf` clears.
        let (cfg, root) = test_cfg("retry");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.fail_image("registry unreachable");

        assert!(
            migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
                .await
                .is_err()
        );
        assert!(
            !cfg.bot_dir("bot1").exists(),
            "the staged directory must be gone"
        );

        // And the retry, once the registry is back, works.
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(report.moved, vec!["stitch.toml", "stitch.key"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn staging_claims_a_file_exclusively() {
        // Two migrations of one bot could both pass an `exists()` probe and both record
        // the same file as theirs — then one rollback deletes the other's staged config
        // while it is mid-flight, after it has already removed the old container.
        let (_cfg, root) = test_cfg("stagingclaim");
        let mut first = Staging::open(root.join("bot1")).unwrap();
        first
            .create("stitch.toml", |to| {
                std::fs::write(to, "first").map_err(Into::into)
            })
            .unwrap();

        // A second attempt must not be able to claim the same name.
        let mut second = Staging::open(root.join("bot1")).unwrap();
        let err = second
            .create("stitch.toml", |to| {
                std::fs::write(to, "second").map_err(Into::into)
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
        // And the loser recorded nothing, so its rollback can't delete the winner's file.
        second.discard();
        assert_eq!(
            std::fs::read_to_string(root.join("bot1/stitch.toml")).unwrap(),
            "first"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_rollback_keeps_files_the_operator_already_had_there() {
        // `check` only refuses on config files, so the target can legitimately
        // hold a README, a backup, or a ledger recovered by hand. A failure the
        // operator didn't cause must not eat them.
        let (cfg, root) = test_cfg("keepfiles");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);

        let target = cfg.bot_dir("bot1");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("NOTES.md"), "ledger recovered by hand").unwrap();
        std::fs::write(target.join("stitch.56.0xf39fd.slot-nonces.json"), "{}").unwrap();

        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.fail_image("registry unreachable");
        assert!(
            migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
                .await
                .is_err()
        );

        assert_eq!(
            std::fs::read_to_string(target.join("NOTES.md")).unwrap(),
            "ledger recovered by hand"
        );
        assert!(target.join("stitch.56.0xf39fd.slot-nonces.json").exists());
        // Ours are gone, so the retry isn't blocked.
        assert!(!target.join("stitch.toml").exists());
        assert!(!target.join("stitch.key").exists());
        assert!(target.exists(), "a directory we didn't create is not ours");

        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(report.moved, vec!["stitch.toml", "stitch.key"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_pinned_bot_comes_back_on_the_image_it_was_running() {
        // This action changes the layout. An adopted bot can be pinned to a digest
        // or built from a fork, and quietly swapping its trading binary for
        // whatever STITCH_PANEL_BOT_IMAGE points at is an upgrade nobody clicked.
        let (cfg, root) = test_cfg("pinned");
        seed_flat(&root, "bot1");
        const FORK: &str = "ghcr.io/acme/stitch-fork@sha256:abc";

        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels.insert(LABEL_COMPOSE_SERVICE.into(), "bot1".into());
        c.mounts = flat_layout_mounts(root.to_str().unwrap(), "bot1");
        c.image = FORK.into();
        let bot = discover(&[c.clone()], &cfg).get("bot1").unwrap().clone();

        let docker = FakeDocker::new().with_container(c);
        migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();

        assert!(
            docker.calls().contains(&Call::EnsureImage {
                image: FORK.into(),
                refresh: false
            }),
            "{:?}",
            docker.calls()
        );
        let listed = docker.list_all().await.unwrap();
        let new = listed.iter().find(|c| c.name == "stitch-bot1").unwrap();
        assert_eq!(new.image, FORK);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_remove_that_fails_discards_the_staging_and_brings_the_bot_back() {
        // Same undo as a failed stop: the old container is still there (just
        // stopped), and a leftover stitch.toml in the target would make every
        // retry look "already migrated".
        let (cfg, root) = test_cfg("rmfail");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.fail_remove("removal conflict");

        let err = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("removal conflict"));
        assert!(
            !cfg.bot_dir("bot1").exists(),
            "the staged directory must be gone"
        );
        assert_eq!(
            docker.state_of("stitch-bot1"),
            Some(ContainerState::Running),
            "the operator asked for a layout change, not an outage"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_stop_that_fails_discards_the_staging_and_brings_the_bot_back() {
        // The stop is the last step that can fail while the old container is still
        // there. Leaving the staged files behind would make `check` treat the bot
        // as already migrated and refuse every retry.
        let (cfg, root) = test_cfg("stopfail");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.fail_next("daemon is busy");

        let err = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("daemon is busy"));
        assert!(
            !cfg.bot_dir("bot1").exists(),
            "the staged directory must be gone"
        );
        assert_eq!(
            docker.state_of("stitch-bot1"),
            Some(ContainerState::Running),
            "the operator asked for a layout change, not an outage"
        );

        // And the retry works once the daemon is happy again.
        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(report.moved, vec!["stitch.toml", "stitch.key"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn the_originals_are_left_in_place() {
        // Copy, not move: if anything downstream fails the operator can still
        // bring the old container back from their existing compose file.
        let (cfg, root) = test_cfg("copy");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert!(root.join("stitch.bot1.toml").exists());
        assert!(root.join("stitch.bot1.key").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_turnkey_bots_public_key_survives_migration() {
        // Turnkey parks TURNKEY_API_PUBLIC_KEY in stitch.env, not the TOML. Reading
        // the signer from the staged target (which has no env yet) would build a
        // container that starts and then exits because the public key is missing.
        let (cfg, root) = test_cfg("turnkey");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        let mut toml = corridor.toml_template.to_string();
        toml.push_str(
            "\n[signer]\nprovider = \"turnkey\"\norganization_id = \"org-1\"\n\
             sign_with = \"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"\n\
             operator_address = \"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"\n",
        );
        std::fs::write(root.join("stitch.bot1.toml"), toml).unwrap();
        std::fs::write(
            root.join("stitch.bot1.env"),
            "TURNKEY_API_PUBLIC_KEY='02pub'\n",
        )
        .unwrap();
        std::fs::write(root.join("turnkey-api.key"), "PRIVKEY\n").unwrap();

        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();

        assert!(
            report.moved.iter().any(|m| m == "stitch.env"),
            "the env file has to move with the bot: {:?}",
            report.moved
        );
        assert!(root.join("bot1/stitch.env").exists());
        let created = docker.create_specs();
        let spec = created.last().expect("migration creates a replacement");
        assert!(
            spec.env.iter().any(|e| e == "TURNKEY_API_PUBLIC_KEY=02pub"),
            "the replacement must carry the public key, got {:?}",
            spec.env
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_stopped_bot_stays_stopped() {
        let (cfg, root) = test_cfg("stopped");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Exited);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Exited));
        // Real Docker Engine rejects a stop on an already-exited container; we
        // skip the call entirely so that never comes up.
        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert!(!report.started);
        assert!(
            !docker.calls().contains(&Call::Start("stitch-bot1".into())),
            "migrating must not start a bot the operator had stopped"
        );
        assert!(
            !docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "an already-stopped bot must not be stopped again"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_restarting_bot_is_stopped_before_its_ledger_is_read() {
        // `restarting` makes `is_running()` false, but the process is only gone
        // until the backoff elapses. Skipping the stop lets a tick advance the
        // ledger while it's being copied, and lets the remove land during an exited
        // interval — the replacement then comes up on a ledger that's already
        // stale and reuses nonces belonging to orders on the book.
        let (cfg, root) = test_cfg("restarting");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Restarting);
        assert!(!bot.state.is_running(), "the state this test is about");

        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Restarting));
        docker.set_container_files(vec![(
            "stitch.56.0xf39fd.slot-nonces.json".into(),
            b"{\"slots\":[]}".to_vec(),
        )]);

        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(report.ledgers_recovered.len(), 1);
        assert!(
            report.started,
            "a bot the daemon was restarting comes back up"
        );
        assert_eq!(
            docker.calls(),
            vec![
                Call::EnsureImage {
                    image: cfg.bot_image.clone(),
                    refresh: false
                },
                // The stop, and it comes before the read.
                Call::Stop {
                    name: "stitch-bot1".into(),
                    grace_secs: STOP_GRACE_SECS
                },
                Call::ReadFiles {
                    name: "stitch-bot1".into(),
                    dir: RUN_DIR.into()
                },
                Call::Remove {
                    name: "stitch-bot1".into(),
                    force: false
                },
                Call::Create("stitch-bot1".into()),
                Call::Start("stitch-bot1".into()),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_container_the_panel_cannot_quiesce_is_refused_up_front() {
        // A paused bot is frozen mid-tick and can't act on SIGTERM, so the stop
        // would become a kill; removing and unknown say nothing reliable about
        // whether a process is live. Killing either to copy a ledger is the loss
        // the migration exists to prevent, so it refuses and says what to do.
        let (cfg, root) = test_cfg("noquiesce");
        seed_flat(&root, "bot1");

        for state in [
            ContainerState::Paused,
            ContainerState::Removing,
            ContainerState::Unknown,
        ] {
            let bot = flat_bot(&root, &cfg, "bot1", state);
            let err = check(&bot, &cfg).unwrap_err().to_string();
            assert!(err.contains(state.as_str()), "{state:?}: {err}");

            // And the full migration refuses without touching Docker or staging.
            let docker = FakeDocker::new().with_container(container("stitch-bot1", state));
            assert!(
                migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
                    .await
                    .is_err()
            );
            assert!(docker.calls().is_empty(), "{state:?} must not be touched");
            assert!(!cfg.bot_dir("bot1").exists(), "{state:?} staged anyway");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn the_container_ledger_wins_over_a_stale_host_copy() {
        // A flat-layout host file can only be a manual snapshot: the live bot kept
        // advancing nonces inside the container. Preferring the host copy would
        // put the replacement back on nonces that still belong to orders on the book.
        let (cfg, root) = test_cfg("ledger-prefer");
        seed_flat(&root, "bot1");
        let ledger = "stitch.56.0xabc.slot-nonces.json";
        std::fs::write(root.join(ledger), b"{\"host\":\"stale\"}").unwrap();
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let files = FakeFiles {
            files: vec![(ledger.into(), b"{\"container\":\"live\"}".to_vec())],
            error: None,
        };

        let report = migrate(&bot, &cfg, &docker, Some(&files), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(report.ledgers_recovered, vec![ledger]);
        assert_eq!(
            std::fs::read(root.join("bot1").join(ledger)).unwrap(),
            b"{\"container\":\"live\"}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_failed_ledger_read_aborts_instead_of_destroying_the_only_copy() {
        // The read is the last chance to get the ledger: the remove that follows
        // deletes the container holding it. A transient failure must therefore roll
        // back and leave everything retryable, not be filed as a loss and pressed
        // on with — the ledger is still in there.
        let (cfg, root) = test_cfg("noledger");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let files = FakeFiles {
            files: Vec::new(),
            error: Some("permission denied".into()),
        };

        let err = migrate(&bot, &cfg, &docker, Some(&files), OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("permission denied"), "{msg}");
        // And it says how to force it through, for a ledger that never will read.
        assert!(msg.contains("accept ledger loss"), "{msg}");

        // The container is still there, running, with its ledger.
        assert!(
            !docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. })),
            "the only copy must not be removed: {:?}",
            docker.calls()
        );
        assert_eq!(
            docker.state_of("stitch-bot1"),
            Some(ContainerState::Running),
            "the bot goes back the way it was found"
        );
        assert!(
            !cfg.bot_dir("bot1").exists(),
            "the staged directory must be gone so the retry isn't blocked"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_rollback_that_cannot_restart_the_bot_says_so() {
        // The rollback is best-effort in the sense that the original failure is the
        // one to report — but not silent. Swallowing this told the operator "retry,
        // nothing changed" while their bot sat stopped and off the book.
        let (cfg, root) = test_cfg("rollbackfail");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let files = FakeFiles {
            files: Vec::new(),
            error: Some("permission denied".into()),
        };
        // The stop lands, the ledger read fails, and the daemon is gone by the time
        // the rollback tries to put the bot back.
        docker.fail_start("daemon gone");

        let err = migrate(&bot, &cfg, &docker, Some(&files), OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        // Both failures, and the manual way out.
        assert!(msg.contains("permission denied"), "{msg}");
        assert!(msg.contains("daemon gone"), "{msg}");
        assert!(msg.contains("STOPPED"), "{msg}");
        assert!(msg.contains("docker start stitch-bot1"), "{msg}");
        // And it must not claim the bot was put back.
        assert!(
            !msg.contains("back the way it was"),
            "the old lie must be gone: {msg}"
        );
        assert_eq!(
            docker.state_of("stitch-bot1"),
            Some(ContainerState::Exited),
            "the test's premise: it really is stopped"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_stop_that_never_happened_is_not_restarted() {
        // A failed stop means the container was never stopped, so there is nothing to
        // put back. Firing a `start` at it anyway would fail against a still-running
        // container and manufacture an outage report out of nothing.
        let (cfg, root) = test_cfg("nostopnostart");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        docker.fail_next("daemon is busy");

        let err = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("daemon is busy"), "{msg}");
        assert!(!msg.contains("STOPPED"), "no phantom outage: {msg}");
        assert!(
            !docker.calls().iter().any(|c| matches!(c, Call::Start(_))),
            "nothing was stopped, so nothing may be started: {:?}",
            docker.calls()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn an_unreadable_ledger_can_be_accepted_explicitly() {
        // The escape hatch: an adopted bot on a custom image with no run directory,
        // or one too large to pull through the archive API, would otherwise never be
        // migratable from the panel. The operator says so, and the report says what
        // it cost.
        let (cfg, root) = test_cfg("acceptloss");
        seed_flat(&root, "bot1");
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let files = FakeFiles {
            files: Vec::new(),
            error: Some("permission denied".into()),
        };

        let report = migrate(&bot, &cfg, &docker, Some(&files), OnLedgerLoss::Accept)
            .await
            .unwrap();
        let loss = report
            .ledger_loss
            .as_deref()
            .expect("loss must be reported");
        assert!(loss.contains("permission denied"));
        // The message tells the operator what it actually costs them.
        let msg = report.message();
        assert!(msg.contains("until they expire"));
        assert!(msg.contains("can't replace them"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_panel_that_cannot_read_containers_aborts_rather_than_removing_blind() {
        // No reader means the panel never looks inside before deleting the container.
        // A host copy is no substitute — in the flat layout it can only be a stale
        // manual snapshot — so this is a failed read like any other.
        let (cfg, root) = test_cfg("noreader");
        seed_flat(&root, "bot1");
        std::fs::write(root.join("stitch.56.0xabc.slot-nonces.json"), "{}").unwrap();
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));

        let err = migrate(&bot, &cfg, &docker, None, OnLedgerLoss::Abort)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not available"), "{err:#}");
        assert_eq!(
            docker.state_of("stitch-bot1"),
            Some(ContainerState::Running)
        );

        // Still possible on purpose, for a backend that will never grow the ability.
        let report = migrate(&bot, &cfg, &docker, None, OnLedgerLoss::Accept)
            .await
            .unwrap();
        assert!(report.ledger_loss.is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_ledger_already_on_the_host_is_carried_over() {
        let (cfg, root) = test_cfg("hostledger");
        seed_flat(&root, "bot1");
        std::fs::write(root.join("stitch.56.0xabc.slot-nonces.json"), "{}").unwrap();
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));

        let report = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();
        assert_eq!(
            report.ledgers_recovered,
            vec!["stitch.56.0xabc.slot-nonces.json"]
        );
        // Found one, so there's nothing to warn about.
        assert!(report.ledger_loss.is_none());
        assert!(root.join("bot1/stitch.56.0xabc.slot-nonces.json").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn migrating_an_already_good_bot_is_refused() {
        let (cfg, root) = test_cfg("already");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(root.join("bot-a"), corridor, KEY).unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Running);
        c.labels
            .insert(crate::panel::naming::LABEL_BOT.into(), "bot-a".into());
        c.mounts =
            crate::panel::docker::fake::dir_layout_mounts(root.join("bot-a").to_str().unwrap());
        let bot = discover(&[c], &cfg).get("bot-a").unwrap().clone();

        let err = check(&bot, &cfg).unwrap_err().to_string();
        assert!(err.contains("already uses"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn migration_refuses_to_overwrite_an_existing_config_directory() {
        let (cfg, root) = test_cfg("occupied");
        seed_flat(&root, "bot1");
        // A directory already holding a working setup must not be clobbered.
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(root.join("bot1"), corridor, KEY).unwrap();

        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let err = check(&bot, &cfg).unwrap_err().to_string();
        assert!(err.contains("already holds config files"));

        // And the full migration refuses too, without touching Docker.
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        assert!(
            migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
                .await
                .is_err()
        );
        assert!(
            docker.calls().is_empty(),
            "nothing may be stopped or removed"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_bot_with_no_readable_config_cannot_be_migrated() {
        let (cfg, root) = test_cfg("unreadable");
        let mut c = container("stitch-bot-x", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.into(), "bot-x".into());
        c.mounts = flat_layout_mounts("/somewhere/else", "bot-x");
        let bot = discover(&[c], &cfg).get("bot-x").unwrap().clone();
        let err = check(&bot, &cfg).unwrap_err().to_string();
        assert!(err.contains("can't read"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_missing_secret_aborts_before_the_container_is_touched() {
        let (cfg, root) = test_cfg("nokey");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        // Config but no key: the bot can't run without a signer secret, and
        // guessing one is worse than stopping.
        std::fs::write(root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);

        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        let err = migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("signer secret"));
        assert!(
            docker.calls().is_empty(),
            "the running bot must be left alone when we can't complete the move"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_rollback_leaves_the_operators_own_files_alone() {
        // `check` deliberately tolerates files the operator put in the target by hand.
        // The handover used to sweep the whole directory, so a migration that then
        // failed had already chowned their README to the bot's uid — and the rollback,
        // which only deletes what it created, had no record to put it back from.
        let (cfg, root) = test_cfg("ownership");
        seed_flat(&root, "bot1");
        let target = cfg.bot_dir("bot1");
        std::fs::create_dir_all(&target).unwrap();
        let theirs = target.join("NOTES.md");
        std::fs::write(&theirs, "recovered by hand").unwrap();

        let before = crate::panel::provision::owner_uid(&theirs);
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        // Fails after staging has handed the directory over, and before the stop.
        docker.fail_image("registry unreachable");
        assert!(
            migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
                .await
                .is_err()
        );

        assert_eq!(
            crate::panel::provision::owner_uid(&theirs),
            before,
            "a file the panel didn't write must not change hands"
        );
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "recovered by hand"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_migrated_key_is_owner_only_in_its_new_home() {
        use std::os::unix::fs::PermissionsExt;
        let (cfg, root) = test_cfg("perms");
        seed_flat(&root, "bot1");
        // Start from a too-permissive key, as a hand-made setup might.
        std::fs::set_permissions(
            root.join("stitch.bot1.key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let bot = flat_bot(&root, &cfg, "bot1", ContainerState::Running);
        let docker =
            FakeDocker::new().with_container(container("stitch-bot1", ContainerState::Running));
        migrate(&bot, &cfg, &docker, Some(&docker), OnLedgerLoss::Abort)
            .await
            .unwrap();

        let mode = std::fs::metadata(root.join("bot1/stitch.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the key must not stay world-readable");
        std::fs::remove_dir_all(&root).ok();
    }
}
