// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Log tailing and the two one-shot runs.
//!
//! All three are Server-Sent Events. SSE over WebSockets because the traffic is
//! one-directional, it survives `tailscale serve` without an upgrade dance, and
//! the browser reconnects on its own.
//!
//! The level on each line is parsed here rather than in the browser so the colour
//! rule lives in one place. Lines are already length-capped by the Docker layer;
//! this module bounds the *rate* nothing, deliberately — a bot that floods its log
//! is a bot the operator needs to see flooding.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use super::{ApiError, AppState};
use crate::panel::docker::{LogLine, LogOptions, LogSource, RunEvent};
use crate::panel::inventory::{Bot, Fleet, WalletId, Warning};
use crate::panel::provision::{self, one_shot_spec, signer_runtime_at, OneShot};

/// Ceiling on the replay a client can ask for. A tail of a million lines is a
/// denial of service against the panel's own memory, not a useful request.
const MAX_TAIL: usize = 5_000;

/// How often to send an SSE comment when the bot is quiet, so proxies and load
/// balancers don't decide the connection is dead.
const KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailQuery {
    /// How many historical lines to replay before following.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Follow the log. Off gives a one-shot dump that ends, which is what the
    /// "copy the last 500 lines" button wants.
    #[serde(default)]
    pub follow: Option<bool>,
}

/// One log line, as the UI consumes it.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LineBody {
    pub text: String,
    /// `stdout` or `stderr`.
    pub stream: &'static str,
    /// `error`, `warn`, `info`, `debug`, `trace`, or `plain` when the line carries
    /// no recognisable level.
    pub level: &'static str,
}

impl From<LogLine> for LineBody {
    fn from(line: LogLine) -> Self {
        // Strip first, then classify: the level word arrives wrapped in colour
        // codes, and `\x1b[31mERROR` has an alphanumeric `m` in front of the word
        // that would defeat the boundary check below.
        let text = strip_ansi(&line.text);
        Self {
            level: level_of(&text, line.source),
            stream: match line.source {
                LogSource::Stdout => "stdout",
                LogSource::Stderr => "stderr",
            },
            text,
        }
    }
}

/// Drop ANSI escape sequences from a log line.
///
/// The bot colours its own output, and an adopted container was started without
/// `NO_COLOR` set, so the raw stream is full of `\x1b[2m`. The panel colours lines
/// itself from [`level_of`], so those sequences would render as literal `[2m`
/// garbage in front of every timestamp.
///
/// Handles CSI (`\x1b[...`, ended by a byte in `@`..`~`) and the two-byte escapes
/// `tracing` can emit; an unterminated sequence at the end of a line is dropped
/// rather than passed through.
fn strip_ansi(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs to a BEL or a string terminator.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            // Any other two-byte escape: both bytes go.
            _ => {}
        }
    }
    out
}

/// Classify a line for colouring.
///
/// The bot logs through `tracing`'s default formatter, which puts the level as a
/// bare uppercase word early in the line. Matching on a word boundary keeps a bot
/// that merely mentions "error" in a message from turning the whole line red.
pub fn level_of(text: &str, source: LogSource) -> &'static str {
    for (needle, level) in [
        ("ERROR", "error"),
        ("WARN", "warn"),
        ("INFO", "info"),
        ("DEBUG", "debug"),
        ("TRACE", "trace"),
    ] {
        if contains_word(text, needle) {
            return level;
        }
    }
    // Anything on stderr with no level is still worth flagging: panics and the
    // runtime's own complaints land there unformatted.
    match source {
        LogSource::Stderr => "error",
        LogSource::Stdout => "plain",
    }
}

/// Whether `needle` appears in `text` bounded by non-alphanumeric characters, so
/// `ERROR` matches `2026-01-01 ERROR stitch:` but not `ERRORS_TOTAL`.
fn contains_word(text: &str, needle: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(needle).any(|(at, _)| {
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let end = at + needle.len();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        before_ok && after_ok
    })
}

/// Tail a bot's container log.
pub async fn tail(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<TailQuery>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    let container = bot
        .require_container()
        .map_err(ApiError::conflict)?
        .to_string();

    let opts = LogOptions {
        follow: query.follow.unwrap_or(true),
        tail: query
            .tail
            .unwrap_or(LogOptions::default().tail)
            .min(MAX_TAIL),
    };
    let stream = state.docker.logs(&container, opts).map(|item| {
        Ok::<Event, Infallible>(match item {
            Ok(line) => json_event("line", &LineBody::from(line)),
            // A broken log stream is itself news. Send it as an error event rather
            // than closing silently, so the UI can say why the tail stopped.
            Err(e) => json_event("error", &serde_json::json!({ "message": format!("{e:#}") })),
        })
    });

    Ok(sse(stream))
}

