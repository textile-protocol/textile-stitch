// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The cNGN quote curve: a stitch-pricing centre that moves on the stitch's
//! own naira inventory, and prices each quote across the stretch of inventory
//! that trade moves it through rather than at the point it starts from.
//!
//! # Units
//!
//! The operator, the reference book and both endpoints live in **NGN per
//! USDT**. The bot's order amounts live in the reciprocal, **USDT per cNGN**.
//! Everything in here is computed in NGN per USDT, crossing the boundary at
//! exactly two doors: the feed's reference mid is inverted INTO NGN once, in
//! [`Curve::new`], and the finished price is inverted BACK once, in
//! [`Curve::price`]. No formula between the two ever mixes units.
//!
//! That is not a style choice. A shade that raises the centre in NGN per USDT
//! lowers it in USDT per cNGN, so a formula mirrored into the bot's units
//! turns every sign around and leans the curve *into* naira instead of out of
//! it. One inversion, at the boundary, asserted by the endpoint tests below.
//!
//! # The rule
//!
//! Two knobs set where the curve runs, each an offset in NGN from the feed's
//! reference book (`reference` on the price feed; Bybit P2P for cNGN today):
//!
//! ```text
//! at f = 0  (no naira)    the stitch SELLS USDT at  referenceMid - start_below_mid
//! at f = 1  (all naira)   the stitch BUYS  USDT at  referenceBuy + end_above_buy
//! ```
//!
//! Both default to zero and both only ever widen the curve. Those are
//! *all-in* prices, after the spread and the venue fee, so they are what a
//! taker actually sees. Everything else is derived from them.
//!
//! Note `s0` is **negative**: at zero knobs the curve starts one spread below
//! the mid, which is exactly what lands the all-in selling price on the mid. A
//! shade that starts at zero is wrong by a full spread across the bottom of
//! the range, and a round-trip test will not catch it — only the endpoints
//! will.

/// How far a quote's own size moves the share before the price is taken, and
/// the shape of the curve in between. Per corridor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurveParams {
    /// NGN below the reference mid where the curve starts. Zero or more.
    pub start_below_mid: f64,
    /// NGN above the reference buy where the curve ends. Zero or more.
    pub end_above_buy: f64,
    /// Exponent on the inventory share. [`DEFAULT_SHAPE`].
    pub shape: f64,
}

/// Keeps the stitch near the reference mid through normal inventory and only
/// defends at the top: at half full the curve has spent a quarter of its span.
pub const DEFAULT_SHAPE: f64 = 2.0;

impl Default for CurveParams {
    fn default() -> Self {
        Self {
            start_below_mid: 0.0,
            end_above_buy: 0.0,
            shape: DEFAULT_SHAPE,
        }
    }
}

/// A curve sized for one tick's reference book and one request's all-in cost.
///
/// Built per quote rather than held, because `all_in_bps` carries the venue
/// fee off the request and the reference book arrives on the feed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Curve {
    /// The raw reference mid, NGN per USDT.
    mid: f64,
    /// Shade at `f = 0`, bps. Negative at zero knobs.
    s0: f64,
    /// Shade at `f = 1`, bps.
    s1: f64,
    shape: f64,
    all_in_bps: f64,
}

impl Curve {
    /// Build the curve for this tick, or `None` when the inputs cannot
    /// produce one and the caller must quote flat.
    ///
    /// `reference_mid` is in the bot's own units (debt per collateral), as the
    /// feed serves it; it is inverted here once and not again.
    pub fn new(
        reference_mid: f64,
        half_range_bps: f64,
        all_in_bps: f64,
        p: &CurveParams,
    ) -> Option<Self> {
        if !reference_mid.is_finite() || reference_mid <= 0.0 {
            return None;
        }
        if !half_range_bps.is_finite() || half_range_bps < 0.0 {
            return None;
        }
        if !all_in_bps.is_finite() || !(0.0..10_000.0).contains(&all_in_bps) {
            return None;
        }
        if !p.shape.is_finite() || p.shape <= 0.0 {
            return None;
        }
        if !p.start_below_mid.is_finite()
            || p.start_below_mid < 0.0
            || !p.end_above_buy.is_finite()
            || p.end_above_buy < 0.0
        {
            return None;
        }
        // The one inversion. Everything below is NGN per USDT.
        let mid = 1.0 / reference_mid;
        let buy = mid * (1.0 + half_range_bps / 10_000.0);
        let start = mid - p.start_below_mid;
        if start <= 0.0 {
            return None;
        }
        let s0 = ((start / (mid * (1.0 + all_in_bps / 10_000.0))) - 1.0) * 10_000.0;
        let s1 =
            (((buy + p.end_above_buy) / (mid * (1.0 - all_in_bps / 10_000.0))) - 1.0) * 10_000.0;
        // A crossed reference, or knobs that leave the end below the start,
        // is not a curve. The caller quotes flat.
        (s0.is_finite() && s1.is_finite() && s1 > s0).then_some(Self {
            mid,
            s0,
            s1,
            shape: p.shape,
            all_in_bps,
        })
    }

