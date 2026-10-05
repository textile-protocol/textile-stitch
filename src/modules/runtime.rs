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
pub const MAX_DECISIONS: usize = 200;

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
    dir: PathBuf,
    writer: tokio::sync::watch::Sender<Status>,
}
impl Runtime {
    pub fn new(config: ModulesConfig, dir: &Path) -> Result<Self> {
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
        Ok(Self {
            config,
            balances: Arc::new(RwLock::new(None)),
            state: Arc::new(Mutex::new(State {
                history: vec![],
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
        if context.price_at <= context.now
            && context.price.is_finite()
            && context.price > 0.0
            && history
                .last()
                .is_none_or(|p| context.price_at > p.timestamp)
        {
            history.push(PricePoint {
                timestamp: context.price_at,
                price: context.price,
            });
        }
        history.retain(|p| {
            p.timestamp <= context.now
                && context.now - p.timestamp <= self.config.spreads.window_secs
        });
        if history.len() > 3601 {
            history.drain(..history.len() - 3601);
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
