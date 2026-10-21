// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Acts on the liquidity schedule ([`crate::modules::liquidity`]).
//!
//! One process-scoped loop reads the vault, asks the pure planner for the next
//! step, publishes the quote cap the engine applies while a lower is pending,
//! and in Live mode sends the step from the bot's own wallet:
//!
//! - floor changes go through the operator's Zodiac Roles grant, so the vault
//!   sees the operator-admin Safe as `msg.sender` and the bot never holds the
//!   operator-admin key;
//! - recalls call `prepareSettlement` directly, which anyone may call.
//!
//! At most one transaction is in flight. Every step is re-derived from a fresh
//! read, so a recall that lands late, or twice, only costs gas: a second
//! recall finds liquid already there.
//!
//! A floor change is different. Two that can both still execute may land out
//! of order, and a lower that lands after the cap was lifted drops the floor
//! under quotes signed in between. So a floor change keeps the in-flight slot,
//! and its cap, until the chain settles it: the account nonce it was signed
//! at is used. Nothing
//! resends or cancels it. The wallet already bumps its fee while it waits for
//! a receipt; one still stuck after that is the operator's to replace.
use super::math::format_units;
use super::*;
use crate::chain::rpc::unresolved_nonce;
use crate::config::RfqCapacity;
use crate::modules::liquidity::{self, ActionRecord, LiquidityStatus, Plan, Step, VaultLiquidity};
use crate::modules::runtime::{PendingFloorRecord, Runtime, LIQUIDITY_CAP_TTL_SECS};
use crate::protocol::roles::{encode_exec_transaction_with_role, encode_target};
use crate::protocol::vault::{
    address_from_word, encode_min_liquid_settlement, encode_operator_admin,
    encode_prepare_settlement, encode_set_min_liquid_settlement, encode_yield_adapter,
};
use alloy_primitives::B256;
use std::collections::VecDeque;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Custody and MPC signers can hold a transaction for approval. The
/// in-flight slot stays taken while this runs, so nothing queues behind it.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// The engine measures live quotes on every level refresh (about once a
/// second). Older than this means the session is down: do not lower on it.
const COMMITMENT_MAX_AGE_SECS: u64 = 10;
const RETRY_AFTER_FAILURE_SECS: u64 = 60;
/// A transaction that may still land gets time to show up before the next try.
const RETRY_AFTER_UNKNOWN_SECS: u64 = 300;
const MAX_ACTIONS: usize = 20;

/// One vault read: what the planner needs plus the authority checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    vault: VaultLiquidity,
    operator_admin: Address,
    /// The Roles modifier's `target()`, when an authority is configured.
    authority_target: Option<Address>,
}

fn reading_calls(vault: Address, modifier: Option<Address>) -> Vec<Call> {
    [
        Call::new(vault, encode_min_liquid_settlement()),
        Call::new(vault, encode_liquid_settlement()),
        Call::new(vault, encode_free_settlement()),
        Call::new(vault, encode_yield_adapter()),
        Call::new(vault, encode_paused()),
        Call::new(vault, encode_close_only()),
        Call::new(vault, encode_operator_admin()),
    ]
    .into_iter()
    .chain(modifier.map(|m| Call::new(m, encode_target())))
    .collect()
}

/// Every vault view is required: a step decided on a partial read could lower
/// the floor of a paused vault or recall against a stale balance. The
/// modifier's `target()` is optional; a missing one only blocks floor changes.
fn decode_reading(results: &[Option<Bytes>]) -> Option<Reading> {
    let word = |i: usize| results.get(i)?.as_ref().map(decode_uint);
    Some(Reading {
        vault: VaultLiquidity {
            floor: word(0)?,
            liquid: word(1)?,
            free: word(2)?,
            has_adapter: !address_from_word(word(3)?).is_zero(),
            paused: !word(4)?.is_zero(),
            close_only: !word(5)?.is_zero(),
        },
        operator_admin: address_from_word(word(6)?),
        authority_target: word(7).map(address_from_word),
    })
}

/// The operator's grant: which modifier to call and with which role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Authority {
    modifier: Address,
    role_key: B256,
}

/// A transaction the loop may send for a step.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Transaction {
    to: Address,
    data: Vec<u8>,
    describe: String,
    /// Set for `setMinLiquidSettlement`. Tracked until its nonce is used, so
    /// no second floor change goes out while it can still execute; a lower
    /// also keeps quotes capped at its target until then.
    floor_change: Option<FloorChange>,
}

/// The floor a change sets, and which way it moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FloorChange {
    target: U256,
    lowers: bool,
}

/// The transaction for `step`, `Ok(None)` when the step sends nothing, or the
/// reason the floor cannot be moved through the configured authority.
fn transaction(
    step: Step,
    vault: Address,
    reading: &Reading,
    authority: Option<Authority>,
    decimals: u8,
) -> Result<Option<Transaction>, String> {
    let floor_change = |target: U256, lowers: bool| -> Result<Option<Transaction>, String> {
        let Some(a) = authority else {
            return Err("No Roles authority configured, so the floor cannot be changed".into());
        };
        match reading.authority_target {
            Some(t) if t == reading.operator_admin => {}
            Some(t) => {
                return Err(format!(
                    "The Roles modifier acts for {t}, but the vault's operator admin is {}",
                    reading.operator_admin
                ))
            }
            None => return Err("Could not read the Roles modifier's target".into()),
        }
        let inner = encode_set_min_liquid_settlement(target);
        Ok(Some(Transaction {
            to: a.modifier,
            data: encode_exec_transaction_with_role(vault, &inner, a.role_key),
            describe: format!(
                "{} floor to {}",
                if lowers { "Lower" } else { "Raise" },
                format_units(target, decimals)
            ),
            floor_change: Some(FloorChange { target, lowers }),
        }))
    };
    match step {
        Step::RaiseFloor(target) => floor_change(target, false),
        Step::LowerFloor(target) => floor_change(target, true),
        Step::Recall(needed) => Ok(Some(Transaction {
            to: vault,
            data: encode_prepare_settlement(needed),
            describe: format!("Recall until {} is liquid", format_units(needed, decimals)),
            floor_change: None,
        })),
        Step::Hold | Step::Drain | Step::Wait => Ok(None),
    }
}

/// Keep, clear or set the engine's quote cap. The loop's only writer of it,
/// once per tick.
///
/// While a lower is pending or about to be sent (`pinned` is its target) the
/// cap never lapses, is never cleared and never rises above `pinned`: that is
/// the floor the chain may still land, whatever the schedule wants now. A
/// deposit that raises the planned target must not let quotes past it.
///
/// Otherwise only a `confirmed` plan (built on a vault read) moves the cap:
/// it sets the plan's `quote_cap`, or clears it, including when the vault's
/// state means the floor does not apply. Without one the last cap, the zero
/// cap a live schedule starts with included, stays in force with no expiry
/// until a read can say otherwise.
fn publish_cap(
    modules: &Runtime,
    quote_cap: Option<U256>,
    now: u64,
    pinned: Option<U256>,
    confirmed: bool,
) {
    match (pinned, confirmed, quote_cap) {
        (Some(pinned), _, cap) => {
            modules.set_liquidity_cap(Some(cap.map_or(pinned, |c| c.min(pinned))), u64::MAX)
        }
        (None, false, _) => modules.renew_liquidity_cap(u64::MAX),
        (None, true, Some(cap)) => {
            modules.set_liquidity_cap(Some(cap), now.saturating_add(LIQUIDITY_CAP_TTL_SECS))
        }
        (None, true, None) => modules.set_liquidity_cap(None, 0),
    }
}

