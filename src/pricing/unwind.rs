// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Profit unwind: when the feed has moved so the naira a stitch holds is worth
//! more than it was when the stitch took it in, the inventory curve lifts by a
//! slice of that gain. Takers bringing dollars get more naira for them, so the
//! pile clears faster, and no ask the lift moves goes past what the naira cost.
//!
//! # Units
//!
//! Everything here is in **NGN per USDT**, the unit the operator and the
//! curve's knobs live in. The host inverts the feed once before calling in.
//!
//! # The rule
//!
//! ```text
//! gain     = cost - feedMid                        (NGN per USDT)
//! full     = 0                                     if gain < min_gain_ngn
//!          = min(cap_ngn, share * gain)            otherwise (NGN per USDT)
//! lift(f)  = full * w(f)
//! w(f)     = 0 at or below start, rising evenly to 1 at f = 1
//! ```
//!
//! The lift stays in NGN until the curve turns it into bps against its own
//! reference mid, the one its shades are applied to, so `cap_ngn` is the most
//! the centre moves in NGN whatever the gap between the feed and the
//! reference. It's a shade like the curve's own: at any one share both sides
//! move by the same `lift(f)`, and a quote pays its mean over the stretch of
//! share it walks ([`average_weight`]). Out and back over the same stretch integrate
//! to the same number, so a round trip nets exactly minus the spread whatever
//! the share does in between. The ask is then floored at the cost
//! ([`floored_ask_ngn`]), which can only make a round trip cost more.
//!
//! # What a taker can and can't move
//!
//! Every naira that comes in, bought or deposited, enters the basis at the
//! feed of the read it lands in. A naira sale or withdrawal leaves at the
//! average cost. So a taker's trade can pull the cost towards the feed (less
//! gain) but never away from it, and the spread the stitch earns on a fill is
//! never counted as gain. The cost is frozen in a [`Snapshot`] for
//! `refresh_secs`, so the lift doesn't churn on every fill, with one
//! exception: new naira that pulls the cost down lowers the snapshot at once,
//! so the ask floor never sits above what the naira now held is worth. It's
//! never raised early.
//!
//! Pulling the cost down lowers the lift, and that is a taker's one lever:
//! drain the stitch at the full lift, sell a little back to dilute the cost,
//! return the rest at the lower lift. [`lift_limit_ngn`] holds the full lift
//! to what that can't beat after the round trip, and the curve applies it to
//! every quote.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The operator's knobs, from `[modules.profit_unwind]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnwindParams {
    /// Share of the gain passed back as a better price, 0 to 1.
    pub share: f64,
    /// The most the lift ever reaches, NGN per USDT.
    pub cap_ngn: f64,
    /// Gains under this, NGN per USDT, move nothing.
    pub min_gain_ngn: f64,
    /// The naira share, percent, where profit unwind starts.
    pub start_pct: f64,
    /// How often the cost is frozen again.
    pub refresh_secs: u64,
}

/// Balances smaller than this are dust: the basis resets rather than carry a
/// cost per naira computed from fractions.
const DUST: f64 = 1e-6;

/// What the naira the stitch holds was worth when it came in, kept from its
/// own balance reads.
///
/// Naira in, from a fill or a deposit, enters at the feed of that read. Naira
/// out, sold or withdrawn, leaves at the average cost, so the cost per naira
/// doesn't change. Only the naira balance is read: what happened on the dollar
/// side (this bot's fills, a sibling bot on the same wallet, gas) can't skew
/// the basis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Basis {
    /// Naira held, as of the last read.
    pub naira: f64,
    /// What that naira was worth when it came in, in dollars.
    pub dollars: f64,
}

impl Basis {
    const EMPTY: Self = Self {
        naira: 0.0,
        dollars: 0.0,
    };

    /// A first read, or a basis that can't be trusted: what is held is taken
    /// at the feed, so there is no gain.
    pub fn start(naira: f64, mid_ngn: f64) -> Self {
        Self::EMPTY.observe(naira, mid_ngn)
    }

