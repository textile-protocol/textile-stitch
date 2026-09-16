// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The fleet: what's there, and starting, stopping and removing it.
//!
//! Stop is a graceful stop with the bot's tick grace period, never Docker's
//! `pause` — see the note in [`crate::panel::docker`] for why freezing a bot with
//! live orders on the book is worse than shutting it down.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::logs;
use super::{ApiError, AppState};
use crate::panel::docker::{ContainerState, STOP_GRACE_SECS};
use crate::panel::inventory::{Bot, ConfigSummary, Fleet, Layout, Warning};
use crate::panel::versions::{PublishedVersion, ROLLBACK_CHOICES};
use crate::panel::{compose, migrate, provision, PanelRuntime};

/// One bot, as the UI sees it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotBody {
    pub name: String,
    /// The operator's own name for the bot, when set. `name` stays the id.
    pub display_name: Option<String>,
    pub origin: String,
    pub layout: String,
    pub container: Option<String>,
    pub state: String,
    /// The daemon's own words, e.g. "Up 3 hours".
    pub status: String,
    pub running: bool,
    /// Whether there is a live process to shut down, so the UI offers Stop rather
    /// than Start.
    ///
    /// Not the same question as `running`, which means "actively quoting". A
    /// `restarting` container isn't quoting between attempts but the restart policy
    /// launches it again the moment its backoff elapses, and a `paused` one is
    /// frozen mid-tick — offering Start for either leaves the operator unable to
    /// quiesce a crash-looping bot from the UI. Derived from the same
    /// `ContainerState::is_terminal` the panel's own lifecycle code uses, so the
    /// frontend never has to keep its own list of Docker states in sync.
    pub can_stop: bool,
    pub image: Option<String>,
    /// GitHub release for the running image (`v0.1.226`), when one can be
    /// attributed. The image field stays the Docker ref; the UI shows this.
    pub version: Option<String>,
    pub created_unix: Option<i64>,
    /// Whether the panel can edit this bot's config at all.
    pub editable: bool,
    /// Whether the layout migration would work on this bot right now.
    pub can_migrate: bool,
    /// Why not, when it can't. Shown next to a disabled button rather than
    /// discovered by clicking it.
    pub migrate_blocked_reason: Option<String>,
    /// Whether an approval run can be started right now.
    pub can_approve: bool,
    /// Whether this bot's approvals go through its custodian rather than a
    /// one-shot container. The wizard offers a different button for these, and
    /// `canApprove` stays false because the bot itself still cannot sign.
    pub custody_approvals: bool,
    /// Why not, when it can't. Same reason as `migrateBlockedReason`: the operator
    /// should read it before clicking, not after.
    pub approve_blocked_reason: Option<String>,
    /// The bot that must be stopped to unblock approval. Often this bot, but a
    /// sibling on the same wallet when that sibling is the one that can broadcast.
    pub approve_blocked_by: Option<String>,
    /// Whether a withdraw can run right now. Stricter than approval: every
    /// live process on the wallet has to be down, quoting or not.
    pub can_withdraw: bool,
    pub withdraw_blocked_reason: Option<String>,
    pub withdraw_blocked_by: Option<String>,
    pub config: Option<ConfigBody>,
    pub warnings: Vec<WarningBody>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigBody {
    pub corridor_id: Option<String>,
    pub corridor_label: Option<String>,
    /// Every pair the bot quotes, in pool order.
    pub pairs: Vec<String>,
    pub network_label: Option<String>,
    pub chain_id: u64,
    pub pools: usize,
    /// The signing address. Never the key.
    pub operator_address: Option<String>,
    pub signer: String,
    /// Address page on this chain's explorer, when we know the host and have
    /// an operator address. The panel only surfaces it for a hot wallet.
    pub explorer_url: Option<String>,
    /// The OperatorVault funding this bot's quotes, when `[vault]` is set. Null
    /// means the capital sits in the operator wallet itself.
    pub vault_address: Option<String>,
    /// Explorer page for `vault_address`. Always safe to link: a vault is a
    /// contract on the chain, never an offchain MPC identity.
    pub vault_explorer_url: Option<String>,
    /// `not-connected` / `waiting` / `seated`: whether Textile sends this bot
    /// quotes. The page folds it into the state pill.
    pub venue: crate::panel::inventory::VenueSeat,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningBody {
    pub kind: &'static str,
    pub message: String,
    pub blocks_editing: bool,
}

impl From<&ConfigSummary> for ConfigBody {
    fn from(c: &ConfigSummary) -> Self {
        Self {
            corridor_id: c.corridor_id.clone(),
            pairs: c.pairs.clone(),
            network_label: c.network_label.clone(),
            corridor_label: c.corridor_label.clone(),
            chain_id: c.chain_id,
            pools: c.pools,
            operator_address: c.operator_address.clone(),
            signer: c.signer.clone(),
            explorer_url: c
                .operator_address
                .as_deref()
                .and_then(|address| crate::setup::address_explorer_url(c.chain_id, address)),
            vault_address: c.vault_address.clone(),
            venue: c.venue,
            vault_explorer_url: c
                .vault_address
                .as_deref()
                .and_then(|address| crate::setup::address_explorer_url(c.chain_id, address)),
        }
    }
}

impl From<&Warning> for WarningBody {
    fn from(w: &Warning) -> Self {
        Self {
            kind: w.kind(),
            message: w.message(),
            blocks_editing: w.blocks_editing(),
        }
    }
}

/// Where this bot's `panel.json` lives: its own directory. A flat-layout bot
/// shares the bots root with every other flat bot, so a label there would name
/// all of them at once; it gets none.
fn label_dir(bot: &Bot) -> Option<std::path::PathBuf> {
    (bot.layout != Layout::FlatFiles)
        .then(|| bot.config_dir())
        .flatten()
}

/// Project a discovered bot into its JSON shape.
pub fn to_body(bot: &Bot, state: &AppState, fleet: &Fleet) -> BotBody {
    let migrate_check = migrate::check(bot, &state.cfg);
    // Needs the fleet, not just this bot: another bot sharing the operator wallet
    // blocks an approval just as much as this one being live does.
    let approve_check = super::logs::approve_check(bot, fleet);
    let withdraw_check = super::logs::withdraw_check(bot, fleet);
    BotBody {
        name: bot.name.clone(),
        display_name: label_dir(bot).and_then(|dir| crate::panel::label::read_display_name(&dir)),
        origin: bot.origin.as_str().to_string(),
        layout: bot.layout.as_str().to_string(),
        container: bot.container_name.clone(),
        state: bot.state.as_str().to_string(),
        status: bot.status.clone(),
        running: bot.state.is_running(),
        can_stop: bot.container_name.is_some() && !bot.state.is_terminal(),
        image: bot.image.clone(),
        version: None,
        created_unix: bot.created_unix,
        editable: bot.is_editable(),
        can_migrate: migrate_check.is_ok(),
        migrate_blocked_reason: migrate_check.err().map(|e| format!("{e:#}")),
        can_approve: approve_check.is_ok(),
        custody_approvals: bot.config.as_ref().is_some_and(|c| c.custody_approvals),
        approve_blocked_reason: approve_check.err().map(|e| format!("{e:#}")),
        approve_blocked_by: super::logs::approve_blocked_by(bot, fleet),
        can_withdraw: withdraw_check.is_ok(),
        withdraw_blocked_reason: withdraw_check.err().map(|e| format!("{e:#}")),
        withdraw_blocked_by: super::logs::withdraw_blocked_by(bot, fleet),
        config: bot.config.as_ref().map(ConfigBody::from),
        warnings: bot.warnings.iter().map(WarningBody::from).collect(),
    }
}

async fn with_version(mut body: BotBody, bot: &Bot, state: &AppState, wait: bool) -> BotBody {
    body.version = running_version(state, bot, wait).await;
    body
}

async fn running_version(state: &AppState, bot: &Bot, wait: bool) -> Option<String> {
    let lookup = bot
        .image_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .or(bot.image.as_deref())?;
    let labels = state
        .docker
        .local_image_labels(lookup)
        .await
        .unwrap_or_default();
    let repo_digests = if bot
        .image
        .as_deref()
        .is_none_or(crate::panel::updates::is_content_digest_ref)
    {
        state
            .docker
            .local_image_digests(lookup)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let source = crate::panel::versions::release_lookup_image(
        bot.image.as_deref(),
        &repo_digests,
        &state.cfg.bot_image,
    );
    let needed_sha = crate::panel::versions::release_sha_hint(bot.image.as_deref(), &labels);
    let releases = if wait {
        crate::panel::versions::release_index_await(
            &source,
            needed_sha.as_deref(),
            crate::panel::versions::DETAIL_RELEASE_WAIT,
        )
        .await
    } else {
        crate::panel::versions::release_index(&source, needed_sha.as_deref())
    };
    crate::panel::versions::version_for_image(&releases, bot.image.as_deref(), &labels)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FleetBody {
    bots: Vec<BotBody>,
    /// Image panel-created bots get, so the UI can show what a new bot will run.
    bot_image: String,
    /// Where configs live, for the "your files are here" line in the UI.
    bots_dir: String,
}

pub async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let fleet = state.fleet().await?;
    // Discovery already yields name order (BTreeMap), but sort here too so the
    // fleet page stays alphabetical even if that ever changes.
    let mut bots = Vec::new();
    for b in fleet.bots() {
        bots.push(with_version(to_body(b, &state, &fleet), b, &state, false).await);
    }
    bots.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(FleetBody {
        bots,
        bot_image: state.cfg.bot_image.clone(),
        bots_dir: state.cfg.bots_dir.display().to_string(),
    })
    .into_response())
}

pub async fn show(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let (bot, fleet) = state.bot_and_fleet(&name).await?;
    Ok(Json(with_version(to_body(&bot, &state, &fleet), &bot, &state, true).await).into_response())
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameBody {
    /// The new name, or null / empty to go back to showing the wallet id.
    pub display_name: Option<String>,
}

/// `PATCH /api/bots/{name}/name`: set the display name. Nothing else moves:
/// the id, the container, the venue registration and the files all stay.
pub async fn rename(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RenameBody>,
) -> Result<Response, ApiError> {
    let (bot, fleet) = state.bot_and_fleet(&name).await?;
    let dir = label_dir(&bot).ok_or_else(|| {
        ApiError::conflict(format!(
            "{name} has no config directory of its own to remember a name in"
        ))
    })?;
    let wanted = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    crate::panel::label::write_display_name(&dir, wanted).map_err(ApiError::bad_request)?;
    // `to_body` reads the label file itself, so the fleet from before the
    // write already answers with the new name.
    Ok(Json(to_body(&bot, &state, &fleet)).into_response())
}

/// A lifecycle action's result. Carries the bot's new state so the UI doesn't
/// have to refetch, and a message when there is something to say.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionBody {
    bot: BotBody,
    message: Option<String>,
}

async fn action_response(
    state: &AppState,
    name: &str,
    message: Option<String>,
) -> Result<Response, ApiError> {
    let (bot, fleet) = state.bot_and_fleet(name).await?;
    Ok(Json(ActionBody {
        bot: with_version(to_body(&bot, state, &fleet), &bot, state, true).await,
        message,
    })
    .into_response())
}

/// Claim this bot's operator wallet for the duration of a launch.
///
/// Same protocol as the approve route, deliberately — see
/// [`WalletLocks`](super::logs::WalletLocks). A launch and an approval are both "a
/// process is about to sign with this key", and a check that reads a flag and then
/// calls Docker leaves a window the other side can pass through. So this takes the
/// wallet's claim and holds it across the Docker call, rather than asking whether
/// anyone else has it.
///
/// `bot` must be read from the authoritative config — the file the container will
/// launch from — so the wallet claimed is the wallet that will sign. A stale snapshot
/// claims the wallet the bot is leaving.
///
/// Returned rather than dropped: the caller has to keep it alive until the container
/// is actually up, or the gap reopens.
///
/// Unconditional on the config, unlike `approve_check`'s taker/closer test: *every*
/// bot runs the allowance preflight at live start, so starting any bot on that wallet
/// broadcasts, maker-only or not.
pub async fn claim_for_launch(
    bot: &Bot,
    state: &AppState,
) -> Result<Option<logs::WalletClaim>, ApiError> {
    let claim = state.wallet_locks.try_claim_for(bot).ok_or_else(|| {
        ApiError::conflict(format!(
            "{}'s operator wallet is busy — an approval is running against it, or another bot on \
             it is being launched. Starting now means two processes reading the same pending \
             nonce, and one of the two transactions is lost. Wait for that to finish.",
            bot.name
        ))
    })?;

    // The claim covers other *launches*, not bots that are already up: a running bot
    // holds no claim, so the lock says "free" while its taker spends nonces. The fleet
    // is the other half of the question, and it's asked after the claim is held so
    // nothing can start on this wallet between the answer and the action.
    //
    // Only when this bot isn't already a live transactor. If it is, the overlap exists
    // already and refusing the restart or recreate that might fix it helps nobody.
    if !logs::already_transacting(bot) {
        let fleet = state.fleet().await?;
        logs::no_live_sibling_on_the_wallet(bot, &fleet).map_err(ApiError::conflict)?;
    }
    Ok(claim)
}

/// The config lock a launch holds, or `None` when the bot has no panel-writable
/// config — nothing a save could move out from under it. Held until the container has
/// started; dropping it early reopens the window.
pub type ConfigGuard = Option<tokio::sync::OwnedMutexGuard<()>>;

/// Take the config lock and re-read the bot under it, so everything that follows acts
/// on the config the container will actually load — not one a concurrent settings save
/// is about to change. The same lock settings saves hold across their write, so a
/// launch and a save on one bot serialize.
pub async fn lock_config(name: &str, state: &AppState) -> Result<(ConfigGuard, Bot), ApiError> {
    let mut bot = state.bot(name).await?;
    // A flat-layout migration moves a bot's config to a new path, so the path read here
    // can be obsolete by the time the lock is granted: we'd hold the *old* path's lock
    // while a save on the *new* path changed the wallet. So after re-reading under the
    // lock, confirm the bot is still on the path we locked; if a migration moved it,
    // drop the lock and take the new one. Bounded — a migration doesn't repeat rapidly.
    for _ in 0..3 {
        let Some(path) = bot.config_panel_path.clone() else {
            return Ok((None, bot));
        };
        let guard = state.config_locks.for_path(&path).lock_owned().await;
        // Re-read under the lock: any save that was mid-flight has finished, so this is
        // the config the container will launch from.
        let fresh = state.bot(name).await?;
        if fresh.config_panel_path.as_deref() == Some(path.as_path()) {
            return Ok((Some(guard), fresh));
        }
        // The config moved out from under the lock — take the new path's lock instead.
        drop(guard);
        bot = fresh;
    }
    Err(ApiError::conflict(format!(
        "{name}'s config path kept moving while it was being locked — a migration is probably in \
         flight. Try again once it has finished."
    )))
}

/// What a launch holds: the bot it will act on (re-read under the config lock), the
/// wallet claim, and the config lock itself, all for the read → claim → launch sequence
/// a launch needs to be atomic against a settings save. Without it a raw save can move
/// the config from wallet A to B between the read and `docker.start`, so the claim
/// guards A while the container loads B and an approval or sibling on B starts alongside
/// it. Hold `_config` across the Docker launch — drop it only once the container has
/// started.
pub struct LaunchGuard {
    pub bot: Bot,
    pub claim: Option<logs::WalletClaim>,
    _config: ConfigGuard,
}

pub async fn lock_and_claim_for_launch(
    name: &str,
    state: &AppState,
) -> Result<LaunchGuard, ApiError> {
    let (config, bot) = lock_config(name, state).await?;
    let claim = claim_for_launch(&bot, state).await?;
    Ok(LaunchGuard {
        bot,
        claim,
        _config: config,
    })
}

/// Make an ambiguous start/restart error safe before releasing the launch's claim.
///
/// A `docker start`/`restart` can return an error the connection dropped *after* Docker
/// acted, so the container may already be running its allowance preflight on the wallet
/// this launch claimed — and for a maker-only config the fleet check doesn't treat that
/// live bot as transacting, so releasing the claim would let a sibling collide on the
/// pending nonce. So confirm the container is gone (stop it) before letting the claim go;
/// if the stop fails, hand the claim to a task that holds the wallet until it is. Returns
/// the original error. The launch's config lock is safe to release (the container has
/// started or not) — only the wallet claim needs settling.
pub(crate) async fn settle_ambiguous_launch<H: Send + 'static>(
    state: &AppState,
    container: &str,
    held: Option<H>,
    err: ApiError,
) -> ApiError {
    let Some(held) = held else {
        return err; // no identifiable wallet was claimed — nothing to guard.
    };
    match state.docker.stop(container, STOP_GRACE_SECS).await {
        Ok(()) => err, // confirmed gone; the claim drops here, safe.
        Err(se) => {
            tracing::error!(
                "couldn't stop {container} after an ambiguous start error; holding its wallet until it's gone: {se:#}"
            );
            crate::panel::docker::hold_until_stopped(
                state.docker.clone(),
                container.to_string(),
                held,
            );
            err
        }
    }
}

