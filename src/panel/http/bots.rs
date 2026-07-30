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
use crate::panel::docker::STOP_GRACE_SECS;
use crate::panel::inventory::{Bot, ConfigSummary, Fleet, Warning};
use crate::panel::{compose, migrate, provision};

/// One bot, as the UI sees it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotBody {
    pub name: String,
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
    /// Why not, when it can't. Same reason as `migrateBlockedReason`: the operator
    /// should read it before clicking, not after.
    pub approve_blocked_reason: Option<String>,
    pub config: Option<ConfigBody>,
    pub warnings: Vec<WarningBody>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigBody {
    pub corridor_id: Option<String>,
    pub corridor_label: Option<String>,
    pub chain_id: u64,
    pub pools: usize,
    /// The signing address. Never the key.
    pub operator_address: Option<String>,
    pub signer: String,
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
            corridor_label: c.corridor_label.clone(),
            chain_id: c.chain_id,
            pools: c.pools,
            operator_address: c.operator_address.clone(),
            signer: c.signer.clone(),
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

/// Project a discovered bot into its JSON shape.
pub fn to_body(bot: &Bot, state: &AppState, fleet: &Fleet) -> BotBody {
    let migrate_check = migrate::check(bot, &state.cfg);
    // Needs the fleet, not just this bot: another bot sharing the operator wallet
    // blocks an approval just as much as this one being live does.
    let approve_check = super::logs::approve_check(bot, fleet);
    BotBody {
        name: bot.name.clone(),
        origin: bot.origin.as_str().to_string(),
        layout: bot.layout.as_str().to_string(),
        container: bot.container_name.clone(),
        state: bot.state.as_str().to_string(),
        status: bot.status.clone(),
        running: bot.state.is_running(),
        can_stop: bot.container_name.is_some() && !bot.state.is_terminal(),
        image: bot.image.clone(),
        created_unix: bot.created_unix,
        editable: bot.is_editable(),
        can_migrate: migrate_check.is_ok(),
        migrate_blocked_reason: migrate_check.err().map(|e| format!("{e:#}")),
        can_approve: approve_check.is_ok(),
        approve_blocked_reason: approve_check.err().map(|e| format!("{e:#}")),
        config: bot.config.as_ref().map(ConfigBody::from),
        warnings: bot.warnings.iter().map(WarningBody::from).collect(),
    }
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
    Ok(Json(FleetBody {
        bots: fleet
            .bots()
            .iter()
            .map(|b| to_body(b, &state, &fleet))
            .collect(),
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
        bot: to_body(&bot, state, &fleet),
        message,
    })
    .into_response())
}

/// Reserve this bot's operator wallet for the duration of a launch.
///
/// Same protocol as the approve route, deliberately — see
/// [`WalletReservations`](super::logs::WalletReservations). A launch and an approval
/// are both "a process is about to sign with this key", and a check that reads a flag
/// and then calls Docker leaves a window the other side can pass through. So this
/// takes the reservation and holds it across the Docker call, rather than asking
/// whether anyone else has it.
///
/// Returned rather than dropped: the caller has to keep it alive until the container
/// is actually up, or the gap reopens.
///
/// Unconditional on the config, unlike `approve_check`'s taker/closer test: *every*
/// bot runs the allowance preflight at live start, so starting any bot on that wallet
/// broadcasts, maker-only or not.
pub async fn reserve_for_launch(
    bot: &Bot,
    state: &AppState,
) -> Result<Option<logs::WalletGuard>, ApiError> {
    let guard = state.reservations.reserve_for(bot).ok_or_else(|| {
        ApiError::conflict(format!(
            "{}'s operator wallet is busy — an approval is running against it, or another bot on \
             it is being launched. Starting now means two processes reading the same pending \
             nonce, and one of the two transactions is lost. Wait for that to finish.",
            bot.name
        ))
    })?;

    // The reservation covers other *launches*, not bots that are already up: a running
    // bot holds no reservation, so the set says "free" while its taker spends nonces.
    // The fleet is the other half of the question, and it's asked after the reservation
    // so nothing can start on this wallet between the answer and the action.
    //
    // Only when this bot isn't already a live transactor. If it is, the overlap exists
    // already and refusing the restart or recreate that might fix it helps nobody.
    if !logs::already_transacting(bot) {
        let fleet = state.fleet().await?;
        logs::no_live_sibling_on_the_wallet(bot, &fleet).map_err(ApiError::conflict)?;
    }
    Ok(guard)
}

pub async fn start(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    super::require_actionable(&bot)?;
    let container = bot.require_container().map_err(ApiError::conflict)?;
    // Held across the start, not checked before it.
    let _wallet = reserve_for_launch(&bot, &state).await?;
    state.docker.start(container).await?;
    tracing::info!(bot = %name, "started");
    action_response(&state, &name, None).await
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
    let bot = state.bot(&name).await?;
    super::require_actionable(&bot)?;
    let container = bot.require_container().map_err(ApiError::conflict)?;
    if bot.state.is_terminal() {
        return Err(ApiError::conflict(format!(
            "{name} is {} — there is nothing to restart, and `docker restart` on a stopped \
             container starts it. Use Start if you mean to put it back on the book.",
            bot.state.as_str()
        )));
    }
    let _wallet = reserve_for_launch(&bot, &state).await?;
    state.docker.restart(container, STOP_GRACE_SECS).await?;
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
    let bot = state.bot(&name).await?;
    super::require_actionable(&bot)?;
    if let Some(container) = &bot.container_name {
        stop_before_destroying(&state, &bot, container).await?;
        state.docker.remove(container, true).await?;
    }

    let mut message = format!("{name}'s container is gone. Its config is still on disk.");
    if query.delete_config {
        let dir = state.cfg.bot_dir(&name);
        // Only ever inside the bots root: an adopted bot's config can live
        // anywhere on the host, and the panel is not going to recursively delete
        // a directory it doesn't own.
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "the container was removed, but deleting {} failed: {e}",
                        dir.display()
                    ),
                )
            })?;
            message = format!("{name} and its config directory are gone.");
        } else {
            message = format!(
                "{name}'s container is gone. Its config isn't under {}, so the panel left it \
                 alone — delete it by hand if you meant to.",
                state.cfg.bots_dir.display()
            );
        }
    }

    tracing::info!(bot = %name, delete_config = query.delete_config, "removed");
    Ok(Json(serde_json::json!({ "message": message })).into_response())
}

