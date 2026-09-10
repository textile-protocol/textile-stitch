// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Inventory-lean quoting. The book leans both spreads against the wallet's
//! own inventory so it self-rebalances and never freezes one-sided, while no
//! quote ever crosses fair: every offset is clamped to `floor_bps`, the
//! measured accuracy of the price feed vs live Pyth, so every fill is at fair
//! or better by construction.
//!
//! The lean input is inventory only — never price momentum, never recent
//! flow. Inventory share `x` (collateral value share, 0 = all stable, 1 = all
//! soft) is recomputed on every observed fill and at most once per
//! [`X_REFRESH_SECS`] otherwise, and the smoothed offsets move at most
//! [`MAX_LEAN_STEP_BPS`] per update so the lean can't be whipsawed. A fair
//! jump beyond [`MAX_FAIR_JUMP_BPS`] in one tick pulls both quotes for that
//! tick.

/// Inventory band edges: balanced inside [0.40, 0.60], leaning toward the
/// critical edges at 0.85 / 0.15 where the heavy side loses its quote.
const BALANCED_LOW: f64 = 0.40;
const BALANCED_HIGH: f64 = 0.60;
const CRITICAL_LOW: f64 = 0.15;
const CRITICAL_HIGH: f64 = 0.85;

/// Pull both quotes when fair moves more than this in one tick.
pub const MAX_FAIR_JUMP_BPS: f64 = 25.0;
/// Largest move of a smoothed offset per inventory update.
pub const MAX_LEAN_STEP_BPS: f64 = 0.5;
/// Refresh the inventory share at most this often, absent a fill.
pub const X_REFRESH_SECS: u64 = 60;
/// Default balanced-zone half-spread, in bps.
pub const DEFAULT_BASE_BPS: f64 = 1.0;
/// Default extra widening of the heavy side's far quote at the critical edge.
pub const DEFAULT_WIDE_BPS: f64 = 3.0;

/// Lean rollout mode for a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeanMode {
    Off,
    /// Compute and log the lean quotes next to the live ones; no behavior change.
    Shadow,
    /// Quote the live book off the lean prices.
    Live,
}

/// Tunables. `floor_bps` is the tightest honest spread — the measured p95 of
/// the feed's error vs live Pyth — and every offset is clamped to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeanParams {
    pub base_bps: f64,
    pub wide_bps: f64,
    pub floor_bps: f64,
}

/// Target offsets in bps off fair for an inventory share. `None` = that side
/// is pulled (critical zone).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeanOffsets {
    pub bid_bps: Option<f64>,
    pub ask_bps: Option<f64>,
}

/// One tick's lean quotes: prices (debt per collateral), or `None` for a
/// pulled side. `pulled` names the reason when both sides are down at once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeanDecision {
    pub x: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub pulled: Option<&'static str>,
}

/// Collateral value share of the wallet: `coll·fair / (coll·fair + debt)`,
/// both legs valued in debt units. `None` when the wallet is empty or fair is
/// unusable — there is no meaningful share to lean on.
pub fn inventory_share(
    collateral_atomic: u128,
    collateral_decimals: u8,
    debt_atomic: u128,
    debt_decimals: u8,
    fair: f64,
) -> Option<f64> {
    if !fair.is_finite() || fair <= 0.0 {
        return None;
    }
    let collateral_value =
        collateral_atomic as f64 / 10f64.powi(i32::from(collateral_decimals)) * fair;
    let debt_value = debt_atomic as f64 / 10f64.powi(i32::from(debt_decimals));
    let total = collateral_value + debt_value;
    (total > 0.0).then(|| collateral_value / total)
}