    /// Average cost in NGN per USDT, or `None` while it holds no naira.
    pub fn cost(&self) -> Option<f64> {
        (self.naira > DUST && self.dollars > 0.0).then(|| self.naira / self.dollars)
    }

    /// The basis after a new read of `naira` at feed `mid_ngn`. Unchanged for
    /// a move under dust or an input it can't trust.
    pub fn observe(self, naira: f64, mid_ngn: f64) -> Self {
        if !(naira.is_finite() && mid_ngn.is_finite() && mid_ngn > 0.0) {
            return self;
        }
        let naira = naira.max(0.0);
        if (naira - self.naira).abs() <= DUST {
            return self;
        }
        if naira <= DUST {
            return Self::EMPTY;
        }
        let dollars = if naira > self.naira {
            self.dollars + (naira - self.naira) / mid_ngn
        } else {
            // Out at the average cost: B *= N_after / N_before.
            self.dollars * naira / self.naira
        };
        Self { naira, dollars }
    }
}

/// The cost the lift is sized on, frozen until the next refresh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    /// Average cost, NGN per USDT.
    pub cost_ngn: f64,
    pub at: u64,
}

impl Snapshot {
    /// Due for a fresh one.
    pub fn due(&self, now: u64, refresh_secs: u64) -> bool {
        now < self.at || now - self.at >= refresh_secs
    }

    /// Too old to price off: the bot stopped refreshing it (paused, a clock
    /// jump). Twice the refresh, so one late tick doesn't switch it off.
    pub fn stale(&self, now: u64, refresh_secs: u64) -> bool {
        now < self.at || now - self.at > stale_after(refresh_secs)
    }
}

/// How long a snapshot, or a saved basis, stays trustworthy: twice the
/// refresh.
pub fn stale_after(refresh_secs: u64) -> u64 {
    refresh_secs.saturating_mul(2)
}

/// How much of the full lift applies at naira share `f`: none at or below the
/// start, then rising evenly to all of it at `f = 1`. Rising rather than
/// switching on keeps a share that wobbles across the start from jumping the
/// price.
pub fn weight(f: f64, start_pct: f64) -> f64 {
    let start = start_fraction(start_pct);
    let f = f.clamp(0.0, 1.0);
    if f <= start {
        0.0
    } else {
        (f - start) / (1.0 - start)
    }
}

/// The mean of [`weight`] over the stretch of share between `f0` and `f1`,
/// in either order: what a quote walking that stretch pays, the way the curve
/// averages its own shade. A zero-width stretch is the point weight.
pub fn average_weight(f0: f64, f1: f64, start_pct: f64) -> f64 {
    let (a, b) = (f0.clamp(0.0, 1.0), f1.clamp(0.0, 1.0));
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    if hi - lo <= f64::EPSILON {
        return weight(lo, start_pct);
    }
    let start = start_fraction(start_pct);
    // The integral of w from 0 to f.
    let area = |f: f64| {
        if f <= start {
            0.0
        } else {
            (f - start).powi(2) / (2.0 * (1.0 - start))
        }
    };
    (area(hi) - area(lo)) / (hi - lo)
}

fn start_fraction(start_pct: f64) -> f64 {
    (start_pct / 100.0).clamp(0.0, 0.99)
}

/// The most the full lift may be, NGN per USDT, so that a change in the lift
/// between a taker's legs never pays them.
///
/// Out and back at one lift cancel. But the lift can fall between the legs:
/// naira a taker sells back enters the basis at the feed and pulls the cost
/// down. A taker who drains the stitch at the old lift and fills it back at a
/// lower one keeps at most `full * mean(w)` over the stretch, and the whole
/// book (share 1 to 0) averages `w` to `(1 - start) / 2`. A round trip is only
/// sure to cost them twice the spread: the venue's fee arrives per request,
/// and the two legs needn't pay the same one, or any. So the full lift is held
/// to `full * (1 - start) / 2 <= 2 * spread * mid`. `spread_bps` is the
/// corridor spread alone, the signed order's half-width; `mid_ngn` the mid the
/// curve applies shades to.
pub fn lift_limit_ngn(spread_bps: f64, mid_ngn: f64, start_pct: f64) -> f64 {
    4.0 * (spread_bps / 10_000.0) * mid_ngn / (1.0 - start_fraction(start_pct))
}