/// Recreate a bot's container from its config on disk, in the panel's layout.
///
/// This is how a bot whose container was removed comes back, and how an operator
/// picks up a new image tag.
pub async fn recreate(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
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

    // `wants_to_be_up`, not `is_running`: a bot Docker is restarting is one the
    // operator means to have up, and Recreate is often how they install the image
    // that stops it crashing. Leaving that replacement in `created` strands the bot
    // in exactly the case the action was meant to fix.
    let restart_after = bot.state.wants_to_be_up();
    // Recreate starts the replacement, so it's a bot-launching action like Start. The
    // reservation is held for the rest of the handler — the create and the start both
    // sit inside it.
    let _wallet = if restart_after {
        reserve_for_launch(&bot, &state).await?
    } else {
        None
    };

    // Everything that can fail happens before the old container is destroyed:
    // reading the signer out of the config, and getting the image onto the host.
    // Removing first and only then discovering the image can't be pulled would
    // leave the operator with no bot and nothing to bring back.
    let signer = provision::signer_runtime(&dir)?;
    let corridor = bot.config.as_ref().and_then(|c| c.corridor_id.clone());
    // The configured image, not the one it's running: recreate is the action that
    // exists to pick up a new one. Everything else preserves what the bot runs.
    let spec = provision::bot_container_spec(
        &state.cfg,
        &name,
        &state.cfg.bot_image,
        &signer,
        corridor.as_deref(),
    );
    provision::check_file_mounts(&spec.binds, &state.cfg).map_err(ApiError::conflict)?;
    state.docker.ensure_image(&spec.image, true).await?;

    if let Some(container) = &bot.container_name {
        stop_before_destroying(&state, &bot, container).await?;
        state.docker.remove(container, true).await?;
    }

    // The old container is gone by now. `ensure_image` above proves the image exists,
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
        state.docker.start(&spec.name).await.map_err(|e| {
            ApiError::internal(&e.context(format!(
                "starting {name} after recreating it. The new container exists and holds the                  right config, so Start will bring it up once the cause is fixed."
            )))
        })?;
    }

    tracing::info!(bot = %name, image = %spec.image, "recreated");
    action_response(
        &state,
        &name,
        Some(format!(
            "{name} was recreated on {}{}.",
            spec.image,
            if restart_after {
                " and started"
            } else {
                " and left stopped, because it wasn't up before"
            }
        )),
    )
    .await
}

/// Move a bot off the flat-file layout so its nonce ledger survives recreation.
pub async fn migrate_layout(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<MigrateQuery>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    super::require_actionable(&bot)?;
    migrate::check(&bot, &state.cfg).map_err(ApiError::conflict)?;
    // Migration brings a live bot back up at the end, so it launches one too. Held
    // for the whole migration, which is the window that matters.
    let _wallet = if bot.state.wants_to_be_up() {
        reserve_for_launch(&bot, &state).await?
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
        "bot": to_body(&fresh, &state, &fresh_fleet),
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
    async fn an_empty_host_lists_no_bots() {
        let h = harness("empty");
        let (status, body) = h.get("/api/bots").await;
        assert_eq!(status, StatusCode::OK);
        let v = Harness::parse(&body);
        assert!(v["bots"].as_array().unwrap().is_empty());
        assert!(v["botImage"].as_str().unwrap().contains("textile-stitch"));
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
        assert!(bot["config"]["operatorAddress"]
            .as_str()
            .unwrap()
            .starts_with("0x"));
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
        seed_panel_bot(&h, "bot-a");
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
        seed_panel_bot(&h, "bot-a");
        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!h.root.join("bot-a").exists());
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
            .reservations
            .reserve(wallet)
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
            .reservations
            .reserve(bot.wallet().unwrap())
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
}