/// The quote rule: target offsets for an inventory share. Balanced quotes
/// ±base; a heavy book tightens its unloading side toward the floor and
/// widens the accumulating side by `wide·t`; past the critical edge the
/// accumulating side is pulled. Every offset is clamped to the floor
/// (invariant: quotes never cross fair by less than the feed's accuracy).
pub fn target_offsets(x: f64, p: &LeanParams) -> LeanOffsets {
    // A base below the floor would quote tighter than the feed can honestly
    // support; the floor wins.
    let base = p.base_bps.max(p.floor_bps);
    let (bid, ask) = if x > CRITICAL_HIGH {
        // Critical soft-heavy: unload at the floor, stop buying.
        (None, Some(p.floor_bps))
    } else if x > BALANCED_HIGH {
        let t = (x - BALANCED_HIGH) / (CRITICAL_HIGH - BALANCED_HIGH);
        (
            Some(base + p.wide_bps * t),
            Some(base - (base - p.floor_bps) * t),
        )
    } else if x >= BALANCED_LOW {
        (Some(base), Some(base))
    } else if x >= CRITICAL_LOW {
        // Mirror image: stable-heavy tightens the bid, widens the ask.
        let t = (BALANCED_LOW - x) / (BALANCED_LOW - CRITICAL_LOW);
        (
            Some(base - (base - p.floor_bps) * t),
            Some(base + p.wide_bps * t),
        )
    } else {
        // Critical stable-heavy: buy at the floor, stop selling.
        (Some(p.floor_bps), None)
    };
    LeanOffsets {
        bid_bps: bid.map(|b| b.max(p.floor_bps)),
        ask_bps: ask.map(|a| a.max(p.floor_bps)),
    }
}

/// Move `current` toward `target` by at most `max_step`.
pub fn step_toward(current: f64, target: f64, max_step: f64) -> f64 {
    current + (target - current).clamp(-max_step, max_step)
}

/// True when fair moved more than [`MAX_FAIR_JUMP_BPS`] since the last tick.
pub fn fair_jumped(prev: f64, fair: f64) -> bool {
    prev > 0.0 && ((fair - prev).abs() / prev) * 10_000.0 > MAX_FAIR_JUMP_BPS
}

/// How long a fill signal keeps forcing balance re-reads when the wallet
/// hasn't moved yet. Fill transactions are submitted, not awaited — the
/// balance change lands a block or two later — so the signal stays pending
/// until the wallet actually moves. The cap stops a dropped or reverted tx
/// from pinning per-tick re-reads forever; it mirrors the taker's and
/// closer's own resubmit cooldown.
pub const FILL_SETTLE_TIMEOUT_SECS: u64 = 180;

/// Per-pool lean state across ticks: the smoothed offsets, the last inventory
/// share and when it was measured, and the last fair for jump detection.
#[derive(Debug, Clone)]
pub struct LeanState {
    bid_offset_bps: f64,
    ask_offset_bps: f64,
    x: Option<f64>,
    last_x_at: u64,
    /// Unix time of the last fill signal whose balance change we haven't seen.
    fill_pending_since: Option<u64>,
    last_balances: Option<(u128, u128)>,
    last_fair: Option<f64>,
}

impl LeanState {
    /// Fresh state: offsets start at the balanced spread, no inventory read yet.
    pub fn new(params: &LeanParams) -> Self {
        let start = params.base_bps.max(params.floor_bps);
        Self {
            bid_offset_bps: start,
            ask_offset_bps: start,
            x: None,
            last_x_at: 0,
            fill_pending_since: None,
            last_balances: None,
            last_fair: None,
        }
    }

    /// True when the inventory share should be re-read: never measured, a fill
    /// signal is pending, or the refresh window lapsed.
    pub fn needs_inventory(&self, now: u64) -> bool {
        self.fill_pending_since.is_some()
            || self.x.is_none()
            || now.saturating_sub(self.last_x_at) >= X_REFRESH_SECS
    }

    /// A fill was observed (or its transaction submitted) — re-read balances
    /// every tick until the wallet's move shows up.
    pub fn note_fill(&mut self, now: u64) {
        self.fill_pending_since = Some(now);
    }