/// A floor change that is not known to have failed. Saved to disk once it's
/// signed, before its first broadcast (see [`Runtime::save_pending_floor`]),
/// so a restart resumes with the slot held (and its cap pinned) and settles
/// it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingFloor {
    target: U256,
    lowers: bool,
    /// Where it was signed for. A bot pointed elsewhere refuses to start
    /// rather than settle it there ([`PendingFloorRecord::foreign_to`]).
    chain_id: u64,
    vault: Address,
    /// The account that signs it. Only that account's nonces settle it.
    sender: Address,
    /// The nonce it is signed at. `None` in memory until it's signed, when
    /// nothing can have reached the chain yet.
    nonce: Option<u64>,
    /// A send in this process is still waiting on it and settles it itself.
    /// Otherwise (no receipt, or saved by an earlier run) the loop settles it
    /// against the chain.
    sending: bool,
}

impl PendingFloor {
    /// What gets saved. `None` before it's signed: nothing to track.
    fn record(&self) -> Option<PendingFloorRecord> {
        Some(PendingFloorRecord {
            version: crate::modules::VERSION,
            target: self.target,
            lowers: self.lowers,
            chain_id: self.chain_id,
            vault: self.vault,
            sender: self.sender,
            nonce: self.nonce?,
            signed_tx: None,
        })
    }

    /// A change an earlier run saved. Nothing in this process is sending it.
    fn resumed(record: PendingFloorRecord) -> Self {
        Self {
            target: record.target,
            lowers: record.lowers,
            chain_id: record.chain_id,
            vault: record.vault,
            sender: record.sender,
            nonce: Some(record.nonce),
            sending: false,
        }
    }

    fn describe(&self) -> String {
        let change = if self.lowers { "lower" } else { "raise" };
        match self.nonce {
            Some(nonce) => format!("floor {change} at {} nonce {nonce}", self.sender),
            None => format!("floor {change} from {}, not signed yet", self.sender),
        }
    }