/// Whether a bot's own process can broadcast a transaction right now.
///
/// Two halves, and both matter. The taker and closer legs call the reactor on
/// chain; a maker-only config signs Permit2 orders offchain and consumes no nonce.
/// And the container has to have something alive in it — `restarting` counts,
/// because it executes again the moment its backoff elapses, which is exactly long
/// enough to collide.
///
/// A config the panel couldn't parse reads as "not transacting": no bot is trading
/// on a config that doesn't load.
fn can_transact(bot: &Bot) -> bool {
    bot.container_name.is_some()
        && !bot.state.is_terminal()
        && bot.config.as_ref().is_some_and(|c| c.sends_transactions)
}

/// Whether an approval run would race another process holding the same key.
///
/// `stitch approve` broadcasts `ERC20.approve` from the operator wallet. So do the
/// taker and closer legs inside a live bot, and both paths build a transaction the
/// same way: read the pending nonce from the node, sign, send. Two processes on one
/// key can't see each other's read, so they can sign the same nonce — one
/// transaction then replaces or rejects the other, and what's lost is either the
/// approval or a fill the bot had already priced and committed to.
///
/// The nonce sequence belongs to a `(chain, address)` pair, not to a container, so
/// the whole fleet is checked and not just the selected bot. Running the same key
/// in two config directories is an ordinary way to quote two corridors on one
/// chain, and either of those bots can spend the nonce this approval wants.
///
/// This covers bots. It cannot cover a second *approval*, which isn't in the fleet
/// yet when the check runs — [`WalletLocks`] does that.
pub fn approve_check(bot: &Bot, fleet: &Fleet) -> anyhow::Result<()> {
    if can_transact(bot) {
        anyhow::bail!(
            "{} is {} with its taker or closer leg on, so it can broadcast from the same wallet \
             at any moment. Running an approval now means two processes picking the same nonce, \
             and one of the two transactions is lost — the approval, or a fill the bot already \
             committed to. Stop {} first, approve, then start it again.",
            bot.name,
            bot.state.as_str(),
            bot.name
        );
    }
    no_live_sibling_on_the_wallet(bot, fleet)
}

/// The live bot that must be stopped before an approval can run, if any.
///
/// Same cases as [`approve_check`]: this bot is itself transacting, or a sibling
/// on the same wallet is. The UI Stop button has to name that bot — stopping the
/// selected one does nothing when the sibling is the one spending nonces.
pub fn approve_blocked_by(bot: &Bot, fleet: &Fleet) -> Option<String> {
    if can_transact(bot) {
        return stoppable_name(bot);
    }
    stoppable_name(live_sibling_on_wallet(
        &bot.name,
        bot.wallet().as_ref()?,
        fleet,
    )?)
}

/// A Stop target the panel can actually aim at.
///
/// Duplicate names collapse into one fleet row, and [`super::require_actionable`]
/// then 409s every lifecycle action. Offering that name as the recovery button
/// would send the operator into a dead end.
fn stoppable_name(bot: &Bot) -> Option<String> {
    if bot.warnings.iter().any(Warning::blocks_actions) {
        None
    } else {
        Some(bot.name.clone())
    }
}

/// Refuse when a *different* bot on the same operator wallet is live and can
/// broadcast from it.
///
/// Split out of [`approve_check`] because a bot launch needs exactly this half and
/// not the other one: "this bot is live" is what a launch is about to change, so
/// checking it would refuse every start.
///
/// A reservation alone doesn't cover this. Reservations only exist for the duration
/// of a launch or an approval — a bot that is *already* running holds nothing, so the
/// set says the wallet is free while a live taker is spending its nonces. This is the
/// fleet half of the same question, and it has to be asked after the reservation is
/// held or the answer can go stale before it's used.
pub fn no_live_sibling_on_the_wallet(bot: &Bot, fleet: &Fleet) -> anyhow::Result<()> {
    // A wallet the panel can't identify can't be compared — and can't be signed
    // with either, so there is no nonce at stake.
    let Some(wallet) = bot.wallet() else {
        return Ok(());
    };
    no_live_sibling_on_wallet_id(&bot.name, &wallet, fleet)
}

/// As [`no_live_sibling_on_the_wallet`], but keyed on a wallet directly rather than a
/// bot's current one. A settings save that changes a bot's wallet has to check the
/// wallet it is *moving to*, which isn't the one its on-disk `Bot` still reports.
pub fn no_live_sibling_on_wallet_id(
    name: &str,
    wallet: &WalletId,
    fleet: &Fleet,
) -> anyhow::Result<()> {
    match live_sibling_on_wallet(name, wallet, fleet) {
        None => Ok(()),
        Some(other) => anyhow::bail!(
            "{name} shares its operator wallet ({wallet}) with {}, which is {} and can broadcast \
             from it. Both would read the same pending nonce and one transaction would be lost. \
             Stop {} first.",
            other.name,
            other.state.as_str(),
            other.name
        ),
    }
}

fn live_sibling_on_wallet<'a>(name: &str, wallet: &WalletId, fleet: &'a Fleet) -> Option<&'a Bot> {
    fleet.bots().iter().find(|other| {
        other.name != name && other.wallet().as_ref() == Some(wallet) && can_transact(other)
    })
}