    /// Recompute the inventory share from fresh balances and step the smoothed
    /// offsets toward the zone targets (one bounded step per update). A pulled
    /// side keeps its last offset, so it resumes from there when it re-opens.
    #[allow(clippy::too_many_arguments)]
    pub fn set_inventory(
        &mut self,
        collateral_atomic: u128,
        collateral_decimals: u8,
        debt_atomic: u128,
        debt_decimals: u8,
        fair: f64,
        now: u64,
        params: &LeanParams,
    ) {
        // A pending fill settles when the wallet actually moves — the fill tx
        // is submitted, not confirmed, so the first re-read can predate the
        // balance change — or when the settle window lapses (dropped tx).
        let balances = (collateral_atomic, debt_atomic);
        let settled = self.last_balances != Some(balances)
            || self
                .fill_pending_since
                .is_some_and(|t| now.saturating_sub(t) >= FILL_SETTLE_TIMEOUT_SECS);
        if settled {
            self.fill_pending_since = None;
        }
        self.last_balances = Some(balances);
        self.last_x_at = now;
        self.x = inventory_share(
            collateral_atomic,
            collateral_decimals,
            debt_atomic,
            debt_decimals,
            fair,
        );
        let Some(x) = self.x else { return };
        let targets = target_offsets(x, params);
        if let Some(t) = targets.bid_bps {
            self.bid_offset_bps = step_toward(self.bid_offset_bps, t, MAX_LEAN_STEP_BPS);
        }
        if let Some(t) = targets.ask_bps {
            self.ask_offset_bps = step_toward(self.ask_offset_bps, t, MAX_LEAN_STEP_BPS);
        }
    }