    /// Whether `floor` already reads at or past the target, in the
    /// direction the change moves it.
    fn reached(&self, floor: U256) -> bool {
        if self.lowers {
            floor <= self.target
        } else {
            floor >= self.target
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloorOutcome {
    Landed,
    /// Its nonce is used up and the floor is short of its target: it
    /// reverted or another transaction took the nonce.
    CannotLand,
    Pending,
}

/// Settle a floor change against the chain. Only its sender's consumed nonce
/// does: until then it can still execute, whatever the floor reads now
/// (another admin transaction may have moved it). `latest` (the sender's next
/// unmined nonce) is read before `floor`, so a consumed nonce with the floor
/// short of `target` means the change did not take effect.
fn floor_outcome(change: &PendingFloor, floor: U256, latest: u64) -> FloorOutcome {
    match change.nonce {
        Some(nonce) if latest <= nonce => FloorOutcome::Pending,
        Some(_) if change.reached(floor) => FloorOutcome::Landed,
        // Used without taking effect, or never given a nonce, so never signed.
        _ => FloorOutcome::CannotLand,
    }
}

/// What the loop says while a floor change waits on its nonce.
fn unsettled(change: &PendingFloor, me: Address) -> String {
    let signer = if change.sender == me {
        ""
    } else {
        " (a previous signer)"
    };
    let capped = if change.lowers {
        ", and quotes stay capped at its target"
    } else {
        ""
    };
    format!(
        "The {}{signer} has not settled yet. Nothing else is sent until that nonce is used{capped}. \
         If it is stuck, replace that nonce from the signing account",
        change.describe()
    )
}

#[derive(Default)]
struct Execution {
    in_flight: Option<String>,
    pending: Option<PendingFloor>,
    /// The signed bytes of a change an earlier run saved, to broadcast once.
    rebroadcast: Option<String>,
    retry_at: u64,
    actions: VecDeque<ActionRecord>,
}

impl Execution {
    fn record(&mut self, action: ActionRecord) {
        self.actions.push_back(action);
        while self.actions.len() > MAX_ACTIONS {
            self.actions.pop_front();
        }
    }

    /// A floor change the loop has to settle against the chain.
    fn awaiting_chain(&self) -> Option<PendingFloor> {
        self.pending.filter(|p| !p.sending)
    }

    /// Resume where an earlier run left off.
    fn resume(saved: Option<PendingFloorRecord>) -> Self {
        let rebroadcast = saved.as_ref().and_then(|r| r.signed_tx.clone());
        let pending = saved.map(PendingFloor::resumed);
        Self {
            in_flight: pending.map(|p| format!("Unconfirmed {}", p.describe())),
            pending,
            rebroadcast,
            ..Default::default()
        }
    }

    /// Free the slot after a send settles; the next step waits for one fresh read.
    fn settle(&mut self, now: u64, retry_after: u64, action: ActionRecord) {
        self.in_flight = None;
        self.pending = None;
        self.rebroadcast = None;
        self.retry_at = now.saturating_add(retry_after);
        self.record(action);
    }
}

/// Remove the saved change once it settles. Settling never fails on this: a
/// stale file only makes the next run settle a change that already settled.
fn forget_saved(modules: &Runtime) {
    if let Err(e) = modules.save_pending_floor(None) {
        error!(error = %format!("{e:#}"), "could not remove the settled liquidity floor change");
    }
}

/// Everything the loop needs that does not change while it runs.
struct Liquidity {
    wallet: Arc<Wallet>,
    vault: Address,
    modules: Runtime,
    authority: Option<Authority>,
    decimals: u8,
    live: bool,
    execution: Arc<Mutex<Execution>>,
}

pub(super) async fn liquidity_loop(
    wallet: Wallet,
    vault: Address,
    modules: Runtime,
    settlement_decimals: u8,
) {
    let me = Liquidity::new(wallet, vault, modules, settlement_decimals);
    let calls = reading_calls(vault, me.authority.map(|a| a.modifier));
    let mut batcher = None;
    loop {
        if batcher.is_none() {
            batcher = tokio::time::timeout(READ_TIMEOUT, Batcher::detect(me.wallet.rpc()))
                .await
                .ok()
                .and_then(Result::ok);
        }
        // Before the vault read, so a floor read is never older than it.
        let latest = me.latest_nonce().await;
        let reader = batcher.unwrap_or_else(Batcher::sequential);
        let reading = tokio::time::timeout(READ_TIMEOUT, reader.read(me.wallet.rpc(), &calls))
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|results| decode_reading(&results));
        me.tick(reading, latest).await;
        if me.finished() {
            info!("liquidity schedule is off and no floor change is pending; loop stopped");
            return;
        }
        tokio::time::sleep(TICK).await;
    }
}

/// With the schedule off, the loop only settles a floor change an earlier
/// run sent: it keeps a lower's cap and sends nothing.
fn settling_only(plan: Plan) -> Plan {
    Plan {
        target: None,
        step: Step::Wait,
        quote_cap: None,
        message: "The schedule is off. Waiting for the last floor change to land or for its \
                  nonce to be used"
            .into(),
        ..plan
    }
}

impl Liquidity {
    fn new(wallet: Wallet, vault: Address, modules: Runtime, decimals: u8) -> Self {
        let authority = modules.config.liquidity.authority.as_ref().and_then(|a| {
            Some(Authority {
                modifier: a.modifier().ok()?,
                role_key: a.key().ok()?,
            })
        });
        Self {
            wallet: Arc::new(wallet),
            vault,
            authority,
            decimals,
            live: modules.config.mode == crate::modules::Mode::Live,
            execution: Arc::new(Mutex::new(Execution::resume(modules.saved_pending_floor()))),
            modules,
        }
    }

    /// The schedule is off and no floor change is left to settle.
    fn finished(&self) -> bool {
        !self.modules.config.liquidity.enabled
            && self.execution.lock().is_ok_and(|e| e.pending.is_none())
    }

    /// The next unmined nonce of the account that signed a floor change, read
    /// only while one waits on it. That may be a signer this bot no longer uses.
    async fn latest_nonce(&self) -> Option<u64> {
        let sender = self.execution.lock().ok()?.awaiting_chain()?.sender;
        tokio::time::timeout(
            READ_TIMEOUT,
            self.wallet.rpc().latest_transaction_count(sender),
        )
        .await
        .ok()?
        .ok()
    }

    /// Settle a floor change whose send gave up, once a read shows how it ended.
    fn reconcile(&self, reading: Option<&Reading>, latest: Option<u64>, now: u64) {
        let (Some(reading), Some(latest)) = (reading, latest) else {
            return;
        };
        let Ok(mut execution) = self.execution.lock() else {
            return;
        };
        let Some(pending) = execution.awaiting_chain() else {
            return;
        };
        let action = |ok: bool, detail: String| ActionRecord {
            at: now,
            action: format!(
                "{} floor to {}",
                if pending.lowers { "Lower" } else { "Raise" },
                format_units(pending.target, self.decimals)
            ),
            ok,
            detail,
        };
        let settled = match floor_outcome(&pending, reading.vault.floor, latest) {
            FloorOutcome::Landed => action(true, format!("landed ({})", pending.describe())),
            FloorOutcome::CannotLand => action(
                false,
                format!(
                    "the nonce of the {} was used without it taking effect",
                    pending.describe()
                ),
            ),
            FloorOutcome::Pending => return,
        };
        execution.settle(now, TICK.as_secs(), settled);
        drop(execution);
        forget_saved(&self.modules);
    }

    async fn tick(&self, reading: Option<Reading>, latest: Option<u64>) {
        let now = unix_now();
        self.reconcile(reading.as_ref(), latest, now);
        self.rebroadcast_saved().await;
        let cfg = &self.modules.config.liquidity;
        let commitments = self
            .modules
            .liquidity_commitments(now, COMMITMENT_MAX_AGE_SECS);
        let plan = liquidity::plan(cfg, now, reading.as_ref().map(|r| &r.vault), commitments);
        let plan = if cfg.enabled {
            plan
        } else {
            settling_only(plan)
        };
        // A change that may still land keeps its cap in any mode: a lower
        // pins its target, and so does one this tick is about to send, from
        // before it is signed; a raise goes to the per-quote shortfall cap,
        // which measures liquid against its target on every balance read. A
        // poisoned lock cannot say what is pending: pin at zero.
        let (pending, raising) = match self.execution.lock() {
            Ok(e) => (
                e.pending.filter(|p| p.lowers).map(|p| p.target),
                e.pending.filter(|p| !p.lowers).map(|p| p.target),
            ),
            Err(_) => (Some(U256::ZERO), None),
        };
        self.modules.set_pending_raise(raising);
        let lowering = match plan.step {
            Step::LowerFloor(target) if self.live => Some(target),
            _ => None,
        };
        // Only Live caps for the schedule itself.
        let quote_cap = plan.quote_cap.filter(|_| self.live);
        let confirmed = reading.is_some() || !self.live;
        publish_cap(
            &self.modules,
            quote_cap,
            now,
            pending.or(lowering),
            confirmed,
        );
        let message = match (self.live, &reading) {
            (true, Some(r)) => self.act(&plan, r, now).await,
            (false, _) if !matches!(plan.step, Step::Hold | Step::Wait) => {
                Some(format!("Preview only: {}", plan.message))
            }
            _ => None,
        }
        .unwrap_or_else(|| plan.message.clone());
        self.report(&plan, reading.as_ref(), commitments, message, now);
    }

    /// Send the plan's transaction if nothing is in flight. Returns a message
    /// that replaces the plan's when the bot cannot act on it.
    async fn act(&self, plan: &Plan, reading: &Reading, now: u64) -> Option<String> {
        let retry_at = {
            let execution = self.execution.lock().ok()?;
            if let Some(p) = execution.awaiting_chain() {
                return Some(unsettled(&p, self.wallet.address()));
            }
            if execution.in_flight.is_some() {
                return None;
            }
            execution.retry_at
        };
        let tx = match transaction(
            plan.step,
            self.vault,
            reading,
            self.authority,
            self.decimals,
        ) {
            Ok(Some(tx)) => tx,
            Ok(None) => return None,
            Err(why) => return Some(why),
        };
        if now < retry_at {
            return Some(format!("{}. Retrying in {}s", plan.message, retry_at - now));
        }
        let me = self.wallet.address();
        let balance = tokio::time::timeout(READ_TIMEOUT, self.wallet.rpc().get_balance(me)).await;
        if matches!(balance, Ok(Ok(b)) if b.is_zero()) {
            return Some(format!("{me} needs gas to {}", tx.describe.to_lowercase()));
        }
        self.spawn_send(tx);
        None
    }

    /// Claim the slot and send. A floor change is saved with its chain,
    /// vault, sender and nonce once it's signed and before its first
    /// broadcast; if that save fails it is not sent. A restart while the
    /// signer still waits for approval leaves nothing saved: that signature,
    /// if it ever comes, has no one to broadcast it.
    fn spawn_send(&self, tx: Transaction) {
        let tracked = tx.floor_change.map(|c| PendingFloor {
            target: c.target,
            lowers: c.lowers,
            chain_id: self.wallet.chain_id(),
            vault: self.vault,
            sender: self.wallet.address(),
            nonce: None,
            sending: true,
        });
        let Ok(mut execution) = self.execution.lock() else {
            return;
        };
        execution.in_flight = Some(tx.describe.clone());
        execution.pending = tracked;
        drop(execution);
        // From before it is signed, not at the next tick: it can land within one.
        if let Some(t) = tracked.filter(|t| !t.lowers) {
            self.modules.set_pending_raise(Some(t.target));
        }
        let wallet = self.wallet.clone();
        let execution = self.execution.clone();
        let modules = self.modules.clone();
        let explain = self.refusal_hint(&tx);
        tokio::spawn(async move {
            info!(action = %tx.describe, to = %tx.to, "liquidity schedule sending");
            let reserve = |nonce: u64, signed: &Bytes| -> anyhow::Result<()> {
                let Some(t) = tracked else {
                    return Ok(());
                };
                let record = PendingFloor {
                    nonce: Some(nonce),
                    ..t
                }
                .record()
                .map(|r| PendingFloorRecord {
                    signed_tx: Some(alloy_primitives::hex::encode_prefixed(signed)),
                    ..r
                });
                modules
                    .save_pending_floor(record)
                    .context("could not save the pending floor change, so it was not sent")
            };
            let sent = wallet
                .send_and_wait_reserving(
                    tx.to,
                    Bytes::from(tx.data),
                    U256::ZERO,
                    RECEIPT_TIMEOUT,
                    reserve,
                )
                .await;
            let now = unix_now();
            let unresolved = sent.as_ref().err().and_then(unresolved_nonce);
            let (ok, detail, retry_after) = match sent {
                Ok(receipt) => (
                    true,
                    receipt
                        .get("transactionHash")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("confirmed")
                        .to_owned(),
                    // One fresh read before acting on the new state.
                    TICK.as_secs(),
                ),
                Err(e) => {
                    warn!(action = %tx.describe, may_land = unresolved.is_some(), error = %format!("{e:#}"), "liquidity schedule transaction failed");
                    let retry = if unresolved.is_some() {
                        RETRY_AFTER_UNKNOWN_SECS
                    } else {
                        RETRY_AFTER_FAILURE_SECS
                    };
                    (false, format!("{e:#}{explain}"), retry)
                }
            };
            let Ok(mut execution) = execution.lock() else {
                return;
            };
            let action = ActionRecord {
                at: now,
                action: tx.describe.clone(),
                ok,
                detail,
            };
            match (tracked, unresolved) {
                // No receipt, but it may still land: keep the slot (and its
                // cap); the loop settles it against the chain. `reserve`
                // already saved it, signed bytes included.
                (Some(t), Some(nonce)) => {
                    execution.in_flight =
                        Some(format!("{} (nonce {nonce}, unconfirmed)", tx.describe));
                    execution.pending = Some(PendingFloor {
                        nonce: Some(nonce),
                        sending: false,
                        ..t
                    });
                    execution.record(action);
                }
                // Landed or refused, a lower's cap stays pinned until the
                // next tick plans from a fresh read.
                (tracked, _) => {
                    execution.settle(now, retry_after, action);
                    drop(execution);
                    if tracked.is_some() {
                        forget_saved(&modules);
                    }
                }
            }
        });
    }

    /// Broadcast a change an earlier run saved, once, from its signed bytes,
    /// in case that run stopped between saving and broadcasting it. Only
    /// while the schedule runs live and the change was signed by this bot's
    /// current signer: that's the one key and configuration the panel counts
    /// as this bot sending (`Config::sends_transactions`), so its
    /// one-sender-per-key guard still holds. Otherwise, a rotated signer
    /// included, the change waits for its nonce like any other. The same
    /// bytes are the same transaction, so this can complete it but never
    /// send a second one; a node that already has it, or a used nonce, just
    /// refuses.
    async fn rebroadcast_saved(&self) {
        if !(self.live && self.modules.config.liquidity.enabled) {
            return;
        }
        let me = self.wallet.address();
        let Some(signed) = self.execution.lock().ok().and_then(|mut e| {
            let ours = e.awaiting_chain().is_some_and(|p| p.sender == me);
            e.rebroadcast.take().filter(|_| ours)
        }) else {
            return;
        };
        let Ok(raw) = alloy_primitives::hex::decode(&signed) else {
            warn!("the saved floor change's signed bytes are unreadable; not re-broadcasting");
            return;
        };
        let sent =
            tokio::time::timeout(READ_TIMEOUT, self.wallet.rpc().send_raw(&raw.into())).await;
        match sent {
            Ok(Ok(hash)) => info!(%hash, "re-broadcast the saved liquidity floor change"),
            Ok(Err(e)) => {
                info!(error = %format!("{e:#}"), "the node refused the saved floor change again; usually it already has it or its nonce is used")
            }
            Err(_) => warn!("re-broadcasting the saved floor change timed out"),
        }
    }

    /// What to check when a floor change is refused. Gas estimation runs the
    /// whole Roles path, so a missing role, a scope that does not cover this
    /// call, or a modifier that is not enabled on the Safe all land here.
    fn refusal_hint(&self, tx: &Transaction) -> String {
        match self.authority {
            Some(a) if tx.to == a.modifier => format!(
                ". Check that role {} is assigned to {} and scoped to setMinLiquidSettlement on {}",
                a.role_key,
                self.wallet.address(),
                self.vault
            ),
            _ => String::new(),
        }
    }

    fn report(
        &self,
        plan: &Plan,
        reading: Option<&Reading>,
        commitments: Option<liquidity::Commitments>,
        message: String,
        now: u64,
    ) {
        let cfg = &self.modules.config.liquidity;
        let (in_flight, actions) = self
            .execution
            .lock()
            .map(|e| (e.in_flight.clone(), e.actions.iter().cloned().collect()))
            .unwrap_or_default();
        let amount = |v: U256| Some(v.to_string());
        self.modules.liquidity_status(LiquidityStatus {
            at: now,
            market_hours: plan.market_hours,
            next_change_at: liquidity::next_change(cfg, now),
            sender: self.wallet.address().to_string(),
            operator_admin: reading.map(|r| r.operator_admin.to_string()),
            authority_target: reading
                .and_then(|r| r.authority_target)
                .map(|a| a.to_string()),
            floor: reading.and_then(|r| amount(r.vault.floor)),
            target: plan.target.and_then(amount),
            liquid: reading.and_then(|r| amount(r.vault.liquid)),
            free: reading.and_then(|r| amount(r.vault.free)),
            committed: commitments.and_then(|c| amount(c.committed)),
            // In every mode: Preview and a schedule that is off still pin
            // the cap of a lower that may land, and the panel should say so.
            quote_cap: self.modules.liquidity_quote_cap(now).and_then(amount),
            step: plan.step.kind(),
            message,
            in_flight,
            actions,
        });
    }
}

impl Engine {
    /// Settlement this bot's live quotes have promised, for the liquidity
    /// loop, which only lowers once it fits under the loop's cap. Nothing is
    /// recorded while the vault pair is not loaded yet: the loop then holds
    /// any lower.
    pub(super) fn publish_liquidity_commitments(&self, now: u64) {
        let Some(modules) = &self.modules else {
            return;
        };
        if !modules.config.liquidity.enabled {
            return;
        }
        let Some(settlement) = self
            .vault_policy
            .read()
            .ok()
            .and_then(|p| p.map(|p| p.settlement))
        else {
            return;
        };
        let Some(committed) = self.reserved_settlement(settlement, now) else {
            return;
        };
        modules.record_liquidity_commitments(committed, now);
    }

    /// Live claims paying `settlement` on both sides of every book
    /// ([`Engine::reserved_token`]). `None` when no configured pool trades it.
    fn reserved_settlement(&self, settlement: Address, now: u64) -> Option<U256> {
        self.configured
            .iter()
            .any(|b| b.debt == settlement || b.collateral == settlement)
            .then(|| self.reserved_token(settlement, now))
    }
}

/// What one book may still promise in settlement under a token-wide `cap`:
/// the cap less every live claim paying settlement on other books.
/// `token_reserved` is all claims paying the token (`U256::MAX` when one
/// can't be attributed, which leaves nothing), `own_reserved` this book's
/// share of them, which its `Exact` capacity already counts.
pub(super) fn book_allowance(cap: U256, token_reserved: U256, own_reserved: U256) -> U256 {
    cap.saturating_sub(token_reserved.saturating_sub(own_reserved))
}

/// `book` with the side that pays `settlement` capped at `cap` in total. An
/// `Exact` cap counts this pool's live claims against it (see
/// `available_capacity`), so the cap bounds what is already promised plus
/// anything new.
pub(super) fn cap_settlement_side(
    book: &CorridorBook,
    settlement: Address,
    cap: U256,
) -> CorridorBook {
    let capped = |side: Option<RfqCapacity>| {
        side.map(|c| {
            RfqCapacity::Exact(match c {
                RfqCapacity::Exact(n) => n.min(cap),
                RfqCapacity::Wallet => cap,
            })
        })
    };
    let mut out = book.clone();
    if book.debt == settlement {
        out.buy_capacity_debt = capped(book.buy_capacity_debt);
    }
    if book.collateral == settlement {
        out.sell_capacity_collateral = capped(book.sell_capacity_collateral);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const VAULT: Address = address!("00000000000000000000000000000000000000aa");
    const SAFE: Address = address!("00000000000000000000000000000000000000bb");
    const MODIFIER: Address = address!("00000000000000000000000000000000000000cc");

    fn word(v: U256) -> Option<Bytes> {
        Some(Bytes::from(v.to_be_bytes::<32>().to_vec()))
    }
    fn addr(a: Address) -> Option<Bytes> {
        word(U256::from_be_slice(a.as_slice()))
    }

    fn reading() -> Reading {
        Reading {
            vault: VaultLiquidity {
                floor: U256::ZERO,
                liquid: U256::from(10u8),
                free: U256::from(100u8),
                has_adapter: true,
                paused: false,
                close_only: false,
            },
            operator_admin: SAFE,
            authority_target: Some(SAFE),
        }
    }

    fn authority() -> Option<Authority> {
        Some(Authority {
            modifier: MODIFIER,
            role_key: B256::repeat_byte(7),
        })
    }

    fn runtime(mode: crate::modules::Mode) -> Runtime {
        runtime_with_floor(mode, 0)
    }

    fn runtime_with_floor(mode: crate::modules::Mode, floor_bps: u32) -> Runtime {
        let dir = std::env::temp_dir().join(format!("stitch-liquidity-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        runtime_in(&dir, mode, floor_bps)
    }

    fn runtime_in(dir: &std::path::Path, mode: crate::modules::Mode, floor_bps: u32) -> Runtime {
        let mut cfg = crate::modules::ModulesConfig {
            mode,
            ..Default::default()
        };
        cfg.liquidity.enabled = true;
        cfg.liquidity.market_floor_bps = floor_bps;
        cfg.liquidity.off_hours_floor_bps = floor_bps;
        cfg.liquidity.authority = Some(crate::modules::config::RolesAuthority {
            roles_modifier: MODIFIER.to_string(),
            role_key: B256::repeat_byte(7).to_string(),
        });
        Runtime::new(cfg, dir, "http://localhost/price").unwrap()
    }

    fn bot_signer() -> crate::signer::DynSigner {
        let key = crate::signer::parse_private_key(
            "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
        )
        .unwrap();
        Arc::new(crate::signer::LocalSigner::new(key))
    }

    fn bot_address() -> Address {
        bot_signer().address()
    }

    fn looper(modules: Runtime) -> Liquidity {
        // Nothing listens here: every send fails before broadcast.
        Liquidity::new(
            Wallet::new("http://127.0.0.1:1", bot_signer(), 8453),
            VAULT,
            modules,
            6,
        )
    }

    fn high_floor() -> Reading {
        Reading {
            vault: VaultLiquidity {
                floor: U256::from(500u64),
                liquid: U256::from(500u64),
                free: U256::from(1_000u64),
                ..reading().vault
            },
            ..reading()
        }
    }

    #[tokio::test]
    async fn a_lower_caps_quotes_first_and_holds_the_cap_while_it_is_sent() {
        let modules = runtime(crate::modules::Mode::Live);
        let me = looper(modules.clone());
        assert_eq!(me.authority, authority());

        // No engine reading yet: cap at the new floor, send nothing.
        me.tick(Some(high_floor()), None).await;
        let now = unix_now();
        assert_eq!(modules.scheduled_quote_cap(now), Some(U256::ZERO));
        let status = modules.status_snapshot().liquidity.unwrap();
        assert_eq!(status.step, crate::modules::liquidity::StepKind::Drain);
        assert!(me.execution.lock().unwrap().in_flight.is_none());

        // The engine measured nothing live under that cap: lower.
        modules.record_liquidity_commitments(U256::ZERO, now);
        me.tick(Some(high_floor()), None).await;
        {
            let execution = me.execution.lock().unwrap();
            if execution.in_flight.is_some() {
                assert_eq!(execution.pending.map(|l| l.target), Some(U256::ZERO));
            }
        }
        // The send fails (no node). The cap was pinned before it was signed
        // and stays until the next tick plans from a fresh read.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if me.execution.lock().unwrap().actions.len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the failed lower is recorded");
        let execution = me.execution.lock().unwrap();
        assert!(execution.pending.is_none() && execution.in_flight.is_none());
        assert!(!execution.actions[0].ok);
        assert_eq!(execution.actions[0].action, "Lower floor to 0");
        assert!(execution.retry_at > now);
        assert_eq!(modules.scheduled_quote_cap(unix_now()), Some(U256::ZERO));
    }

    /// A lower whose send gave up at nonce 5, signed for a floor of 100.
    fn unconfirmed(me: &Liquidity) {
        let mut execution = me.execution.lock().unwrap();
        execution.in_flight = Some("Lower floor to 0.0001 (nonce 5, unconfirmed)".into());
        execution.pending = Some(PendingFloor {
            sender: bot_address(),
            lowers: true,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(100u64),
            nonce: Some(5),
            sending: false,
        });
    }

    #[tokio::test]
    async fn an_unconfirmed_lower_keeps_its_own_cap_until_the_chain_settles_it() {
        // Half of free settlement stays liquid, so a deposit raises the target.
        let modules = runtime_with_floor(crate::modules::Mode::Live, 5_000);
        let me = looper(modules.clone());
        unconfirmed(&me);
        // A deposit doubles free settlement: the schedule now wants a higher
        // floor than the one that may still land, and reads fail meanwhile.
        let deposit = Reading {
            vault: VaultLiquidity {
                free: U256::from(2_000u64),
                ..high_floor().vault
            },
            ..high_floor()
        };
        for (reading, n) in [
            (Some(deposit), Some(5)),
            (None, Some(5)),
            (Some(deposit), None),
        ] {
            me.tick(reading, n).await;
            assert_eq!(
                modules.scheduled_quote_cap(u64::MAX - 1),
                Some(U256::from(100u64)),
                "pinned at the signed target and never lapsing"
            );
            assert!(me.execution.lock().unwrap().in_flight.is_some());
        }
        let status = modules.status_snapshot().liquidity.unwrap();
        assert!(status.message.contains("nonce 5"), "{}", status.message);

        // The nonce is used up and the floor never moved: it cannot land.
        me.tick(Some(deposit), Some(6)).await;
        let execution = me.execution.lock().unwrap();
        assert!(execution.pending.is_none() && execution.in_flight.is_none());
        let last = execution.actions.back().unwrap();
        assert!(!last.ok && last.detail.contains("nonce 5"), "{last:?}");
    }

    #[tokio::test]
    async fn a_lower_from_a_rotated_signer_waits_for_that_accounts_nonce() {
        let modules = runtime(crate::modules::Mode::Live);
        let me = looper(modules.clone());
        me.execution.lock().unwrap().pending = Some(PendingFloor {
            sender: SAFE,
            lowers: true,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(100u64),
            nonce: Some(5),
            sending: false,
        });
        me.execution.lock().unwrap().in_flight = Some("Lower floor".into());
        me.tick(Some(high_floor()), Some(5)).await;
        let status = modules.status_snapshot().liquidity.unwrap();
        assert!(
            status.message.contains("previous signer"),
            "{}",
            status.message
        );
        assert!(
            me.execution.lock().unwrap().actions.is_empty(),
            "nothing sent"
        );
        assert!(modules
            .scheduled_quote_cap(u64::MAX - 1)
            .is_some_and(|cap| cap <= U256::from(100u64)));
        // Settled only once the old account's nonce is used.
        me.tick(Some(high_floor()), Some(6)).await;
        assert!(me.execution.lock().unwrap().pending.is_none());
    }

    #[tokio::test]
    async fn a_restart_resumes_a_pending_lower_with_its_cap_in_force() {
        let dir = std::env::temp_dir().join(format!("stitch-liquidity-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let before = runtime_in(&dir, crate::modules::Mode::Live, 0);
        let lower = PendingFloor {
            sender: bot_address(),
            lowers: true,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(100u64),
            nonce: Some(3),
            sending: true,
        };
        before.save_pending_floor(lower.record()).unwrap();

        // Pinned at construction, before any loop or quote runs.
        let after = runtime_in(&dir, crate::modules::Mode::Live, 0);
        assert_eq!(
            after.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(100u64))
        );
        let me = looper(after.clone());
        let resumed = me.execution.lock().unwrap().awaiting_chain();
        assert_eq!(
            resumed,
            Some(PendingFloor::resumed(lower.record().unwrap()))
        );
        assert!(me.execution.lock().unwrap().in_flight.is_some());

        // The floor shows it landed: settled, and the saved copy is gone.
        let landed = Reading {
            vault: VaultLiquidity {
                floor: U256::from(100u64),
                ..high_floor().vault
            },
            ..high_floor()
        };
        me.tick(Some(landed), Some(9)).await;
        assert!(me.execution.lock().unwrap().pending.is_none());
        let next = runtime_in(&dir, crate::modules::Mode::Live, 0);
        assert_eq!(next.saved_pending_floor(), None);
        // Only the starting cap every live schedule restarts with.
        assert_eq!(next.scheduled_quote_cap(u64::MAX - 1), Some(U256::ZERO));
    }

    /// A restart broadcasts the saved signed bytes once, in case the run that
    /// saved them stopped before broadcasting, and only while the change's
    /// nonce is still unused. A record from before signed bytes were saved
    /// still loads; it just has nothing to re-broadcast.
    #[tokio::test]
    async fn a_restart_broadcasts_the_saved_signed_change_once() {
        use axum::{routing::post, Json, Router};
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = sent.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(req): Json<serde_json::Value>| {
                let seen = seen.clone();
                async move {
                    if req["method"] == "eth_sendRawTransaction" {
                        seen.lock()
                            .unwrap()
                            .push(req["params"][0].as_str().unwrap().to_owned());
                    }
                    Json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 1, "result": format!("0x{}", "11".repeat(32))
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let dir = std::env::temp_dir().join(format!("stitch-liquidity-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let lower = PendingFloor {
            sender: bot_address(),
            lowers: true,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(100u64),
            nonce: Some(5),
            sending: false,
        };
        let saved = PendingFloorRecord {
            signed_tx: Some("0xdeadbeef".into()),
            ..lower.record().unwrap()
        };
        runtime_in(&dir, crate::modules::Mode::Live, 0)
            .save_pending_floor(Some(saved))
            .unwrap();
        // The schedule holds the floor at 500, so only the saved lower caps.
        let modules = runtime_in(&dir, crate::modules::Mode::Live, 5_000);
        let me = Liquidity::new(
            Wallet::new(url.clone(), bot_signer(), 8453),
            VAULT,
            modules.clone(),
            6,
        );
        me.tick(Some(high_floor()), Some(5)).await;
        me.tick(Some(high_floor()), Some(5)).await;
        assert_eq!(*sent.lock().unwrap(), vec!["0xdeadbeef".to_owned()]);
        // Preview isn't counted as sending from this key, so it doesn't.
        let preview = Liquidity::new(
            Wallet::new(url.clone(), bot_signer(), 8453),
            VAULT,
            runtime_in(&dir, crate::modules::Mode::Shadow, 5_000),
            6,
        );
        preview.tick(Some(high_floor()), Some(5)).await;
        assert_eq!(sent.lock().unwrap().len(), 1);
        // Nor does a bot whose signer was rotated: those bytes are from a key
        // the panel no longer reserves for it.
        runtime_in(&dir, crate::modules::Mode::Live, 0)
            .save_pending_floor(Some(PendingFloorRecord {
                sender: SAFE,
                signed_tx: Some("0xdeadbeef".into()),
                ..lower.record().unwrap()
            }))
            .unwrap();
        let rotated = Liquidity::new(
            Wallet::new(url, bot_signer(), 8453),
            VAULT,
            runtime_in(&dir, crate::modules::Mode::Live, 5_000),
            6,
        );
        rotated.tick(Some(high_floor()), Some(5)).await;
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(100u64))
        );

        let older: PendingFloorRecord = serde_json::from_str(
            &serde_json::to_string(&lower.record().unwrap())
                .unwrap()
                .replace(",\"signed_tx\":null", ""),
        )
        .unwrap();
        assert_eq!(older.signed_tx, None);
    }

    #[tokio::test]
    async fn turning_the_schedule_off_still_settles_a_saved_lower() {
        let dir = std::env::temp_dir().join(format!("stitch-liquidity-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let lower = PendingFloor {
            sender: bot_address(),
            lowers: true,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(100u64),
            nonce: Some(5),
            sending: false,
        };
        runtime_in(&dir, crate::modules::Mode::Live, 0)
            .save_pending_floor(lower.record())
            .unwrap();
        let mut cfg = crate::modules::ModulesConfig {
            mode: crate::modules::Mode::Live,
            ..Default::default()
        };
        cfg.liquidity.enabled = false;
        let modules = Runtime::new(cfg, &dir, "http://localhost/price").unwrap();
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(100u64)),
            "pinned before anything else runs"
        );
        let me = looper(modules.clone());
        me.tick(Some(high_floor()), Some(5)).await;
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(100u64))
        );
        assert!(!me.finished());
        let status = modules.status_snapshot().liquidity.unwrap();
        assert!(status.message.contains("nonce 5"), "{}", status.message);

        me.tick(Some(high_floor()), Some(6)).await;
        assert!(me.finished());
        // The read that settled it clears the cap, since the schedule is off.
        assert_eq!(modules.scheduled_quote_cap(0), None);
    }

    #[tokio::test]
    async fn preview_keeps_the_cap_of_a_lower_that_may_still_land() {
        let modules = runtime(crate::modules::Mode::Shadow);
        let me = looper(modules.clone());
        unconfirmed(&me);
        me.tick(Some(high_floor()), Some(5)).await;
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(100u64))
        );
        me.tick(Some(high_floor()), Some(6)).await;
        assert_eq!(modules.scheduled_quote_cap(0), None);
    }

    #[tokio::test]
    async fn an_unconfirmed_lower_that_lands_frees_the_slot() {
        let modules = runtime(crate::modules::Mode::Live);
        let me = looper(modules.clone());
        unconfirmed(&me);
        let landed = Reading {
            vault: VaultLiquidity {
                floor: U256::from(100u64),
                ..high_floor().vault
            },
            ..high_floor()
        };
        me.tick(Some(landed), Some(6)).await;
        let execution = me.execution.lock().unwrap();
        assert!(execution.pending.is_none() && execution.in_flight.is_none());
        assert!(execution.actions.back().unwrap().ok);
    }

    #[test]
    fn a_lower_is_settled_only_by_the_chain() {
        let lower = PendingFloor {
            sender: bot_address(),
            lowers: true,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(100u64),
            nonce: Some(5),
            sending: false,
        };
        let (target, above) = (U256::from(100u64), U256::from(500u64));
        // The floor reads at the target, but nonce 5 can still execute: an
        // admin could raise the floor and this lower land under quotes.
        assert_eq!(floor_outcome(&lower, target, 5), FloorOutcome::Pending);
        // However long it is gone from the pool: peers may still hold it.
        assert_eq!(floor_outcome(&lower, above, 5), FloorOutcome::Pending);
        assert_eq!(floor_outcome(&lower, target, 6), FloorOutcome::Landed);
        assert_eq!(floor_outcome(&lower, above, 6), FloorOutcome::CannotLand);
        // A raise counts as landed once the floor is at or above it.
        let raise = PendingFloor {
            lowers: false,
            target: U256::from(600u64),
            ..lower
        };
        assert_eq!(
            floor_outcome(&raise, U256::from(700u64), 6),
            FloorOutcome::Landed
        );
        assert_eq!(floor_outcome(&raise, above, 6), FloorOutcome::CannotLand);
        // A lower that never got a nonce was never signed, and has nothing to save.
        let unsigned = PendingFloor {
            nonce: None,
            ..lower
        };
        assert_eq!(floor_outcome(&unsigned, above, 0), FloorOutcome::CannotLand);
        assert_eq!(unsigned.record(), None);
    }

    /// A change stuck at its nonce is never sent again or cancelled: the
    /// slot and the cap hold, and the panel says which nonce to replace.
    #[tokio::test]
    async fn a_stuck_change_is_never_resent_or_cancelled() {
        // The schedule holds the floor at 500, so only the stuck lower caps.
        let modules = runtime_with_floor(crate::modules::Mode::Live, 5_000);
        // A fresh, funded balance read, so the status shows only the pinned cap.
        *modules.balances.write().unwrap() = Some(crate::modules::runtime::Balances {
            settlement: U256::from(1_000u64),
            corridor: U256::ZERO,
            liquid: Some(crate::modules::runtime::LiquidReading {
                liquid: U256::from(500u64),
                floor: U256::from(500u64),
            }),
            at: unix_now(),
        });
        let me = looper(modules.clone());
        unconfirmed(&me);
        for _ in 0..3 {
            me.tick(Some(high_floor()), Some(5)).await;
        }
        let execution = me.execution.lock().unwrap();
        assert!(execution.actions.is_empty(), "nothing sent");
        assert!(execution.in_flight.is_some());
        assert_eq!(execution.pending.and_then(|p| p.nonce), Some(5));
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(100u64))
        );
        let status = modules.status_snapshot().liquidity.unwrap();
        assert!(
            status.message.contains("replace that nonce"),
            "{}",
            status.message
        );
        assert_eq!(status.quote_cap.as_deref(), Some("100"));
    }

