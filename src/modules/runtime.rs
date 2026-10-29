// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use super::*;
use alloy_primitives::U256;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

pub const STATUS_FILE: &str = "modules-status.json";
const ATTEMPTS_FILE: &str = "modules-attempt.json";
/// The dynamic-spreads price window, so a restart (every module save is one)
/// resumes warm instead of quoting at the cap until the feed ticks again.
pub const HISTORY_FILE: &str = "modules-history.json";
/// A floor change that may still land. Kept across restarts so a pending
/// lower's quote cap is back in force before the first quote is signed, and
/// no second floor change goes out while the first can still execute.
const PENDING_FLOOR_FILE: &str = "liquidity-pending.json";
pub const MAX_DECISIONS: usize = 200;
/// Most samples the window keeps: one per second over the longest allowed window.
const MAX_HISTORY: usize = 3601;

/// Every module file goes through `write_toml_atomic`, which stages in one fixed
/// temp per target and clears a leftover temp before it starts. Two writers on
/// the same file would delete or rename each other's temp, and a restart inside
/// one process has two: the new runtime's first snapshot while the old one's
/// last is still landing. One lock keeps module writes one at a time; they are
/// small and coalesced, so it is never held for long.
static WRITES: Mutex<()> = Mutex::new(());

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let json = serde_json::to_string(value)?;
    let _one_at_a_time = WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::setup::write_toml_atomic(path, &json)
}

#[derive(Clone, Copy, Debug)]
pub struct Balances {
    pub settlement: U256,
    pub corridor: U256,
    /// Liquid settlement and the floor, from the same read. `None` when
    /// either view failed.
    pub liquid: Option<LiquidReading>,
    pub at: u64,
}