    /// This tick's lean quotes off `fair`. A >25bps fair jump pulls both sides
    /// for exactly this tick; otherwise prices come from the smoothed offsets
    /// (clamped to the floor) and the current zone decides which sides quote.
    /// With no inventory read yet, both sides quote the balanced spread.
    pub fn decide(&mut self, fair: f64, params: &LeanParams) -> LeanDecision {
        let jumped = self.last_fair.is_some_and(|prev| fair_jumped(prev, fair));
        self.last_fair = Some(fair);
        if jumped {
            return LeanDecision {
                x: self.x,
                bid: None,
                ask: None,
                pulled: Some("fair jumped >25bps in one tick"),
            };
        }
        let zone = self.x.map(|x| target_offsets(x, params));
        let bid_on = zone.is_none_or(|z| z.bid_bps.is_some());
        let ask_on = zone.is_none_or(|z| z.ask_bps.is_some());
        let bid =
            bid_on.then(|| fair * (1.0 - self.bid_offset_bps.max(params.floor_bps) / 10_000.0));
        let ask =
            ask_on.then(|| fair * (1.0 + self.ask_offset_bps.max(params.floor_bps) / 10_000.0));
        LeanDecision {
            x: self.x,
            bid,
            ask,
            pulled: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> LeanParams {
        LeanParams {
            base_bps: 1.0,
            wide_bps: 3.0,
            floor_bps: 0.5,
        }
    }

    #[test]
    fn inventory_share_spans_zero_to_one() {
        // All debt → 0; all collateral → 1; equal value → 0.5.
        assert_eq!(inventory_share(0, 6, 1_000_000, 6, 3300.0), Some(0.0));
        assert_eq!(inventory_share(1_000_000, 6, 0, 6, 3300.0), Some(1.0));
        // 1 XAUt at fair 3300 vs 3300 USDT.
        let x = inventory_share(1_000_000, 6, 3_300_000_000, 6, 3300.0).unwrap();
        assert!((x - 0.5).abs() < 1e-12);
    }

    #[test]
    fn inventory_share_normalizes_decimals() {
        // 1 unit each at 18dp vs 6dp, fair 1.0 → still half/half.
        let x = inventory_share(1_000_000_000_000_000_000, 18, 1_000_000, 6, 1.0).unwrap();
        assert!((x - 0.5).abs() < 1e-12);
    }

    #[test]
    fn inventory_share_is_none_when_unmeasurable() {
        assert_eq!(inventory_share(0, 6, 0, 6, 3300.0), None);
        assert_eq!(inventory_share(1, 6, 1, 6, 0.0), None);
        assert_eq!(inventory_share(1, 6, 1, 6, f64::NAN), None);
    }

    #[test]
    fn balanced_zone_quotes_base_both_sides() {
        for x in [0.40, 0.50, 0.60] {
            let o = target_offsets(x, &params());
            assert_eq!(o.bid_bps, Some(1.0));
            assert_eq!(o.ask_bps, Some(1.0));
        }
    }

    #[test]
    fn soft_heavy_tightens_ask_and_widens_bid() {
        // Midway (x = 0.725, t = 0.5): ask = base − (base−floor)/2, bid = base + wide/2.
        let o = target_offsets(0.725, &params());
        assert!((o.ask_bps.unwrap() - 0.75).abs() < 1e-12);
        assert!((o.bid_bps.unwrap() - 2.5).abs() < 1e-12);
        // At the critical edge (t = 1): ask at the floor, bid fully widened.
        let o = target_offsets(0.85, &params());
        assert!((o.ask_bps.unwrap() - 0.5).abs() < 1e-12);
        assert!((o.bid_bps.unwrap() - 4.0).abs() < 1e-12);
    }

    #[test]
    fn critical_soft_heavy_pulls_the_bid() {
        let o = target_offsets(0.86, &params());
        assert_eq!(o.bid_bps, None);
        assert_eq!(o.ask_bps, Some(0.5));
    }

    #[test]
    fn stable_heavy_mirrors_the_lean() {
        let o = target_offsets(0.275, &params());
        assert!((o.bid_bps.unwrap() - 0.75).abs() < 1e-12);
        assert!((o.ask_bps.unwrap() - 2.5).abs() < 1e-12);
        let o = target_offsets(0.10, &params());
        assert_eq!(o.ask_bps, None);
        assert_eq!(o.bid_bps, Some(0.5));
    }

    #[test]
    fn offsets_never_drop_below_the_floor() {
        // A floor above base (the 60s /price case: 3bps floor, 1bp base) wins
        // everywhere, including the balanced zone.
        let p = LeanParams {
            base_bps: 1.0,
            wide_bps: 3.0,
            floor_bps: 3.0,
        };
        for i in 0..=100 {
            let o = target_offsets(f64::from(i) / 100.0, &p);
            for off in [o.bid_bps, o.ask_bps].into_iter().flatten() {
                assert!(off >= p.floor_bps, "offset {off} under floor at x={i}");
            }
        }
    }

    #[test]
    fn step_toward_is_bounded_both_ways() {
        assert_eq!(step_toward(1.0, 4.0, 0.5), 1.5);
        assert_eq!(step_toward(4.0, 1.0, 0.5), 3.5);
        assert_eq!(step_toward(1.0, 1.2, 0.5), 1.2);
    }

    #[test]
    fn fair_jump_detects_only_big_one_tick_moves() {
        assert!(!fair_jumped(3300.0, 3300.0 * 1.0024)); // 24bps
        assert!(fair_jumped(3300.0, 3300.0 * 1.0026)); // 26bps
        assert!(fair_jumped(3300.0, 3300.0 * 0.9974));
        assert!(!fair_jumped(0.0, 3300.0)); // unusable prev
    }

    #[test]
    fn inventory_refresh_is_forced_by_fills_and_the_window() {
        let p = params();
        let mut s = LeanState::new(&p);
        assert!(s.needs_inventory(0)); // never measured
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 100, &p);
        assert!(!s.needs_inventory(130)); // 30s in
        assert!(s.needs_inventory(160)); // window lapsed
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 160, &p);
        s.note_fill(160);
        assert!(s.needs_inventory(161)); // fill forces a re-read
    }