    #[tokio::test]
    async fn a_raise_that_may_still_land_holds_the_slot() {
        let modules = runtime_with_floor(crate::modules::Mode::Live, 5_000);
        let me = looper(modules.clone());
        me.execution.lock().unwrap().pending = Some(PendingFloor {
            sender: bot_address(),
            lowers: false,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(900u64),
            nonce: Some(5),
            sending: false,
        });
        me.execution.lock().unwrap().in_flight = Some("Raise floor".into());
        // The schedule now wants a smaller raise: it must not go out while
        // the first one can still execute after it.
        let low = Reading {
            vault: VaultLiquidity {
                floor: U256::ZERO,
                liquid: U256::from(1_000u64),
                ..high_floor().vault
            },
            ..high_floor()
        };
        me.tick(Some(low), Some(5)).await;
        let status = modules.status_snapshot().liquidity.unwrap();
        assert!(status.message.contains("floor raise"), "{}", status.message);
        assert!(
            me.execution.lock().unwrap().actions.is_empty(),
            "nothing sent"
        );
        assert_eq!(
            modules.scheduled_quote_cap(0),
            None,
            "a funded raise pins no cap"
        );
        // Its nonce used and the floor at its target: settled, slot free.
        let raised = Reading {
            vault: VaultLiquidity {
                floor: U256::from(900u64),
                ..low.vault
            },
            ..low
        };
        me.tick(Some(raised), Some(6)).await;
        let execution = me.execution.lock().unwrap();
        assert!(execution.pending.is_none());
        assert!(execution.actions.back().unwrap().ok);
    }

