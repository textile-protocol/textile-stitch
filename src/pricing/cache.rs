// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! One live reference per feed URL, shared by the RFQ and tick loops.
//!
//! Admission updates the history, quote and hold under one lock. A jump
//! observed by either loop removes the price for every reader immediately;
//! reading a cached price never counts as another feed observation. Network
//! fetches happen outside the lock. The saved anchor is loaded once per URL
//! and recorded by the same owner, so the loops cannot establish independent
//! trusted prices even when their requests finish in a different order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::anchor::Anchor;
use super::feed::Quote;
use super::jump::{observe, Hold, JumpGuard, Print, PrintHistory, Verdict};
use super::tick::usable;

/// One admission policy for every producer. Freshness is the widest window
/// of any consumer on these feeds; readers still check their own windows.
/// Using a producer's tighter window here could cache an unguarded "stale"
/// print that a more permissive consumer would then accept.
#[derive(Debug, Clone, Copy)]
pub struct FeedPolicy {
    pub jump: JumpGuard,
    pub staleness_secs: u64,
}

#[derive(Default)]
struct FeedState {
    anchor: Option<Anchor>,
    history: PrintHistory,
    /// Start of the poll that supplied the current cold-start candidate.
    startup_poll: Option<Instant>,
    quote: Option<Quote>,
    hold: Option<Hold>,
}

/// Cloneable handle to the process's guarded prices. An unusable quote is
/// retained so consumers can distinguish degraded from stale; it never
/// moves the jump history. A held print or failed fetch leaves no quote.
#[derive(Clone)]
pub struct PriceCache {
    feeds: Arc<Mutex<HashMap<String, FeedState>>>,
    config_dir: Option<PathBuf>,
    policy: FeedPolicy,
}

impl PriceCache {
    /// `None` keeps anchors in memory only (including dry runs).
    pub fn new(config_dir: Option<&Path>, policy: FeedPolicy) -> Self {
        Self {
            feeds: Arc::new(Mutex::new(HashMap::new())),
            config_dir: config_dir.map(Path::to_path_buf),
            policy,
        }
    }

    pub fn get(&self, url: &str) -> Option<Quote> {
        self.feeds.lock().ok()?.get(url)?.quote.clone()
    }

    pub fn hold(&self, url: &str) -> Option<Hold> {
        self.feeds.lock().ok()?.get(url)?.hold
    }

    /// A failed fetch invalidates the price without forgetting the trusted
    /// baseline or pending candidate. Readers report the outage, not a jump.
    pub fn invalidate(&self, url: &str) {
        if let Ok(mut feeds) = self.feeds.lock() {
            if let Some(state) = feeds.get_mut(url) {
                state.quote = None;
                state.hold = None;
            }
        }
    }

    /// Admit one actual fetch. Call once per response, never on cache reuse.
    /// Capture `started` before fetching: concurrent requests must not confirm
    /// a cold-start candidate, even if one response arrives much later.
    /// Returns the current history for diagnostics; callers do not own it.
    pub fn admit(&self, started: Instant, url: &str, quote: Quote, now: u64) -> PrintHistory {
        let policy = &self.policy;
        let Ok(mut feeds) = self.feeds.lock() else {
            return PrintHistory::default();
        };
        let state = feeds.entry(url.to_string()).or_default();
        if state.anchor.is_none() {
            let (anchor, history) = Anchor::load(self.config_dir.as_deref(), url);
            state.anchor = Some(anchor);
            state.history = history;
        }
        if usable(&quote, now, policy.staleness_secs).is_err() {
            state.quote = Some(quote);
            state.hold = None;
            return state.history;
        }
        let print = Print {
            price: quote.price,
            observed_at: quote.timestamp,
        };
        let (mut history, mut verdict) = observe(state.history, print, &policy.jump);
        if state.history.trusted.is_none() {
            // Both producers poll immediately at startup. Only a request begun
            // at least one second after the candidate's poll may confirm it.
            // Use monotonic request time, not response time or the feed's clock:
            // cron-sampled feeds may legitimately return the same sample again.
            let later_poll = state.startup_poll.is_some_and(|first| {
                started.saturating_duration_since(first) >= std::time::Duration::from_secs(1)
            });
            if history.trusted.is_some() && !later_poll {
                history = state.history;
                verdict = Verdict::Held(Hold::WarmingUp);
            } else if history.candidate != state.history.candidate {
                // A replaced candidate needs its own confirmation. A delayed
                // older response cannot wind the confirmation clock backwards.
                state.startup_poll = Some(state.startup_poll.map_or(started, |p| p.max(started)));
            }
        }
        if history.trusted.is_some() {
            state.startup_poll = None;
        }
        state.history = history;
        state
            .anchor
            .as_mut()
            .expect("anchor loaded")
            .record(&history, &policy.jump);
        match verdict {
            Verdict::Trusted | Verdict::Reanchored { .. } => {
                log_release(url, state.hold, &verdict, print);
                state.hold = None;
                state.quote = Some(quote);
            }
            // A lagging, out-of-line replica cannot replace the newer state
            // or release a hold seen by the other loop.
            Verdict::Held(Hold::Regressed) => {
                debug!(feed = %url, price = print.price, "older reference price out of line; ignored");
            }
            Verdict::Held(hold) => {
                log_hold(url, state.hold, &hold, &policy.jump);
                state.quote = None;
                state.hold = Some(hold);
            }
        }
        history
    }

