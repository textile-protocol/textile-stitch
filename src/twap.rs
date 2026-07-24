// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Rolling time-weighted average price (TWAP) over the feed's observations.
//!
//! The maker can center its spread on a short TWAP of the feed instead of the
//! instantaneous value. The average moves slowly, so the book stops chasing
//! every tick: a transient spike leaves the quote near the settled mean — the
//! ask sells *into* the spike above the reverting average and keeps the spread
//! when it snaps back — while a persistent move flows into the average and the
//! center converges within one window.
//!
//! The feed is a step function: each observation `(timestamp, price)` holds
//! until the next one (last observation carried forward). The TWAP integrates
//! that step function over the trailing window ending at wall-clock `now`, so
//! a slow feed (e.g. one that re-samples every 3 minutes) weights each sample
//! by how long it was actually live, and a fast feed weights every tick
//! equally. A gap between observations longer than `max_gap_secs` (the feed's
//! own staleness bound) resets the history: carrying a price across a window
//! the feed never observed would quote off data the bot itself refused to
//! trade on.

use std::collections::VecDeque;

/// Rolling TWAP state for one pool's feed.
#[derive(Debug, Clone)]
pub struct Twap {
    window_secs: u64,
    max_gap_secs: u64,
    /// `(observation_ts, price)`, strictly increasing timestamps. The front
    /// sample may predate the window — it carries its price forward into it.
    samples: VecDeque<(u64, f64)>,
    /// Wall-clock `now` of the previous `observe` — used to detect a gap in the
    /// BOT's own observation loop (sleep / hung await), which feed timestamps
    /// alone cannot see.
    last_now: Option<u64>,
}

impl Twap {
    pub fn new(window_secs: u64, max_gap_secs: u64) -> Self {
        Self {
            window_secs: window_secs.max(1),
            max_gap_secs,
            samples: VecDeque::new(),
            last_now: None,
        }
    }