    /// Whatever the schedule wants now, a raise that may still land caps
    /// quotes at what's liquid while that's short of its target, on every
    /// balance read: once it lands, the venue admits bundles up to the target.
    #[tokio::test]
    async fn a_raise_that_may_still_land_caps_quotes_at_what_is_liquid() {
        // Off-hours: the schedule now wants no floor at all.
        let modules = runtime(crate::modules::Mode::Live);
        let read = |liquid: u64| {
            *modules.balances.write().unwrap() = Some(crate::modules::runtime::Balances {
                settlement: U256::from(1_000u64),
                corridor: U256::ZERO,
                liquid: Some(crate::modules::runtime::LiquidReading {
                    liquid: U256::from(liquid),
                    floor: U256::ZERO,
                }),
                at: unix_now(),
            });
        };
        let me = looper(modules.clone());
        me.execution.lock().unwrap().pending = Some(PendingFloor {
            sender: bot_address(),
            lowers: false,
            chain_id: 8453,
            vault: VAULT,
            target: U256::from(900u64),
            nonce: Some(5),
            sending: false,
        });
        me.execution.lock().unwrap().in_flight = Some("Raise floor".into());
        let funded = Reading {
            vault: VaultLiquidity {
                floor: U256::ZERO,
                liquid: U256::from(950u64),
                ..high_floor().vault
            },
            ..high_floor()
        };
        read(950);
        me.tick(Some(funded), Some(5)).await;
        assert_eq!(modules.liquidity_quote_cap(unix_now()), None, "funded");
        // A fill drains liquid between ticks: the next quote is capped, with
        // the on-chain floor still at zero.
        read(300);
        assert_eq!(
            modules.liquidity_quote_cap(unix_now()),
            Some(U256::from(300u64))
        );
        // Its nonce used without it landing: settled, and nothing is pending.
        me.tick(Some(funded), Some(6)).await;
        assert!(me.execution.lock().unwrap().pending.is_none());
        assert_eq!(modules.liquidity_quote_cap(unix_now()), None);
    }