    /// Shade at one inventory share, bps off the reference mid.
    pub fn shade_bps(&self, f: f64) -> f64 {
        self.s0 + (self.s1 - self.s0) * f.clamp(0.0, 1.0).powf(self.shape)
    }

    /// The shade a quote actually pays: the mean over the stretch of share it
    /// moves the stitch through, not the shade at the share it started from.
    ///
    /// **This is what makes the design safe rather than merely nice.** Quoting
    /// the starting shade leaves a round trip open: trade in the direction the
    /// stitch is loaded, which moves the share and improves the other side,
    /// then trade straight back. Averaging closes it structurally — out and
    /// back integrate to the same number, so the shade cancels and a round
    /// trip nets exactly minus the spread, at every size and every starting
    /// share, with no rate limits and no timing guards.
    ///
    /// Order does not matter, by construction: it is the mean value of the
    /// shade over the interval. A zero-width stretch has no mean, so it falls
    /// back to the point shade.
    pub fn average_shade_bps(&self, f0: f64, f1: f64) -> f64 {
        let (lo, hi) = (f0.clamp(0.0, 1.0), f1.clamp(0.0, 1.0));
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let width = hi - lo;
        if width <= f64::EPSILON {
            return self.shade_bps(lo);
        }
        let p = self.shape + 1.0;
        self.s0 + (self.s1 - self.s0) * (hi.powf(p) - lo.powf(p)) / (p * width)
    }

    /// The price for a quote that moves the share from `f0` to `f1`, in the
    /// bot's own units (debt per collateral), carrying `order_half_bps`
    /// around the curved centre.
    ///
    /// **`order_half_bps` is the half-width of the SIGNED ORDER — the
    /// corridor spread alone, never the all-in.** The venue injects its fee
    /// on top of the order at fill time, so an order that carried the all-in
    /// would charge the taker the fee twice and miss the knob endpoints by
    /// exactly `fee_bps`. The all-in's place is [`Curve::new`], where it
    /// sizes `s0`/`s1` so that the TAKER-REALIZED price — the order with the
    /// venue fee injected — is what lands on the knobs. Pass the all-in here
    /// only to compute that realized price directly, as the tests do.
    ///
    /// `bid` is the stitch buying collateral — it pays debt out and takes
    /// naira in, so the share rises. In NGN per USDT that is the stitch
    /// *selling* dollars, the upper of the two prices; inverted it is the
    /// lower one, which is what a bid must be.
    pub fn price(&self, bid: bool, f0: f64, f1: f64, order_half_bps: f64) -> Option<f64> {
        if !order_half_bps.is_finite() || !(0.0..10_000.0).contains(&order_half_bps) {
            return None;
        }
        let centre = self.mid * (1.0 + self.average_shade_bps(f0, f1) / 10_000.0);
        let half = order_half_bps / 10_000.0;
        // NGN per USDT: selling dollars is the dearer side.
        let ngn = if bid {
            centre * (1.0 + half)
        } else {
            centre * (1.0 - half)
        };
        (ngn.is_finite() && ngn > 0.0).then(|| 1.0 / ngn)
    }

    /// Where the stitch stops being the cheapest dollars in the market: the
    /// share at which its all-in selling price reaches the Bybit buy. For the
    /// shadow log and for tuning the exponent, not for pricing.
    pub fn cheapest_until(&self, half_range_bps: f64) -> f64 {
        let target = ((1.0 + half_range_bps / 10_000.0) / (1.0 + self.all_in_bps / 10_000.0) - 1.0)
            * 10_000.0;
        let t = (target - self.s0) / (self.s1 - self.s0);
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        t.powf(1.0 / self.shape)
    }

