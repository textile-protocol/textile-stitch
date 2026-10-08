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
pub const MAX_DECISIONS: usize = 200;
/// Most samples the window keeps: one per second over the longest allowed window.
const MAX_HISTORY: usize = 3601;

#[derive(Clone, Copy, Debug)]
pub struct Balances {
    pub settlement: U256,
    pub corridor: U256,
    pub at: u64,
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
            let write = tokio::task::spawn_blocking(move || -> Result<()> {
                crate::setup::write_toml_atomic(&path, &serde_json::to_string(&saved)?)
            })
            .await;
            if !matches!(write, Ok(Ok(()))) {
                tracing::error!("module price history could not be saved");
            }
        }
    });
    writer
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
        let status = Status {
            version: VERSION,
            config: config.clone(),
            at: crate::time::unix_now(),
            decisions: vec![],
            rebalance_status: "Waiting for fresh vault data".into(),
            next_attempt_at,
            quote_status: Some(QuoteStatus {
                at: crate::time::unix_now(),
                state: QuoteState::WaitingForSession,
                message: "Waiting for an authenticated RFQ session".into(),
            }),
        };
        // Write an initial snapshot synchronously: unwritable storage must fail startup.
        crate::setup::write_toml_atomic(&dir.join(STATUS_FILE), &serde_json::to_string(&status)?)?;
        // A slow disk may coalesce snapshots, but must keep the latest state
        // (especially disconnects) rather than dropping it behind an old write.
        let (writer, mut receiver) = tokio::sync::watch::channel(status.clone());
        let status_path = dir.join(STATUS_FILE);
        tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let snapshot = receiver.borrow_and_update().clone();
                let path = status_path.clone();
                let write = tokio::task::spawn_blocking(move || -> Result<()> {
                    crate::setup::write_toml_atomic(&path, &serde_json::to_string(&snapshot)?)
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
        })
    }
    pub fn decide(&self, context: &Context) -> Decision {
        let mut state = self.state.lock().expect("module state poisoned");
        let history = &mut state.history;
        let sampled = context.price_at <= context.now
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
        crate::setup::write_toml_atomic(
            &self.dir.join(ATTEMPTS_FILE),
            &serde_json::to_string(&Attempt {
                version: VERSION,
                next_attempt_at: next,
            })?,
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
}