    #[tokio::test]
    async fn a_pending_lower_caps_at_the_lesser_of_its_target_and_the_plan() {
        let modules = runtime(crate::modules::Mode::Live);
        let pinned = Some(U256::from(100u64));
        for (cap, want) in [(Some(300u64), 100u64), (Some(40), 40), (None, 100)] {
            publish_cap(&modules, cap.map(U256::from), 0, pinned, true);
            assert_eq!(
                modules.scheduled_quote_cap(u64::MAX - 1),
                Some(U256::from(want)),
                "plan cap {cap:?}"
            );
        }
        publish_cap(&modules, None, 0, None, true);
        assert_eq!(modules.scheduled_quote_cap(0), None);
    }

    #[tokio::test]
    async fn a_live_schedule_starts_at_a_zero_cap_until_a_trusted_read() {
        // Half of free settlement liquid, so the first trusted read sets a
        // real cap rather than happening to land on zero.
        let modules = runtime_with_floor(crate::modules::Mode::Live, 5_000);
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::ZERO),
            "from construction, with no expiry"
        );
        let me = looper(modules.clone());
        // No vault read: unchanged.
        me.tick(None, None).await;
        assert_eq!(modules.scheduled_quote_cap(u64::MAX - 1), Some(U256::ZERO));
        // Floor 500 funded by 500 liquid on 1,000 free: nothing to cap.
        me.tick(Some(high_floor()), None).await;
        assert_eq!(modules.scheduled_quote_cap(0), None);
        // Preview and a schedule that is off start uncapped.
        assert_eq!(
            runtime(crate::modules::Mode::Shadow).scheduled_quote_cap(0),
            None
        );
    }

    #[tokio::test]
    async fn failed_reads_keep_the_last_cap_without_expiry() {
        let modules = runtime_with_floor(crate::modules::Mode::Live, 5_000);
        let me = looper(modules.clone());
        // Fills spent liquid under the floor: capped at what is liquid.
        let drained = Reading {
            vault: VaultLiquidity {
                liquid: U256::from(120u64),
                ..high_floor().vault
            },
            ..high_floor()
        };
        me.tick(Some(drained), None).await;
        let now = unix_now();
        assert_eq!(modules.scheduled_quote_cap(now), Some(U256::from(120u64)));
        assert_eq!(
            modules.scheduled_quote_cap(now + LIQUIDITY_CAP_TTL_SECS + 1),
            None,
            "a trusted cap carries the usual TTL"
        );
        // Reads fail: the same cap holds, and no longer lapses.
        me.tick(None, None).await;
        assert_eq!(
            modules.scheduled_quote_cap(u64::MAX - 1),
            Some(U256::from(120u64))
        );
        let status = modules.status_snapshot().liquidity.unwrap();
        assert_eq!(status.step, crate::modules::liquidity::StepKind::Wait);
        // A trusted read that shows the vault paused: the floor does not
        // apply, so the cap goes.
        let paused = Reading {
            vault: VaultLiquidity {
                paused: true,
                ..drained.vault
            },
            ..drained
        };
        me.tick(Some(paused), None).await;
        assert_eq!(modules.scheduled_quote_cap(0), None);
    }

    #[tokio::test]
    async fn preview_records_the_plan_without_capping_or_sending() {
        let modules = runtime(crate::modules::Mode::Shadow);
        let me = looper(modules.clone());
        // As if a cap at the new floor had been in force when the engine measured.
        modules.set_liquidity_cap(Some(U256::ZERO), u64::MAX);
        modules.record_liquidity_commitments(U256::ZERO, unix_now());
        me.tick(Some(high_floor()), None).await;
        assert_eq!(modules.scheduled_quote_cap(unix_now()), None);
        assert!(me.execution.lock().unwrap().in_flight.is_none());
        let status = modules.status_snapshot().liquidity.unwrap();
        assert_eq!(status.step, crate::modules::liquidity::StepKind::LowerFloor);
        assert!(
            status.message.starts_with("Preview only"),
            "{}",
            status.message
        );
        assert_eq!(status.quote_cap, None);
        assert_eq!(status.floor.as_deref(), Some("500"));
    }

    #[test]
    fn reading_decodes_every_view_in_order() {
        let results = vec![
            word(U256::from(5u8)),
            word(U256::from(6u8)),
            word(U256::from(7u8)),
            addr(MODIFIER),
            word(U256::ZERO),
            word(U256::from(1u8)),
            addr(SAFE),
            addr(SAFE),
        ];
        let r = decode_reading(&results).unwrap();
        assert_eq!(r.vault.floor, U256::from(5u8));
        assert_eq!(r.vault.liquid, U256::from(6u8));
        assert_eq!(r.vault.free, U256::from(7u8));
        assert!(r.vault.has_adapter && !r.vault.paused && r.vault.close_only);
        assert_eq!(r.operator_admin, SAFE);
        assert_eq!(r.authority_target, Some(SAFE));
        assert_eq!(reading_calls(VAULT, Some(MODIFIER)).len(), results.len());
    }

    #[test]
    fn a_missing_vault_view_is_no_reading_but_a_missing_target_is_not() {
        let mut results = vec![
            word(U256::from(5u8)),
            word(U256::from(6u8)),
            word(U256::from(7u8)),
            addr(MODIFIER),
            word(U256::ZERO),
            word(U256::ZERO),
            addr(SAFE),
        ];
        assert_eq!(decode_reading(&results).unwrap().authority_target, None);
        results[4] = None;
        assert_eq!(decode_reading(&results), None, "paused unknown");
    }

    #[test]
    fn floor_changes_go_through_the_roles_modifier() {
        let tx = transaction(
            Step::RaiseFloor(U256::from(1_500_000u64)),
            VAULT,
            &reading(),
            authority(),
            6,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tx.to, MODIFIER);
        assert_eq!(
            tx.data,
            encode_exec_transaction_with_role(
                VAULT,
                &encode_set_min_liquid_settlement(U256::from(1_500_000u64)),
                B256::repeat_byte(7)
            )
        );
        assert_eq!(tx.describe, "Raise floor to 1.5");
        assert_eq!(
            tx.floor_change,
            Some(FloorChange {
                target: U256::from(1_500_000u64),
                lowers: false
            }),
            "a raise is tracked too"
        );
        let lower = transaction(
            Step::LowerFloor(U256::ZERO),
            VAULT,
            &reading(),
            authority(),
            6,
        )
        .unwrap()
        .unwrap();
        assert_eq!(lower.floor_change.map(|c| c.target), Some(U256::ZERO));
    }

    #[test]
    fn recalls_go_straight_to_the_vault() {
        let tx = transaction(Step::Recall(U256::from(42u8)), VAULT, &reading(), None, 0)
            .unwrap()
            .unwrap();
        assert_eq!(tx.to, VAULT);
        assert_eq!(tx.data, encode_prepare_settlement(U256::from(42u8)));
    }

    #[test]
    fn floor_changes_refuse_without_a_matching_authority() {
        let step = Step::RaiseFloor(U256::from(1u8));
        assert!(transaction(step, VAULT, &reading(), None, 0).is_err());
        let elsewhere = Reading {
            authority_target: Some(MODIFIER),
            ..reading()
        };
        let err = transaction(step, VAULT, &elsewhere, authority(), 0).unwrap_err();
        assert!(err.contains("operator admin"), "{err}");
        let unread = Reading {
            authority_target: None,
            ..reading()
        };
        assert!(transaction(step, VAULT, &unread, authority(), 0).is_err());
    }

    #[test]
    fn steps_without_a_transaction_send_nothing() {
        for step in [Step::Hold, Step::Drain, Step::Wait] {
            assert_eq!(
                transaction(step, VAULT, &reading(), authority(), 6),
                Ok(None)
            );
        }
    }

    #[test]
    fn sibling_books_share_one_settlement_cap() {
        let cap = U256::from(100u64);
        // Alone: the whole cap, its own claims counted by its Exact capacity.
        assert_eq!(
            book_allowance(cap, U256::from(30u64), U256::from(30u64)),
            cap
        );
        // A sibling holds 70: this book may promise only the remaining 30,
        // however much of it it holds itself.
        assert_eq!(
            book_allowance(cap, U256::from(100u64), U256::from(30u64)),
            U256::from(30u64)
        );
        assert_eq!(
            book_allowance(cap, U256::from(70u64), U256::ZERO),
            U256::from(30u64)
        );
        // Siblings already past the cap, or an unattributable claim: nothing.
        assert_eq!(
            book_allowance(cap, U256::from(150u64), U256::ZERO),
            U256::ZERO
        );
        assert_eq!(book_allowance(cap, U256::MAX, U256::from(5u64)), U256::ZERO);
    }

    #[test]
    fn the_cap_bounds_only_the_side_paying_settlement() {
        let settlement = address!("0000000000000000000000000000000000000002");
        let book = CorridorBook {
            slug: "c".into(),
            collateral: address!("0000000000000000000000000000000000000001"),
            debt: settlement,
            collateral_decimals: 6,
            debt_decimals: 6,
            buy_spread: None,
            sell_spread: None,
            buy_capacity_debt: Some(RfqCapacity::Wallet),
            sell_capacity_collateral: Some(RfqCapacity::Wallet),
            feed_url: String::new(),
            staleness_secs: 10,
        };
        let capped = cap_settlement_side(&book, settlement, U256::from(9u8));
        assert_eq!(
            capped.buy_capacity_debt,
            Some(RfqCapacity::Exact(U256::from(9u8)))
        );
        assert_eq!(capped.sell_capacity_collateral, Some(RfqCapacity::Wallet));
        let tighter = CorridorBook {
            buy_capacity_debt: Some(RfqCapacity::Exact(U256::from(4u8))),
            ..book.clone()
        };
        assert_eq!(
            cap_settlement_side(&tighter, settlement, U256::from(9u8)).buy_capacity_debt,
            Some(RfqCapacity::Exact(U256::from(4u8)))
        );
        let off = CorridorBook {
            buy_capacity_debt: None,
            ..book
        };
        assert_eq!(
            cap_settlement_side(&off, settlement, U256::ZERO).buy_capacity_debt,
            None,
            "a side that is off stays off"
        );
    }
}