pub async fn start(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    // Config lock + wallet claim held across the start, so a save can't move the config
    // out from under the claim between here and `docker.start`.
    let launch = lock_and_claim_for_launch(&name, &state).await?;
    super::require_actionable(&launch.bot)?;
    refuse_rfq_only_start_without_credential(&launch.bot, state.cfg.runtime)?;
    // Start only has work to do on a stopped container. If the bot is already up — two
    // Start requests raced and the config lock let the second re-read it *after* the
    // first started it, or a stale UI click lost that race — `docker start` is at best a
    // no-op and on some daemons an error. Treating that expected rejection as an
    // ambiguous launch would hand a healthy, just-started container to `settle`, which
    // stops it. So report the already-live bot instead of touching Docker. Only the
    // genuinely-live states short-circuit: `created`/`exited`/`dead` stay real Start
    // targets, and `unknown` (a config-only bot with no container) falls through to
    // `require_container` so it gets the proper "no container" error, not "already up".
    if matches!(
        launch.bot.state,
        ContainerState::Running | ContainerState::Restarting | ContainerState::Paused
    ) {
        return action_response(
            &state,
            &name,
            Some(format!(
                "{name} is already {} — nothing to start.",
                launch.bot.state.as_str()
            )),
        )
        .await;
    }
    let container = launch
        .bot
        .require_container()
        .map_err(ApiError::conflict)?
        .to_string();
    if let Err(e) = state.docker.start(&container).await {
        return Err(settle_ambiguous_launch(
            &state,
            &container,
            launch.claim,
            ApiError::internal(&e),
        )
        .await);
    }
    tracing::info!(bot = %name, "started");
    action_response(&state, &name, None).await
}

/// RFQ-only bots quote nothing until Connect writes a maker key. Create already
/// refuses auto-start; the bot page must too.
///
/// Only refuse when *every* leg is off. The ladder is one leg, RFQ is another,
/// and the taker/closer legs run independently of both — so a bot whose Connect
/// came back flagged (or landed on a chain with no live RFQ corridor) still has
/// real work to do if it fills resting limit orders, and telling that operator
/// to Connect again would be wrong twice over.
fn refuse_rfq_only_start_without_credential(
    bot: &Bot,
    runtime: crate::panel::PanelRuntime,
) -> Result<(), ApiError> {
    let path = match super::settings::config_path(bot) {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let Ok(toml) = super::settings::read_toml(&path) else {
        return Ok(());
    };
    let Ok(cfg) = crate::config::Config::from_toml(&toml) else {
        return Ok(());
    };
    if config_has_a_live_leg(&cfg, &path, runtime) {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "Connect this bot to Textile on Settings before Start — until then it quotes nothing."
            .to_string(),
    ))
}

/// Would this config do anything if the bot came up right now?
///
/// The ladder is one leg, RFQ another, and the taker/closer legs run
/// independently of both — so a bot whose Connect came back flagged (or landed
/// on a chain with no live RFQ corridor) still has real work to do if it fills
/// resting limit orders. Only "every leg off" is worth refusing over.
///
/// Shared by Start, Restart, and the settings save that would turn the ladder
/// off: all three are ways to leave a bot running and quoting nothing.
pub(super) fn config_has_a_live_leg(
    cfg: &crate::config::Config,
    path: &std::path::Path,
    runtime: crate::panel::PanelRuntime,
) -> bool {
    config_has_a_live_leg_with(cfg, path, runtime, false)
}

/// As [`config_has_a_live_leg`], but `credential_present` lets a caller say the
/// maker key is arriving in this same request. The settings save writes
/// `rfq-api.key` *after* its guards run, so judging that save against the disk
/// alone would refuse the one request that fixes the bot.
///
/// It only stands in for the credential. Everything else — an active, quotable
/// `[rfq]`, a postable ladder, a runnable taker or closer — is still required,
/// so a key pasted onto a config with nothing to run is not a live leg.
pub(super) fn config_has_a_live_leg_with(
    cfg: &crate::config::Config,
    path: &std::path::Path,
    runtime: crate::panel::PanelRuntime,
    credential_present: bool,
) -> bool {
    // `book_enabled` is a switch, not a leg: an adopted or raw-edited config can
    // have the ladder on with no side that drafts an order. Same bar the
    // settings guard applies before it will turn the ladder on.
    let ladder_posts = cfg.book_enabled
        && cfg
            .pools
            .iter()
            .any(|p| p.tokens_parse() && (p.buy_postable() || p.sell_postable()));
    if ladder_posts || cfg.has_independent_leg() {
        return true;
    }
    let has_key = crate::panel::inventory::rfq_key_reaches_bot(cfg, path, runtime);
    cfg.rfq_quotable() && (has_key || credential_present)
}

pub async fn stop(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    super::require_actionable(&bot)?;
    let container = bot.require_container().map_err(ApiError::conflict)?;
    state.docker.stop(container, STOP_GRACE_SECS).await?;
    tracing::info!(bot = %name, "stopped");
    action_response(
        &state,
        &name,
        Some(format!(
            "{name} was asked to shut down and had {STOP_GRACE_SECS}s to finish its tick. Orders \
             it already signed stay on the book until they expire."
        )),
    )
    .await
}

/// Bounce a running bot. Refuses when there is nothing to bounce.
///
/// `docker restart` on a stopped container *starts* it, so without this guard
/// Restart is a second Start button wearing the wrong label — and the panel shows
/// both next to each other on a stopped bot. Clicking it would put a deliberately
/// stopped trading bot back on the book, including one straight out of the wizard
/// that is waiting for its allowance to be approved. `settings.rs` already avoids
/// exactly this when deciding whether a config save should bounce the bot.
pub async fn restart(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let launch = lock_and_claim_for_launch(&name, &state).await?;
    super::require_actionable(&launch.bot)?;
    // Same bar as Start: a bounce that brings the bot back with no live leg
    // leaves the panel showing "running" over a bot that quotes nothing.
    refuse_rfq_only_start_without_credential(&launch.bot, state.cfg.runtime)?;
    if launch.bot.state.is_terminal() {
        return Err(ApiError::conflict(format!(
            "{name} is {} — there is nothing to restart, and `docker restart` on a stopped \
             container starts it. Use Start if you mean to put it back on the book.",
            launch.bot.state.as_str()
        )));
    }
    let container = launch
        .bot
        .require_container()
        .map_err(ApiError::conflict)?
        .to_string();
    if let Err(e) = state.docker.restart(&container, STOP_GRACE_SECS).await {
        return Err(settle_ambiguous_launch(
            &state,
            &container,
            launch.claim,
            ApiError::internal(&e),
        )
        .await);
    }
    tracing::info!(bot = %name, "restarted");
    action_response(&state, &name, None).await
}