/// The lift and what it was sized on, for pricing and the module status.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Shift {
    /// The lift at 100% naira, NGN per USDT. At share `f` the curve lifts
    /// by this times [`weight`].
    pub full_ngn: f64,
    /// The gain per dollar against the live feed, NGN per USDT. Negative
    /// when the naira is worth less than it cost.
    pub gain_ngn: f64,
}

/// The lift for one price. Zero for anything it can't trust: a stale
/// snapshot, a gain under the floor, a loss, a non-finite input.
pub fn shift(p: &UnwindParams, snap: &Snapshot, feed_mid_ngn: f64, now: u64) -> Shift {
    if !(feed_mid_ngn.is_finite() && feed_mid_ngn > 0.0 && snap.cost_ngn.is_finite()) {
        return Shift::default();
    }
    let gain_ngn = snap.cost_ngn - feed_mid_ngn;
    let none = Shift {
        full_ngn: 0.0,
        gain_ngn,
    };
    if snap.stale(now, p.refresh_secs) || gain_ngn <= 0.0 || gain_ngn < p.min_gain_ngn {
        return none;
    }
    let full_ngn = p.cap_ngn.max(0.0).min(p.share.clamp(0.0, 1.0) * gain_ngn);
    if !full_ngn.is_finite() {
        return none;
    }
    Shift { full_ngn, gain_ngn }
}

/// The ask's price with the lift, in NGN per USDT the stitch pays out per
/// dollar it takes in (the order price, before the venue's fee).
///
/// The lift may raise the ask towards the cost but never past it, and never
/// below where the plain curve already is: a plain curve that already sells
/// under cost is the curve's choice, and profit unwind adds nothing to it.
/// Raising an ask means more naira per dollar for the taker, so the floor can
/// only cost a round trip more. A cost it can't trust gives the plain price.
pub fn floored_ask_ngn(lifted: f64, plain: f64, cost_ngn: f64) -> f64 {
    if !(cost_ngn.is_finite() && cost_ngn > 0.0) {
        return plain;
    }
    lifted.min(plain.max(cost_ngn))
}

/// Whose naira a basis describes. A saved basis is picked up only by the same
/// holder, on the same chain and pair, marked against the same feed price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub chain_id: u64,
    /// The wallet, or the vault, the balances are read from. Lowercase hex.
    pub address: String,
    /// The corridor's two tokens. Lowercase hex.
    pub collateral: String,
    pub debt: String,
    /// Which feed price marks the naira: `served` or `unadjusted`. Preview
    /// and Live read different ones, so a basis built in one isn't the other's.
    pub price: String,
}

/// One corridor's basis as saved, with whose it is and when it was written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub holder: Holder,
    pub basis: Basis,
    pub saved_at: u64,
}

impl Saved {
    /// Whether `holder` may pick this up at `now`. A basis that sat unwatched
    /// longer than [`stale_after`] (profit unwind off, the bot down) may not
    /// describe what is held now, so it isn't.
    pub fn adoptable(&self, holder: &Holder, now: u64, refresh_secs: u64) -> bool {
        self.holder == *holder
            && self.saved_at <= now
            && now - self.saved_at <= stale_after(refresh_secs)
    }
}

/// One corridor's profit unwind state: whose naira it tracks, the basis it
/// keeps, and the snapshot it prices off.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracker {
    pub holder: Holder,
    pub basis: Basis,
    pub snapshot: Option<Snapshot>,
}