    #[test]
    fn a_fill_signal_stays_pending_until_the_wallet_moves() {
        let p = params();
        let mut s = LeanState::new(&p);
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 100, &p);
        assert!(!s.needs_inventory(110));
        s.note_fill(110);
        // The fill tx is submitted but not landed: the re-read sees unchanged
        // balances, so the signal stays pending and the next tick reads again.
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 115, &p);
        assert!(s.needs_inventory(116));
        // The fill lands: balances moved, the signal settles.
        s.set_inventory(900_000, 6, 3_630_000_000, 6, 3300.0, 120, &p);
        assert!(!s.needs_inventory(121));
    }

    #[test]
    fn a_dropped_fill_tx_stops_forcing_re_reads_after_the_settle_window() {
        let p = params();
        let mut s = LeanState::new(&p);
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 100, &p);
        s.note_fill(100);
        // Still pending inside the window with unchanged balances...
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 150, &p);
        assert!(s.needs_inventory(151));
        // ...but a tx that never lands can't pin per-tick re-reads forever.
        let after = 100 + FILL_SETTLE_TIMEOUT_SECS;
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, after, &p);
        assert!(!s.needs_inventory(after + 1));
    }

    #[test]
    fn offsets_step_gradually_toward_the_lean() {
        let p = params();
        let mut s = LeanState::new(&p);
        // All-XAUt wallet: bid target base+wide = 4.0, but one update moves 0.5.
        s.set_inventory(1_000_000, 6, 0, 6, 3300.0, 60, &p);
        let d = s.decide(3300.0, &p);
        assert_eq!(d.x, Some(1.0));
        // Critical zone pulls the bid immediately; the ask offset stepped
        // 1.0 → 0.5, one bounded step toward the floor.
        assert_eq!(d.bid, None);
        let ask = d.ask.unwrap();
        assert!((ask - 3300.0 * (1.0 + 0.5 / 10_000.0)).abs() < 1e-6);
    }

    #[test]
    fn fair_jump_pulls_both_sides_for_one_tick() {
        let p = params();
        let mut s = LeanState::new(&p);
        s.set_inventory(1_000_000, 6, 3_300_000_000, 6, 3300.0, 60, &p);
        assert!(s.decide(3300.0, &p).pulled.is_none());
        let d = s.decide(3310.0, &p); // ~30bps jump
        assert!(d.pulled.is_some());
        assert_eq!(d.bid, None);
        assert_eq!(d.ask, None);
        // Next tick at the new level re-quotes.
        let d = s.decide(3310.0, &p);
        assert!(d.pulled.is_none());
        assert!(d.bid.is_some() && d.ask.is_some());
    }

    #[test]
    fn quotes_never_cross_fair_at_any_inventory() {
        // The hard invariant: ask ≥ F·(1+floor), bid ≤ F·(1−floor), for every
        // inventory share and however long the smoothing has run.
        let p = params();
        let fair = 3300.0;
        for i in 0..=100u128 {
            let coll = i * 10_000;
            let debt = (100 - i) * 33_000_000;
            let mut s = LeanState::new(&p);
            for step in 0..40 {
                s.set_inventory(coll, 6, debt, 6, fair, 60 * (step + 1), &p);
            }
            let d = s.decide(fair, &p);
            let bid_max = fair * (1.0 - p.floor_bps / 10_000.0);
            let ask_min = fair * (1.0 + p.floor_bps / 10_000.0);
            if let Some(bid) = d.bid {
                assert!(bid <= bid_max * (1.0 + 1e-12), "bid {bid} crosses at i={i}");
            }
            if let Some(ask) = d.ask {
                assert!(ask >= ask_min * (1.0 - 1e-12), "ask {ask} crosses at i={i}");
            }
            assert!(
                d.bid.is_some() || d.ask.is_some(),
                "book fully dark at i={i}"
            );
        }
    }

    #[test]
    fn no_inventory_reading_quotes_the_balanced_spread() {
        let p = params();
        let mut s = LeanState::new(&p);
        let d = s.decide(3300.0, &p);
        assert_eq!(d.x, None);
        assert!((d.bid.unwrap() - 3300.0 * (1.0 - 1.0 / 10_000.0)).abs() < 1e-6);
        assert!((d.ask.unwrap() - 3300.0 * (1.0 + 1.0 / 10_000.0)).abs() < 1e-6);
    }

    #[test]
    fn a_pulled_side_resumes_from_its_last_offset() {
        let p = params();
        let mut s = LeanState::new(&p);
        // Critical soft-heavy: bid pulled, its offset untouched at 1.0.
        s.set_inventory(1_000_000, 6, 0, 6, 3300.0, 60, &p);
        assert_eq!(s.decide(3300.0, &p).bid, None);
        // Back inside the leaning band: the bid re-opens near where it left.
        s.set_inventory(1_000_000, 6, 1_000_000_000, 6, 3300.0, 120, &p);
        let d = s.decide(3300.0, &p);
        let bid = d.bid.unwrap();
        assert!((bid - 3300.0 * (1.0 - 1.5 / 10_000.0)).abs() < 1e-6);
    }
}