/// Shut a container down before destroying it, giving the bot its full window.
///
/// A bot in a terminal state (created / exited / dead) has nothing to stop, and
/// the daemon complaining about that is noise. Anything else — running, paused,
/// restarting, removing, unknown — can still be mid-tick: removing it anyway
/// means `SIGKILL` while it may be signing or broadcasting. The panel refuses
/// and says so rather than deciding that for the operator.
async fn stop_before_destroying(
    state: &AppState,
    bot: &Bot,
    container: &str,
) -> Result<(), ApiError> {
    match state.docker.stop(container, STOP_GRACE_SECS).await {
        Ok(()) => Ok(()),
        Err(e) if bot.state.is_terminal() => {
            tracing::debug!(bot = %bot.name, error = %e, "stop of a terminal container failed");
            Ok(())
        }
        Err(e) => Err(ApiError::conflict(format!(
            "{} is {} and would not stop ({e:#}), so the panel left it alone rather than \
             killing it mid-tick. Stop it by hand with `docker stop {container}` and try again.",
            bot.name,
            bot.state.as_str()
        ))),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateQuery {
    /// Migrate even if the old container's nonce ledger can't be read, accepting
    /// that it's destroyed with the container. Off by default and a separate
    /// confirmation in the UI: the first attempt rolls back so a transient daemon
    /// error is a retry, not a permanent loss.
    #[serde(default)]
    pub accept_ledger_loss: bool,
}

impl MigrateQuery {
    fn on_ledger_loss(&self) -> migrate::OnLedgerLoss {
        if self.accept_ledger_loss {
            migrate::OnLedgerLoss::Accept
        } else {
            migrate::OnLedgerLoss::Abort
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveQuery {
    /// Also delete the bot's config directory, including its signer secret. Off by
    /// default, and a separate confirmation in the UI, because it is the one
    /// irreversible action the panel can take.
    #[serde(default)]
    pub delete_config: bool,
}

pub async fn remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> Result<Response, ApiError> {
    // Need the fleet when deleteConfig is set: another Compose service can mount
    // the same directory under a different name, and wiping it would erase that
    // sibling's key while its container is still live.
    let (bot, fleet) = state.bot_and_fleet(&name).await?;
    super::require_actionable(&bot)?;

    // A config-only row with "keep the files" is a no-op that used to report
    // success ("container is gone") and leave the bot on the fleet page.
    if bot.container_name.is_none() && !query.delete_config {
        return Err(ApiError::bad_request(format!(
            "{name} has no container. Confirm deleting its config if you want it gone from the fleet."
        )));
    }

    if query.delete_config {
        refuse_shared_config_delete(&bot, &fleet, &state.cfg)?;
        refuse_funded_key_delete(&state, &bot).await?;
    }

    let had_container = bot.container_name.is_some();
    if let Some(container) = &bot.container_name {
        stop_before_destroying(&state, &bot, container).await?;
        // The stop can take a tick's grace (up to 30 s), and a deposit can land
        // in that window. Ask again now that nothing is quoting and the
        // container is still there to restart, rather than after it is gone.
        if query.delete_config {
            refuse_funded_key_delete(&state, &bot).await?;
        }
        state.docker.remove(container, true).await?;
    }

    let message = if query.delete_config {
        match delete_bot_config(&bot, &state.cfg)? {
            ConfigDelete::Removed => {
                if had_container {
                    format!("{name} and its config are gone.")
                } else {
                    format!("{name}'s config is gone.")
                }
            }
            ConfigDelete::NotOwned => {
                if had_container {
                    format!(
                        "{name}'s container is gone. Its config isn't under {}, so the panel left \
                         it alone — delete it by hand if you meant to.",
                        state.cfg.bots_dir.display()
                    )
                } else {
                    format!(
                        "{name}'s config isn't under {}, so the panel left it alone — delete it \
                         by hand if you meant to.",
                        state.cfg.bots_dir.display()
                    )
                }
            }
        }
    } else {
        format!("{name}'s container is gone. Its config is still on disk.")
    };

    tracing::info!(bot = %name, delete_config = query.delete_config, "removed");
    Ok(Json(serde_json::json!({ "message": message })).into_response())
}

/// Outcome of trying to wipe a bot's on-disk config from the panel's bots root.
enum ConfigDelete {
    Removed,
    /// Config lives outside the mounted bots root (or isn't known). Never delete
    /// a path the panel doesn't own.
    NotOwned,
}

/// What deleting this bot's config would remove from disk.
enum DeleteTarget<'a> {
    /// Just the bot's own files: a flat-layout bot, or a config sitting
    /// directly in the bots root (never `rm -rf` that whole tree).
    Files(&'a std::path::Path),
    /// The bot's own directory.
    Dir(&'a std::path::Path),
}

/// The files the panel owns for this bot — the mounted path, not `bots/<name>/`
/// assumed from the routing name. Compose services are often named differently
/// from the directory they mount (`foo` → `bots/custom-dir`), and flat-layout
/// bots store `stitch.<name>.toml` loose in the bots root. `None` for a config
/// outside the bots root, which the panel never deletes.
fn owned_config_target<'a>(
    bot: &'a Bot,
    cfg: &crate::panel::PanelConfig,
) -> Option<DeleteTarget<'a>> {
    let config_path = bot.config_panel_path.as_deref()?;
    if !config_path.starts_with(&cfg.bots_dir) {
        return None;
    }
    match bot.layout {
        Layout::FlatFiles => Some(DeleteTarget::Files(config_path)),
        Layout::Directory | Layout::Unknown => {
            let dir = config_path.parent()?;
            if dir == cfg.bots_dir.as_path() {
                Some(DeleteTarget::Files(config_path))
            } else if dir.starts_with(&cfg.bots_dir) {
                Some(DeleteTarget::Dir(dir))
            } else {
                None
            }
        }
    }
}

/// Refuse to delete a key that still guards money. The page shows the same
/// reason next to Remove, but the page's read can be seconds old and a deposit
/// can land in between, so the route reads the wallet again itself: once
/// before stopping anything (a funded bot is not stopped for nothing) and once
/// more after the stop, right before the only irreversible thing the panel
/// does. Only when the delete would actually remove files: a config the panel
/// doesn't own keeps its key either way.
async fn refuse_funded_key_delete(state: &AppState, bot: &Bot) -> Result<(), ApiError> {
    if owned_config_target(bot, &state.cfg).is_none() {
        return Ok(());
    }
    let funding = super::funding::read_funding(state, bot).await?;
    match funding.remove_blocked_by {
        Some(why) => Err(ApiError::conflict(why)),
        None => Ok(()),
    }
}

fn delete_bot_config(bot: &Bot, cfg: &crate::panel::PanelConfig) -> Result<ConfigDelete, ApiError> {
    match owned_config_target(bot, cfg) {
        None => Ok(ConfigDelete::NotOwned),
        Some(DeleteTarget::Files(config_path)) => {
            delete_flat_config_files(config_path)?;
            Ok(ConfigDelete::Removed)
        }
        Some(DeleteTarget::Dir(dir)) => {
            std::fs::remove_dir_all(dir).map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("deleting {} failed: {e}", dir.display()),
                )
            })?;
            Ok(ConfigDelete::Removed)
        }
    }
}

/// Refuse deleteConfig when another fleet bot still claims the same config path
/// (or a path inside the directory we are about to `rm -rf`).
///
/// Inventory only flags duplicate *names*. Two Compose services can mount one
/// directory under different service names; wiping that directory for either
/// would erase the other's signer key while its container stays live.
fn refuse_shared_config_delete(
    bot: &Bot,
    fleet: &Fleet,
    cfg: &crate::panel::PanelConfig,
) -> Result<(), ApiError> {
    let Some(path) = bot.config_panel_path.as_ref() else {
        return Ok(());
    };
    if !path.starts_with(&cfg.bots_dir) {
        return Ok(());
    }
    let wipe_dir = match bot.layout {
        Layout::FlatFiles => None,
        Layout::Directory | Layout::Unknown => match path.parent() {
            // File-only wipe when the config sits directly in the bots root —
            // same as delete_bot_config — so siblings sharing that root are fine.
            Some(dir) if dir == cfg.bots_dir.as_path() => None,
            Some(dir) => Some(dir),
            None => None,
        },
    };

    let sibling = fleet.bots().iter().find(|other| {
        if other.name == bot.name {
            return false;
        }
        // A config-only row for the same directory is the same files under a
        // different name (compose service `foo` mounting `bots/custom-dir`),
        // not a second live bot. Only a container still using the path is a
        // reason to refuse.
        if other.container_name.is_none() {
            return false;
        }
        let Some(other_path) = other.config_panel_path.as_ref() else {
            return false;
        };
        if other_path == path {
            return true;
        }
        match wipe_dir {
            Some(dir) => other_path.starts_with(dir),
            None => false,
        }
    });

    if let Some(other) = sibling {
        return Err(ApiError::conflict(format!(
            "{} shares its config with {}, so the panel won't delete the files while that \
             bot is still on the fleet. Remove {} first, or remove {} without deleting config.",
            bot.name, other.name, other.name, bot.name
        )));
    }
    Ok(())
}

/// Wipe a flat-layout bot's toml, its signer secret, and a per-bot env file.
///
/// The secret is resolved the same way mounts are: this bot's signer → one
/// canonical name → [`provision::find_beside`] (derived first, then that
/// backend's fallback). Never walk every backend — after the derived key is
/// gone, a Turnkey fallback would delete a neighbour's `turnkey-api.key`.
/// `stitch.env` stays derived-only so a shared bare `stitch.env` is left alone.
fn delete_flat_config_files(config_path: &std::path::Path) -> Result<(), ApiError> {
    let mut to_delete = std::collections::BTreeSet::new();
    match provision::signer_runtime_at(config_path) {
        Ok(rt) => {
            if let Some(path) = provision::find_beside(config_path, &rt.secret_file) {
                to_delete.insert(path);
            }
        }
        Err(e) => {
            tracing::warn!(
                config = %config_path.display(),
                error = %e,
                "couldn't read signer for config delete; trying derived hot-wallet key only"
            );
            if let Some(path) = provision::find_beside_derived(config_path, "stitch.key") {
                to_delete.insert(path);
            }
        }
    }
    if let Some(path) = provision::find_beside_derived(config_path, "stitch.env") {
        to_delete.insert(path);
    }

    for path in &to_delete {
        if let Err(e) = std::fs::remove_file(path) {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("deleting {} failed: {e}", path.display()),
            ));
        }
    }
    std::fs::remove_file(config_path).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deleting {} failed: {e}", config_path.display()),
        )
    })?;
    Ok(())
}

/// Pull a refreshable bot image and recreate this bot on it.
///
/// Unlike recovery [`recreate`] (which always uses `STITCH_PANEL_BOT_IMAGE` as
/// written), Update resolves pinned `sha-*` tags and digests to the repository's
/// `:latest` — same rule as panel self-update — so a pin can still pick up a
/// newer publish. Mutable tags keep their tag.
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let image = crate::panel::updates::update_target_image(&state.cfg.bot_image)
        .unwrap_or_else(|| state.cfg.bot_image.clone());
    // Strict pull: Update must not fall back to a stale cached tag the way
    // recovery Recreate does when an unauthenticated refresh fails.
    recreate_on_image(state, name, image, Rebuild::Update).await
}

/// The published builds this bot could be rolled back to.
///
/// Newest first when the commits behind the tags could be read; see
/// [`VersionOrdering`](crate::panel::versions::VersionOrdering) for what the reply
/// says when they couldn't.
///
/// Answers even when the rollback itself is refused: the picker shows the reason
/// beside a disabled list rather than making the operator click to find out.
pub async fn versions(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let blocked = rollback_check(&state, &bot).await.err();

    // Process runtime has no per-bot image, so there is nothing to list and no
    // reason to ask a registry about it.
    let listed = if state.cfg.runtime == PanelRuntime::Process {
        Ok(Vec::new())
    } else {
        crate::panel::versions::list_published(&state.cfg.bot_image, ROLLBACK_CHOICES).await
    };
    let (published, listing_error) = match listed {
        Ok(v) => (v, None),
        // A registry the panel can't read is a missing feature, not a broken
        // page: the rest of the detail screen still works.
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };

    // Key off the running image id, like the update check: a mutable tag
    // resolves to whatever was pulled last, which may not be what this
    // container started from.
    let local_digests = match bot
        .image_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .or(bot.image.as_deref())
    {
        Some(lookup) => state
            .docker
            .local_image_digests(lookup)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let versions: Vec<VersionBody> = published
        .into_iter()
        .map(|version| VersionBody {
            current: crate::panel::versions::is_current(
                &version,
                &local_digests,
                bot.image.as_deref(),
            ),
            version,
        })
        .collect();

    Ok(Json(VersionsBody {
        can_roll_back: blocked.is_none() && !versions.is_empty(),
        blocked_reason: blocked,
        listing_error,
        current_image: bot.image.clone(),
        ordering: crate::panel::versions::ordering_of(
            &versions
                .iter()
                .map(|v| v.version.clone())
                .collect::<Vec<_>>(),
        ),
        versions,
    })
    .into_response())
}

/// One published build in the rollback picker.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionBody {
    #[serde(flatten)]
    version: PublishedVersion,
    /// This is the build the container is on now.
    current: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionsBody {
    /// Empty when the registry couldn't be listed. Newest first only when
    /// `ordering` says so.
    versions: Vec<VersionBody>,
    /// What the order of `versions` is worth — the UI must not call the first
    /// row the newest when nothing could place it.
    ordering: crate::panel::versions::VersionOrdering,
    current_image: Option<String>,
    can_roll_back: bool,
    /// Why a rollback would be refused, when it would.
    blocked_reason: Option<String>,
    /// Why the list is empty, when asking the registry failed.
    listing_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackBody {
    /// Registry tag of the published build to go back to, e.g. `sha-14cd877`.
    /// The repository is the panel's own — a caller chooses a version, never an
    /// image.
    pub tag: String,
}

/// Recreate this bot on an earlier published build.
///
/// The inverse of [`update`], and deliberately the same machinery: pull the
/// chosen reference, then rebuild the container from the config already on disk.
/// The config is *not* rolled back with it — it is the operator's, not the
/// release's — so an older binary meets whatever `stitch.toml` says today. That
/// is the risk the UI warns about, and the reason the reply says to watch the
/// logs.
pub async fn rollback(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RollbackBody>,
) -> Result<Response, ApiError> {
    let tag = body.tag.trim().to_string();
    crate::panel::versions::check_rollback_tag(&tag).map_err(ApiError::bad_request)?;

    let bot = state.bot(&name).await?;
    rollback_check(&state, &bot)
        .await
        .map_err(ApiError::conflict)?;

    let image = crate::panel::versions::rollback_image(&state.cfg.bot_image, &tag)
        .ok_or_else(|| ApiError::conflict(no_registry_path(&state.cfg.bot_image)))?;
    if bot.image.as_deref() == Some(image.as_str()) {
        return Err(ApiError::conflict(format!(
            "{name} already runs {image}, so there is nothing to roll back to."
        )));
    }

    recreate_on_image(state, name, image, Rebuild::Rollback).await
}

fn no_registry_path(configured: &str) -> String {
    format!(
        "{configured} has no registry path, so the panel can't pull another version of it — \
         point STITCH_PANEL_BOT_IMAGE at ghcr.io/textile-protocol/textile-stitch."
    )
}

/// Whether this bot can be moved onto a chosen published build right now.
///
/// One function for both sides: [`versions`] shows the reason next to a disabled
/// picker and [`rollback`] refuses with it, so the button and the API can't
/// disagree about what is allowed. [`recreate_on_image`] re-checks the ledger and
/// channel rules itself — this is the readable refusal, not the guard.
async fn rollback_check(state: &AppState, bot: &Bot) -> Result<(), String> {
    if state.cfg.runtime == PanelRuntime::Process {
        return Err(format!(
            "this panel supervises bots as local processes, which all run the installed stitch \
             binary — {} has no image of its own to roll back. Install an earlier release from \
             the Stitch menu bar (or system tray) instead.",
            bot.name
        ));
    }
    if let Err(e) = super::require_editable(bot) {
        return Err(e.message);
    }
    // Before the directory check, not after: a flat-layout bot's config sits in
    // the bots root itself, so the path comparison below would refuse it with a
    // message naming the same directory twice. The ledger is the real reason.
    if bot.layout == Layout::FlatFiles {
        return Err(format!(
            "{} still uses the flat file layout, so its slot-nonce ledger lives inside the \
             container. Rolling back recreates that container and would throw the ledger away, \
             leaving live orders on the book that the bot can't replace. Migrate first.",
            bot.name
        ));
    }
    let Some(dir) = bot.config_dir() else {
        return Err(format!("{} has no config directory.", bot.name));
    };
    if dir != state.cfg.bot_dir(&bot.name) {
        return Err(format!(
            "{}'s config is at {}, not under {}. Migrate it to the per-bot directory layout \
             first, or roll it back from your own compose file.",
            bot.name,
            dir.display(),
            state.cfg.bots_dir.display()
        ));
    }
    if !crate::panel::updates::bot_eligible_for_update(
        bot.image.as_deref(),
        bot.image_id.as_deref(),
        &state.cfg.bot_image,
        state.docker.as_ref(),
        bot.origin == crate::panel::inventory::Origin::Panel,
    )
    .await
    {
        let current = bot.image.as_deref().unwrap_or("(unknown image)");
        return Err(format!(
            "{} runs {current}, which isn't on {}'s repository and tag channel. The panel only \
             moves bots between builds it publishes — roll this one back from your own compose \
             file.",
            bot.name, state.cfg.bot_image
        ));
    }
    Ok(())
}

/// Recreate a bot's container from its config on disk, in the panel's layout.
///
/// This is how a bot whose container was removed comes back, and how an operator
/// picks up a new image tag. Uses `STITCH_PANEL_BOT_IMAGE` as configured, except
/// for a bot already pinned to one build — see [`recreate_image`]. Prefer
/// [`update`] when the goal is "pull a newer publish".
pub async fn recreate(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let image = state.cfg.bot_image.clone();
    recreate_on_image(state, name, image, Rebuild::Recreate).await
}

/// The image a recovery Recreate rebuilds on.
///
/// Normally the configured one. But a bot that was rolled back runs an earlier
/// build deliberately, and Recreate is the button an operator reaches for when a
/// container is stuck — quietly moving it back onto `STITCH_PANEL_BOT_IMAGE`'s
/// channel there would reinstall the release they rolled away from, which is the
/// one thing the rollback promised wouldn't happen. So a bot sitting on an
/// immutable pin of the configured repository keeps it.
///
/// A rebuild that isn't about the image must not swap the trading binary
/// underneath.
///
/// Everything else falls back to the configured image — a mutable channel (which
/// is what "recreate on the current release" means anyway), a bare `sha256:…`
/// id, another repository's image, or no readable image at all.
///
/// "Pinned" is [`is_pin_image_ref`](crate::panel::updates::is_pin_image_ref)'s
/// definition, the same one `/api/updates` uses to decide a bot may leave a pin,
/// so the two can't disagree about what a pin is.
fn recreate_image(current: Option<&str>, configured: &str) -> String {
    let pinned = current.is_some_and(|image| {
        // `same_image_repository` also keeps bare `sha256:…` ids out: a content
        // id carries no repository, so nothing proves it belongs to our channel.
        crate::panel::updates::is_pin_image_ref(image)
            && crate::panel::updates::same_image_repository(image, configured)
    });
    match (pinned, current) {
        (true, Some(image)) => image.to_string(),
        _ => configured.to_string(),
    }
}

/// Why a bot's container is being rebuilt.
///
/// Recovery Recreate uses the configured image and tolerates a cached copy of
/// it. Update and Roll back both move the bot onto a *published* reference, so
/// both demand a successful pull and both refuse the layouts and channels where
/// replacing the container would lose the nonce ledger or land on someone
/// else's image. Only the wording differs, and it differs in every refusal an
/// operator reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rebuild {
    Recreate,
    Update,
    Rollback,
}

impl Rebuild {
    /// The pull must succeed — never fall back to a stale local tag.
    fn require_fresh(self) -> bool {
        !matches!(self, Rebuild::Recreate)
    }

    /// How refusals and confirmations name the action.
    fn label(self) -> &'static str {
        match self {
            Rebuild::Recreate => "Recreate",
            Rebuild::Update => "Update",
            Rebuild::Rollback => "Roll back",
        }
    }
}

