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
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: u32,
    pub config: ModulesConfig,
    pub at: u64,
    pub decisions: Vec<Observation>,
    pub rebalance_status: String,
    pub next_attempt_at: u64,
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
    writer: tokio::sync::mpsc::Sender<Status>,
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
        };
        // Write an initial snapshot synchronously: unwritable storage must fail startup.
        crate::setup::write_toml_atomic(&dir.join(STATUS_FILE), &serde_json::to_string(&status)?)?;
        let (writer, mut receiver) = tokio::sync::mpsc::channel::<Status>(1);
        let status_path = dir.join(STATUS_FILE);
        tokio::spawn(async move {
            while let Some(snapshot) = receiver.recv().await {
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
            });
            if state.status.decisions.len() > MAX_DECISIONS {
                state.status.decisions.remove(0);
            }
            state.status.at = context.now;
            let _ = self.writer.try_send(state.status.clone());
        }
        decision
    }
    pub fn status(&self, message: impl Into<String>) {
        let mut s = self.state.lock().expect("module state poisoned");
        s.status.rebalance_status = message.into();
        s.status.at = crate::time::unix_now();
        let _ = self.writer.try_send(s.status.clone());
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