    /// Record one feed observation. Unusable prices are ignored — the tick
    /// loop's `is_price_usable` gate skips the whole pool on them before this
    /// is ever called, so the drop here is defense in depth, not the primary
    /// guard (a caller that skipped the gate would otherwise keep quoting off
    /// `value()`'s last valid sample through a feed malfunction). A repeated
    /// timestamp updates the price in place (a corrected sample, or a slow
    /// feed served twice); a timestamp that runs backwards, a feed gap past
    /// `max_gap_secs`, or a gap in the BOT's own observation clock past the
    /// window resets the history to this sample alone. Prunes samples that
    /// ended before the window starting at `now`. Returns `Some(reason)` when it
    /// reset, so the caller can surface the (otherwise silent) degradation.
    pub fn observe(&mut self, ts: u64, price: f64, now: u64) -> Option<&'static str> {
        if !price.is_finite() || price <= 0.0 {
            return None;
        }
        let mut reset: Option<&'static str> = None;
        // Bot-side observation gap: if THIS bot stopped observing for longer
        // than the window (macOS sleep under launchd, a hung RPC/indexer await
        // stretching a tick), the feed kept printing but we weren't looking.
        // Carrying the pre-gap price forward would reconstruct a step function
        // that never existed and quote off it. Reset — same principle as the
        // feed-gap reset below, but keyed on OUR clock, which feed timestamps
        // cannot see. Bounded by the window (not `max_gap_secs`, which is the
        // feed's staleness tolerance for slow feeds like cNGN): once we've been
        // dark longer than the window there is nothing left to average anyway.
        if let Some(last) = self.last_now {
            if now.saturating_sub(last) > self.window_secs {
                self.samples.clear();
                reset = Some("bot observation gap");
            }
        }
        self.last_now = Some(now);
        match self.samples.back().copied() {
            Some((last_ts, _)) if ts < last_ts => {
                // The source's clock went backwards (a feed restart or
                // failover): the history's ordering is no longer trustworthy.
                self.samples.clear();
                self.samples.push_back((ts, price));
                reset = Some("feed clock reversal");
            }
            Some((last_ts, last_price)) if ts == last_ts => {
                if price != last_price {
                    if let Some(back) = self.samples.back_mut() {
                        back.1 = price;
                    }
                }
            }
            Some((last_ts, _)) if ts.saturating_sub(last_ts) > self.max_gap_secs => {
                // The feed skipped more than its own staleness bound; the bot
                // wasn't quoting through that gap, so don't average across it.
                self.samples.clear();
                self.samples.push_back((ts, price));
                reset = Some("feed gap");
            }
            _ => self.samples.push_back((ts, price)),
        }
        self.prune(now);
        reset
    }

    /// The time-weighted average over `[now - window, now]`, integrating the
    /// step function the samples describe. `None` before the first
    /// observation. With a single just-observed sample this equals that price
    /// (graceful warmup: the TWAP starts at spot and earns its smoothing as
    /// the window fills). Every segment boundary is capped at `now`: a feed
    /// timestamp ahead of the local clock (ordinary skew — the staleness gate
    /// tolerates it via saturating arithmetic) must not extend its
    /// predecessor's weight past `now`, or the older price would dominate the
    /// average outside the window while the newest observation got none.
    pub fn value(&self, now: u64) -> Option<f64> {
        let (&(_, last_price), n) = (self.samples.back()?, self.samples.len());
        let start = now.saturating_sub(self.window_secs);
        let mut weighted = 0.0;
        let mut total = 0.0;
        for (i, &(ts, price)) in self.samples.iter().enumerate() {
            let seg_start = ts.max(start);
            let seg_end = if i + 1 < n {
                self.samples[i + 1].0.min(now)
            } else {
                now
            };
            let weight = seg_end.saturating_sub(seg_start) as f64;
            weighted += price * weight;
            total += weight;
        }
        // Zero weight (a single sample observed at `now`, or clock skew):
        // fall back to the latest observation.
        Some(if total > 0.0 {
            weighted / total
        } else {
            last_price
        })
    }

    /// Drop samples whose whole lifetime ended before the window start,
    /// keeping the newest of them to carry its price into the window.
    fn prune(&mut self, now: u64) {
        let start = now.saturating_sub(self.window_secs);
        while self.samples.len() > 1 && self.samples[1].0 <= start {
            self.samples.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn twap() -> Twap {
        Twap::new(120, 900)
    }

    #[test]
    fn empty_twap_has_no_value() {
        assert_eq!(twap().value(1_000), None);
    }

    #[test]
    fn a_single_fresh_sample_reads_as_spot() {
        let mut t = twap();
        t.observe(1_000, 3_000.0, 1_000);
        assert_eq!(t.value(1_000), Some(3_000.0));
    }

    #[test]
    fn averages_are_weighted_by_each_samples_live_time() {
        // p=100 live over [0,60), p=200 over [60,120): equal halves → 150.
        let mut t = twap();
        t.observe(0, 100.0, 0);
        t.observe(60, 200.0, 60);
        assert_eq!(t.value(120), Some(150.0));
        // Asymmetric weights: 100 for 90s, 200 for 30s → 125.
        assert_eq!(t.value(90), Some((100.0 * 60.0 + 200.0 * 30.0) / 90.0));
    }

    #[test]
    fn a_transient_spike_barely_moves_the_average() {
        // Flat 100 for 110s, then a +100% spike for the last 10s of the
        // window: the center moves ~8%, not 100% — the ask sells into the
        // spike ~92% below it instead of chasing it.
        let mut t = twap();
        t.observe(0, 100.0, 0);
        t.observe(110, 200.0, 110);
        let v = t.value(120).unwrap();
        assert!((v - (100.0 * 110.0 + 200.0 * 10.0) / 120.0).abs() < 1e-9);
        assert!(v < 110.0);
    }

    #[test]
    fn a_persistent_move_fully_converges_within_one_window() {
        let mut t = twap();
        t.observe(0, 100.0, 0);
        t.observe(10, 200.0, 10);
        // 120s later the old level has left the window entirely.
        assert_eq!(t.value(200), Some(200.0));
    }

    #[test]
    fn the_last_sample_before_the_window_carries_forward() {
        // A slow feed: one observation, window slides past it — the price
        // still holds (last observation carried forward).
        let mut t = twap();
        t.observe(0, 100.0, 0);
        assert_eq!(t.value(500), Some(100.0));
    }

    #[test]
    fn pruning_keeps_the_carry_forward_sample() {
        let mut t = twap();
        t.observe(0, 100.0, 0);
        t.observe(10, 110.0, 10);
        // The bot keeps observing the held 110 through the 390s the feed holds
        // it (steps ≤ window, so no bot-gap reset), then the feed re-samples.
        t.observe(10, 110.0, 120);
        t.observe(10, 110.0, 240);
        t.observe(10, 110.0, 360);
        t.observe(400, 120.0, 400); // 390s FEED gap > window, < max_gap
                                    // Only (10, 110) carries into [280, 400]; (0, 100) is prunable.
        let v = t.value(400).unwrap();
        assert_eq!(v, 110.0);
        assert!(t.samples.len() <= 2);
    }

    #[test]
    fn a_repeated_timestamp_updates_in_place() {
        // A feed that re-samples every 3 min serves the same observation on
        // every 5s tick; it must not stack duplicate samples.
        let mut t = twap();
        t.observe(0, 100.0, 0);
        for now in [5, 10, 15] {
            t.observe(0, 100.0, now);
        }
        assert_eq!(t.samples.len(), 1);
        // A corrected price at the same timestamp replaces the sample.
        t.observe(0, 101.0, 20);
        assert_eq!(t.samples.len(), 1);
        assert_eq!(t.value(0), Some(101.0));
    }

    #[test]
    fn a_backwards_timestamp_resets_the_history() {
        let mut t = twap();
        t.observe(100, 100.0, 100);
        t.observe(50, 300.0, 100); // feed failover with an older clock
        assert_eq!(t.value(100), Some(300.0));
        assert_eq!(t.samples.len(), 1);
    }

    #[test]
    fn a_feed_gap_past_the_staleness_bound_resets_the_history() {
        // The feed's timestamp leaps > 900s max_gap in one step (a feed clock
        // glitch) while the bot keeps ticking — the FEED-gap arm resets even
        // though the bot itself never went dark. The pre-glitch price must not
        // dominate the post-recovery average.
        let mut t = twap(); // window 120, max_gap 900
        t.observe(0, 100.0, 0);
        t.observe(5, 100.0, 5); // bot ticking normally
        let reason = t.observe(1_000, 200.0, 10); // feed ts +995 > 900; bot now only +5
        assert_eq!(reason, Some("feed gap"));
        assert_eq!(t.value(10), Some(200.0));
    }

    #[test]
    fn a_bot_observation_gap_past_the_window_resets_even_when_feed_ts_are_close() {
        // The bug this guards: the bot slept 600s while the feed kept printing
        // every ~10s. On wake the fresh print's ts is only ~600s past the last
        // one the bot happened to observe — under the 900s feed max_gap, so the
        // feed-gap arm would NOT fire — but the bot's OWN clock jumped 600s > the
        // 120s window. Without this reset the pre-sleep price carries forward and
        // dominates the average (a step that never existed); with it, we start
        // fresh at spot.
        let mut t = twap(); // window 120, max_gap 900
        t.observe(1_000, 3_000.0, 1_000);
        // 600s later: feed ts moved only 600 (≤ 900 max_gap), but the bot's
        // observation clock jumped 600 (> 120 window).
        let reason = t.observe(1_600, 2_940.0, 1_600);
        assert_eq!(reason, Some("bot observation gap"));
        assert_eq!(t.value(1_600), Some(2_940.0)); // fresh spot, not ~3_000
    }

    #[test]
    fn observe_returns_none_on_a_normal_append() {
        let mut t = twap();
        assert_eq!(t.observe(0, 100.0, 0), None);
        assert_eq!(t.observe(5, 101.0, 5), None);
    }

    #[test]
    fn a_gap_within_the_staleness_bound_carries_forward() {
        // cNGN-style feed: re-samples every ~180s under a 900s bound, while the
        // bot ticks every ~5s and re-observes the held price. The 180s-old price
        // legitimately covers the gap — the bot never went dark, so no bot-gap
        // reset and LOCF holds.
        let mut t = twap();
        t.observe(0, 100.0, 0);
        t.observe(0, 100.0, 100); // bot still observing the held price mid-gap
        t.observe(180, 200.0, 180);
        // Window [60,180]: p=100 was live for all of it; the sample at t=180
        // has held for zero seconds so far.
        assert_eq!(t.value(180), Some(100.0));
    }

    #[test]
    fn unusable_prices_are_ignored() {
        let mut t = twap();
        t.observe(0, f64::NAN, 0);
        t.observe(1, f64::INFINITY, 1);
        t.observe(2, 0.0, 2);
        t.observe(3, -5.0, 3);
        assert_eq!(t.value(3), None);
        t.observe(4, 100.0, 4);
        assert_eq!(t.value(4), Some(100.0));
    }

    #[test]
    fn future_timestamps_do_not_panic_or_skew_negative() {
        // Clock skew: the feed's ts is ahead of local now. Weights saturate
        // at zero and the value falls back to the latest observation.
        let mut t = twap();
        t.observe(2_000, 100.0, 1_000);
        assert_eq!(t.value(1_000), Some(100.0));
    }

    #[test]
    fn a_future_sample_cannot_extend_its_predecessors_weight_past_now() {
        // Clock skew with history: the newest observation's ts (1000) is 60s
        // ahead of local now (940). Its predecessor must be weighted only up
        // to `now`, not up to the future ts — otherwise the older price
        // collects extra weight from time that hasn't happened, and the mid
        // sits stale for as long as the skew lasts.
        let mut t = twap();
        t.observe(860, 100.0, 860);
        t.observe(880, 300.0, 880);
        t.observe(1_000, 200.0, 940);
        // True LOCF over [820, 940]: 100 for 20s, 300 for 60s → 250. An
        // uncapped boundary would weight 300 for 120s instead → ~271.
        assert_eq!(t.value(940), Some((100.0 * 20.0 + 300.0 * 60.0) / 80.0));
    }

    #[test]
    fn zero_window_is_clamped_to_one_second() {
        // window(0) clamps to 1s. With a 1s window, any tick spaced > 1s apart
        // trips the bot-observation-gap reset, so a 1s window degenerates to
        // spot-tracking. The point of the test is only that it clamps and never
        // panics or divides by zero.
        let mut t = Twap::new(0, 900);
        t.observe(0, 100.0, 0);
        t.observe(10, 200.0, 10); // 10s > 1s window -> bot-gap reset to spot
        assert_eq!(t.value(10), Some(200.0));
        assert_eq!(t.value(11), Some(200.0));
    }
}