/// Recreate onto a specific image reference (configured pin, an Update target,
/// or an earlier published build).
///
/// For anything but recovery Recreate the registry pull must succeed — never
/// fall back to a local copy. Recreate keeps the softer
/// [`DockerApi::ensure_image`] refresh path so a private image pulled by hand
/// still comes back when the daemon can't re-authenticate.
async fn recreate_on_image(
    state: AppState,
    name: String,
    image: String,
    rebuild: Rebuild,
) -> Result<Response, ApiError> {
    let require_fresh = rebuild.require_fresh();
    // Config lock held across the whole recreate: the signer is read from the config
    // dir and the wallet is claimed from it, so a save that moved the config mid-recreate
    // would launch a container whose signer and claim disagree. `_config` lives to the
    // end of the handler.
    let (_config, bot) = lock_config(&name, &state).await?;
    // Recovery keeps a pinned bot on its build; Update and Roll back were handed
    // the exact reference they mean. Decided here rather than in the handler so
    // it reads the snapshot the lock protects.
    let image = match rebuild {
        Rebuild::Recreate => recreate_image(bot.image.as_deref(), &image),
        Rebuild::Update | Rebuild::Rollback => image,
    };
    super::require_editable(&bot)?;
    let dir = bot
        .config_dir()
        .ok_or_else(|| ApiError::conflict(format!("{name} has no config directory")))?;
    // Only a bot whose config sits in the panel's own root can be recreated with
    // the panel's mounts; anything else would come back pointing at the wrong
    // paths.
    if dir != state.cfg.bot_dir(&name) {
        return Err(ApiError::conflict(format!(
            "{name}'s config is at {}, not under {}. Migrate it to the per-bot directory layout \
             first, or recreate it from your own compose file.",
            dir.display(),
            state.cfg.bots_dir.display()
        )));
    }
    // Update and Roll back rebuild without migrating. Flat layout keeps the
    // slot-nonce ledger inside the container, so a recreate drops it and live
    // orders can collide. The UI already hides the button; refuse the API too
    // (stale clients / curl).
    if require_fresh && bot.layout == Layout::FlatFiles {
        return Err(ApiError::conflict(format!(
            "{name} still uses the flat file layout. Migrate it to the per-bot directory \
             layout first — {} recreates the container and would drop the in-container \
             nonce ledger.",
            rebuild.label()
        )));
    }
    // Same gate as /api/updates: wrong repo or tag channel (fork, :canary vs
    // :latest) must not be recreated onto STITCH_PANEL_BOT_IMAGE via Update.
    // Recreate still uses the configured image for recovery — that path is explicit.
    // Bare `sha256:…` image ids go through RepoDigests so digest-only containers
    // can still leave the pin.
    if require_fresh
        && !crate::panel::updates::bot_eligible_for_update(
            bot.image.as_deref(),
            bot.image_id.as_deref(),
            &state.cfg.bot_image,
            state.docker.as_ref(),
            bot.origin == crate::panel::inventory::Origin::Panel,
        )
        .await
    {
        let current = bot.image.as_deref().unwrap_or("(unknown image)");
        return Err(ApiError::conflict(format!(
            "{name} runs {current}, which is not on the update channel for {}. {} \
             only moves bots on STITCH_PANEL_BOT_IMAGE's repository and tag \
             channel (same-repo sha-* / digest pins may move to the resolved \
             target) — recreate it from your own compose file, or change \
             STITCH_PANEL_BOT_IMAGE.",
            state.cfg.bot_image,
            rebuild.label()
        )));
    }

    // `wants_to_be_up`, not `is_running`: a bot Docker is restarting is one the
    // operator means to have up, and Recreate is often how they install the image
    // that stops it crashing. Leaving that replacement in `created` strands the bot
    // in exactly the case the action was meant to fix.
    let restart_after = bot.state.wants_to_be_up();
    // Recreate starts the replacement, so it's a bot-launching action like Start. The
    // reservation is held for the rest of the handler — the create and the start both
    // sit inside it — and passed into the launch so an ambiguous start settles it.
    let wallet = if restart_after {
        claim_for_launch(&bot, &state).await?
    } else {
        None
    };

    recreate_container(
        &state,
        &name,
        &bot,
        &dir,
        &image,
        rebuild,
        restart_after,
        wallet,
    )
    .await?;

    crate::panel::updates::clear_cache();
    tracing::info!(bot = %name, %image, action = rebuild.label(), "recreated");
    let ending = if restart_after {
        " and started"
    } else {
        " and left stopped, because it wasn't up before"
    };
    let message = match rebuild {
        // Say the pin out loud. An operator who rolls back and forgets will
        // otherwise wonder why this bot stopped picking up releases.
        Rebuild::Rollback => format!(
            "{name} was rolled back to {image}{ending}. It stays on that version until you \
             Update it again — Recreate keeps the pin — and its config was not rolled back \
             with it, so watch its logs."
        ),
        _ => format!("{name} was recreated on {image}{ending}."),
    };
    action_response(&state, &name, Some(message)).await
}

/// Rebuild a bot's container from the config on disk *now*, in the panel's layout.
///
/// Shared by Recreate and Update. The caller holds the config lock and the wallet
/// claim(s) across it. Everything that can fail — reading the signer, the mount
/// preflight, pulling the image — happens before the old container is destroyed, so
/// a failure leaves the config directory intact and the operator something to retry
/// from.
async fn recreate_container(
    state: &AppState,
    name: &str,
    bot: &Bot,
    dir: &std::path::Path,
    image: &str,
    rebuild: Rebuild,
    restart_after: bool,
    claim: Option<logs::WalletClaim>,
) -> Result<(), ApiError> {
    let signer = provision::signer_runtime(dir)?;
    let corridor = bot.config.as_ref().and_then(|c| c.corridor_id.clone());
    // The image the caller chose (configured pin for Recreate, refreshable
    // target for Update), not the one the bot is running.
    let spec = provision::bot_container_spec(&state.cfg, name, image, &signer, corridor.as_deref());
    provision::check_file_mounts(&spec.binds, &state.cfg).map_err(ApiError::conflict)?;
    if rebuild.require_fresh() {
        state
            .docker
            .require_fresh_image(&spec.image)
            .await
            .map_err(|e| {
                ApiError::internal(&e.context(format!(
                    "{}: pulling {image} failed — the bot was left on its current container \
                     rather than recreating onto a possibly stale local copy",
                    rebuild.label()
                )))
            })?;
    } else {
        state.docker.ensure_image(&spec.image, true).await?;
    }

    if let Some(container) = &bot.container_name {
        stop_before_destroying(state, bot, container).await?;
        state.docker.remove(container, true).await?;
    }

    // The old container is gone by now. The image check above proves the image exists,
    // which is the failure this ordering was designed around — but a create can still
    // fail on a bad bind or a daemon that goes away, and there is no un-remove. So the
    // error says what state the bot is actually in, because "recreate failed" on its
    // own would leave an operator guessing whether their config survived.
    state.docker.create(&spec).await.map_err(|e| {
        ApiError::internal(&e.context(format!(
            "creating {name}'s replacement container. Its config directory is untouched at {}, so              nothing is lost — fix the cause and use Recreate again, or bring it up from an              exported compose file. The bot has no container until then.",
            dir.display()
        )))
    })?;
    if restart_after {
        if let Err(e) = state.docker.start(&spec.name).await {
            // Start can return an error after the daemon already brought the container
            // up (the reply dropped mid-flight). Releasing the wallet claim now would let
            // a sibling launch on the same wallet while this one is quietly live, so
            // settle it: stop the container and only then let the claim go, or hold it
            // until the stop lands.
            let err = ApiError::internal(&e.context(format!(
                "starting {name} after recreating it. The new container exists and holds the                  right config, so Start will bring it up once the cause is fixed."
            )));
            return Err(settle_ambiguous_launch(state, &spec.name, claim, err).await);
        }
    }
    Ok(())
}

/// Move a bot off the flat-file layout so its nonce ledger survives recreation.
pub async fn migrate_layout(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<MigrateQuery>,
) -> Result<Response, ApiError> {
    // Config lock across the whole migration: it moves the config file and brings the
    // bot back up, so a save racing it would write to a file being moved out from under
    // it. Held to the end of the handler.
    let (_config, bot) = lock_config(&name, &state).await?;
    super::require_actionable(&bot)?;
    migrate::check(&bot, &state.cfg).map_err(ApiError::conflict)?;
    // Migration brings a live bot back up at the end, so it launches one too. Held
    // for the whole migration, which is the window that matters.
    let _wallet = if bot.state.wants_to_be_up() {
        claim_for_launch(&bot, &state).await?
    } else {
        None
    };

    // `lock_config` above holds the *source* path's lock — the flat/legacy config the bot
    // runs from now. Migration writes the config into the per-bot layout path and builds
    // the replacement container there, but nothing yet guards that *target* path. A
    // concurrent action keyed on it — a `wizard::create` for the same name, a save, a
    // launch — could write or start at the target while the move is mid-flight. So take
    // the target lock too, held to the end, unless the bot already sits on it (then the
    // source lock already covers it and re-locking the same mutex from this task would
    // deadlock).
    let target = state.cfg.bot_dir(&name).join("stitch.toml");
    let _target_lock = if bot.config_panel_path.as_deref() != Some(target.as_path()) {
        Some(state.config_locks.for_path(&target).lock_owned().await)
    } else {
        None
    };

    let files = state.files.clone();
    let report = migrate::migrate(
        &bot,
        &state.cfg,
        state.docker.as_ref(),
        files.as_deref(),
        query.on_ledger_loss(),
    )
    .await?;
    tracing::info!(bot = %name, "migrated to the per-bot directory layout");

    let (fresh, fresh_fleet) = state.bot_and_fleet(&name).await?;
    Ok(Json(serde_json::json!({
        "bot": with_version(to_body(&fresh, &state, &fresh_fleet), &fresh, &state, true).await,
        "message": report.message(),
        "movedFiles": report.moved,
        "ledgersRecovered": report.ledgers_recovered,
        "ledgerLoss": report.ledger_loss,
        "started": report.started,
    }))
    .into_response())
}