impl Tracker {
    /// A corridor's first read this run. The saved basis carries on when it
    /// is this holder's and recent ([`Saved::adoptable`]); anything else
    /// starts over at the feed, which can lose a gain but never invent one.
    pub fn open(
        saved: Option<&Saved>,
        holder: Holder,
        naira: f64,
        mid_ngn: f64,
        now: u64,
        refresh_secs: u64,
    ) -> Self {
        let basis = saved
            .filter(|s| s.adoptable(&holder, now, refresh_secs))
            .map_or_else(
                || Basis::start(naira, mid_ngn),
                |s| s.basis.observe(naira, mid_ngn),
            );
        Self {
            holder,
            basis,
            snapshot: None,
        }
        .refreshed(now, refresh_secs)
    }

    /// The tracker after a new read, with the snapshot refreshed when due.
    pub fn observe(&self, naira: f64, mid_ngn: f64, now: u64, refresh_secs: u64) -> Self {
        Self {
            basis: self.basis.observe(naira, mid_ngn),
            ..self.clone()
        }
        .refreshed(now, refresh_secs)
    }

    /// What goes to disk.
    pub fn saved(&self, now: u64) -> Saved {
        Saved {
            holder: self.holder.clone(),
            basis: self.basis,
            saved_at: now,
        }
    }

    /// The snapshot after a read: frozen again when due; otherwise kept,
    /// except that a lower cost (new naira diluting it) replaces it at once,
    /// on the same refresh schedule, and holding nothing clears it. A higher
    /// cost waits for the refresh.
    fn refreshed(self, now: u64, refresh_secs: u64) -> Self {
        // Holding nothing has no cost, so there is nothing to unwind.
        let cost = self.basis.cost();
        let snapshot = match (self.snapshot, cost) {
            (Some(s), Some(cost_ngn)) if !s.due(now, refresh_secs) => Some(Snapshot {
                cost_ngn: cost_ngn.min(s.cost_ngn),
                ..s
            }),
            (_, cost) => cost.map(|cost_ngn| Snapshot { cost_ngn, at: now }),
        };
        Self { snapshot, ..self }
    }
}

/// The file the bases live in, next to `rfq-reservations.json`.
pub const BASIS_FILE: &str = "profit-unwind-basis.json";

const BASIS_FILE_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct StoredBases {
    version: u32,
    /// By corridor slug.
    bases: BTreeMap<String, Saved>,
}

/// Every corridor's saved basis. A missing file is a first run; a corrupt one
/// or another version is logged and dropped, which restarts each basis at the
/// feed (the spec's degraded case: the lift waits until a gain builds again).
pub fn load_bases(path: &Path) -> BTreeMap<String, Saved> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<StoredBases>(&raw) {
        Ok(stored) if stored.version == BASIS_FILE_VERSION => stored.bases,
        Ok(stored) => {
            tracing::warn!(version = stored.version, path = %path.display(), "profit unwind basis has an unknown version; starting over at the feed");
            BTreeMap::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "profit unwind basis unreadable; starting over at the feed");
            BTreeMap::new()
        }
    }
}