    /// Shade at the two ends, for the shadow log.
    pub fn endpoints_bps(&self) -> (f64, f64) {
        (self.s0, self.s1)
    }
}

/// The stitch's naira share and the total it is a share of, both valued in
/// debt units. `None` when the wallet is empty or the reference is unusable —
/// there is no share to lean on and the caller must quote flat.
///
/// `reference_mid` is debt per collateral, as the feed serves it.
pub fn inventory_share(
    collateral_atomic: u128,
    collateral_decimals: u8,
    debt_atomic: u128,
    debt_decimals: u8,
    reference_mid: f64,
) -> Option<(f64, f64)> {
    if !reference_mid.is_finite() || reference_mid <= 0.0 {
        return None;
    }
    let collateral = collateral_atomic as f64 / 10f64.powi(i32::from(collateral_decimals));
    let debt = debt_atomic as f64 / 10f64.powi(i32::from(debt_decimals));
    let collateral_value = collateral * reference_mid;
    let total = collateral_value + debt;
    (total.is_finite() && total > 0.0).then(|| (collateral_value / total, total))
}

/// How far a size in debt units moves the share on a wallet worth `total`.
///
/// A fill at fair moves value from one leg to the other without changing the
/// total, so the share moves by exactly the trade's share of the wallet.
pub fn share_step(size_debt: f64, total_debt: f64) -> f64 {
    if !total_debt.is_finite() || total_debt <= 0.0 || !size_debt.is_finite() {
        return 0.0;
    }
    (size_debt / total_debt).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID_NGN: f64 = 1380.78;
    const HALF_RANGE: f64 = 9.40;
    const ALL_IN: f64 = 2.0;

    fn dpc() -> f64 {
        1.0 / MID_NGN
    }
    fn buy_ngn() -> f64 {
        MID_NGN * (1.0 + HALF_RANGE / 10_000.0)
    }
    fn curve(start: f64, end: f64) -> Curve {
        Curve::new(
            dpc(),
            HALF_RANGE,
            ALL_IN,
            &CurveParams {
                start_below_mid: start,
                end_above_buy: end,
                shape: DEFAULT_SHAPE,
            },
        )
        .expect("priceable")
    }
    /// The stitch's all-in price for selling dollars, back in NGN per USDT:
    /// the order price with the venue fee already realized, which is what
    /// must land on the knobs (here spread + fee are passed together).
    fn sells_usdt(c: &Curve, f: f64) -> f64 {
        1.0 / c.price(true, f, f, ALL_IN).unwrap()
    }
    /// The stitch's all-in price for buying dollars, back in NGN per USDT.
    fn buys_usdt(c: &Curve, f: f64) -> f64 {
        1.0 / c.price(false, f, f, ALL_IN).unwrap()
    }

    #[test]
    fn the_curve_starts_and_ends_exactly_on_the_knobs() {
        // The test that catches both likely errors: inverted units, and a
        // shade that starts at zero instead of s0.
        for (start, end) in [(0.0, 0.0), (0.5, 1.0), (0.0, 2.0), (1.25, 0.25)] {
            let c = curve(start, end);
            let got_start = sells_usdt(&c, 0.0);
            let got_end = buys_usdt(&c, 1.0);
            let want_start = MID_NGN - start;
            let want_end = buy_ngn() + end;
            assert!(
                ((got_start / want_start - 1.0) * 10_000.0).abs() < 0.01,
                "start {start}/{end}: {got_start} vs {want_start}"
            );
            assert!(
                ((got_end / want_end - 1.0) * 10_000.0).abs() < 0.01,
                "end {start}/{end}: {got_end} vs {want_end}"
            );
        }
    }

    #[test]
    fn the_shade_starts_one_spread_below_the_mid() {
        // s0 is negative, and that is what puts the selling price on the mid.
        let c = curve(0.0, 0.0);
        let (s0, s1) = c.endpoints_bps();
        assert!((s0 + ALL_IN).abs() < 0.01, "s0 = {s0}, expected about -2");
        assert!(
            (s1 - (HALF_RANGE + ALL_IN)).abs() < 0.01,
            "s1 = {s1}, expected about 11.4"
        );
        // Span is halfRange + 2 x allIn, not halfRange + allIn.
        assert!((s1 - s0 - (HALF_RANGE + 2.0 * ALL_IN)).abs() < 0.01);
    }

    #[test]
    fn the_average_is_order_free() {
        let c = curve(0.0, 0.0);
        for (a, b) in [(0.0, 0.18), (0.3, 0.55), (0.62, 0.95), (0.1, 0.9)] {
            let out = c.average_shade_bps(a, b);
            let back = c.average_shade_bps(b, a);
            assert!((out - back).abs() < 1e-12, "{a}..{b}: {out} vs {back}");
        }
        // A zero-width stretch is the point shade.
        assert!((c.average_shade_bps(0.6, 0.6) - c.shade_bps(0.6)).abs() < 1e-12);
        // And it lies strictly between the endpoints it walks.
        let avg = c.average_shade_bps(0.4, 0.8);
        assert!(avg > c.shade_bps(0.4) && avg < c.shade_bps(0.8));
    }

    #[test]
    fn a_round_trip_costs_the_taker_the_spread_at_every_size_and_share() {
        // The whole safety argument, swept. Includes the edges a naive sweep
        // skips: f0 at 0 and 1, and trades spanning the whole book.
        for (start, end) in [(0.0, 0.0), (0.5, 1.0), (3.0, 3.0)] {
            let c = curve(start, end);
            let mut worst = f64::NEG_INFINITY;
            for i in 0..=100 {
                let f0 = f64::from(i) / 100.0;
                for j in 1..=100 {
                    let t = f64::from(j) / 100.0;
                    // Out and back over the same stretch, both directions.
                    if f0 + t <= 1.0 {
                        let f1 = f0 + t;
                        let out = c.price(true, f0, f1, ALL_IN).unwrap();
                        let back = c.price(false, f1, f0, ALL_IN).unwrap();
                        worst = worst.max((out / back - 1.0) * 10_000.0);
                    }
                    if f0 - t >= 0.0 {
                        let f1 = f0 - t;
                        let out = c.price(false, f0, f1, ALL_IN).unwrap();
                        let back = c.price(true, f1, f0, ALL_IN).unwrap();
                        worst = worst.max((back / out - 1.0) * 10_000.0);
                    }
                }
            }
            assert!(
                worst < 0.0,
                "knobs {start}/{end}: a round trip paid the taker {worst} bps"
            );
            assert!(
                (worst + 2.0 * ALL_IN).abs() < 0.05,
                "knobs {start}/{end}: expected about -{} bps, got {worst}",
                2.0 * ALL_IN
            );
        }
    }

    #[test]
    fn consecutive_quotes_on_one_side_price_the_same_as_one_big_one() {
        // Reservations hand out disjoint stretches, so whoever executes
        // first, the pieces must sum to the whole move. Exact, not close.
        let c = curve(0.0, 0.0);
        let (f0, a, b) = (0.5, 0.2, 0.15);
        let first = c.price(true, f0, f0 + a, ALL_IN).unwrap();
        let second = c.price(true, f0 + a, f0 + a + b, ALL_IN).unwrap();
        // Collateral out for equal debt slices, versus one quote for both.
        let split = a / first + b / second;
        let whole = (a + b) / c.price(true, f0, f0 + a + b, ALL_IN).unwrap();
        assert!(
            ((split / whole - 1.0) * 10_000.0).abs() < 1e-6,
            "split {split} vs whole {whole}"
        );
    }

    #[test]
    fn a_crossed_reference_has_no_curve() {
        // The Bybit P2P book is crossed on roughly a third of ticks. The
        // caller must quote flat rather than lean backwards.
        assert!(Curve::new(dpc(), -1.0, ALL_IN, &CurveParams::default()).is_none());
        // The knobs only ever widen, so they cannot cross the endpoints — but
        // a start knob that swallows the whole mid leaves no price at all.
        let swallowed = CurveParams {
            start_below_mid: MID_NGN,
            end_above_buy: 0.0,
            shape: DEFAULT_SHAPE,
        };
        assert!(Curve::new(dpc(), HALF_RANGE, ALL_IN, &swallowed).is_none());
        // While a merely silly knob is a wide curve, not a refusal.
        let silly = CurveParams {
            start_below_mid: 500.0,
            end_above_buy: 0.0,
            shape: DEFAULT_SHAPE,
        };
        assert!(Curve::new(dpc(), HALF_RANGE, ALL_IN, &silly).is_some());
    }

    #[test]
    fn unusable_inputs_produce_no_curve() {
        let p = CurveParams::default();
        assert!(Curve::new(0.0, HALF_RANGE, ALL_IN, &p).is_none());
        assert!(Curve::new(f64::NAN, HALF_RANGE, ALL_IN, &p).is_none());
        assert!(Curve::new(dpc(), f64::INFINITY, ALL_IN, &p).is_none());
        assert!(Curve::new(dpc(), HALF_RANGE, -1.0, &p).is_none());
        let bad_shape = CurveParams {
            shape: 0.0,
            ..CurveParams::default()
        };
        assert!(Curve::new(dpc(), HALF_RANGE, ALL_IN, &bad_shape).is_none());
        let negative_knob = CurveParams {
            start_below_mid: -1.0,
            ..CurveParams::default()
        };
        assert!(Curve::new(dpc(), HALF_RANGE, ALL_IN, &negative_knob).is_none());
    }

    #[test]
    fn the_share_spans_zero_to_one_and_carries_the_total() {
        // 1_000 cNGN at 1/1380.78 USDT each, against 1_000 USDT.
        let (f, v) = inventory_share(1_000_000_000, 6, 1_000_000_000, 6, dpc()).unwrap();
        assert!(f > 0.0 && f < 1.0);
        assert!((v - (1_000.0 * dpc() + 1_000.0)).abs() < 1e-9);
        // All one leg or the other.
        assert_eq!(inventory_share(0, 6, 1_000_000, 6, dpc()).unwrap().0, 0.0);
        assert_eq!(inventory_share(1_000_000, 6, 0, 6, dpc()).unwrap().0, 1.0);
        // Decimals are normalised, not assumed equal.
        let (f18, _) = inventory_share(1_000_000_000_000_000_000, 18, 1_000_000, 6, 1.0).unwrap();
        assert!((f18 - 0.5).abs() < 1e-12);
        // Nothing to lean on.
        assert!(inventory_share(0, 6, 0, 6, dpc()).is_none());
        assert!(inventory_share(1, 6, 1, 6, 0.0).is_none());
    }

    #[test]
    fn a_fill_moves_the_share_by_its_own_share_of_the_wallet() {
        assert!((share_step(30_000.0, 300_000.0) - 0.1).abs() < 1e-12);
        assert_eq!(share_step(1.0, 0.0), 0.0);
        assert_eq!(share_step(f64::NAN, 100.0), 0.0);
    }

    #[test]
    fn invariants_hold_over_a_randomized_sweep() {
        // A deterministic pseudo-random sweep (seeded LCG, no clock, no rand
        // dependency) across the whole input space the hand-picked cases
        // cannot cover: 2,000 markets x several trades each. Every invariant
        // here is one of the design's safety claims.
        let mut state = 0x5eed_cafe_f00d_u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64) / ((1u64 << 53) as f64)
        };
        for case in 0..2_000 {
            let mid_ngn = 50.0 + next() * 19_950.0;
            let half = next() * 80.0;
            let all_in = 0.5 + next() * 79.5;
            let p = CurveParams {
                start_below_mid: next() * mid_ngn * 0.02,
                end_above_buy: next() * mid_ngn * 0.02,
                shape: 0.5 + next() * 3.5,
            };
            let Some(c) = Curve::new(1.0 / mid_ngn, half, all_in, &p) else {
                panic!("case {case}: valid inputs must make a curve");
            };
            // Endpoints land on the knobs, by construction, to f64 accuracy.
            let sell0 = 1.0 / c.price(true, 0.0, 0.0, all_in).unwrap();
            let buy1 = 1.0 / c.price(false, 1.0, 1.0, all_in).unwrap();
            let want0 = mid_ngn - p.start_below_mid;
            let want1 = mid_ngn * (1.0 + half / 10_000.0) + p.end_above_buy;
            assert!(((sell0 - want0) / want0).abs() < 1e-9, "case {case}: start");
            assert!(((buy1 - want1) / want1).abs() < 1e-9, "case {case}: end");
            // The average is order-free, and a round trip at the realized
            // (all-in) price always costs the taker about twice the all-in.
            let f0 = next();
            let t = next() * (1.0 - f0);
            let f1 = f0 + t;
            assert!(
                (c.average_shade_bps(f0, f1) - c.average_shade_bps(f1, f0)).abs() < 1e-9,
                "case {case}: order"
            );
            let out = c.price(true, f0, f1, all_in).unwrap();
            let back = c.price(false, f1, f0, all_in).unwrap();
            let pnl_bps = (out / back - 1.0) * 10_000.0;
            assert!(pnl_bps < 0.0, "case {case}: taker profited {pnl_bps}");
            assert!(
                (pnl_bps + 2.0 * all_in).abs() < 0.02 * all_in + 0.01,
                "case {case}: round trip {pnl_bps} vs -{}",
                2.0 * all_in
            );
            // Splitting a move never changes what it pays in total.
            let cut = f0 + t * next();
            if t > 1e-9 {
                let a = (cut - f0) / c.price(true, f0, cut, all_in).unwrap();
                let b = (f1 - cut) / c.price(true, cut, f1, all_in).unwrap();
                let whole = (f1 - f0) / c.price(true, f0, f1, all_in).unwrap();
                assert!(
                    ((a + b) / whole - 1.0).abs() < 1e-9,
                    "case {case}: split pays differently from whole"
                );
            }
            // More naira marks cNGN down: both prices fall as the share rises.
            let (lo, hi) = (f0.min(f1), f0.max(f1));
            if hi - lo > 1e-6 {
                for bid in [true, false] {
                    let near = c.price(bid, lo, lo, all_in).unwrap();
                    let far = c.price(bid, hi, hi, all_in).unwrap();
                    assert!(far < near, "case {case}: price must fall as f rises");
                }
            }
        }
    }

    #[test]
    #[allow(clippy::excessive_precision)] // pinned verbatim from the model's 17-digit print
    fn golden_values_match_the_independent_model() {
        // Pinned from a separate implementation of the spec's maths (the
        // design simulator, JavaScript, NGN-space with one inversion),
        // computed to 17 significant digits. Agreement here is what the loose
        // bps tolerances elsewhere cannot give: a dropped s0 term (2 bps), a
        // mirrored sign (~19 bps) or a double inversion all miss by orders of
        // magnitude more than the 1e-9 relative bound.
        let close = |got: f64, want: f64, what: &str| {
            assert!(
                ((got - want) / want).abs() < 1e-9,
                "{what}: got {got:.17}, want {want:.17}"
            );
        };
        let c0 = curve(0.0, 0.0);
        let (s0, s1) = c0.endpoints_bps();
        close(s0, -1.999_600_079_983_787_6, "zero knobs s0");
        close(s1, 11.402_280_456_089_819, "zero knobs s1");
        close(
            c0.price(true, 0.37, 0.52, ALL_IN).unwrap(),
            0.000_724_034_324_518_878_70,
            "zero knobs bid 0.37->0.52",
        );
        close(
            c0.price(false, 0.80, 0.62, ALL_IN).unwrap(),
            0.000_724_026_221_741_098_15,
            "zero knobs ask 0.80->0.62",
        );
        close(
            c0.average_shade_bps(0.30, 0.55),
            0.490_916_052_969_891_05,
            "zero knobs avg shade 0.30..0.55",
        );
        let c1 = curve(0.5, 1.0);
        close(
            c1.price(true, 0.0, 0.001, ALL_IN).unwrap(),
            0.000_724_490_682_463_469_25,
            "knobs .5/1 bid 0.00->0.001",
        );
        close(
            c1.price(false, 1.0, 0.98, ALL_IN).unwrap(),
            0.000_723_059_848_690_340_62,
            "knobs .5/1 ask 1.00->0.98",
        );
        close(
            c1.shade_bps(0.9),
            14.035_466_807_876_642,
            "knobs .5/1 point shade at 0.9",
        );
    }

    #[test]
    fn the_stitch_is_the_cheapest_dollars_well_past_half_full() {
        // The exponent is doing real work: at f^2 the line sits around 84%.
        let c = curve(0.0, 0.0);
        let f = c.cheapest_until(HALF_RANGE);
        assert!((0.80..0.88).contains(&f), "cheapest until {f}");
        // Wider knobs reach past the Bybit buy, so the crossing of that line
        // comes EARLIER even though the curve starts cheaper.
        let wide = curve(0.5, 1.0);
        assert!(wide.cheapest_until(HALF_RANGE) < f);
    }
}