/// The whole fleet as a compose file, for disaster recovery.
///
/// Generated from what's running, never round-tripped, so it can't be the panel's
/// source of truth and can't drift into a half-parsed state.
pub async fn compose_export(State(state): State<AppState>) -> Result<Response, ApiError> {
    let fleet = state.fleet().await?;
    let yaml = compose::render(&fleet, &state.cfg);
    Ok((
        [
            (header::CONTENT_TYPE, "application/yaml; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"docker-compose.yml\"",
            ),
        ],
        yaml,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::super::mock_chain::{mock_rpc, MockChain};
    use super::super::testkit::{harness, Harness};
    use crate::panel::docker::fake::{container, dir_layout_mounts, flat_layout_mounts, Call};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::{LABEL_BOT, LABEL_COMPOSE_SERVICE};
    use crate::setup;
    use axum::http::StatusCode;
    use std::path::Path;

    /// Write a real corridor config into a per-bot directory.
    fn write_bot(root: &Path, name: &str) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(root.join(name), corridor, super::super::testkit::TEST_KEY)
            .expect("writing the test bot config");
        super::super::testkit::keep_book_on(&root.join(name).join("stitch.toml"));
    }

    /// Point a seeded bot's config at a mock node, and its feed and API at
    /// a closed port, so a wallet read never leaves the machine. Deleting a
    /// config reads the wallet first (the funded-key gate), so every test
    /// that deletes one goes through here.
    fn point_at_chain(h: &Harness, name: &str, rpc_url: &str) {
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace("https://bsc-dataseed.binance.org", rpc_url)
            .replace("https://api.textilecredit.com", "http://127.0.0.1:1");
        std::fs::write(&path, toml).unwrap();
    }

    /// A seeded bot whose wallet reads as empty.
    async fn seed_with_empty_wallet(
        h: &Harness,
        name: &str,
    ) -> crate::panel::http::mock_chain::MockNode {
        let node = mock_rpc(MockChain::default()).await;
        seed_panel_bot(h, name);
        point_at_chain(h, name, &node.url);
        node
    }

    /// USDT on BSC, as the corridor template spells it.
    const BSC_USDT: &str = "0x55d398326f99059fF775485246999027B3197955";

    async fn delete_config(h: &Harness, name: &str) -> (StatusCode, String) {
        h.send(
            axum::http::Request::builder()
                .method("DELETE")
                .uri(format!("/api/bots/{name}?deleteConfig=true"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
    }

    /// A running, panel-native bot with the good layout.
    fn seed_panel_bot(h: &Harness, name: &str) {
        seed_panel_bot_in_state(h, name, ContainerState::Running);
    }

    /// A bot whose taker leg is on, so its own process broadcasts from the operator
    /// wallet. The shipped corridors are maker-only, which is why it's switched on by
    /// hand. Same `TEST_KEY` as every other seeded bot, so two of these share a wallet.
    fn seed_transacting(h: &Harness, name: &str, state: ContainerState) {
        seed_panel_bot_in_state(h, name, state);
        let config = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&config).unwrap() + "\nlimit_taker_enabled = true\n";
        std::fs::write(&config, toml).unwrap();
    }

    fn seed_panel_bot_in_state(h: &Harness, name: &str, state: ContainerState) {
        write_bot(&h.root, name);
        let mut c = container(&format!("stitch-{name}"), state);
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    #[tokio::test]
    async fn a_display_name_is_set_cleared_and_never_touches_the_id() {
        let h = harness("rename");
        seed_panel_bot(&h, "bot-a");

        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/name",
                serde_json::json!({ "displayName": " Lagos desk " }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["name"], "bot-a", "the id is untouched");
        assert_eq!(v["displayName"], "Lagos desk");
        assert!(h.root.join("bot-a/panel.json").exists());
        assert!(
            !std::fs::read_to_string(h.root.join("bot-a/stitch.toml"))
                .unwrap()
                .contains("Lagos"),
            "the bot's own config is not where the panel keeps its label"
        );

        // It comes back on a plain read too.
        let (_, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(Harness::parse(&body)["displayName"], "Lagos desk");

        // Too long is refused with the limit in the message.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/name",
                serde_json::json!({ "displayName": "x".repeat(41) }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("40"), "{body}");

        // Empty clears it and removes the file.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/name",
                serde_json::json!({ "displayName": "" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(Harness::parse(&body)["displayName"].is_null());
        assert!(!h.root.join("bot-a/panel.json").exists());
    }

    #[tokio::test]
    async fn a_flat_layout_bot_cannot_be_named() {
        // Its only directory is the bots root, shared with every other flat
        // bot: a panel.json there would rename all of them at once.
        let h = harness("rename-flat");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        std::fs::write(
            h.root.join("stitch.bot1.key"),
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        let (status, body) = h
            .patch_json(
                "/api/bots/bot1/name",
                serde_json::json!({ "displayName": "Lagos desk" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(!h.root.join("panel.json").exists());
    }

    #[tokio::test]
    async fn a_launch_holds_the_config_lock_so_a_save_serialises_behind_it() {
        // The race the launch paths used to have: a raw save could move the config from
        // one wallet to another between the handler's read and `docker.start`, so the
        // claim guarded the wallet the bot was leaving. Holding the config lock — the
        // same one settings saves take across their write — across read → claim → launch
        // closes it: a save can't run until the launch has started the container.
        let h = harness("launch-holds-lock");
        seed_panel_bot(&h, "bot-a");
        let path = h
            .state
            .bot("bot-a")
            .await
            .unwrap()
            .config_panel_path
            .unwrap();

        let launch = super::lock_and_claim_for_launch("bot-a", &h.state)
            .await
            .unwrap();
        // While the launch is in flight, a save can't take the config lock.
        assert!(
            h.state.config_locks.for_path(&path).try_lock().is_err(),
            "the launch must hold the config lock so a concurrent save waits"
        );
        drop(launch);
        // And it's released once the launch is done.
        assert!(
            h.state.config_locks.for_path(&path).try_lock().is_ok(),
            "the config lock must be released after the launch"
        );
    }

    #[tokio::test]
    async fn every_non_terminal_state_offers_stop_rather_than_start() {
        // `running` means "actively quoting", which is the wrong question for a
        // lifecycle button: a restarting bot isn't quoting between attempts but the
        // restart policy relaunches it, and a paused one is frozen mid-tick. Keying
        // the UI off `running` leaves an operator unable to quiesce either.
        for (state, can_stop) in [
            (ContainerState::Running, true),
            (ContainerState::Restarting, true),
            (ContainerState::Paused, true),
            (ContainerState::Created, false),
            (ContainerState::Exited, false),
            (ContainerState::Dead, false),
        ] {
            let h = harness(&format!("canstop-{}", state.as_str()));
            seed_panel_bot_in_state(&h, "bot-a", state);
            let (status, body) = h.get("/api/bots/bot-a").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["canStop"], can_stop, "{state:?}: {body}");
        }
    }

    #[tokio::test]
    async fn a_bot_with_no_container_has_nothing_to_stop() {
        // Config on disk, container gone: Recreate is the only way forward, and
        // offering Stop would just 409.
        let h = harness("canstop-nocontainer");
        write_bot(&h.root, "bot-a");
        let (status, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["canStop"], false, "{body}");
    }

    #[tokio::test]
    async fn a_bot_reports_its_release_version() {
        let h = harness("bot-version");
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:sha-24e9192",
        );
        crate::panel::versions::set_test_release_index(std::collections::HashMap::from([(
            "24e9192".into(),
            "v0.1.226".into(),
        )]));
        struct ResetReleases;
        impl Drop for ResetReleases {
            fn drop(&mut self) {
                crate::panel::versions::clear_test_release_index();
            }
        }
        let _reset = ResetReleases;
        let (status, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["version"], "v0.1.226", "{body}");
    }

    #[tokio::test]
    async fn a_forked_bot_resolves_releases_from_its_own_repository() {
        let h = harness("bot-version-fork");
        seed_panel_bot(&h, "bot-a");
        h.docker
            .set_container_image("stitch-bot-a", "ghcr.io/someone/fork:sha-24e9192");
        crate::panel::versions::set_test_release_index_for_repo(
            "textile-protocol/textile-stitch",
            std::collections::HashMap::from([("24e9192".into(), "v0.1.226".into())]),
        );
        crate::panel::versions::set_test_release_index_for_repo(
            "someone/fork",
            std::collections::HashMap::from([("24e9192".into(), "v9.9.9".into())]),
        );
        struct ResetReleases;
        impl Drop for ResetReleases {
            fn drop(&mut self) {
                crate::panel::versions::clear_test_release_index();
            }
        }
        let _reset = ResetReleases;
        let (status, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["version"], "v9.9.9", "{body}");
    }

    #[tokio::test]
    async fn an_empty_host_lists_no_bots() {
        let h = harness("empty");
        let (status, body) = h.get("/api/bots").await;
        assert_eq!(status, StatusCode::OK);
        let v = Harness::parse(&body);
        assert!(v["bots"].as_array().unwrap().is_empty());
        assert!(v["botImage"].as_str().unwrap().contains("textile-stitch"));
    }

    #[tokio::test]
    async fn the_fleet_list_is_alphabetical_by_bot_name() {
        let h = harness("fleet-alpha");
        write_bot(&h.root, "zeta");
        write_bot(&h.root, "alpha");
        write_bot(&h.root, "mid");
        let (status, body) = h.get("/api/bots").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let parsed = Harness::parse(&body);
        let names: Vec<_> = parsed["bots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[tokio::test]
    async fn a_panel_bot_is_listed_with_its_config_and_no_secrets() {
        let h = harness("list");
        seed_panel_bot(&h, "bot-a");
        let (status, body) = h.get("/api/bots").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        let bot = &v["bots"][0];
        assert_eq!(bot["name"], "bot-a");
        assert_eq!(bot["origin"], "panel");
        assert_eq!(bot["layout"], "directory");
        assert_eq!(bot["running"], true);
        assert_eq!(bot["editable"], true);
        assert_eq!(bot["config"]["chainId"], 56);
        assert_eq!(bot["config"]["signer"], "hot-wallet");
        // The operator address is derived and shown; the key never is.
        let operator = bot["config"]["operatorAddress"]
            .as_str()
            .expect("hot-wallet bots expose the derived address");
        assert!(operator.starts_with("0x"), "{operator}");
        assert_eq!(
            bot["config"]["explorerUrl"].as_str(),
            Some(format!("https://bscscan.com/address/{operator}").as_str())
        );
        assert!(!body.contains(super::super::testkit::TEST_KEY));
        assert!(!body.to_lowercase().contains("private"));
    }

    #[tokio::test]
    async fn start_stop_and_restart_drive_the_container() {
        let h = harness("lifecycle");
        seed_panel_bot(&h, "bot-a");

        let (status, _) = h
            .post_json("/api/bots/bot-a/stop", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Exited)
        );

        let (status, _) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running)
        );

        let (status, _) = h
            .post_json("/api/bots/bot-a/restart", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);

        // The stop must use the tick grace period, not a kill.
        assert!(h.docker.calls().contains(&Call::Stop {
            name: "stitch-bot-a".into(),
            grace_secs: 30,
        }));
    }

    #[tokio::test]
    async fn start_refuses_rfq_only_without_a_credential() {
        let h = harness("start-rfq-noconnect");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Exited);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
    }

    /// The sequencing the onboarding wizard relies on: a bot that has
    /// Connected (it holds a credential) but whose access request Textile has
    /// not approved yet carries `[rfq] enabled = false`, and Start is still
    /// refused. The wizard's waiting screen owns the start after approval; it
    /// must not be possible to start earlier and quote into nothing.
    #[tokio::test]
    async fn start_still_refused_while_access_pending() {
        let h = harness("start-rfq-pending");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let toml_path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&toml_path).unwrap();
        std::fs::write(
            &toml_path,
            format!(
                "{toml}\n[rfq]\nenabled = false\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\nmaker_id = \"clmakerenroll1\"\nvalidation_contract = \"0xBCA5E344077AaC751A1C548a45F28215bB7ec165\"\n"
            ),
        )
        .unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Exited);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Start { .. })),
            "nothing was started: {:?}",
            h.docker.calls()
        );
    }

    /// An RFQ-only bot whose only credential is `STITCH_RFQ_API_KEY_FILE` in
    /// its own `stitch.env` — no panel-written `rfq-api.key`.
    fn seed_env_only_rfq_bot(h: &super::super::testkit::Harness) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        let paths = setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let toml = std::fs::read_to_string(&paths.toml).unwrap()
            + "\n[rfq]\nenabled = true\nurl = \"wss://api.textilecredit.com/v2/maker/stream\"\n\
               maker_id = \"mk_test\"\n\
               validation_contract = \"0x00000000000000000000000000000000000000aa\"\n";
        std::fs::write(&paths.toml, toml).unwrap();
        let key_path = h.root.join("bot-a").join("maker.key");
        std::fs::write(&key_path, "tx_live_secret\n").unwrap();
        let env = std::fs::read_to_string(&paths.env).unwrap_or_default()
            + &format!("STITCH_RFQ_API_KEY_FILE='{}'\n", key_path.display());
        std::fs::write(&paths.env, env).unwrap();
        assert!(
            !setup::rfq_api_key_is_set(h.root.join("bot-a")),
            "this bot must have no panel-written rfq-api.key"
        );
        let mut c = container("stitch-bot-a", ContainerState::Exited);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);
    }

    #[tokio::test]
    async fn start_accepts_an_rfq_credential_from_stitch_env_in_process_mode() {
        // Process-mode / Desktop bots are children of the panel: they inherit
        // its environment and get their own stitch.env applied, so a key held
        // that way is real and Start must not refuse it.
        let h = super::super::testkit::harness_process("start-rfq-envkey-process");
        seed_env_only_rfq_bot(&h);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running)
        );
    }

    #[tokio::test]
    async fn start_honours_a_blank_credential_override_in_stitch_env() {
        // `apply_env_file` sets every assignment it parses, blanks included, so
        // `STITCH_RFQ_API_KEY=` in stitch.env *overrides* an inherited variable
        // with an empty one. The guard must read that as "no key", not fall
        // back to the panel's own environment.
        let h = super::super::testkit::harness_process("start-rfq-blank-override");
        seed_env_only_rfq_bot(&h);
        let paths = h.root.join("bot-a");
        std::fs::write(
            paths.join("stitch.env"),
            "STITCH_PRIVATE_KEY_FILE='stitch.key'\nSTITCH_RFQ_API_KEY=\n",
        )
        .unwrap();
        std::env::set_var("STITCH_RFQ_API_KEY", "tx_live_inherited");

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        std::env::remove_var("STITCH_RFQ_API_KEY");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
    }

    #[tokio::test]
    async fn restart_refuses_a_bot_with_no_live_leg() {
        // Restart is another way to leave the panel showing a running bot that
        // quotes nothing, so it takes the same bar as Start.
        let h = harness("restart-rfq-noconnect");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Running);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot-a/restart", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { .. })),
            "nothing may be bounced: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn start_reads_the_last_duplicate_env_assignment() {
        // `apply_env_file` calls `cmd.env` per line, so with the key assigned
        // twice the child keeps the last. A good key followed by a blank means
        // the child gets nothing, whatever the first line said.
        let h = super::super::testkit::harness_process("start-rfq-dup-env");
        seed_env_only_rfq_bot(&h);
        let paths = h.root.join("bot-a");
        let good = paths.join("maker.key").display().to_string();
        std::fs::write(
            paths.join("stitch.env"),
            format!(
                "STITCH_PRIVATE_KEY_FILE='stitch.key'\n\
                 STITCH_RFQ_API_KEY_FILE='{good}'\n\
                 STITCH_RFQ_API_KEY_FILE=\n"
            ),
        )
        .unwrap();

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
    }

    #[tokio::test]
    async fn start_refuses_a_book_bot_whose_ladder_cannot_post() {
        // `book_enabled = true` is a switch, not a leg. An adopted or
        // raw-edited config can have the ladder on with no side that drafts an
        // order, and the main loop then posts nothing while the panel says
        // "running".
        let h = harness("start-book-no-ladder");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        let paths = setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        super::super::testkit::keep_book_on(&paths.toml);
        let stripped = std::fs::read_to_string(&paths.toml)
            .unwrap()
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("buy_total_liquidity_debt")
                    && !t.starts_with("buy_min_slice_debt")
                    && !t.starts_with("sell_total_liquidity_collateral")
                    && !t.starts_with("sell_min_slice_debt")
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&paths.toml, stripped).unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Exited);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn start_refuses_when_the_env_credential_file_is_gone() {
        // A variable naming a deleted or empty file is not a credential: the
        // runtime falls past it and declines to spawn the responder, so letting
        // Start through would leave a "running" bot quoting nothing.
        let h = super::super::testkit::harness_process("start-rfq-envkey-missing");
        seed_env_only_rfq_bot(&h);
        std::fs::remove_file(h.root.join("bot-a").join("maker.key")).unwrap();

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
    }

    #[tokio::test]
    async fn start_ignores_a_stitch_env_rfq_credential_in_docker_mode() {
        // `bot_container_spec` builds the container env explicitly — it never
        // sources stitch.env and never inherits the panel's environment. So the
        // same bot that starts fine as a process cannot load its key in Docker,
        // and letting Start through would leave it up and silently not quoting.
        let h = harness("start-rfq-envkey-docker");
        seed_env_only_rfq_bot(&h);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Connect this bot"), "{body}");
    }

    #[tokio::test]
    async fn start_allows_an_unconnected_bot_that_still_takes_limit_orders() {
        // Connect can come back flagged or land on a chain with no live RFQ
        // corridor: the maker key is saved but `rfq_enabled` stays false. The
        // taker leg runs independently of both the ladder and RFQ, so this bot
        // still fills users' resting orders and must be allowed to start.
        let h = harness("start-rfq-taker");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        let paths = setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let toml = std::fs::read_to_string(&paths.toml).unwrap() + "\nlimit_taker_enabled = true\n";
        std::fs::write(&paths.toml, toml).unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Exited);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running)
        );
    }

    #[tokio::test]
    async fn a_running_bot_that_refuses_to_stop_is_not_destroyed() {
        // Force-removing it is a SIGKILL mid-tick, and the tick can be signing or
        // broadcasting. The 30s grace exists for exactly that, so a stop the
        // daemon refuses has to surface rather than be stepped over.
        let h = harness("stopfail");
        seed_panel_bot(&h, "bot-a");
        h.docker.fail_next("container is restarting, try again");

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("would not stop"), "{body}");
        assert!(body.contains("is running"), "{body}");
        assert!(body.contains("docker stop stitch-bot-a"), "{body}");
        assert!(
            h.docker.exists("stitch-bot-a"),
            "the container must survive"
        );
    }

    #[tokio::test]
    async fn a_paused_bot_that_refuses_to_stop_is_not_destroyed() {
        // Paused is frozen mid-tick — not a terminal state. Treating it like
        // exited would let Remove force-kill a bot that can still be signing.
        let h = harness("pausefail");
        write_bot(&h.root, "bot-a");
        let mut c = container("stitch-bot-a", ContainerState::Paused);
        c.labels.insert(LABEL_BOT.to_string(), "bot-a".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(c);
        h.docker.fail_next("cannot stop a paused container");

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("is paused"), "{body}");
        assert!(body.contains("would not stop"), "{body}");
        assert!(h.docker.exists("stitch-bot-a"));
    }

    #[tokio::test]
    async fn recreate_asks_the_registry_even_when_the_image_is_cached() {
        // Recreate is the upgrade path. ensure_image's cache-hit short-circuit
        // would leave a mutable `:latest` on whatever the host already had.
        let h = harness("recreate-refresh");
        seed_panel_bot(&h, "bot-a");

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            h.docker.calls().contains(&Call::EnsureImage {
                image: h.state.cfg.bot_image.clone(),
                refresh: true,
            }),
            "recreate must refresh, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn a_stopped_bot_is_removed_even_if_the_daemon_grumbles() {
        // Nothing to shut down gracefully, so a stop error here is noise and must
        // not block the removal the operator asked for.
        let h = harness("stopnoise");
        seed_panel_bot(&h, "bot-a");
        crate::panel::docker::DockerApi::stop(&*h.docker, "stitch-bot-a", 0)
            .await
            .unwrap();
        h.docker.fail_next("container already stopped");

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.docker.exists("stitch-bot-a"));
    }

    #[tokio::test]
    async fn a_duplicate_name_blocks_every_lifecycle_action() {
        // Two containers, one bot entry. An action would hit whichever one
        // discovery picked, and delete-with-config would take the files out from
        // under the other while it's still trading. The warning already says not
        // to use the controls; this makes that true.
        let h = harness("dupe-actions");
        seed_panel_bot(&h, "bot-a");
        let mut rival = container("other-bot-a", ContainerState::Running);
        rival
            .labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot-a".to_string());
        rival.mounts = dir_layout_mounts(&h.root.join("bot-a").display().to_string());
        h.docker.add_container(rival);

        for action in ["stop", "start", "restart", "migrate"] {
            let (status, body) = h
                .post_json(&format!("/api/bots/bot-a/{action}"), serde_json::json!({}))
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
            assert!(body.contains("More than one container"), "{action}: {body}");
        }

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // Nothing was touched: both containers still there, config still on disk.
        assert!(h.docker.exists("stitch-bot-a"));
        assert!(h.docker.exists("other-bot-a"));
        assert!(h.root.join("bot-a").join("stitch.toml").exists());
    }

    #[tokio::test]
    async fn stopping_warns_that_live_orders_outlive_the_bot() {
        let h = harness("stop-msg");
        seed_panel_bot(&h, "bot-a");
        let (_, body) = h
            .post_json("/api/bots/bot-a/stop", serde_json::json!({}))
            .await;
        let v = Harness::parse(&body);
        assert!(
            v["message"].as_str().unwrap().contains("stay on the book"),
            "{body}"
        );
        assert_eq!(v["bot"]["running"], false);
    }

    #[tokio::test]
    async fn a_daemon_failure_on_start_is_reported_not_swallowed() {
        let h = harness("start-fail");
        // Stopped, so Start actually reaches `docker start` — a running bot short-circuits
        // before it, since starting an already-live container is a no-op.
        seed_panel_bot_in_state(&h, "bot-a", ContainerState::Exited);
        h.docker.fail_next("no space left on device");
        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("no space left on device"), "{body}");
    }

    #[tokio::test]
    async fn a_config_only_bot_cannot_be_started_and_says_why() {
        // A bot whose container was removed still shows up, so the operator can
        // see its config — but starting it is a recreate, not a start.
        let h = harness("no-container");
        write_bot(&h.root, "bot-a");
        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("no container"), "{body}");
    }

    #[tokio::test]
    async fn delete_removes_the_container_and_keeps_the_config() {
        let h = harness("delete");
        seed_panel_bot(&h, "bot-a");
        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.docker.exists("stitch-bot-a"));
        assert!(h.root.join("bot-a/stitch.toml").exists());
        assert!(body.contains("still on disk"), "{body}");
    }

    #[tokio::test]
    async fn delete_with_the_flag_also_removes_the_config_directory() {
        let h = harness("delete-config");
        let _node = seed_with_empty_wallet(&h, "bot-a").await;
        let (status, body) = delete_config(&h, "bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.root.join("bot-a").exists());
    }

    #[tokio::test]
    async fn deleting_the_config_is_refused_while_the_wallet_holds_money() {
        // The key is the only way to that money. The page disables Remove on
        // the same rule, but its read can be seconds old, so the route reads
        // the wallet again itself.
        let h = harness("delete-config-funded");
        let node =
            mock_rpc(MockChain::default().balance(BSC_USDT, 25_000_000_000_000_000_000)).await;
        seed_panel_bot(&h, "bot-a");
        point_at_chain(&h, "bot-a", &node.url);
        let (status, body) = delete_config(&h, "bot-a").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("$25.00"), "{body}");
        assert!(body.contains("Withdraw first"), "{body}");
        assert!(h.root.join("bot-a/stitch.key").exists(), "nothing deleted");
        assert!(h.docker.exists("stitch-bot-a"), "nothing destroyed");
    }

    #[tokio::test]
    async fn deleting_the_config_is_refused_while_the_wallet_cannot_be_read() {
        // A wallet that can't be read could hold anything.
        let h = harness("delete-config-unreadable");
        seed_panel_bot(&h, "bot-a");
        point_at_chain(&h, "bot-a", "http://127.0.0.1:1");
        let (status, body) = delete_config(&h, "bot-a").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("cannot read this wallet"), "{body}");
        assert!(h.root.join("bot-a/stitch.key").exists(), "nothing deleted");
    }

    #[tokio::test]
    async fn deleting_a_config_with_no_key_needs_no_wallet_read() {
        // No key, nothing to lose: the gate does not even ask the chain.
        let h = harness("delete-config-no-key");
        let node = seed_with_empty_wallet(&h, "bot-a").await;
        std::fs::remove_file(h.root.join("bot-a/stitch.key")).unwrap();
        let (status, body) = delete_config(&h, "bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.root.join("bot-a").exists());
        assert_eq!(node.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn deleting_an_adopted_bots_config_outside_the_root_is_refused_politely() {
        // The panel will not recursively delete a directory it doesn't own.
        let h = harness("delete-foreign");
        let mut c = container("stitch-adopted", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "adopted".to_string());
        c.mounts = dir_layout_mounts("/srv/elsewhere/adopted");
        h.docker.add_container(c);

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/adopted?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("delete it by hand"), "{body}");
    }

    #[tokio::test]
    async fn delete_config_follows_the_mounted_directory_not_the_bot_name() {
        // Compose service `foo` mounting `bots/custom-dir` — the old path used
        // bots/foo and left the real config behind.
        let h = harness("delete-compose-path");
        let node = mock_rpc(MockChain::default()).await;
        write_bot(&h.root, "custom-dir");
        point_at_chain(&h, "custom-dir", &node.url);
        let mut c = container("stitch-foo", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "foo".to_string());
        c.mounts = dir_layout_mounts(&h.root.join("custom-dir").display().to_string());
        h.docker.add_container(c);

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/foo?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.docker.exists("stitch-foo"));
        assert!(
            !h.root.join("custom-dir").exists(),
            "mounted config dir must be deleted"
        );
        assert!(
            !h.root.join("foo").exists(),
            "must not invent bots/foo from the service name"
        );
        assert!(body.contains("gone"), "{body}");
    }

    #[tokio::test]
    async fn delete_config_removes_flat_layout_files() {
        let h = harness("delete-flat");
        let node = mock_rpc(MockChain::default()).await;
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        let toml = h.root.join("stitch.bot1.toml");
        let key = h.root.join("stitch.bot1.key");
        std::fs::write(
            &toml,
            corridor
                .toml_template
                .replace("https://bsc-dataseed.binance.org", &node.url)
                .replace("https://api.textilecredit.com", "http://127.0.0.1:1"),
        )
        .unwrap();
        std::fs::write(&key, super::super::testkit::TEST_KEY).unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot1?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.docker.exists("stitch-bot1"));
        assert!(!toml.exists(), "flat toml must be deleted");
        assert!(!key.exists(), "flat key must be deleted");
        // The bots root itself must survive — only the bot's files go.
        assert!(h.root.is_dir());
    }

    #[tokio::test]
    async fn delete_config_removes_a_config_only_bot() {
        let h = harness("delete-config-only");
        let node = mock_rpc(MockChain::default()).await;
        write_bot(&h.root, "bot-a");
        point_at_chain(&h, "bot-a", &node.url);
        let (status, body) = delete_config(&h, "bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.root.join("bot-a").exists());
        assert!(body.contains("config is gone"), "{body}");
    }

    #[tokio::test]
    async fn delete_without_config_flag_refuses_a_config_only_bot() {
        // Otherwise Remove reported success and left the row on the fleet page.
        let h = harness("delete-config-only-noop");
        write_bot(&h.root, "bot-a");
        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("no container"), "{body}");
        assert!(h.root.join("bot-a/stitch.toml").exists());
    }

    #[tokio::test]
    async fn delete_flat_does_not_wipe_a_neighbours_canonical_secret() {
        // A hot-wallet bot must not delete a Turnkey secret that lives next to
        // it under the bare canonical name — that belongs to another bot.
        let h = harness("delete-flat-neighbour");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        std::fs::write(
            h.root.join("stitch.bot1.key"),
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let neighbour_secret = h.root.join("turnkey-api.key");
        std::fs::write(&neighbour_secret, "neighbour-secret").unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot1?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.root.join("stitch.bot1.toml").exists());
        assert!(!h.root.join("stitch.bot1.key").exists());
        assert!(
            neighbour_secret.exists(),
            "neighbour's canonical secret must survive"
        );
        assert_eq!(
            std::fs::read_to_string(&neighbour_secret).unwrap(),
            "neighbour-secret"
        );
    }

    #[tokio::test]
    async fn delete_flat_removes_a_turnkey_bots_canonical_secret() {
        // Compose often keeps turnkey-api.key at the bare name beside
        // stitch.<bot>.toml. Derived-only delete would leave the credential.
        let h = harness("delete-flat-turnkey");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        let toml = h.root.join("stitch.bot1.toml");
        let mut body = corridor.toml_template.to_string();
        body.push_str(
            "\n[signer]\nprovider = \"turnkey\"\n\
             organization_id = \"org-1\"\n\
             sign_with = \"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"\n\
             operator_address = \"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"\n",
        );
        std::fs::write(&toml, body).unwrap();
        let secret = h.root.join("turnkey-api.key");
        std::fs::write(&secret, "turnkey-secret").unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        // Flat mounts assume stitch.bot1.key; point the secret mount at the
        // canonical Turnkey file this bot actually uses.
        c.mounts[1].source = secret.clone();
        h.docker.add_container(c);

        let (status, resp) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot1?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        assert!(!toml.exists());
        assert!(
            !secret.exists(),
            "this bot's canonical Turnkey secret must be deleted"
        );
    }

    #[tokio::test]
    async fn delete_config_refuses_when_another_bot_shares_the_directory() {
        let h = harness("delete-shared-dir");
        write_bot(&h.root, "shared");
        for (cname, service) in [("stitch-alpha", "alpha"), ("stitch-beta", "beta")] {
            let mut c = container(cname, ContainerState::Running);
            c.labels
                .insert(LABEL_COMPOSE_SERVICE.to_string(), service.to_string());
            c.mounts = dir_layout_mounts(&h.root.join("shared").display().to_string());
            h.docker.add_container(c);
        }

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/alpha?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("shares its config"), "{body}");
        assert!(body.contains("beta"), "{body}");
        assert!(
            h.docker.exists("stitch-alpha"),
            "refused delete must not remove the container"
        );
        assert!(h.docker.exists("stitch-beta"));
        assert!(
            h.root.join("shared/stitch.toml").exists(),
            "shared config must survive"
        );
    }

    #[tokio::test]
    async fn a_flat_layout_bot_is_flagged_and_offered_a_migration() {
        let h = harness("flat");
        // The flat layout keeps both files loose in one directory.
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        std::fs::write(
            h.root.join("stitch.bot1.key"),
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        let (status, body) = h.get("/api/bots/bot1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["layout"], "flat-files");
        assert_eq!(v["canMigrate"], true);
        let kinds: Vec<_> = v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["kind"].as_str().unwrap().to_string())
            .collect();
        assert!(
            kinds.contains(&"ledgerNotPersisted".to_string()),
            "{kinds:?}"
        );
    }

    #[tokio::test]
    async fn migrating_moves_the_bot_into_the_per_bot_layout() {
        let h = harness("migrate");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        std::fs::write(
            h.root.join("stitch.bot1.key"),
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        h.docker.set_container_files(vec![(
            "stitch.56.0xabc.slot-nonces.json".to_string(),
            b"{}".to_vec(),
        )]);
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot1/migrate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["bot"]["layout"], "directory");
        assert_eq!(v["started"], true);
        assert!(h.root.join("bot1/stitch.toml").exists());
        assert!(h
            .root
            .join("bot1/stitch.56.0xabc.slot-nonces.json")
            .exists());
    }

    #[tokio::test]
    async fn migrating_an_already_good_bot_is_a_conflict() {
        let h = harness("migrate-noop");
        seed_panel_bot(&h, "bot-a");
        let (status, body) = h
            .post_json("/api/bots/bot-a/migrate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("already uses"), "{body}");
    }

    #[tokio::test]
    async fn recreate_brings_a_container_less_bot_back() {
        let h = harness("recreate");
        write_bot(&h.root, "bot-a");
        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(h.docker.exists("stitch-bot-a"));
        // It wasn't up before, so it isn't started now.
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Created)
        );
        assert!(body.contains("wasn't up before"), "{body}");
    }

    #[tokio::test]
    async fn restart_is_refused_on_a_stopped_bot() {
        // `docker restart` on a stopped container starts it, so an unguarded Restart
        // is a second Start button with the wrong label — and the UI shows both next
        // to each other. Clicking it would put a deliberately stopped bot, or one
        // straight out of the wizard waiting on its allowance, back on the book.
        for state in [
            ContainerState::Created,
            ContainerState::Exited,
            ContainerState::Dead,
        ] {
            let h = harness(&format!("restart-{}", state.as_str()));
            seed_panel_bot_in_state(&h, "bot-a", state);

            let (status, body) = h
                .post_json("/api/bots/bot-a/restart", serde_json::json!({}))
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{state:?}: {body}");
            assert!(body.contains("nothing to restart"), "{body}");
            assert!(
                !h.docker
                    .calls()
                    .iter()
                    .any(|c| matches!(c, Call::Restart { .. })),
                "{state:?} must not reach the daemon"
            );
            // The UI is told, so the button is disabled rather than 409ing.
            let (_, body) = h.get("/api/bots/bot-a").await;
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["canStop"], false, "{body}");
        }
    }

    #[tokio::test]
    async fn restart_still_works_on_a_live_bot() {
        for state in [ContainerState::Running, ContainerState::Restarting] {
            let h = harness(&format!("restart-live-{}", state.as_str()));
            seed_panel_bot_in_state(&h, "bot-a", state);
            let (status, body) = h
                .post_json("/api/bots/bot-a/restart", serde_json::json!({}))
                .await;
            assert_eq!(status, StatusCode::OK, "{state:?}: {body}");
        }
    }

    #[tokio::test]
    async fn a_bot_cannot_be_started_while_an_approval_owns_its_wallet() {
        // The other side of the approve guard. With the bot stopped the approval is
        // allowed, and then nothing stopped a second tab from starting the bot while
        // that approval is still broadcasting — both read the same pending nonce.
        // Unconditional on taker/closer, because every bot runs the allowance
        // preflight at live start.
        let h = harness("start-during-approval");
        seed_panel_bot_in_state(&h, "bot-a", ContainerState::Exited);
        let bot = h.state.bot("bot-a").await.expect("seeded");
        let wallet = bot.wallet().expect("a hot wallet has an address");
        let _claim = h
            .state
            .wallet_locks
            .try_claim(wallet)
            .expect("nothing else holds it");

        for action in ["start", "recreate"] {
            let (status, body) = h
                .post_json(&format!("/api/bots/bot-a/{action}"), serde_json::json!({}))
                .await;
            // Recreate on a stopped bot doesn't start it, so only Start is refused.
            let expected = if action == "start" {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            assert_eq!(status, expected, "{action}: {body}");
            if action == "start" {
                assert!(body.contains("approval is running"), "{body}");
            }
        }
    }

    #[tokio::test]
    async fn starting_a_bot_is_refused_when_a_live_sibling_shares_its_wallet() {
        // A reservation only exists for the duration of a launch or an approval, so a
        // bot that is *already* running holds nothing and the set says the wallet is
        // free while its taker spends nonces. The fleet is the other half of the
        // question, and only asking the reservation missed it entirely.
        let h = harness("start-live-sibling");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        seed_transacting(&h, "bot-b", ContainerState::Running);

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("shares its operator wallet"), "{body}");
        assert!(body.contains("bot-b"), "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Exited),
            "nothing may have been started"
        );
    }

    #[tokio::test]
    async fn a_maker_only_sibling_does_not_block_a_start() {
        // Same wallet, but a maker consumes no account nonce. Blocking here would make
        // every multi-corridor fleet unstartable one bot at a time.
        let h = harness("start-maker-sibling");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        seed_panel_bot(&h, "bot-b");
        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn an_existing_overlap_does_not_block_the_restart_that_might_fix_it() {
        // Two live transacting bots on one wallet are already racing. Refusing to
        // restart one doesn't remove the overlap, it just stops the operator acting on
        // it — so the sibling check only applies when the launch would *add* a signer.
        let h = harness("restart-existing-overlap");
        seed_transacting(&h, "bot-a", ContainerState::Running);
        seed_transacting(&h, "bot-b", ContainerState::Running);

        let (status, body) = h
            .post_json("/api/bots/bot-a/restart", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn recreating_a_live_bot_is_refused_while_an_approval_owns_its_wallet() {
        // Recreate starts the replacement when the bot was up, so it launches a bot
        // just as Start does.
        let h = harness("recreate-during-approval");
        seed_panel_bot(&h, "bot-a");
        let bot = h.state.bot("bot-a").await.expect("seeded");
        let _claim = h
            .state
            .wallet_locks
            .try_claim(bot.wallet().unwrap())
            .expect("nothing else holds it");

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("approval is running"), "{body}");
        assert!(h.docker.exists("stitch-bot-a"), "nothing may be destroyed");
    }

    #[tokio::test]
    async fn recreate_refuses_when_the_signer_secret_is_missing() {
        // The case the guard uniquely catches. The config parses, so nothing upstream
        // objects, and the key is only ever *mounted* — never read by the panel. Without
        // the check, Docker is handed a bind source that isn't there, creates
        // `stitch.key` as a directory, and the bot comes up unable to sign.
        let h = harness("recreate-no-key");
        seed_panel_bot(&h, "bot-a");
        std::fs::remove_file(h.root.join("bot-a/stitch.key")).unwrap();

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("stitch.key"), "{body}");
        assert!(body.contains("silently create it as a directory"), "{body}");
        // The old container is untouched: the check runs before anything destructive.
        assert!(h.docker.exists("stitch-bot-a"), "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. } | Call::Create(_))),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn recreating_a_crash_looping_bot_starts_the_replacement() {
        // Recreate is how an operator installs the image that stops the crash loop.
        // `restarting` makes `is_running()` false, so keying the restart off it left
        // the replacement sitting in `created` — the bot stranded by the very action
        // meant to rescue it.
        let h = harness("recreate-restarting");
        seed_panel_bot_in_state(&h, "bot-a", ContainerState::Restarting);

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running),
            "a bot Docker was restarting must come back up"
        );
        assert!(body.contains("and started"), "{body}");
    }

    #[tokio::test]
    async fn recreating_a_paused_bot_leaves_it_stopped_and_says_so() {
        // Paused can't be reproduced in a fresh container, so the choice is start or
        // leave stopped. It doesn't say the bot was meant to be on the book, so it
        // stays stopped and the message tells the operator.
        let h = harness("recreate-paused");
        seed_panel_bot_in_state(&h, "bot-a", ContainerState::Paused);

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Created)
        );
        assert!(body.contains("wasn't up before"), "{body}");
    }

    #[tokio::test]
    async fn recreate_keeps_the_old_container_when_the_image_cannot_be_pulled() {
        // Recreate is destructive: it removes before it creates. Docker's create
        // endpoint never pulls, so if the image is missing the create fails — and
        // finding that out after the remove would delete a working bot with
        // nothing to replace it. The image check has to come first.
        let h = harness("recreate-nopull");
        seed_panel_bot(&h, "bot-a");
        h.docker.fail_image("manifest unknown");

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_ne!(status, StatusCode::OK, "{body}");
        assert!(body.contains("manifest unknown"), "{body}");
        assert!(
            h.docker.exists("stitch-bot-a"),
            "the bot must survive a failed pull"
        );
    }

    #[tokio::test]
    async fn the_compose_export_is_a_downloadable_file() {
        let h = harness("compose");
        seed_panel_bot(&h, "bot-a");
        let (status, body) = h.get("/api/compose-export").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("services:"), "{body}");
        assert!(body.contains("bot-a"), "{body}");
        // Never the key material, even though the export names the key file.
        assert!(!body.contains(super::super::testkit::TEST_KEY));
    }

    #[tokio::test]
    async fn settle_stops_the_container_and_releases_the_claim_when_the_stop_lands() {
        // The common ambiguous case: `start` reported an error but the daemon may have
        // brought the container up on the claimed wallet. Settling stops it, and once the
        // stop confirms it's gone the claim is safe to drop.
        let h = harness("settle-release");
        seed_panel_bot(&h, "bot-a");
        let wallet = h.state.bot("bot-a").await.unwrap().wallet().unwrap();
        let claim = h
            .state
            .wallet_locks
            .try_claim(wallet.clone())
            .expect("nothing else holds it");

        let _err = super::settle_ambiguous_launch(
            &h.state,
            "stitch-bot-a",
            Some(claim),
            super::ApiError::conflict("boom"),
        )
        .await;

        assert!(
            !h.state.wallet_locks.is_claimed(&wallet),
            "a confirmed stop releases the claim"
        );
        assert!(
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "settle must stop the possibly-live container: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn settle_holds_the_claim_when_the_container_cannot_be_stopped() {
        // If the stop can't confirm the container is gone, releasing the claim would let a
        // sibling launch on the same wallet while this one may still be live. So the claim
        // is handed to the hold task and stays held until a stop lands.
        let h = harness("settle-hold");
        seed_panel_bot(&h, "bot-a"); // running, so the hold task sees it live
        let wallet = h.state.bot("bot-a").await.unwrap().wallet().unwrap();
        let claim = h
            .state
            .wallet_locks
            .try_claim(wallet.clone())
            .expect("nothing else holds it");
        h.docker.fail_stop("daemon unreachable");

        let _err = super::settle_ambiguous_launch(
            &h.state,
            "stitch-bot-a",
            Some(claim),
            super::ApiError::conflict("boom"),
        )
        .await;

        // The stop failed and the container still reads live, so the claim is held.
        assert!(
            h.state.wallet_locks.is_claimed(&wallet),
            "a claim can't be released until the container is confirmed stopped"
        );
    }

    #[tokio::test]
    async fn an_ambiguous_start_settles_the_wallet_rather_than_leaking_it() {
        // End to end through the Start handler: a `start` that errors must not leave the
        // container potentially live on a released wallet — the handler settles it.
        let h = harness("start-settle");
        seed_panel_bot_in_state(&h, "bot-a", ContainerState::Exited);
        h.docker.fail_start("daemon dropped the connection");

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_ne!(
            status,
            StatusCode::OK,
            "the start error must surface: {body}"
        );
        assert!(
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Stop { .. })),
            "an ambiguous start must be settled with a stop: {:?}",
            h.docker.calls()
        );
        // The stop confirmed it gone, so the wallet is free for the next attempt.
        let wallet = h.state.bot("bot-a").await.unwrap().wallet().unwrap();
        assert!(
            !h.state.wallet_locks.is_claimed(&wallet),
            "the claim must be released once the stop confirms the container is gone"
        );
    }

    #[tokio::test]
    async fn starting_an_already_running_bot_is_a_no_op_not_a_shutdown() {
        // The overlapping-Start race: the config lock serializes two Starts, so the
        // second re-reads the bot as already running. `docker start` on a live container
        // can return an error, and treating that as an ambiguous launch would hand the
        // healthy container to settle, which stops it. Start must short-circuit on a
        // non-terminal state instead of touching Docker at all.
        let h = harness("start-already-running");
        seed_panel_bot(&h, "bot-a"); // running

        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Start(_) | Call::Stop { .. })),
            "a running bot must be neither started nor stopped by Start: {:?}",
            h.docker.calls()
        );
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running),
            "the live container must be left running"
        );
    }

    #[tokio::test]
    async fn migration_waits_on_the_target_layout_lock() {
        // The migration moves the config into the per-bot layout path and builds the
        // replacement there. A concurrent action keyed on that target path (a create, a
        // save, a launch) must not run while the move is mid-flight, so the migration
        // holds the target lock too — not just the source path's.
        let h = harness("migrate-target-lock");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        std::fs::write(
            h.root.join("stitch.bot1.key"),
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        // Stand in for a concurrent op holding the target path's lock.
        let target = h.state.cfg.bot_dir("bot1").join("stitch.toml");
        let held = h.state.config_locks.for_path(&target).lock_owned().await;

        let state = h.state.clone();
        let task = tokio::spawn(async move {
            super::migrate_layout(
                axum::extract::State(state),
                axum::extract::Path("bot1".to_string()),
                axum::extract::Query(super::MigrateQuery::default()),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.message)
        });

        // With the target lock held, the migration can't proceed.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            !task.is_finished(),
            "the migration must wait on the target layout lock"
        );

        // Release it and the migration completes.
        drop(held);
        let result = task.await.expect("migration task panicked");
        assert!(result.is_ok(), "migration should succeed: {result:?}");
        assert!(
            h.root.join("bot1/stitch.toml").exists(),
            "the config must have moved into the per-bot layout"
        );
    }

    #[tokio::test]
    async fn update_recreates_on_the_panel_bot_image_with_a_refresh() {
        let h = harness("bot-update");
        seed_panel_bot(&h, "bot-a");
        // Same tag channel as STITCH_PANEL_BOT_IMAGE (:test); a different tag
        // would be refused as off-channel. Stale digest is what Update refreshes.
        h.docker
            .set_container_image("stitch-bot-a", h.state.cfg.bot_image.as_str());

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("recreated"), "{body}");
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::EnsureImage { image, refresh: true }
                    if image == &h.state.cfg.bot_image
            )),
            "update must refresh the panel bot image, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_resolves_a_sha_pin_to_latest_instead_of_recreating_on_the_pin() {
        // Recreate keeps the configured pin. Update must not: a sha-* pin that
        // /api/updates reports as behind would otherwise recreate onto the same
        // old digest forever.
        let h = super::super::testkit::harness_with_bot_image(
            "bot-update-sha-pin",
            "ghcr.io/textile-protocol/textile-stitch:sha-deadbeef",
        );
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:sha-deadbeef",
        );

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let expected = "ghcr.io/textile-protocol/textile-stitch:latest";
        assert!(
            body.contains(expected),
            "response should name the resolved target: {body}"
        );
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::EnsureImage { image, refresh: true } if image == expected
            )),
            "update must pull :latest for a sha-* pin, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_allows_a_bare_sha256_image_id_attributed_via_repo_digests() {
        // Docker often reports Image as a bare content id after the tag is gone.
        // RepoDigests still name the registry repo — Update must use that, not
        // treat `sha256:…` as an off-channel repository named "sha256".
        let h = super::super::testkit::harness_with_bot_image(
            "bot-update-bare-digest",
            "ghcr.io/textile-protocol/textile-stitch:latest",
        );
        seed_panel_bot(&h, "bot-a");
        let digest = "sha256:e67d65aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa055252";
        h.docker.set_container_image("stitch-bot-a", digest);
        h.docker.set_image_digests(
            digest,
            vec!["ghcr.io/textile-protocol/textile-stitch@sha256:e67d65aa".into()],
        );

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::EnsureImage { image, refresh: true }
                    if image == "ghcr.io/textile-protocol/textile-stitch:latest"
            )),
            "bare digest bot must Update onto :latest, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_allows_a_panel_bot_with_a_bare_digest_and_no_repo_digests() {
        // Tag pruned, RepoDigests empty — panel origin is enough to know this
        // bot was launched from STITCH_PANEL_BOT_IMAGE.
        let h = super::super::testkit::harness_with_bot_image(
            "bot-update-bare-digest-panel",
            "ghcr.io/textile-protocol/textile-stitch:latest",
        );
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "sha256:e67d65aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa055252",
        );

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn update_allows_a_sha_pinned_bot_when_the_panel_targets_latest() {
        // Production often pins bots at create time while STITCH_PANEL_BOT_IMAGE
        // is `:latest` (or later moves there). Those bots must still be able to
        // leave the pin via Update — not get a channel-gate conflict.
        let h = super::super::testkit::harness_with_bot_image(
            "bot-update-sha-on-latest",
            "ghcr.io/textile-protocol/textile-stitch:latest",
        );
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:sha-oldc0de",
        );

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let expected = "ghcr.io/textile-protocol/textile-stitch:latest";
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::EnsureImage { image, refresh: true } if image == expected
            )),
            "sha-pinned bot must Update onto the panel's :latest channel, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_allows_a_bot_on_a_different_sha_pin_than_the_panel() {
        // Panel env advanced to a new sha-* pin; existing bots still run the old
        // one. Update resolves pins to :latest, so they must not be stuck.
        let h = super::super::testkit::harness_with_bot_image(
            "bot-update-other-sha",
            "ghcr.io/textile-protocol/textile-stitch:sha-newpin00",
        );
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:sha-oldpin00",
        );

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::EnsureImage { image, refresh: true }
                    if image == "ghcr.io/textile-protocol/textile-stitch:latest"
            )),
            "old sha pin must still Update to :latest, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_refuses_when_the_fresh_pull_fails() {
        // Same contract as panel self-update: a pull failure must not destroy the
        // bot and recreate it onto a stale cached tag.
        let h = harness("bot-update-pull-fail");
        seed_panel_bot(&h, "bot-a");
        // Default fake image is :latest; harness bot_image is :test — put the
        // bot on-channel so we exercise the pull failure, not the channel gate.
        h.docker
            .set_container_image("stitch-bot-a", h.state.cfg.bot_image.as_str());
        h.docker.fail_image("manifest unknown / rate limited");

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(
            body.contains("stale local copy") || body.contains("pulling"),
            "{body}"
        );
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running),
            "failed Update must leave the live container alone"
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. })),
            "must not remove before a successful fresh pull: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_refuses_a_custom_image_bot() {
        // /api/updates already hides these; the endpoint must too so a stale UI
        // or curl can't silently swap a fork onto the panel default.
        let h = harness("bot-update-custom-image");
        seed_panel_bot(&h, "bot-a");
        h.docker
            .set_container_image("stitch-bot-a", "ghcr.io/acme/stitch-fork:v9");

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body.contains("not on the update channel") || body.contains("stitch-fork"),
            "{body}"
        );
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running)
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. })),
            "must not recreate a custom-image bot via Update: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_refuses_a_same_repo_alternate_tag() {
        // :canary shares the repo with :latest but is a different channel.
        let h = harness("bot-update-canary");
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:canary",
        );

        let (status, body) = h
            .post_json("/api/bots/bot-a/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("not on the update channel"), "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. })),
            "must not move a canary bot onto :latest via Update: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn update_refuses_a_flat_layout_bot() {
        // Flat layout keeps the nonce ledger in the container. Update recreates
        // without migrating, so the API must refuse even when the UI is bypassed.
        // Mount individual files from the panel bot dir so the recreate path's
        // `bot_dir` check would otherwise pass.
        let h = harness("bot-update-flat");
        let dir = h.root.join("bot1");
        std::fs::create_dir_all(&dir).unwrap();
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(dir.join("stitch.toml"), corridor.toml_template).unwrap();
        std::fs::write(dir.join("stitch.key"), super::super::testkit::TEST_KEY).unwrap();
        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        // Flat mounts (file-by-file) from the panel bot directory.
        c.mounts = vec![
            crate::panel::docker::MountInfo {
                source: dir.join("stitch.toml"),
                destination: std::path::PathBuf::from("/home/stitch/run/stitch.toml"),
                rw: false,
            },
            crate::panel::docker::MountInfo {
                source: dir.join("stitch.key"),
                destination: std::path::PathBuf::from("/home/stitch/run/stitch.key"),
                rw: false,
            },
        ];
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot1/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body.contains("flat file layout") || body.contains("Migrate"),
            "{body}"
        );
        assert_eq!(
            h.docker.state_of("stitch-bot1"),
            Some(ContainerState::Running),
            "flat-layout Update must leave the container alone"
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. })),
            "must not remove a flat-layout bot on Update: {:?}",
            h.docker.calls()
        );
    }

    /// A running flat-layout bot whose config sits in its own panel directory,
    /// so only the layout — not the config path — is what a caller trips over.
    fn seed_flat_layout_bot(h: &Harness, name: &str) {
        let dir = h.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(dir.join("stitch.toml"), corridor.toml_template).unwrap();
        std::fs::write(dir.join("stitch.key"), super::super::testkit::TEST_KEY).unwrap();
        let mut c = container(&format!("stitch-{name}"), ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), name.to_string());
        c.mounts = vec![
            crate::panel::docker::MountInfo {
                source: dir.join("stitch.toml"),
                destination: std::path::PathBuf::from("/home/stitch/run/stitch.toml"),
                rw: false,
            },
            crate::panel::docker::MountInfo {
                source: dir.join("stitch.key"),
                destination: std::path::PathBuf::from("/home/stitch/run/stitch.key"),
                rw: false,
            },
        ];
        h.docker.add_container(c);
    }

    #[tokio::test]
    async fn rollback_refuses_a_tag_that_doesnt_name_one_build() {
        // `latest` moves on the next release, so pinning to it isn't a pin —
        // and the operator meant Update anyway.
        let h = harness("rollback-channel-tag");
        seed_panel_bot(&h, "bot-a");

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rollback",
                serde_json::json!({ "tag": "latest" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("Use Update"), "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Remove { .. })),
            "a refused tag must not touch the container: {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn rollback_refuses_a_flat_layout_bot() {
        // Same reason as Update: the ledger is inside the container, and a
        // rollback recreates it. Live orders would be left unreplaceable.
        let h = harness("rollback-flat");
        seed_flat_layout_bot(&h, "bot1");

        let (status, body) = h
            .post_json(
                "/api/bots/bot1/rollback",
                serde_json::json!({ "tag": "sha-14cd877" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("flat file layout"), "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot1"),
            Some(ContainerState::Running),
            "a refused rollback must leave the container alone"
        );
    }

    #[tokio::test]
    async fn rollback_refuses_the_build_the_bot_already_runs() {
        let h = super::super::testkit::harness_with_bot_image(
            "rollback-same-build",
            "ghcr.io/textile-protocol/textile-stitch:latest",
        );
        seed_panel_bot(&h, "bot-a");
        h.docker.set_container_image(
            "stitch-bot-a",
            "ghcr.io/textile-protocol/textile-stitch:sha-14cd877",
        );

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rollback",
                serde_json::json!({ "tag": "sha-14cd877" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("already runs"), "{body}");
    }

    #[tokio::test]
    async fn rollback_recreates_the_bot_on_the_chosen_build() {
        let h = harness("rollback-ok");
        seed_panel_bot(&h, "bot-a");
        h.docker
            .set_container_image("stitch-bot-a", h.state.cfg.bot_image.as_str());

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rollback",
                serde_json::json!({ "tag": "sha-14cd877" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let expected = "ghcr.io/textile-protocol/textile-stitch:sha-14cd877";
        assert!(body.contains("rolled back"), "{body}");
        // The pin is the part operators forget, so the reply has to say it.
        assert!(body.contains("stays on that version"), "{body}");
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::EnsureImage { image, refresh: true } if image == expected
            )),
            "rollback must pull the chosen build, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn rollback_keeps_the_container_when_the_chosen_build_cannot_be_pulled() {
        // A tag that never existed, or a registry that's down: the bot must be
        // left standing rather than removed and unable to come back.
        let h = harness("rollback-nopull");
        seed_panel_bot(&h, "bot-a");
        h.docker
            .set_container_image("stitch-bot-a", h.state.cfg.bot_image.as_str());
        h.docker.fail_image("manifest unknown");

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/rollback",
                serde_json::json!({ "tag": "sha-0000000" }),
            )
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.contains("Roll back"), "{body}");
        assert_eq!(
            h.docker.state_of("stitch-bot-a"),
            Some(ContainerState::Running)
        );
    }

    #[test]
    fn recovery_keeps_a_pinned_build_and_nothing_else() {
        use super::recreate_image;
        let configured = "ghcr.io/textile-protocol/textile-stitch:latest";
        let pin = "ghcr.io/textile-protocol/textile-stitch:sha-14cd877";
        // What a rollback leaves behind, and what an operator pins by hand.
        assert_eq!(recreate_image(Some(pin), configured), pin);
        assert_eq!(
            recreate_image(
                Some("ghcr.io/textile-protocol/textile-stitch@sha256:aaaa"),
                configured
            ),
            "ghcr.io/textile-protocol/textile-stitch@sha256:aaaa"
        );
        // A channel is not a pin: recreating means "on the current release".
        assert_eq!(recreate_image(Some(configured), configured), configured);
        assert_eq!(
            recreate_image(
                Some("ghcr.io/textile-protocol/textile-stitch:canary"),
                configured
            ),
            configured
        );
        // Nothing ties a bare content id or a fork to our channel.
        assert_eq!(
            recreate_image(Some("sha256:abcdef"), configured),
            configured
        );
        assert_eq!(
            recreate_image(Some("ghcr.io/acme/stitch-fork:sha-14cd877"), configured),
            configured
        );
        assert_eq!(recreate_image(None, configured), configured);
    }

    #[tokio::test]
    async fn recreate_keeps_a_rolled_back_bot_on_its_build() {
        // The hole this closes: Recreate is the recovery button, and rebuilding
        // a rolled-back bot on :latest would silently reinstall the release the
        // operator rolled away from — while the UI promises the pin holds.
        let h = super::super::testkit::harness_with_bot_image(
            "recreate-keeps-pin",
            "ghcr.io/textile-protocol/textile-stitch:latest",
        );
        seed_panel_bot(&h, "bot-a");
        let pin = "ghcr.io/textile-protocol/textile-stitch:sha-14cd877";
        h.docker.set_container_image("stitch-bot-a", pin);

        let (status, body) = h
            .post_json("/api/bots/bot-a/recreate", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let created = h.docker.create_specs();
        let spec = created.last().expect("a replacement container was created");
        assert_eq!(
            spec.image, pin,
            "recovery must not move a pinned bot onto the configured channel"
        );
        assert!(body.contains("sha-14cd877"), "{body}");
    }

    #[tokio::test]
    async fn versions_says_why_a_rollback_is_blocked_rather_than_failing() {
        // The picker shows the reason beside a disabled list. A blocked bot
        // still gets a 200 — the rest of the Tools tab has to keep working.
        let h = harness("rollback-versions-blocked");
        seed_flat_layout_bot(&h, "bot1");

        let (status, body) = h.get("/api/bots/bot1/versions").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["canRollBack"], false, "{body}");
        assert!(
            v["blockedReason"]
                .as_str()
                .unwrap_or_default()
                .contains("flat file layout"),
            "{body}"
        );
    }
}