/// Whether this bot is itself a live transactor on its wallet.
///
/// Used to tell "this action would add a second signer" from "there are already two
/// and the operator is trying to do something about it". Refusing the second case
/// blocks the recreate or restart that would fix the overlap without removing it.
pub fn already_transacting(bot: &Bot) -> bool {
    can_transact(bot)
}

/// Exclusive claims on operator wallets, so no two processes the panel launches
/// sign from one `(chain, address)` at the same time.
///
/// One `(chain, address)` pair owns one nonce sequence. `stitch approve`, a bot's
/// taker or closer leg, and a settings restart all build a transaction the same
/// way — read the pending nonce, sign, send — and two of them on one wallet can pick
/// the same nonce, so one transaction replaces or rejects the other. What's lost is
/// an approval, or a fill a bot had already priced. So every action about to put a
/// signer on a wallet takes that wallet's claim first and holds it across the whole
/// action, rather than checking a flag and then acting with a gap in between.
///
/// A `tokio::sync::Mutex` per wallet, not a set of flags: the claim is *held* across
/// `.await` points — a Docker restart, or the life of an approval container — and a
/// real lock is what carries the exclusion across a suspension point. `try_lock`
/// rather than `lock().await`: an approval holds its wallet for as long as its
/// container runs, and a launch handler must refuse rather than block an HTTP request
/// for minutes. "Something is already signing on this wallet, wait" is the honest
/// answer, not a stall.
///
/// A running bot the panel didn't launch in this process holds no claim, so the
/// claim alone can't see an already-live sibling on the same wallet. Callers pair it
/// with a fleet check — [`no_live_sibling_on_the_wallet`] — taken *under* the claim,
/// so nothing can start on the wallet between the answer and the action.
///
/// The wallet a claim is keyed on must come from the same authoritative read that
/// drives the action: a claim taken from a stale snapshot names the wallet the bot is
/// *leaving* while the container launches from the file as it is now. The map never
/// evicts — bounded by the number of distinct wallets the host has seen, which is the
/// number of bots on it.
///
/// In-process only, and that's the honest scope: one panel per Docker host, so there
/// is nothing else to coordinate with. Two panels on one wallet is already a
/// configuration nobody should be running.
#[derive(Debug, Default)]
pub struct WalletLocks {
    live: Mutex<HashMap<WalletId, Arc<tokio::sync::Mutex<()>>>>,
}

impl WalletLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock for one wallet, created on first use.
    fn for_wallet(&self, wallet: &WalletId) -> Arc<tokio::sync::Mutex<()>> {
        let mut live = self.lock();
        Arc::clone(live.entry(wallet.clone()).or_default())
    }

    /// Claim the wallet exclusively, or `None` because something else holds it.
    ///
    /// Non-blocking on purpose: a held claim can outlive an HTTP request by the life
    /// of a container, and waiting on that would hang the handler.
    pub fn try_claim(&self, wallet: WalletId) -> Option<WalletClaim> {
        let lock = self.for_wallet(&wallet);
        lock.try_lock_owned().ok().map(|guard| WalletClaim {
            wallet,
            _guard: guard,
        })
    }

    /// Claim a bot's wallet, or `Some(None)` when it hasn't got an identifiable
    /// one — nothing can be signed with a wallet the panel can't name, so there is no
    /// nonce to contend for. `None` means something else holds it.
    pub fn try_claim_for(&self, bot: &Bot) -> Option<Option<WalletClaim>> {
        match bot.wallet() {
            None => Some(None),
            Some(wallet) => self.try_claim(wallet).map(Some),
        }
    }

    /// Whether anything holds this wallet. Advisory only — for telling the UI why a
    /// button is disabled. Never for deciding whether to act: that has to be
    /// [`Self::try_claim`], or the check and the act have a gap between them.
    pub fn is_claimed(&self, wallet: &WalletId) -> bool {
        self.for_wallet(wallet).try_lock().is_err()
    }

    /// A poisoned lock means another thread panicked mid-update. The map holds
    /// independent locks with no cross-entry invariant, so recovering is safe and
    /// better than taking the panel down.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<WalletId, Arc<tokio::sync::Mutex<()>>>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Holds one wallet's claim until dropped. What "until dropped" means is the