pub fn save_bases(path: &Path, bases: BTreeMap<String, Saved>) -> anyhow::Result<()> {
    let stored = StoredBases {
        version: BASIS_FILE_VERSION,
        bases,
    };
    crate::setup::write_file_atomic(path, serde_json::to_string(&stored)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID: f64 = 1380.0;

    fn params() -> UnwindParams {
        UnwindParams {
            share: 0.5,
            cap_ngn: 2.0,
            min_gain_ngn: 0.7,
            start_pct: 0.0,
            refresh_secs: 900,
        }
    }
    fn snap(cost: f64) -> Snapshot {
        Snapshot {
            cost_ngn: cost,
            at: 1_000,
        }
    }
    fn holder() -> Holder {
        Holder {
            chain_id: 56,
            address: "0x00000000000000000000000000000000000000a1".into(),
            collateral: "0x00000000000000000000000000000000000000c0".into(),
            debt: "0x00000000000000000000000000000000000000d0".into(),
            price: "unadjusted".into(),
        }
    }
    /// The lift at share `f`, in NGN per USDT.
    fn lift_ngn(s: Shift, f: f64, start_pct: f64) -> f64 {
        s.full_ngn * weight(f, start_pct)
    }

    #[test]
    fn the_spec_worked_example() {
        // Naira taken in at 1386, the feed at 1380: the gain is 43.5 bps,
        // half of it is 3 NGN, so the 2 NGN cap binds. At 75% naira
        // three quarters of it applies: about 1.5 NGN.
        let s = shift(&params(), &snap(1386.0), MID, 1_000);
        assert!((s.gain_ngn - 6.0).abs() < 1e-9);
        assert!((s.full_ngn - 2.0).abs() < 1e-9, "{}", s.full_ngn);
        assert!((lift_ngn(s, 0.75, 0.0) - 1.5).abs() < 1e-9);
        // Start at 50%: half of it at 75% naira, about 1.0 NGN.
        assert!((lift_ngn(s, 0.75, 50.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_share_of_gain_binds_under_the_cap() {
        // A 2 NGN gain at share 0.5 offers 1 NGN, under the 2 NGN cap.
        let s = shift(&params(), &snap(MID + 2.0), MID, 1_000);
        assert!((lift_ngn(s, 1.0, 0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn nothing_moves_on_a_small_gain_a_loss_or_a_stale_snapshot() {
        let p = params();
        assert_eq!(shift(&p, &snap(MID + 0.5), MID, 1_000).full_ngn, 0.0);
        assert_eq!(shift(&p, &snap(MID - 6.0), MID, 1_000).full_ngn, 0.0);
        assert!(shift(&p, &snap(MID - 6.0), MID, 1_000).gain_ngn < 0.0);
        assert!(shift(&p, &snap(1386.0), MID, 1_000 + 1_800).full_ngn > 0.0);
        assert_eq!(shift(&p, &snap(1386.0), MID, 1_000 + 1_801).full_ngn, 0.0);
        assert_eq!(shift(&p, &snap(1386.0), f64::NAN, 1_000).full_ngn, 0.0);
    }

    #[test]
    fn the_gain_follows_the_feed_live() {
        // Same frozen cost, two prices: the lift tracks the feed.
        let p = params();
        let near = shift(&p, &snap(1386.0), 1385.0, 1_000);
        let far = shift(&p, &snap(1386.0), 1383.0, 1_000);
        assert!(far.full_ngn > near.full_ngn && near.full_ngn > 0.0);
    }

    #[test]
    fn the_weight_starts_at_the_start_and_rises_evenly() {
        assert_eq!(weight(0.5, 50.0), 0.0);
        assert!((weight(0.75, 50.0) - 0.5).abs() < 1e-12);
        assert_eq!(weight(1.0, 50.0), 1.0);
        assert!((weight(0.3, 0.0) - 0.3).abs() < 1e-12);
        // Continuous across the start: no jump.
        assert!(weight(0.5001, 50.0) < 0.001);
    }

    #[test]
    fn the_average_weight_is_the_mean_over_the_stretch() {
        // Against a midpoint-rule integral, across the start and either side.
        let numeric = |lo: f64, hi: f64, start: f64| {
            let n = 100_000;
            let h = (hi - lo) / f64::from(n);
            (0..n)
                .map(|i| weight(lo + (f64::from(i) + 0.5) * h, start))
                .sum::<f64>()
                / f64::from(n)
        };
        for start in [0.0, 50.0, 90.0] {
            for (lo, hi) in [
                (0.0, 1.0),
                (0.3, 0.6),
                (0.45, 0.95),
                (0.9, 0.99),
                (0.1, 0.2),
            ] {
                let got = average_weight(lo, hi, start);
                assert!(
                    (got - numeric(lo, hi, start)).abs() < 1e-8,
                    "{start} {lo}..{hi}"
                );
                // Order-free, like the curve's own average.
                assert_eq!(got, average_weight(hi, lo, start));
            }
        }
        // A zero-width stretch is the point weight.
        assert_eq!(average_weight(0.8, 0.8, 50.0), weight(0.8, 50.0));
        // Entirely below the start: nothing.
        assert_eq!(average_weight(0.1, 0.4, 50.0), 0.0);
    }

    #[test]
    fn the_ask_floor_stops_the_lift_at_the_cost() {
        // The lift raises the ask from 1381.0 to 1382.0; the cost is 1381.5,
        // so the ask stops there.
        assert_eq!(floored_ask_ngn(1382.0, 1381.0, 1381.5), 1381.5);
        // Under the cost the lift goes through.
        assert_eq!(floored_ask_ngn(1381.2, 1381.0, 1381.5), 1381.2);
        // A plain curve already past the cost keeps its own price: the lift
        // adds nothing, and the floor doesn't pull the curve back either.
        assert_eq!(floored_ask_ngn(1383.0, 1382.0, 1381.5), 1382.0);
        // No lift, no change.
        assert_eq!(floored_ask_ngn(1381.0, 1381.0, 1381.5), 1381.0);
        assert_eq!(floored_ask_ngn(1382.0, 1381.0, f64::NAN), 1381.0);
    }

    #[test]
    fn naira_in_enters_at_the_feed_and_naira_out_leaves_at_average_cost() {
        let b = Basis::start(0.0, MID);
        // In at 1380: whatever the dollar side paid, the naira is marked at
        // the feed, so the stitch's spread is never counted as gain.
        let b = b.observe(1_380_000.0, MID);
        assert!((b.cost().unwrap() - MID).abs() < 1e-9);
        // Sell half: the cost per naira doesn't move.
        let b = b.observe(690_000.0, 1375.0);
        assert!((b.cost().unwrap() - MID).abs() < 1e-9);
        // More in at 1390 blends in.
        let b = b.observe(690_000.0 + 1_390_000.0, 1390.0);
        let want = (690_000.0 + 1_390_000.0) / (500.0 + 1_000.0);
        assert!((b.cost().unwrap() - want).abs() < 1e-9);
        // A move under dust changes nothing, a bad feed changes nothing.
        assert_eq!(b.observe(b.naira + 1e-9, 1000.0), b);
        assert_eq!(b.observe(b.naira + 5.0, f64::NAN), b);
    }

    #[test]
    fn a_taker_can_only_pull_the_cost_towards_the_feed() {
        // Holding naira at a 1386 cost with the feed at 1380: any naira a
        // taker sells in lands at 1380, so the gain shrinks, never grows.
        let b = Basis {
            naira: 1_386_000.0,
            dollars: 1_000.0,
        };
        for added in [1.0, 1_000.0, 1_380_000.0, 50_000_000.0] {
            let after = b.observe(b.naira + added, MID).cost().unwrap();
            assert!(after < 1386.0 && after > MID, "{added}: {after}");
        }
        // And a taker buying naira out leaves the cost where it was.
        assert_eq!(b.observe(500_000.0, MID).cost(), b.cost());
    }

    #[test]
    fn emptying_the_naira_resets_the_basis() {
        let b = Basis::start(1_380_000.0, MID).observe(0.0, MID);
        assert_eq!(b, Basis::EMPTY);
        assert!(b.cost().is_none());
        // Refilling starts a fresh cost.
        let b = b.observe(1_390_000.0, 1390.0);
        assert!((b.cost().unwrap() - 1390.0).abs() < 1e-9);
        // A first read at a bad feed holds nothing, then takes the naira in
        // at the next good one.
        let b = Basis::start(1_390_000.0, 0.0);
        assert_eq!(b, Basis::EMPTY);
        assert!((b.observe(1_390_000.0, 1390.0).cost().unwrap() - 1390.0).abs() < 1e-9);
    }

    #[test]
    fn the_snapshot_freezes_the_cost_until_the_refresh() {
        let t = Tracker::open(None, holder(), 1_386_000.0, 1386.0, 1_000, 900);
        let first = t.snapshot.unwrap();
        assert!((first.cost_ngn - 1386.0).abs() < 1e-9);
        // Naira in at a weaker feed raises the basis, but not the snapshot:
        // a higher cost waits for the refresh.
        let t = t.observe(1_386_000.0 + 1_390_000.0, 1390.0, 1_300, 900);
        assert!(t.basis.cost().unwrap() > first.cost_ngn);
        assert_eq!(t.snapshot.unwrap(), first);
        // At the refresh it catches up.
        let t = t.observe(1_386_000.0 + 1_390_000.0, 1390.0, 1_900, 900);
        let next = t.snapshot.unwrap();
        assert!(next.cost_ngn > first.cost_ngn && next.at == 1_900);
        // Emptied, there is nothing to price off, straight away.
        let t = t.observe(0.0, 1380.0, 2_000, 900);
        assert!(t.snapshot.is_none());
    }

    #[test]
    fn new_naira_that_dilutes_the_cost_lowers_the_snapshot_at_once() {
        // 1 naira carried at 1400, then 1,000 more at a 1380 feed: the basis
        // drops to about 1380.02, and the snapshot follows it before the
        // refresh, so the ask floor never sits above the naira now held. The
        // refresh schedule doesn't move.
        let t = Tracker::open(None, holder(), 1.0, 1400.0, 1_000, 900);
        let t = t.observe(1_001.0, 1380.0, 1_300, 900);
        let snap = t.snapshot.unwrap();
        assert_eq!(snap.cost_ngn, t.basis.cost().unwrap());
        assert!((snap.cost_ngn - 1380.02).abs() < 0.01, "{}", snap.cost_ngn);
        assert_eq!(snap.at, 1_000);
        // The lift is sized on the lowered cost: under the floor at 1380.
        let s = shift(&params(), &snap, 1380.0, 1_300);
        assert_eq!(s.full_ngn, 0.0);
    }

    #[test]
    fn a_saved_basis_carries_on_only_for_its_own_holder_and_while_recent() {
        let saved = Saved {
            holder: holder(),
            basis: Basis {
                naira: 1_386_000.0,
                dollars: 1_000.0,
            },
            saved_at: 10_000,
        };
        let reopen = |h: Holder, now: u64| {
            Tracker::open(Some(&saved), h, 1_386_000.0, MID, now, 900)
                .basis
                .cost()
                .unwrap()
        };
        // Same holder, saved a minute ago: the 1386 cost carries on.
        assert!((reopen(holder(), 10_060) - 1386.0).abs() < 1e-9);
        // Anything else starts over at the feed: no gain.
        let other = |f: fn(&mut Holder)| {
            let mut h = holder();
            f(&mut h);
            h
        };
        for h in [
            other(|h| h.chain_id = 42_220),
            other(|h| h.address = "0x00000000000000000000000000000000000000b2".into()),
            other(|h| h.collateral = "0x00000000000000000000000000000000000000c9".into()),
            other(|h| h.price = "served".into()),
        ] {
            assert!((reopen(h, 10_060) - MID).abs() < 1e-9);
        }
        // Unwatched past twice the refresh (unwind switched off, bot down).
        assert!((reopen(holder(), 10_000 + 1_801) - MID).abs() < 1e-9);
        assert!((reopen(holder(), 10_000 + 1_800) - 1386.0).abs() < 1e-9);
        // A clock behind the file is not trusted either.
        assert!((reopen(holder(), 9_999) - MID).abs() < 1e-9);
    }

    #[test]
    fn the_basis_survives_a_restart_and_a_bad_file_starts_over() {
        let dir = std::env::temp_dir().join(format!("unwind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(BASIS_FILE);
        let t = Tracker::open(None, holder(), 1_386_000.0, 1386.0, 5, 900);
        let bases = BTreeMap::from([("cngn-usdt".to_string(), t.saved(5))]);
        save_bases(&path, bases.clone()).unwrap();
        assert_eq!(load_bases(&path), bases);
        // The first file format carried no holder: it starts over.
        std::fs::write(
            &path,
            r#"{"version":1,"bases":{"cngn-usdt":{"naira":1.0,"dollars":1.0,"last_dollars":0.0}}}"#,
        )
        .unwrap();
        assert!(load_bases(&path).is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(load_bases(&path).is_empty());
        assert!(load_bases(&dir.join("missing.json")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
