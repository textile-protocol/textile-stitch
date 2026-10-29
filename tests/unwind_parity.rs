// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The panel redraws profit unwind's lift in TypeScript (`web/src/unwind.ts`
//! and `web/src/curve.ts`), on the inventory curve and on a flat price. This
//! test runs seeded cases through the bot's own `unwind::shift`,
//! `unwind::average_weight`, `Curve::with_lift` and
//! `unwind::lifted_flat_price` and pins the results in
//! `web/src/unwindParity.json`, which the web tests replay against the panel.
//! A change on either side fails one of the two suites.
//!
//! Regenerate after an intended change with
//! `STITCH_WRITE_PARITY=1 cargo test --test unwind_parity`.
use serde::{Deserialize, Serialize};
use stitch_bot::pricing::curve::{Curve, CurveParams};
use stitch_bot::pricing::unwind::{
    average_weight, lift_limit_ngn, lifted_flat_price, shift, Lift, Snapshot, UnwindParams,
};

const CASES: usize = 60;
const SEED: u64 = 0x0b1d_5eed;
const NOW: u64 = 1_000_000;

/// splitmix64: deterministic everywhere, no dependency on a rand version.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn between(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.next() as usize % items.len()]
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Knobs {
    share: f64,
    cap_ngn: f64,
    min_gain_ngn: f64,
    start_pct: f64,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct CurveKnobs {
    start_below_mid: f64,
    end_above_buy: f64,
    shape: f64,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Reference {
    mid: f64,
    half_range_bps: f64,
}

/// One case: the inputs, then what the bot computes from them. Prices are
/// what the taker realizes, NGN per USDT: the signed order (spread only, the
/// ask floored there) with the venue's fee on top.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Case {
    knobs: Knobs,
    cost: f64,
    feed_mid: f64,
    reference: Reference,
    spread_bps: f64,
    fee_bps: f64,
    curve: CurveKnobs,
    stretch: [f64; 2],
    full_ngn: f64,
    gain: f64,
    average_weight: f64,
    bid: f64,
    ask: f64,
    plain_ask: f64,
    /// The same lift on a flat price around the feed, held to the flat
    /// round-trip limit at `spread_bps`: the signed order's price, NGN per
    /// USDT, before the venue's fee.
    flat_bid: f64,
    flat_ask: f64,
}

/// A seeded case. `floored` builds one where the ask floor binds: a heavy
/// naira share, a full share of the gain, and a cost just over the plain ask.
fn case(rng: &mut Rng, floored: bool) -> Case {
    let feed_mid = rng.between(1300.0, 1500.0);
    let knobs = if floored {
        Knobs {
            share: 1.0,
            cap_ngn: 5.0,
            min_gain_ngn: 0.0,
            start_pct: 0.0,
        }
    } else {
        Knobs {
            share: rng.pick(&[0.0, 0.25, 0.5, 1.0]),
            cap_ngn: rng.pick(&[0.0, 1.0, 2.0, 5.0]),
            min_gain_ngn: rng.pick(&[0.0, 0.7, 2.0]),
            start_pct: rng.pick(&[0.0, 30.0, 50.0, 90.0, 99.0]),
        }
    };
    let reference = Reference {
        mid: if floored {
            feed_mid
        } else {
            feed_mid * (1.0 + rng.between(-20.0, 20.0) / 10_000.0)
        },
        half_range_bps: rng.between(2.0, 40.0),
    };
    let spread_bps = rng.between(0.5, 8.0);
    let fee_bps = rng.pick(&[0.0, 1.0, 2.5]);
    let all_in_bps = spread_bps + fee_bps;
    let curve = CurveKnobs {
        start_below_mid: rng.pick(&[0.0, 0.5, -0.5]),
        end_above_buy: if floored {
            2.0
        } else {
            rng.pick(&[0.0, 1.0, 2.0])
        },
        shape: rng.pick(&[1.0, 2.0, 3.0]),
    };
    let (a, b) = (rng.unit(), rng.unit());
    let stretch = if floored {
        [0.97, 0.92]
    } else {
        [a, if rng.unit() < 0.1 { a } else { b }]
    };

    let params = UnwindParams {
        share: knobs.share,
        cap_ngn: knobs.cap_ngn,
        min_gain_ngn: knobs.min_gain_ngn,
        start_pct: knobs.start_pct,
        refresh_secs: 900,
    };
    let plain = Curve::new(
        1.0 / reference.mid,
        reference.half_range_bps,
        all_in_bps,
        &CurveParams {
            start_below_mid: curve.start_below_mid,
            end_above_buy: curve.end_above_buy,
            shape: curve.shape,
        },
    )
    .expect("the seeded knobs make a curve");
    let [f0, f1] = stretch;
    let order = |c: &Curve, bid: bool| 1.0 / c.price(bid, f0, f1, spread_bps).unwrap();
    // The venue adds its fee to the signed order at fill time; the curve is
    // sized so that lands the taker on the all-in.
    let realized = |c: &Curve, bid: bool| {
        let (s, a) = (spread_bps / 10_000.0, all_in_bps / 10_000.0);
        if bid {
            order(c, true) * (1.0 + a) / (1.0 + s)
        } else {
            order(c, false) * (1.0 - a) / (1.0 - s)
        }
    };
    // Mostly gains, some losses and some under the minimum.
    let cost = if floored {
        order(&plain, false) + rng.between(0.02, 0.2)
    } else {
        feed_mid + rng.between(-3.0, 12.0)
    };
    let s = shift(
        &params,
        &Snapshot {
            cost_ngn: cost,
            at: NOW,
        },
        feed_mid,
        NOW,
    );
    let lifted = plain.with_lift(
        Lift {
            full_ngn: s.full_ngn,
            start_pct: knobs.start_pct,
            cost_ngn: cost,
        },
        spread_bps,
    );
    // The flat signed order around the feed, in the bot's units, and the same
    // lift on it.
    let flat_order = |bid: bool| {
        let half = spread_bps / 10_000.0;
        if bid {
            (1.0 - half) / feed_mid
        } else {
            (1.0 + half) / feed_mid
        }
    };
    let flat_lift = Lift {
        full_ngn: s.full_ngn,
        start_pct: knobs.start_pct,
        cost_ngn: cost,
    }
    .held_to(lift_limit_ngn(spread_bps, feed_mid, knobs.start_pct));
    let flat = |bid: bool| {
        1.0 / flat_lift.map_or(flat_order(bid), |l| {
            lifted_flat_price(flat_order(bid), bid, l, feed_mid, f0, f1)
        })
    };
    Case {
        flat_bid: flat(true),
        flat_ask: flat(false),
        full_ngn: s.full_ngn,
        gain: s.gain_ngn,
        average_weight: average_weight(f0, f1, knobs.start_pct),
        bid: realized(&lifted, true),
        ask: realized(&lifted, false),
        plain_ask: realized(&plain, false),
        knobs,
        cost,
        feed_mid,
        reference,
        spread_bps,
        fee_bps,
        curve,
        stretch,
    }
}

#[test]
fn the_panel_fixture_matches_the_bot() {
    let mut rng = Rng(SEED);
    let cases: Vec<Case> = (0..CASES).map(|i| case(&mut rng, i % 5 == 4)).collect();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src/unwindParity.json");
    if std::env::var_os("STITCH_WRITE_PARITY").is_some() {
        let rows: Vec<String> = cases
            .iter()
            .map(|c| serde_json::to_string(c).unwrap())
            .collect();
        std::fs::write(&path, format!("[\n{}\n]\n", rows.join(",\n"))).unwrap();
        return;
    }
    let pinned: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("web/src/unwindParity.json exists"),
    )
    .unwrap();
    // Through the same print and parse as the pinned file, so the last bit
    // of a float (serde_json's parser isn't exact) can't fail the compare.
    let printed: Vec<Case> = serde_json::from_str(&serde_json::to_string(&cases).unwrap()).unwrap();
    assert_eq!(
        pinned, printed,
        "the bot's profit unwind maths changed: regenerate the panel fixture"
    );
    // The fixture has to exercise every branch it claims to pin.
    assert!(cases.iter().any(|c| c.full_ngn == 0.0 && c.gain > 0.0));
    assert!(cases.iter().any(|c| c.gain < 0.0));
    // The lift moving the ask, and the floor stopping it at the cost.
    assert!(cases
        .iter()
        .any(|c| c.ask > c.plain_ask + 1e-6 && c.ask < c.cost - 1e-6));
    // The floor binds on the signed order, so with a fee the taker realizes
    // the cost less the fee, not the cost.
    let at_floor = |c: &Case| {
        c.cost * (1.0 - (c.spread_bps + c.fee_bps) / 10_000.0) / (1.0 - c.spread_bps / 10_000.0)
    };
    assert!(cases.iter().any(|c| c.fee_bps > 0.0
        && c.ask > c.plain_ask + 1e-6
        && (c.ask - at_floor(c)).abs() < 1e-6
        && c.ask < c.cost - 1e-3));
    // On a flat price: lifted, and held to the flat limit.
    let flat_bid = |c: &Case| c.feed_mid / (1.0 - c.spread_bps / 10_000.0);
    assert!(cases.iter().any(|c| c.flat_bid > flat_bid(c) + 1e-6));
    assert!(cases.iter().any(|c| {
        let limit = lift_limit_ngn(c.spread_bps, c.feed_mid, c.knobs.start_pct);
        c.full_ngn > limit && c.flat_bid > flat_bid(c) + 1e-6
    }));
}