/// caller's choice: the length of a Docker call for a launch, or the life of a
/// container for an approval, which hands the claim to the Docker layer.
#[derive(Debug)]
pub struct WalletClaim {
    wallet: WalletId,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl WalletClaim {
    /// The wallet this claim holds.
    pub fn wallet(&self) -> &WalletId {
        &self.wallet
    }
}

/// `stitch approve`: the ERC-20 approval the bot needs before it can quote.
pub async fn approve(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    one_shot(state, &name, OneShot::Approve).await
}

/// A dry run: validate the config and the corridor without posting orders.
pub async fn dry_run(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    one_shot(state, &name, OneShot::DryRun).await
}

/// Claim the operator wallet for an approval, pinned to the config it launches from.
///
/// The approval signs `ERC20.approve` from the operator wallet, so it holds that
/// wallet to itself for the run. The subtle bug this closes: the wallet was claimed
/// from one read of the config and the container launched from another, so a raw save
/// that moved the bot onto a different wallet in between left the claim naming the
/// wallet the bot was leaving while a signer started on the new one.
///
/// So claim from the first read, then re-read under the claim. Nothing else can start
/// on the claimed wallet now, so the fleet check ([`approve_check`]) stays true for
/// the run. If the config still names the wallet we hold, launch from that fresh read.
/// If a save moved it in between, drop the claim and take it again from the fresh
/// wallet — bounded to two rounds, because a second move would need another save to
/// land in the gap between two reads, and refusing beats spinning.
///
/// Returns the bot the caller must launch from (rebound to the read the claim was
/// taken against) and the claim, or `None` when the bot has no identifiable wallet —
/// nothing can be signed with it, so the run fails on its own for want of a key.
async fn reserve_approval(
    state: &AppState,
    name: &str,
    mut bot: Bot,
) -> Result<(Bot, Option<WalletClaim>), ApiError> {
    for _ in 0..2 {
        let Some(wallet) = bot.wallet() else {
            return Ok((bot, None));
        };
        let claim = state
            .wallet_locks
            .try_claim(wallet.clone())
            .ok_or_else(|| {
                ApiError::conflict(format!(
                "{}'s operator wallet is busy — an approval is running against it, or a bot on it \
                 is being started. Wait for that to finish: two processes would read the same \
                 pending nonce and one transaction would be dropped.",
                bot.name
            ))
            })?;
        let (fresh, fleet) = state.bot_and_fleet(name).await?;
        // The config moved between the read we claimed from and this one: the claim
        // names the wallet the bot is leaving. Drop it and re-derive from the fresh
        // read rather than launch a signer on a wallet nothing is guarding.
        if fresh.wallet().as_ref() != Some(&wallet) {
            drop(claim);
            bot = fresh;
            continue;
        }
        approve_check(&fresh, &fleet).map_err(ApiError::conflict)?;
        return Ok((fresh, Some(claim)));
    }
    Err(ApiError::conflict(format!(
        "{name}'s config kept changing while the approval was starting — a settings save is \
         probably in flight. Try again once it has finished."
    )))
}

/// Run a throwaway container with the bot's own config and stream its output.
async fn one_shot(state: AppState, name: &str, which: OneShot) -> Result<Response, ApiError> {
    let (bot, _fleet) = state.bot_and_fleet(name).await?;
    super::require_editable(&bot)?;
    // A dry run signs nothing and sends nothing, so none of this applies to it. An
    // approval broadcasts, so it takes the wallet to itself — and the wallet it holds
    // and the config the container loads have to stay the same, right through to the
    // container's start. That start is deferred until the SSE stream is polled, after
    // this handler returns, so `reserve_approval` verifying the wallet here isn't enough
    // on its own: a save could still move the file before the container reads it. So hold
    // the config lock from the claim until the container has started — `start_hold`
    // carries it to the Docker layer, which drops it the instant the container is up.
    // `bot` is rebound to the read the claim was taken against.
    let (bot, claim, start_hold) = match which {
        OneShot::DryRun => (bot, None, None),
        OneShot::Approve => {
            let (config_guard, bot) = super::bots::lock_config(name, &state).await?;
            let (bot, claim) = reserve_approval(&state, name, bot).await?;
            let start_hold = config_guard.map(|g| Arc::new(g) as crate::panel::docker::Keepalive);
            (bot, claim, start_hold)
        }
    };
    let config = bot
        .config_panel_path
        .as_ref()
        .ok_or_else(|| ApiError::conflict(format!("{name} has no config the panel can read")))?;
    // The one-shot mounts the host path, so it has to be one the daemon can see.
    let host_dir = bot
        .config_host_path
        .as_ref()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "couldn't work out {name}'s config directory on the host"
            ))
        })?;

    // Read the signer from the bot's own config file and mount the files it
    // actually has: a bot adopted on the flat layout is called stitch.bot1.toml,
    // and assuming the canonical name would run this against nothing, or against
    // whichever other bot's config shares the directory.
    let signer = signer_runtime_at(config)?;
    let binds = provision::mounts_for(&bot, &host_dir, &signer).map_err(ApiError::conflict)?;
    provision::check_file_mounts(&binds, &state.cfg).map_err(ApiError::conflict)?;
    let spec = one_shot_spec(
        &provision::image_of(&bot, &state.cfg),
        binds,
        name,
        &signer,
        which,
    );
    tracing::info!(bot = %name, action = which.as_str(), "running one-shot");

    // The claim goes to the Docker layer as a keepalive, not into this closure. The
    // stream ends when the browser stops listening; the container keeps signing until
    // it is removed, and those are different moments. Handing it over means the wallet
    // is released after the reap, so an operator who clicks "Stop watching" can't free
    // it for a second approval while the first is still broadcasting.
    let keepalive = claim.map(|c| Arc::new(c) as crate::panel::docker::Keepalive);
    let stream = state
        .docker
        .run_one_shot(spec, keepalive, start_hold)
        .map(move |item| {
            Ok::<Event, Infallible>(match item {
                Ok(RunEvent::Line(line)) => json_event("line", &LineBody::from(line)),
                Ok(RunEvent::Exited { code }) => json_event(
                    "exit",
                    &serde_json::json!({
                        "code": code,
                        "ok": code == 0,
                        "action": which.as_str(),
                    }),
                ),
                Err(e) => json_event("error", &serde_json::json!({ "message": format!("{e:#}") })),
            })
        });

    Ok(sse(stream))
}