    /// Inject a quote for consumer tests that do not exercise admission.
    #[cfg(test)]
    pub fn set(&self, url: String, quote: Quote) {
        self.feeds.lock().unwrap().entry(url).or_default().quote = Some(quote);
    }
}

#[cfg(test)]
impl Default for PriceCache {
    fn default() -> Self {
        Self::new(
            None,
            FeedPolicy {
                jump: crate::config::PriceGuardConfig::default().jump(),
                staleness_secs: crate::config::RFQ_MAX_STALENESS_SECS,
            },
        )
    }
}

fn log_hold(url: &str, before: Option<Hold>, hold: &Hold, jump: &JumpGuard) {
    let changed = before.as_ref().map(std::mem::discriminant) != Some(std::mem::discriminant(hold));
    let message = hold.describe(jump.settle_secs);
    match hold {
        Hold::WarmingUp if changed => info!(feed = %url, "{message}"),
        Hold::WarmingUp => {}
        _ if changed => warn!(feed = %url, "{message}"),
        _ => debug!(feed = %url, "{message}"),
    }
}

fn log_release(url: &str, before: Option<Hold>, verdict: &Verdict, print: Print) {
    match (before, verdict) {
        (_, Verdict::Reanchored { from }) => warn!(
            feed = %url,
            from,
            to = print.price,
            "reference price held its new level; quoting off it"
        ),
        (Some(Hold::WarmingUp), _) => {
            info!(feed = %url, price = print.price, "reference price confirmed; quoting")
        }
        (Some(_), _) => {
            info!(feed = %url, price = print.price, "reference price back in line; quoting")
        }
        (None, _) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: FeedPolicy = FeedPolicy {
        jump: JumpGuard {
            max_jump_bps: 1_000,
            settle_secs: 300,
        },
        staleness_secs: 60,
    };
    const URL: &str = "https://feed.example/pair";

    fn quote(price: f64, timestamp: u64) -> Quote {
        Quote {
            price,
            timestamp,
            degraded: false,
            reference: None,
        }
    }

    fn poll(at: u64) -> Instant {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *START.get_or_init(Instant::now) + std::time::Duration::from_secs(at)
    }

    fn read(cache: &PriceCache, price: f64, at: u64) -> PrintHistory {
        cache.admit(poll(at), URL, quote(price, at), at)
    }

    #[test]
    fn concurrent_startup_responses_cannot_confirm_each_other() {
        let rfq = PriceCache::new(None, POLICY);
        let tick = rfq.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|s| {
            for producer in [&rfq, &tick] {
                let barrier = barrier.clone();
                s.spawn(move || {
                    barrier.wait();
                    let history = read(producer, 10.0, 100);
                    assert_eq!(history.trusted, None);
                    assert_eq!(producer.get(URL), None);
                });
            }
        });
        assert_eq!(rfq.hold(URL), Some(Hold::WarmingUp));
    }

    #[test]
    fn delayed_responses_wait_for_a_poll_started_a_full_second_later() {
        let dir = std::env::temp_dir().join(format!("stitch-startup-{}", rand::random::<u64>()));
        let rfq = PriceCache::new(Some(&dir), POLICY);
        let tick = rfq.clone();
        let path = dir
            .join(super::super::anchor::DIR)
            .join(super::super::anchor::file_name(URL));
        let sample = quote(1.0, 100);
        rfq.admit(poll(100), URL, sample.clone(), 100);
        // Response time is irrelevant, including across wall-clock seconds.
        for started in [
            poll(99),
            poll(100),
            poll(101) - std::time::Duration::from_nanos(1),
        ] {
            let history = tick.admit(started, URL, sample.clone(), 110);
            assert_eq!(history.trusted, None);
            assert_eq!(rfq.get(URL), None);
            assert_eq!(rfq.hold(URL), Some(Hold::WarmingUp));
            assert!(!path.exists(), "no trusted anchor may be saved yet");
        }
        // The exact one-second boundary qualifies, even for the same sample.
        tick.admit(poll(101), URL, sample.clone(), 110);
        assert_eq!(rfq.get(URL), Some(sample));
        assert_eq!(rfq.hold(URL), None);
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacing_a_startup_candidate_requires_a_later_poll_of_the_new_level() {
        let cache = PriceCache::new(None, POLICY);
        read(&cache, 10.0, 100);
        read(&cache, 1.0, 101);
        // An outage preserves the new candidate and its confirmation clock.
        cache.invalidate(URL);
        // A delayed response from the first poll agrees with the new level,
        // but is still not an independent confirmation of that candidate.
        cache.admit(poll(100), URL, quote(1.0, 101), 105);
        cache.admit(poll(101), URL, quote(1.0, 101), 105);
        assert_eq!(cache.get(URL), None);
        assert_eq!(cache.hold(URL), Some(Hold::WarmingUp));
        cache.admit(poll(102), URL, quote(1.0, 101), 105);
        assert_eq!(cache.get(URL), Some(quote(1.0, 101)));
    }

    #[test]
    fn concurrent_producers_cannot_establish_independent_trusted_prices() {
        let rfq = PriceCache::new(None, POLICY);
        let tick = rfq.clone();
        read(&rfq, 1.0, 100);
        tick.admit(poll(101), URL, quote(1.0, 100), 101);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|s| {
            for producer in [&rfq, &tick] {
                let barrier = barrier.clone();
                s.spawn(move || {
                    barrier.wait();
                    // With independent histories these reads could confirm
                    // a second baseline. They all judge against the shared 1.
                    for _ in 0..10 {
                        let history = read(producer, 10.0, 100);
                        assert_eq!(history.trusted.unwrap().price, 1.0);
                        assert_eq!(producer.get(URL), None);
                    }
                });
            }
        });
        assert_eq!(rfq.hold(URL), tick.hold(URL));
        assert!(matches!(rfq.hold(URL), Some(Hold::Jumped { .. })));
        read(&tick, 1.01, 101);
        assert_eq!(rfq.get(URL), Some(quote(1.01, 101)));
        assert_eq!(rfq.hold(URL), None);
    }

    #[test]
    fn both_producers_share_the_candidate_clock_without_advancing_it_on_rereads() {
        let rfq = PriceCache::new(None, POLICY);
        let tick = rfq.clone();
        read(&rfq, 1.0, 100);
        read(&tick, 1.0, 101);
        read(&rfq, 2.0, 200);
        for _ in 0..5 {
            read(&tick, 2.0, 200);
        }
        assert_eq!(rfq.get(URL), None);
        // An older out-of-line response cannot undo the other loop's hold.
        read(&tick, 2.0, 90);
        assert_eq!(rfq.get(URL), None);
        read(&tick, 2.0, 499);
        assert_eq!(rfq.get(URL), None);
        read(&rfq, 2.0, 500);
        assert_eq!(tick.get(URL), Some(quote(2.0, 500)));
        assert_eq!(tick.hold(URL), None);
    }

    #[test]
    fn unhealthy_reads_and_outages_do_not_erase_the_shared_baseline() {
        let rfq = PriceCache::new(None, POLICY);
        let tick = rfq.clone();
        read(&rfq, 1.0, 100);
        read(&tick, 1.0, 101);
        for (i, (price, stale, degraded)) in [
            (10.0, false, true),
            (10.0, true, false),
            (0.0, false, false),
            (-1.0, false, false),
        ]
        .into_iter()
        .enumerate()
        {
            let now = 110 + i as u64 * 10;
            let bad = Quote {
                price,
                timestamp: if stale { 1 } else { now },
                degraded,
                reference: None,
            };
            tick.admit(poll(now), URL, bad.clone(), now);
            assert_eq!(rfq.get(URL), Some(bad));
            assert_eq!(read(&rfq, 10.0, now + 1).trusted.unwrap().price, 1.0);
            assert_eq!(tick.get(URL), None);
            read(&tick, 1.0, now + 2);
        }
        rfq.invalidate(URL);
        assert_eq!(tick.get(URL), None);
        read(&tick, 10.0, 150);
        assert_eq!(rfq.get(URL), None);
        read(&rfq, 1.0, 151);
        assert_eq!(tick.get(URL), Some(quote(1.0, 151)));
    }

    #[test]
    fn the_shared_anchor_survives_restart_and_is_loaded_once_per_url() {
        let dir = std::env::temp_dir().join(format!("stitch-live-guard-{}", rand::random::<u64>()));
        let rfq = PriceCache::new(Some(&dir), POLICY);
        let tick = rfq.clone();
        read(&rfq, 1.0, 100);
        read(&tick, 1.0, 101);
        let path = dir
            .join(super::super::anchor::DIR)
            .join(super::super::anchor::file_name(URL));
        assert!(path.exists());
        read(&tick, 10.0, 102);
        let restarted = PriceCache::new(Some(&dir), POLICY);
        assert_eq!(read(&restarted, 10.0, 103).trusted.unwrap().price, 1.0);
        assert_eq!(restarted.get(URL), None);
        // Removing the file during a run cannot reset either live producer.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read(&rfq, 10.0, 104).trusted.unwrap().price, 1.0);
        assert_eq!(read(&tick, 10.0, 104).trusted.unwrap().price, 1.0);
        assert_eq!(rfq.get(URL), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