/// `liquidSettlement()` and `minLiquidSettlement()`, read with the balances.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiquidReading {
    pub liquid: U256,
    pub floor: U256,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Observation {
    pub decision: Decision,
    pub price: f64,
    #[serde(with = "atomic")]
    pub settlement: U256,
    #[serde(with = "atomic")]
    pub corridor: U256,
    /// Optional for status files written by older bot releases.
    #[serde(default)]
    pub inputs: Option<DecisionInputs>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct DecisionInputs {
    pub price_at: u64,
    pub balances_at: u64,
    pub base_buy_bps: Option<u32>,
    pub base_sell_bps: Option<u32>,
    pub inventory_buy_bps: Option<u32>,
    pub inventory_sell_bps: Option<u32>,
    pub spread_window: super::strategies::SpreadWindow,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QuoteState {
    WaitingForSession,
    WaitingForVault,
    WaitingForPrice,
    StalePrice,
    /// The jump guard is holding the latest print back.
    SuspectPrice,
    NoCorridor,
    Evaluating,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct QuoteStatus {
    pub at: u64,
    pub state: QuoteState,
    pub message: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: u32,
    pub config: ModulesConfig,
    pub at: u64,
    pub decisions: Vec<Observation>,
    pub rebalance_status: String,
    pub next_attempt_at: u64,
    #[serde(default)]
    pub quote_status: Option<QuoteStatus>,
    /// Absent from status files written before the liquidity schedule existed.
    #[serde(default)]
    pub liquidity: Option<super::liquidity::LiquidityStatus>,
    /// The inventory curve's last reading and recent quotes. Absent while
    /// the curve has never run, and from older bot releases.
    #[serde(default)]
    pub curve: Option<CurveStatus>,
}

/// What the inventory curve has seen, for the panel. Diagnostic only, like
/// the decisions: never read back to price anything.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CurveStatus {
    pub reading: Option<CurveReading>,
    /// Up to [`MAX_DECISIONS`] quotes, oldest first.
    pub quotes: Vec<CurveQuote>,
}

/// One level tick's view of the curve's inputs. Prices are in soft currency
/// per dollar (NGN per USDT), the unit the curve's knobs live in.
#[derive(Clone, Serialize, Deserialize)]
pub struct CurveReading {
    pub at: u64,
    /// Corridor currency's share of what the bot holds, 0..1, before this
    /// side's open quotes.
    pub share: f64,
    /// What the bot holds, valued in the settlement currency.
    pub value: f64,
    pub reference_mid: f64,
    pub half_range_bps: f64,
    /// The venue fee on the last quote request, bps.
    pub fee_bps: u32,
    /// The feed price the flat quote is built from, on the bot's price basis:
    /// `price` in Preview, `unadjustedPrice` in Live where the feed serves
    /// one. Not the reference mid, which only the curve anchors on. Absent in
    /// status written before the bot recorded it.
    pub feed_price: Option<f64>,
    /// Profit unwind, while it runs. Absent from older bot releases.
    #[serde(default)]
    pub unwind: Option<UnwindReading>,
}

/// What profit unwind sees this tick, in soft currency per dollar.
#[derive(Clone, Serialize, Deserialize)]
pub struct UnwindReading {
    /// The frozen average cost the lift is sized on, and when it froze.
    pub cost: f64,
    pub snapshot_at: u64,
    /// The feed price the gain is measured against.
    pub feed_mid: f64,
    /// Cost minus the feed: positive is a gain per dollar.
    pub gain: f64,
    /// How far both sides lift at 100% corridor currency, in soft currency
    /// per dollar. At share `f` it's this times the weight at `f`.
    pub full_shift: f64,
    /// Whether the lift quotes, or is profit unwind's preview.
    pub live: bool,
}

/// One Swap request the curve priced, or answered flat.
#[derive(Clone, Serialize, Deserialize)]
pub struct CurveQuote {
    pub at: u64,
    /// The bot buying the corridor currency (paying settlement).
    pub bot_buys: bool,
    /// The quote went out at the curved price, not flat.
    pub live: bool,
    /// The quoted size, in settlement units.
    pub size: f64,
    /// The share this quote started from (after this side's open quotes) and
    /// where it leaves it. Absent when the curve could not read the share.
    pub share_before: Option<f64>,
    pub share_after: Option<f64>,
    /// The curved quote against the flat one, in bps of soft currency per
    /// dollar: positive gives more of it per dollar. Absent when it went out
    /// flat.
    pub lean_bps: Option<f64>,
    /// Why it went out flat, in the bot's words.
    pub flat_reason: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Attempt {
    version: u32,
    next_attempt_at: u64,
}
#[derive(Serialize, Deserialize)]
struct SavedHistory {
    version: u32,
    /// Feed URL the samples came from. A different feed starts cold.
    source: String,
    points: Vec<PricePoint>,
}

/// The saved samples a new run may reuse: valid prices inside the current
/// window, none in the future, timestamps strictly increasing (the order
/// `decide` appends in), and no more than the window ever holds.
pub fn restorable_history(points: &[PricePoint], now: u64, window_secs: u64) -> Vec<PricePoint> {
    let kept: Vec<PricePoint> = points
        .iter()
        .filter(|p| {
            p.price.is_finite()
                && p.price > 0.0
                && p.timestamp <= now
                && now - p.timestamp <= window_secs
        })
        .fold(Vec::new(), |mut out: Vec<PricePoint>, p| {
            if out.last().is_none_or(|last| p.timestamp > last.timestamp) {
                out.push(*p);
            }
            out
        });
    kept[kept.len().saturating_sub(MAX_HISTORY)..].to_vec()
}

/// Best effort: a missing, unreadable or foreign file only means a cold start,
/// which quotes at the spread cap. It never fails startup.
fn load_history(dir: &Path, source: &str, window_secs: u64, now: u64) -> Vec<PricePoint> {
    let Ok(raw) = std::fs::read_to_string(dir.join(HISTORY_FILE)) else {
        return vec![];
    };
    match serde_json::from_str::<SavedHistory>(&raw) {
        Ok(saved) if saved.version == VERSION && saved.source == source => {
            restorable_history(&saved.points, now, window_secs)
        }
        Ok(_) => vec![],
        Err(_) => {
            tracing::warn!("module price history unreadable; dynamic spreads start cold");
            vec![]
        }
    }
}

/// Coalescing background writer, like the status snapshot: a slow disk may
/// skip intermediate windows but always lands the latest one.
fn spawn_history_writer(dir: &Path, source: String) -> tokio::sync::watch::Sender<Vec<PricePoint>> {
    let (writer, mut receiver) = tokio::sync::watch::channel(Vec::new());
    let path = dir.join(HISTORY_FILE);
    tokio::spawn(async move {
        while receiver.changed().await.is_ok() {
            let saved = SavedHistory {
                version: VERSION,
                source: source.clone(),
                points: receiver.borrow_and_update().clone(),
            };
            let path = path.clone();
            let write =
                tokio::task::spawn_blocking(move || -> Result<()> { write_json(&path, &saved) })
                    .await;
            if !matches!(write, Ok(Ok(()))) {
                tracing::error!("module price history could not be saved");
            }
        }
    });
    writer
}

/// A floor change (`setMinLiquidSettlement`) the liquidity loop has signed
/// and has not seen settle. Saved after it's signed and before its first
/// broadcast, so a change that can reach the chain is always on disk with the
/// chain, vault, account and nonce to settle it against, and one a restart
/// interrupts while the signer waits for approval leaves nothing behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingFloorRecord {
    pub version: u32,
    /// Floor it sets, in atomic settlement units.
    #[serde(with = "atomic")]
    pub target: U256,
    /// A lower, which pins the quote cap at `target` until it settles. A
    /// raise holds the slot and pins what's liquid while that's short of
    /// `target`.
    pub lowers: bool,
    pub chain_id: u64,
    #[serde(with = "super::manual::address")]
    pub vault: alloy_primitives::Address,
    /// The account that signs it. A rotated signer does not settle it.
    #[serde(with = "super::manual::address")]
    pub sender: alloy_primitives::Address,
    pub nonce: u64,
    /// The signed transaction, 0x-hex. A restart broadcasts it once more, in
    /// case the run that saved it stopped before broadcasting it. The same
    /// bytes are the same transaction, so this can complete it but never
    /// send a second one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_tx: Option<String>,
}

impl PendingFloorRecord {
    /// Why this bot can't settle the saved change, or `None` when it can. Its
    /// nonce and floor only mean something on the chain and vault it was
    /// signed for, so a bot moved elsewhere must not read them there.
    pub fn foreign_to(
        &self,
        chain_id: u64,
        vault: Option<alloy_primitives::Address>,
    ) -> Option<String> {
        (self.chain_id != chain_id || vault != Some(self.vault)).then(|| {
            format!(
                "a liquidity floor change from {} (nonce {}) to vault {} on chain {} may still \
                 land, but this bot now runs vault {} on chain {chain_id}. Point it back until \
                 that nonce is used on chain {}, or once you have confirmed that, delete \
                 {PENDING_FLOOR_FILE}",
                self.sender,
                self.nonce,
                self.vault,
                self.chain_id,
                vault.map_or("none".into(), |v| v.to_string()),
                self.chain_id,
            )
        })
    }
}

/// Whether an earlier run left a floor change that may still land in `dir`.
pub fn pending_floor_saved(dir: &Path) -> bool {
    dir.join(PENDING_FLOOR_FILE).exists()
}

/// The modules config a bot with modules off runs while it settles a saved
/// floor change: Preview, every module off. Quotes are left as they are; only
/// a lower's quote cap applies, and the liquidity loop settles it, then stops.
pub fn settling_only_config(config: &ModulesConfig) -> ModulesConfig {
    let mut c = config.clone();
    c.mode = Mode::Shadow;
    c.inventory.enabled = false;
    c.spreads.enabled = false;
    c.rebalance.enabled = false;
    c.liquidity.enabled = false;
    c.curve.enabled = false;
    c
}

/// The saved floor change, if any. Unreadable is an error: starting without
/// it could quote past a floor that is about to drop.
fn load_pending_floor(dir: &Path) -> Result<Option<PendingFloorRecord>> {
    let path = dir.join(PENDING_FLOOR_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let record: PendingFloorRecord = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    ensure!(
        record.version == VERSION,
        "unsupported pending liquidity floor change version"
    );
    Ok(Some(record))
}

/// How long the engine trusts a quote cap the liquidity loop has not renewed.
/// Long enough to ride out a slow vault read; a lower in flight holds it open.
pub const LIQUIDITY_CAP_TTL_SECS: u64 = 120;

/// The liquidity loop and the RFQ engine meet here. The engine owns the
/// reservation ledger, so it publishes what live quotes have committed; the
/// loop owns the schedule, so it publishes the quote cap.
#[derive(Default)]
struct LiquidityShared {
    /// `(committed, the loop's cap in force when the engine measured, measured at)`.
    commitments: Option<(U256, Option<U256>, u64)>,
    /// `(cap, valid through)`.
    cap: Option<(U256, u64)>,
    /// The target of a raise that may still land. The per-quote shortfall cap
    /// measures liquid against it, since the venue admits bundles up to it
    /// the moment it lands.
    pending_raise: Option<U256>,
}

impl LiquidityShared {
    /// The loop's cap, unless it has lapsed.
    fn scheduled_cap(&self, now: u64) -> Option<U256> {
        self.cap
            .filter(|(_, through)| *through >= now)
            .map(|(cap, _)| cap)
    }
}

struct State {
    history: Vec<PricePoint>,
    status: Status,
    fetching: bool,
    ready: Option<(dealer::QuoteRequest, dealer::Quote)>,
}
#[derive(Clone)]
pub struct Runtime {
    pub config: ModulesConfig,
    pub balances: Arc<RwLock<Option<Balances>>>,
    state: Arc<Mutex<State>>,
    pub(crate) dir: PathBuf,
    writer: tokio::sync::watch::Sender<Status>,
    history_writer: tokio::sync::watch::Sender<Vec<PricePoint>>,
    liquidity: Arc<Mutex<LiquidityShared>>,
    /// The floor change saved by the previous run, as found at startup.
    saved_floor: Option<PendingFloorRecord>,
}
impl Runtime {
    /// `price_source` is the feed URL the module prices come from; it keys
    /// the saved price window so a changed feed never inherits old samples.
    pub fn new(config: ModulesConfig, dir: &Path, price_source: &str) -> Result<Self> {
        let path = dir.join(ATTEMPTS_FILE);
        let next_attempt_at = if path.exists() {
            let attempt: Attempt = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            ensure!(
                attempt.version == VERSION,
                "unsupported module attempt version"
            );
            attempt.next_attempt_at
        } else {
            0
        };
        // A saved lower pins the cap whether or not the schedule is still on:
        // turning it off does not stop a sent transaction from landing. The
        // liquidity loop runs until it settles (see `rfq::liquidity`).
        let saved_floor = load_pending_floor(dir)?;
        // A live schedule may restart over an unfunded floor or a raise still
        // landing. Until its first trusted read says what is liquid, quotes
        // promise no settlement at all (see `rfq::liquidity::publish_cap`).
        let starting_cap =
            (config.liquidity.enabled && config.mode == Mode::Live).then_some(U256::ZERO);
        let liquidity = LiquidityShared {
            cap: saved_floor
                .as_ref()
                .filter(|f| f.lowers)
                .map(|f| f.target)
                .or(starting_cap)
                .map(|cap| (cap, u64::MAX)),
            pending_raise: saved_floor.as_ref().filter(|f| !f.lowers).map(|f| f.target),
            ..Default::default()
        };
        let status = Status {
            version: VERSION,
            config: config.clone(),
            at: crate::time::unix_now(),
            decisions: vec![],
            rebalance_status: "Waiting for fresh vault data".into(),
            next_attempt_at,
            liquidity: None,
            curve: None,
            quote_status: Some(QuoteStatus {
                at: crate::time::unix_now(),
                state: QuoteState::WaitingForSession,
                message: "Waiting for an authenticated RFQ session".into(),
            }),
        };
        // Write an initial snapshot synchronously: unwritable storage must fail startup.
        write_json(&dir.join(STATUS_FILE), &status)?;
        // A slow disk may coalesce snapshots, but must keep the latest state
        // (especially disconnects) rather than dropping it behind an old write.
        let (writer, mut receiver) = tokio::sync::watch::channel(status.clone());
        let status_path = dir.join(STATUS_FILE);
        tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let snapshot = receiver.borrow_and_update().clone();
                let path = status_path.clone();
                let write = tokio::task::spawn_blocking(move || -> Result<()> {
                    write_json(&path, &snapshot)
                })
                .await;
                if !matches!(write, Ok(Ok(()))) {
                    tracing::error!("module decision recorder could not write its snapshot");
                }
            }
        });
        let history = load_history(
            dir,
            price_source,
            config.spreads.window_secs,
            crate::time::unix_now(),
        );
        Ok(Self {
            history_writer: spawn_history_writer(dir, price_source.to_owned()),
            config,
            balances: Arc::new(RwLock::new(None)),
            state: Arc::new(Mutex::new(State {
                history,
                status,
                fetching: false,
                ready: None,
            })),
            dir: dir.into(),
            writer,
            liquidity: Arc::new(Mutex::new(liquidity)),
            saved_floor,
        })
    }
    pub fn decide(&self, context: &Context) -> Decision {
        let mut state = self.state.lock().expect("module state poisoned");
        let history = &mut state.history;
        // A degraded print (held, clamped or fallback) isn't the market the
        // window measures, so it never becomes a sample.
        let sampled = context.price_at <= context.now
            && !context.price_degraded
            && context.price.is_finite()
            && context.price > 0.0
            && history
                .last()
                .is_none_or(|p| context.price_at > p.timestamp);
        if sampled {
            history.push(PricePoint {
                timestamp: context.price_at,
                price: context.price,
            });
        }
        history.retain(|p| {
            p.timestamp <= context.now
                && context.now - p.timestamp <= self.config.spreads.window_secs
        });
        if history.len() > MAX_HISTORY {
            history.drain(..history.len() - MAX_HISTORY);
        }
        // Only a new source sample changes what a restart could reuse.
        if sampled {
            let _ = self.history_writer.send_replace(history.clone());
        }
        let decision = evaluate(&self.config, context, history);
        let inventory = decision
            .inventory_bps
            .filter(|_| self.config.inventory.enabled)
            .map(|share| strategies::inventory(&self.config.inventory, context, share));
        let inputs = DecisionInputs {
            price_at: context.price_at,
            balances_at: context.balances_at,
            base_buy_bps: context.base_buy_bps,
            base_sell_bps: context.base_sell_bps,
            inventory_buy_bps: inventory
                .as_ref()
                .map_or(context.base_buy_bps, |p| p.buy_bps),
            inventory_sell_bps: inventory
                .as_ref()
                .map_or(context.base_sell_bps, |p| p.sell_bps),
            spread_window: strategies::spread_window(&self.config.spreads, context.now, history),
        };
        if state
            .status
            .decisions
            .last()
            .is_none_or(|o| o.decision.at != context.now)
        {
            state.status.decisions.push(Observation {
                decision: decision.clone(),
                price: context.price,
                settlement: context.settlement,
                corridor: context.corridor,
                inputs: Some(inputs),
            });
            if state.status.decisions.len() > MAX_DECISIONS {
                state.status.decisions.remove(0);
            }
            state.status.at = context.now;
            let _ = self.writer.send_replace(state.status.clone());
        }
        decision
    }
    /// Diagnostic only. Never changes quote eligibility, prices or reservations.
    pub fn quote_status(&self, state: QuoteState, message: &str, now: u64) {
        let mut s = self.state.lock().expect("module state poisoned");
        if s.status
            .quote_status
            .as_ref()
            .is_some_and(|q| q.at == now && q.state == state && q.message == message)
        {
            return;
        }
        s.status.quote_status = Some(QuoteStatus {
            at: now,
            state,
            message: message.into(),
        });
        s.status.at = now;
        let _ = self.writer.send_replace(s.status.clone());
    }
    #[cfg(test)]
    pub(crate) fn current_quote_status(&self) -> Option<QuoteStatus> {
        let s = self.state.lock().expect("module state poisoned");
        s.status.quote_status.clone()
    }
    #[cfg(test)]
    pub(crate) fn rebalance_status_for_test(&self) -> String {
        let s = self.state.lock().expect("module state poisoned");
        s.status.rebalance_status.clone()
    }
    pub fn status(&self, message: impl Into<String>) {
        let mut s = self.state.lock().expect("module state poisoned");
        s.status.rebalance_status = message.into();
        s.status.at = crate::time::unix_now();
        let _ = self.writer.send_replace(s.status.clone());
    }
    /// Persist pacing BEFORE any network request. Restarts never reset it.
    pub fn begin_quote(&self, request: dealer::QuoteRequest) -> Result<()> {
        let Some(dealer) = self.config.rebalance.dealer.clone() else {
            self.status("No dealer configured");
            return Ok(());
        };
        let now = crate::time::unix_now();
        let mut state = self.state.lock().expect("module state poisoned");
        if state.fetching || state.ready.is_some() || now < state.status.next_attempt_at {
            return Ok(());
        }
        let next = now.saturating_add(self.config.rebalance.cooldown_secs);
        write_json(
            &self.dir.join(ATTEMPTS_FILE),
            &Attempt {
                version: VERSION,
                next_attempt_at: next,
            },
        )?;
        state.status.next_attempt_at = next;
        state.fetching = true;
        drop(state);
        self.status("Requesting dealer price");
        let runtime = self.clone();
        tokio::spawn(async move {
            match dealer::quote(&dealer, &request).await {
                Ok(quote) => {
                    runtime.state.lock().expect("module state poisoned").ready =
                        Some((request, quote));
                    runtime.status("Dealer price received; validating current exposure");
                }
                Err(_) => {
                    runtime.status("Dealer quote failed or was rejected; waiting for cooldown")
                }
            }
            runtime
                .state
                .lock()
                .expect("module state poisoned")
                .fetching = false;
        });
        Ok(())
    }
    pub fn take_quote(&self) -> Option<(dealer::QuoteRequest, dealer::Quote)> {
        self.state.lock().ok()?.ready.take()
    }

    /// The floor change the previous run saved, for the liquidity loop to settle.
    pub fn saved_pending_floor(&self) -> Option<PendingFloorRecord> {
        self.saved_floor.clone()
    }
    /// Save the pending floor change, or remove it once settled. Must succeed
    /// before one is broadcast: a restart that lost a lower would quote past
    /// the new floor, and one that lost a raise could send a second change.
    pub fn save_pending_floor(&self, pending: Option<PendingFloorRecord>) -> Result<()> {
        let path = self.dir.join(PENDING_FLOOR_FILE);
        match pending {
            Some(record) => write_json(&path, &record),
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
        }
    }

    /// The settlement cap new quotes must respect, if one is in force: the
    /// liquidity loop's cap, and for a live schedule the liquid shortfall
    /// from the latest balance read, whichever is lower. The loop runs every
    /// 15 seconds; balances refresh every couple, so a fill that drains liquid
    /// under the floor caps the next quote without waiting for the loop.
    /// A poisoned lock reads as a zero cap: quoting less is the safe failure.
    pub fn liquidity_quote_cap(&self, now: u64) -> Option<U256> {
        let Ok(shared) = self.liquidity.lock() else {
            return Some(U256::ZERO);
        };
        match (
            shared.scheduled_cap(now),
            self.liquid_shortfall_cap(now, shared.pending_raise),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    /// [`super::liquidity::shortfall_cap`] on the latest balance read, while
    /// the schedule runs live, and in any mode while a raise may still land.
    /// Liquid is measured against the higher of the on-chain floor and that
    /// raise's target. A read that is missing or older than
    /// [`super::BALANCES_MAX_AGE_SECS`] caps at zero: it can't show that a fill
    /// hasn't just drained liquid, and quotes should not outlive the read that
    /// bounds them. That's the same bound the modules hold balances to, and
    /// about when quote inventory goes dark.
    fn liquid_shortfall_cap(&self, now: u64, pending_raise: Option<U256>) -> Option<U256> {
        let live = self.config.liquidity.enabled && self.config.mode == Mode::Live;
        if !live && pending_raise.is_none() {
            return None;
        }
        let fresh = self
            .balances
            .read()
            .ok()
            .and_then(|b| *b)
            .filter(|b| now.saturating_sub(b.at) <= super::BALANCES_MAX_AGE_SECS);
        let raise = pending_raise.unwrap_or_default();
        match fresh.and_then(|b| Some((b.liquid?, b.settlement))) {
            Some((r, free)) => super::liquidity::shortfall_cap(r.liquid, r.floor.max(raise), free),
            None => Some(U256::ZERO),
        }
    }
    /// Hand the per-quote shortfall cap the target of a raise that may still
    /// land, or clear it once that raise settles.
    pub fn set_pending_raise(&self, target: Option<U256>) {
        if let Ok(mut shared) = self.liquidity.lock() {
            shared.pending_raise = target;
        }
    }
    /// The liquidity loop's own cap, without the shortfall cap.
    #[cfg(test)]
    pub(crate) fn scheduled_quote_cap(&self, now: u64) -> Option<U256> {
        self.liquidity.lock().ok()?.scheduled_cap(now)
    }
    /// Set or clear the cap. `through` is when it lapses unless renewed.
    pub fn set_liquidity_cap(&self, cap: Option<U256>, through: u64) {
        if let Ok(mut shared) = self.liquidity.lock() {
            shared.cap = cap.map(|c| (c, through));
        }
    }
    /// Move the current cap's expiry to `through` without changing the cap.
    pub fn renew_liquidity_cap(&self, through: u64) {
        if let Ok(mut shared) = self.liquidity.lock() {
            if let Some((cap, _)) = shared.cap {
                shared.cap = Some((cap, through));
            }
        }
    }
    /// Engine side: what live quotes commit now. Recorded with the loop's own
    /// cap in force, not the shortfall cap: that one comes and goes with each
    /// balance read, so it can't vouch that quotes stay under a new floor.
    pub fn record_liquidity_commitments(&self, committed: U256, now: u64) {
        if let Ok(mut shared) = self.liquidity.lock() {
            shared.commitments = Some((committed, shared.scheduled_cap(now), now));
        }
    }
    /// The engine's last measurement, if no older than `max_age` seconds.
    pub fn liquidity_commitments(
        &self,
        now: u64,
        max_age: u64,
    ) -> Option<super::liquidity::Commitments> {
        let shared = self.liquidity.lock().ok()?;
        let (committed, cap_in_effect, at) = shared.commitments?;
        (at <= now && now - at <= max_age).then_some(super::liquidity::Commitments {
            committed,
            cap_in_effect,
        })
    }
    /// The curve's view of its inputs this tick. Written once a second at
    /// most: the level loop calls this on every publish.
    pub fn curve_reading(&self, reading: CurveReading) {
        let mut s = self.state.lock().expect("module state poisoned");
        let status = &mut s.status;
        let curve = status.curve.get_or_insert_with(CurveStatus::default);
        if curve.reading.as_ref().is_some_and(|r| r.at == reading.at) {
            return;
        }
        status.at = reading.at;
        curve.reading = Some(reading);
        let _ = self.writer.send_replace(status.clone());
    }

    /// A quote the curve priced or answered flat.
    pub fn curve_quote(&self, quote: CurveQuote) {
        let mut s = self.state.lock().expect("module state poisoned");
        let status = &mut s.status;
        status.at = quote.at;
        let curve = status.curve.get_or_insert_with(CurveStatus::default);
        curve.quotes.push(quote);
        if curve.quotes.len() > MAX_DECISIONS {
            curve.quotes.remove(0);
        }
        let _ = self.writer.send_replace(status.clone());
    }

    #[cfg(test)]
    pub(crate) fn status_snapshot(&self) -> Status {
        self.state
            .lock()
            .expect("module state poisoned")
            .status
            .clone()
    }
    pub fn liquidity_status(&self, status: super::liquidity::LiquidityStatus) {
        let mut s = self.state.lock().expect("module state poisoned");
        s.status.at = status.at;
        s.status.liquidity = Some(status);
        let _ = self.writer.send_replace(s.status.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fill that drains liquid under the floor caps the very next quote
    /// from the balance read, without waiting for the liquidity loop.
    #[tokio::test]
    async fn a_liquid_shortfall_caps_quotes_from_the_balance_read() {
        let runtime_for = |mode: Mode| {
            let dir =
                std::env::temp_dir().join(format!("stitch-shortfall-{}", rand::random::<u64>()));
            std::fs::create_dir_all(&dir).unwrap();
            let mut config = ModulesConfig {
                mode,
                ..Default::default()
            };
            config.liquidity.enabled = true;
            Runtime::new(config, &dir, "http://localhost/price").unwrap()
        };
        let read = |runtime: &Runtime, liquid: u64, floor: u64| {
            *runtime.balances.write().unwrap() = Some(Balances {
                settlement: U256::from(1_000u64),
                corridor: U256::ZERO,
                liquid: Some(LiquidReading {
                    liquid: U256::from(liquid),
                    floor: U256::from(floor),
                }),
                at: 0,
            });
        };
        let live = runtime_for(Mode::Live);
        // Drop the zero cap a live schedule starts with, as a trusted read would.
        live.set_liquidity_cap(None, 0);
        read(&live, 500, 500);
        assert_eq!(live.liquidity_quote_cap(0), None, "funded");
        read(&live, 499, 500);
        assert_eq!(live.liquidity_quote_cap(0), Some(U256::from(499u64)));
        // The lower of the loop's cap and the shortfall.
        live.set_liquidity_cap(Some(U256::from(300u64)), u64::MAX);
        assert_eq!(live.liquidity_quote_cap(0), Some(U256::from(300u64)));
        live.set_liquidity_cap(Some(U256::from(900u64)), u64::MAX);
        assert_eq!(live.liquidity_quote_cap(0), Some(U256::from(499u64)));
        // A read past the age bound, or none yet, can't vouch for liquid.
        live.set_liquidity_cap(None, 0);
        read(&live, 500, 500);
        let max_age = crate::modules::BALANCES_MAX_AGE_SECS;
        assert_eq!(live.liquidity_quote_cap(max_age), None, "still fresh");
        assert_eq!(
            live.liquidity_quote_cap(max_age + 1),
            Some(U256::ZERO),
            "stale"
        );
        *live.balances.write().unwrap() = None;
        assert_eq!(live.liquidity_quote_cap(0), Some(U256::ZERO), "no read yet");

        let preview = runtime_for(Mode::Shadow);
        read(&preview, 0, 500);
        assert_eq!(preview.liquidity_quote_cap(0), None, "Preview never caps");
    }

    /// A raise that may still land measures liquid against its target in any
    /// mode, from the moment a restart loads it.
    #[tokio::test]
    async fn a_pending_raise_measures_liquid_against_its_target() {
        let dir = std::env::temp_dir().join(format!("stitch-raise-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        write_json(
            &dir.join(PENDING_FLOOR_FILE),
            &PendingFloorRecord {
                version: VERSION,
                target: U256::from(900u64),
                lowers: false,
                chain_id: 8453,
                vault: alloy_primitives::Address::ZERO,
                sender: alloy_primitives::Address::ZERO,
                nonce: 5,
                signed_tx: None,
            },
        )
        .unwrap();
        let preview = Runtime::new(
            ModulesConfig {
                mode: Mode::Shadow,
                ..Default::default()
            },
            &dir,
            "http://localhost/price",
        )
        .unwrap();
        *preview.balances.write().unwrap() = Some(Balances {
            settlement: U256::from(1_000u64),
            corridor: U256::ZERO,
            liquid: Some(LiquidReading {
                liquid: U256::from(300u64),
                floor: U256::ZERO,
            }),
            at: 0,
        });
        assert_eq!(preview.liquidity_quote_cap(0), Some(U256::from(300u64)));
        preview.set_pending_raise(None);
        assert_eq!(
            preview.liquidity_quote_cap(0),
            None,
            "Preview, nothing pending"
        );
    }

    /// A shortfall cap alone never opens the lower gate: it lifts as soon as
    /// a deposit refills liquid, so only the loop's own cap counts.
    #[tokio::test]
    async fn commitments_carry_only_the_loops_own_cap() {
        let dir =
            std::env::temp_dir().join(format!("stitch-commitments-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = ModulesConfig {
            mode: Mode::Live,
            ..Default::default()
        };
        config.liquidity.enabled = true;
        let live = Runtime::new(config, &dir, "http://localhost/price").unwrap();
        live.set_liquidity_cap(None, 0);
        *live.balances.write().unwrap() = Some(Balances {
            settlement: U256::from(1_000u64),
            corridor: U256::ZERO,
            liquid: Some(LiquidReading {
                liquid: U256::ZERO,
                floor: U256::from(500u64),
            }),
            at: 0,
        });
        assert_eq!(live.liquidity_quote_cap(0), Some(U256::ZERO));
        live.record_liquidity_commitments(U256::ZERO, 0);
        assert_eq!(
            live.liquidity_commitments(0, 10).unwrap().cap_in_effect,
            None
        );

        live.set_liquidity_cap(Some(U256::from(300u64)), 60);
        live.record_liquidity_commitments(U256::ZERO, 0);
        assert_eq!(
            live.liquidity_commitments(0, 10).unwrap().cap_in_effect,
            Some(U256::from(300u64))
        );
        live.record_liquidity_commitments(U256::ZERO, 61);
        assert_eq!(
            live.liquidity_commitments(61, 10).unwrap().cap_in_effect,
            None,
            "a lapsed cap is no cap"
        );
    }

    #[test]
    fn concurrent_writes_to_one_module_file_all_land() {
        let dir =
            std::env::temp_dir().join(format!("stitch-module-writes-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STATUS_FILE);
        let writers: Vec<_> = (0..8u32)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    (0..100u32).try_for_each(|n| write_json(&path, &(writer, n)))
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        let last: (u32, u32) =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(last.1, 99);
        let _ = std::fs::remove_dir_all(dir);
    }
}