/// Serialise a payload into a named SSE event.
///
/// Serialisation of our own structs can't fail in practice; if it somehow did,
/// dropping the line would be worse than saying so, so the failure is sent as the
/// event body.
fn json_event<T: Serialize>(name: &str, payload: &T) -> Event {
    match serde_json::to_string(payload) {
        Ok(json) => Event::default().event(name).data(json),
        Err(e) => Event::default().event("error").data(format!(
            r#"{{"message":"couldn't encode a log line: {e}"}}"#
        )),
    }
}

fn sse<S>(stream: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{harness, Harness, TEST_KEY};
    use super::*;
    use crate::panel::docker::fake::{container, dir_layout_mounts, flat_layout_mounts, out, Call};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::{LABEL_BOT, LABEL_COMPOSE_SERVICE};
    use crate::setup;
    use axum::http::StatusCode;

    fn line(text: &str, source: LogSource) -> LogLine {
        LogLine {
            source,
            text: text.to_string(),
        }
    }

    fn seed(h: &Harness, name: &str) {
        seed_in_state(h, name, ContainerState::Running);
    }

    fn seed_in_state(h: &Harness, name: &str, state: ContainerState) {
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let mut c = container(&format!("stitch-{name}"), state);
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    /// Seed a bot whose taker leg is on, so its own process broadcasts
    /// transactions from the operator wallet. The shipped corridors are maker-only,
    /// which is why this has to be switched on by hand.
    fn seed_transacting(h: &Harness, name: &str, state: ContainerState) {
        seed_in_state(h, name, state);
        let config = h.root.join(name).join("stitch.toml");
        // The template's last table is the `[[pools]]` one, so this lands in it.
        let toml = std::fs::read_to_string(&config).unwrap() + "\nlimit_taker_enabled = true\n";
        std::fs::write(&config, toml).unwrap();
    }

    #[test]
    fn levels_come_from_the_bots_own_formatter() {
        assert_eq!(
            level_of("2026-01-01T00:00:00Z  INFO stitch: tick", LogSource::Stdout),
            "info"
        );
        assert_eq!(
            level_of(
                "2026-01-01T00:00:00Z ERROR stitch: rpc failed",
                LogSource::Stdout
            ),
            "error"
        );
        assert_eq!(level_of("WARN slow feed", LogSource::Stdout), "warn");
    }

    #[test]
    fn the_bots_own_colour_codes_are_stripped_and_still_classified() {
        // Real output from a container started without NO_COLOR. Left alone, the
        // viewer shows "[2m2026-..." in front of every line.
        let raw = "\u{1b}[2m2026-07-29T13:15:44.950860Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m \
                   \u{1b}[2mstitch\u{1b}[0m\u{1b}[2m:\u{1b}[0m input token not approved";
        let body = LineBody::from(line(raw, LogSource::Stdout));
        assert_eq!(
            body.text,
            "2026-07-29T13:15:44.950860Z  WARN stitch: input token not approved"
        );
        assert_eq!(body.level, "warn");

        // No space between the colour code and the level word, which is what
        // defeats a boundary check run against the unstripped text.
        let tight = LineBody::from(line("\u{1b}[31mERROR\u{1b}[0m rpc down", LogSource::Stdout));
        assert_eq!(tight.text, "ERROR rpc down");
        assert_eq!(tight.level, "error");
    }

    #[test]
    fn stripping_leaves_ordinary_text_alone_and_eats_a_truncated_escape() {
        assert_eq!(strip_ansi("plain line"), "plain line");
        // Non-ASCII must survive: the banner and some log fields are UTF-8.
        assert_eq!(strip_ansi("cNGN → USDT ✓"), "cNGN → USDT ✓");
        // A line cut mid-escape by the length cap must not leak the fragment.
        assert_eq!(strip_ansi("tail\u{1b}[2"), "tail");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}shell"), "shell");
    }

    #[test]
    fn a_line_that_merely_mentions_a_level_is_not_recoloured() {
        // A metric name or a URL containing the word must not turn the line red.
        assert_eq!(
            level_of("INFO stitch: ERRORS_TOTAL=0", LogSource::Stdout),
            "info"
        );
        assert_eq!(
            level_of("posted to https://x/ERRORLOG", LogSource::Stdout),
            "plain"
        );
    }

    #[test]
    fn unformatted_stderr_is_treated_as_an_error() {
        // Panics and linker complaints arrive with no level at all.
        assert_eq!(
            level_of("thread 'main' panicked", LogSource::Stderr),
            "error"
        );
        assert_eq!(level_of("plain note", LogSource::Stdout), "plain");
    }

    #[tokio::test]
    async fn the_log_tail_streams_lines_as_sse_events() {
        let h = harness("logs");
        seed(&h, "bot-a");
        h.docker.set_log_lines(vec![
            out("2026-01-01T00:00:00Z  INFO stitch: quoting"),
            out("2026-01-01T00:00:01Z ERROR stitch: rpc timeout"),
        ]);

        let (status, body) = h.get("/api/bots/bot-a/logs?follow=false&tail=10").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("event: line"), "{body}");
        assert!(body.contains(r#""level":"info""#), "{body}");
        assert!(body.contains(r#""level":"error""#), "{body}");
        assert!(h
            .docker
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Logs { name, .. } if name == "stitch-bot-a")));
    }

    #[tokio::test]
    async fn an_absurd_tail_request_is_clamped() {
        // Asking for a million lines of replay must not be a way to make the panel
        // buffer a million lines.
        let h = harness("logs-clamp");
        seed(&h, "bot-a");
        let (status, _) = h
            .get("/api/bots/bot-a/logs?follow=false&tail=99999999")
            .await;
        assert_eq!(status, StatusCode::OK);
        // What Docker was actually asked for, not what the constant says.
        let tail = h
            .docker
            .calls()
            .iter()
            .find_map(|c| match c {
                Call::Logs { tail, .. } => Some(*tail),
                _ => None,
            })
            .expect("the log route must have asked Docker for the log");
        assert_eq!(tail, MAX_TAIL);
    }

    #[tokio::test]
    async fn tailing_a_bot_with_no_container_is_a_conflict() {
        let h = harness("logs-nocontainer");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(h.root.join("bot-a"), corridor, TEST_KEY).unwrap();
        let (status, body) = h.get("/api/bots/bot-a/logs").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("no container"), "{body}");
    }

    #[tokio::test]
    async fn approve_streams_output_then_the_exit_code() {
        let h = harness("approve");
        seed(&h, "bot-a");
        h.docker
            .set_log_lines(vec![out("approving USDT for the router")]);
        h.docker.set_one_shot_exit(0);

        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("event: line"), "{body}");
        assert!(body.contains("event: exit"), "{body}");
        assert!(body.contains(r#""ok":true"#), "{body}");
        assert!(body.contains(r#""action":"approve""#), "{body}");
    }

    #[tokio::test]
    async fn approve_is_refused_while_a_transacting_bot_is_live() {
        // Both processes hold the same key and both read the pending nonce for
        // themselves, so they can sign the same one. Whichever lands second is
        // dropped — either the approval, or a fill the bot already committed to.
        let h = harness("approve-live");
        seed_transacting(&h, "bot-a", ContainerState::Running);

        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("nonce"), "{body}");
        assert!(
            h.docker.one_shot_specs().is_empty(),
            "nothing may be started"
        );

        // And the UI is told before the operator clicks, the same way it is for
        // a migration it can't run.
        let (status, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["canApprove"], false);
        assert_eq!(v["approveBlockedBy"], "bot-a");
        assert!(
            v["approveBlockedReason"]
                .as_str()
                .is_some_and(|r| r.contains("nonce")),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_restarting_transacting_bot_is_still_too_live_to_approve_alongside() {
        // Between restart attempts there's no process, so `running` is false — but
        // the daemon starts another the moment the backoff elapses, which is more
        // than long enough to pick the same nonce.
        let h = harness("approve-restarting");
        seed_transacting(&h, "bot-a", ContainerState::Restarting);
        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn approve_works_once_the_transacting_bot_is_stopped() {
        // Which is the whole point of refusing: the operator stops it, approves,
        // and starts it again.
        let h = harness("approve-stopped");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        h.docker.set_one_shot_exit(0);
        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(r#""ok":true"#), "{body}");
    }

    #[tokio::test]
    async fn a_maker_only_bot_can_be_approved_while_it_runs() {
        // A maker signs Permit2 orders offchain and consumes no account nonce, so
        // there is nothing for the approval to collide with. Blocking it would cost
        // an outage for no reason.
        let h = harness("approve-maker");
        seed(&h, "bot-a");
        h.docker.set_one_shot_exit(0);
        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (_, body) = h.get("/api/bots/bot-a").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["canApprove"], true);
        assert!(v["approveBlockedReason"].is_null(), "{body}");
        assert!(v["approveBlockedBy"].is_null(), "{body}");
    }

    #[tokio::test]
    async fn approve_is_refused_when_another_bot_shares_the_wallet() {
        // The nonce sequence belongs to a (chain, address) pair, not to a container.
        // Running one key in two config directories is an ordinary way to quote two
        // corridors, and the stopped bot's approval is racing the *other* bot's
        // transactions — which looking only at the selected container never sees.
        let h = harness("approve-sibling");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        seed_transacting(&h, "bot-b", ContainerState::Running);

        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("shares its operator wallet"), "{body}");
        assert!(body.contains("bot-b"), "{body}");
        assert!(h.docker.one_shot_specs().is_empty());

        let (status, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["canApprove"], false);
        assert_eq!(v["approveBlockedBy"], "bot-b");
    }

    #[tokio::test]
    async fn a_duplicate_named_sibling_does_not_get_a_stop_target() {
        // Two containers collapsed into one fleet row. Stop by that name 409s
        // (require_actionable), so naming it as the recovery button is a dead end.
        // The reason still says who is blocking; the button just isn't offered.
        let h = harness("approve-sibling-dupe");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        seed_transacting(&h, "bot-b", ContainerState::Running);
        let mut rival = container("other-bot-b", ContainerState::Running);
        rival
            .labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot-b".to_string());
        rival.mounts = dir_layout_mounts(&h.root.join("bot-b").display().to_string());
        h.docker.add_container(rival);

        let (status, body) = h.get("/api/bots/bot-a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["canApprove"], false);
        assert!(v["approveBlockedBy"].is_null(), "{body}");
        assert!(
            v["approveBlockedReason"]
                .as_str()
                .is_some_and(|r| r.contains("bot-b")),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_sibling_on_the_same_wallet_that_only_makes_does_not_block() {
        // Same wallet, but a maker consumes no account nonce, so there's nothing to
        // collide with. Blocking here would make every multi-corridor setup stop its
        // whole fleet to approve one token.
        let h = harness("approve-sibling-maker");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        seed(&h, "bot-b"); // running, maker-only
        h.docker.set_one_shot_exit(0);

        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn a_bot_on_a_different_wallet_does_not_block() {
        // Different key, so a different nonce sequence entirely.
        let h = harness("approve-other-wallet");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        seed_transacting(&h, "bot-b", ContainerState::Running);
        // Rewrite bot-b's key so it signs as somebody else.
        let other = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        std::fs::write(
            h.root.join("bot-b").join("stitch.key"),
            format!("{other}\n"),
        )
        .unwrap();

        h.docker.set_one_shot_exit(0);
        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[test]
    fn a_wallet_can_only_have_one_approval_in_flight() {
        // The fleet check can't see a one-shot that hasn't been created yet, so two
        // requests would otherwise both pass it and both start signing. The claim is
        // held by a guard rather than released at the end of the handler, because the
        // container outlives the request that started it.
        let approvals = Arc::new(WalletLocks::new());
        let wallet = WalletId {
            chain_id: 56,
            address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".into(),
        };
        let held = approvals
            .try_claim(wallet.clone())
            .expect("first claim wins");
        assert!(
            approvals.try_claim(wallet.clone()).is_none(),
            "a second approval on the same wallet must be refused"
        );
        // A different wallet is unaffected.
        assert!(approvals
            .try_claim(WalletId {
                chain_id: 1,
                address: wallet.address.clone(),
            })
            .is_some());

        // Dropping the guard frees it, which is what happens when the run's stream
        // ends or the operator navigates away.
        drop(held);
        assert!(
            approvals.try_claim(wallet).is_some(),
            "the slot must be released"
        );
    }

    #[tokio::test]
    async fn the_wallet_claim_outlives_the_stream_that_started_it() {
        // The claim goes to the Docker layer as a keepalive rather than living in the
        // response stream. The stream ends when the browser stops listening; the
        // container keeps signing until it's removed. Releasing on the first of those
        // let a second approval start while the first was still broadcasting.
        let h = harness("approve-keepalive");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        h.docker.set_one_shot_exit(0);
        // Stand in for a container that hasn't been reaped yet.
        h.docker.hold_one_shot_keepalive();

        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The response has been fully read — the stream is long gone — and the wallet
        // is still held, because the container is notionally still there.
        let bot = h.state.bot("bot-a").await.unwrap();
        let wallet = bot.wallet().expect("a hot wallet has an address");
        assert!(
            h.state.wallet_locks.is_claimed(&wallet),
            "the claim must survive the stream ending"
        );
        // A second approval is refused for as long as that holds.
        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("wallet is busy"), "{body}");

        // Reaped: the keepalive goes, and so does the claim.
        h.docker.release_one_shot_keepalive();
        assert!(!h.state.wallet_locks.is_claimed(&wallet));
    }

    #[tokio::test]
    async fn a_launch_and_an_approval_cannot_both_pass_their_own_check() {
        // The race the reservation protocol exists for. `holds()`-then-act had a window
        // on both sides: an approval could claim between a start's check and its Docker
        // call, and a start could launch between the approval's fleet snapshot and its
        // claim. Now both take the same reservation before doing anything, so whichever
        // gets it first makes the other fail rather than both proceeding.
        let h = harness("reservation-both-sides");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        let bot = h.state.bot("bot-a").await.unwrap();
        let wallet = bot.wallet().unwrap();

        // A launch holds it: the approval is refused.
        let launch = h
            .state
            .wallet_locks
            .try_claim(wallet.clone())
            .expect("free to start with");
        let (status, body) = h
            .post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("wallet is busy"), "{body}");
        drop(launch);

        // And the same reservation is what a start takes, so it's genuinely one
        // protocol rather than two checks that happen to agree.
        let approval = h
            .state
            .wallet_locks
            .try_claim(wallet)
            .expect("released again");
        let (status, body) = h
            .post_json("/api/bots/bot-a/start", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("wallet is busy"), "{body}");
        drop(approval);
    }

    #[tokio::test]
    async fn a_dry_run_takes_no_claim_at_all() {
        // It signs nothing, so it has no wallet to reserve and must not block an
        // approval that legitimately wants one.
        let h = harness("dryrun-noclaim");
        seed_transacting(&h, "bot-a", ContainerState::Exited);
        h.docker.set_one_shot_exit(0);
        h.docker.hold_one_shot_keepalive();

        h.post_json("/api/bots/bot-a/dry-run", serde_json::json!({}))
            .await;
        let bot = h.state.bot("bot-a").await.unwrap();
        assert!(!h.state.wallet_locks.is_claimed(&bot.wallet().unwrap()));
    }

    #[tokio::test]
    async fn a_dry_run_is_never_blocked_by_a_live_bot() {
        // A dry run signs nothing and sends nothing, so it can't take a nonce.
        let h = harness("dryrun-live");
        seed_transacting(&h, "bot-a", ContainerState::Running);
        h.docker.set_one_shot_exit(0);
        let (status, body) = h
            .post_json("/api/bots/bot-a/dry-run", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn a_failed_dry_run_reports_its_exit_code() {
        let h = harness("dryrun");
        seed(&h, "bot-a");
        h.docker.set_one_shot_exit(2);
        let (status, body) = h
            .post_json("/api/bots/bot-a/dry-run", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""code":2"#), "{body}");
        assert!(body.contains(r#""ok":false"#), "{body}");
    }

    #[tokio::test]
    async fn a_flat_layout_bot_runs_a_one_shot_against_its_own_files() {
        // Its config is stitch.bot1.toml, not stitch.toml. Reading the canonical
        // name out of the shared directory finds nothing — or, when a canonical
        // config happens to live there, someone else's bot and someone else's key.
        let h = harness("oneshot-flat");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        std::fs::write(h.root.join("stitch.bot1.key"), format!("{TEST_KEY}\n")).unwrap();
        // The trap: an unrelated canonical config sharing the directory.
        setup::write_config(h.root.join("other"), corridor, TEST_KEY).unwrap();
        std::fs::copy(
            h.root.join("other").join("stitch.toml"),
            h.root.join("stitch.toml"),
        )
        .unwrap();
        std::fs::copy(
            h.root.join("other").join("stitch.key"),
            h.root.join("stitch.key"),
        )
        .unwrap();

        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.to_string(), "bot1".to_string());
        c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
        h.docker.add_container(c);

        let (status, body) = h
            .post_json("/api/bots/bot1/dry-run", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let spec = h.docker.one_shot_specs().pop().expect("a one-shot ran");
        let sources: Vec<_> = spec
            .binds
            .iter()
            .map(|b| b.host_path.display().to_string())
            .collect();
        assert_eq!(
            sources,
            vec![
                h.root.join("stitch.bot1.toml").display().to_string(),
                h.root.join("stitch.bot1.key").display().to_string(),
            ],
            "the neighbour's canonical config and key must not be mounted"
        );
    }

    #[tokio::test]
    async fn a_one_shot_runs_the_image_the_bot_runs() {
        // A dry run tells you what the bot would do. On a bot pinned to another
        // image, running the panel's image answers a question nobody asked.
        let h = harness("oneshot-image");
        seed(&h, "bot-a");
        h.docker.set_container_image("stitch-bot-a", "acme/fork:v9");

        h.post_json("/api/bots/bot-a/dry-run", serde_json::json!({}))
            .await;
        let spec = h.docker.one_shot_specs().pop().expect("a one-shot ran");
        assert_eq!(spec.image, "acme/fork:v9");
    }

    #[tokio::test]
    async fn a_one_shot_runs_in_its_own_container_not_the_bots() {
        // Reusing the bot's container name would fight with the running bot; the
        // one-shot has to be a throwaway.
        let h = harness("oneshot-name");
        seed(&h, "bot-a");
        h.post_json("/api/bots/bot-a/approve", serde_json::json!({}))
            .await;
        let one_shots: Vec<_> = h
            .docker
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::OneShot { name, cmd } => Some((name, cmd)),
                _ => None,
            })
            .collect();
        assert_eq!(one_shots.len(), 1);
        assert_ne!(one_shots[0].0, "stitch-bot-a");
        assert!(
            one_shots[0].1.contains(&"approve".to_string()),
            "{:?}",
            one_shots[0].1
        );
    }
}
