// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The panel's Simulate table recomputes the quoted spread in TypeScript
//! (`web/src/modulePresentation.ts`). This test runs seeded cases through the
//! bot's own path, `base_spreads` → `evaluate` → `apply`, and pins the result
//! in `web/src/moduleSpreadParity.json`, which the web tests replay against the
//! panel. A change on either side fails one of the two suites.
//!
//! Regenerate after an intended evaluator change with
//! `STITCH_WRITE_PARITY=1 cargo test --test module_spread_parity`.
use alloy_primitives::{address, U256};
use serde::Serialize;
use stitch_bot::{
    config::RfqCapacity, modules::*, pricing::quote::Spread, rfq::responder::CorridorBook,
};

const CASES: usize = 120;
const SEED: u64 = 0x5717_c4ed;
const NOW: u64 = 1_000_000;
/// Seconds between the window's two samples; measured cases warm up within it.
const SPAN: u64 = 60;

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
    /// Inclusive.
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() % u64::from(hi - lo + 1)) as u32
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn chance(&mut self, percent: u32) -> bool {
        self.range(1, 100) <= percent
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.next() as usize % items.len()]
    }
    /// Mostly small values, sometimes anywhere up to `hi`.
    fn bps(&mut self, small: u32, hi: u32) -> u32 {
        if self.chance(80) {
            self.range(0, small)
        } else {
            self.range(0, hi)
        }
    }
}

#[derive(Serialize)]
struct Window {
    low: f64,
    high: f64,
    extra_bps: u32,
}

#[derive(Serialize)]
struct Case {
    config: ModulesConfig,
    /// The panel's `corridor_spreads` shape: `{kind, value}` or null per side.
    spreads: serde_json::Value,
    /// Settlement per corridor token, for converting an absolute offset.
    price: f64,
    share: u32,
    /// The price window as the status file records it; null while warming up.
    window: Option<Window>,
    /// The bps on the book RFQ prices with, `[buy, sell]`; null is no quote.
    quoted: [Option<u32>; 2],
}

fn config(rng: &mut Rng) -> ModulesConfig {
    let mut c = ModulesConfig {
        mode: Mode::Live,
        ..Default::default()
    };
    c.inventory.enabled = rng.chance(75);
    c.inventory.target_bps = rng.range(1, 9998);
    c.inventory.max_bps = rng.range(c.inventory.target_bps + 1, 9999);
    c.inventory.max_skew_bps = rng.bps(300, 4999);
    c.inventory.spread_floor_bps = rng.bps(60, 4999);
    c.spreads.enabled = rng.chance(75);
    c.spreads.inventory_aware = rng.chance(50);
    c.spreads.multiplier = if rng.chance(60) {
        rng.pick(&[0.0, 0.008, 0.25, 0.5, 1.0, 1.37, 2.0, 2.5, 10.0])
    } else {
        (rng.unit() * 10_000.0).round() / 1_000.0
    };
    c.spreads.max_extra_bps = rng.bps(300, 4999);
    c.spreads.window_secs = u64::from(rng.range(SPAN as u32, 3600));
    c.spreads.warmup_secs = u64::from(rng.range(1, SPAN as u32 - 1));
    c.validate().expect("generated config is valid");
    c
}

/// Holdings near the thresholds that change the formula, or anywhere.
fn share(rng: &mut Rng, c: &ModulesConfig) -> u32 {
    let (t, m) = (c.inventory.target_bps, c.inventory.max_bps);
    if rng.chance(50) {
        rng.pick(&[0, t - 1, t, t + 1, m - 1, m, m + 1, 10_000])
            .min(10_000)
    } else {
        rng.range(0, 10_000)
    }
}

/// One corridor side: off, bps, or an absolute offset in corridor per
/// settlement token, sized against the price so it lands on real bps.
fn spread(rng: &mut Rng, price: f64, max_bps: u32, max_abs_share: f64) -> Option<Spread> {
    match rng.range(1, 100) {
        1..=15 => None,
        16..=75 => Some(Spread::Bps(rng.bps(150, max_bps))),
        _ => Some(Spread::Abs(rng.unit() * max_abs_share / price)),
    }
}

fn spread_json(s: Option<Spread>) -> serde_json::Value {
    match s {
        None => serde_json::Value::Null,
        Some(Spread::Bps(b)) => serde_json::json!({"kind": "bps", "value": b}),
        Some(Spread::Abs(d)) => serde_json::json!({"kind": "abs", "value": d}),
    }
}

fn book(buy: Option<Spread>, sell: Option<Spread>) -> CorridorBook {
    CorridorBook {
        slug: "parity".into(),
        collateral: address!("0000000000000000000000000000000000000001"),
        debt: address!("0000000000000000000000000000000000000002"),
        collateral_decimals: 6,
        debt_decimals: 6,
        buy_spread: buy,
        sell_spread: sell,
        buy_capacity_debt: Some(RfqCapacity::Wallet),
        sell_capacity_collateral: Some(RfqCapacity::Wallet),
        feed_url: "http://localhost/price".into(),
        staleness_secs: 10,
        curve: None,
    }
}

/// Balances at a 1.0 price that put exactly `share` bps of NAV in corridor.
fn context(share: u32, base: (Option<u32>, Option<u32>)) -> Context {
    let corridor = U256::from(u64::from(share) * 1_000);
    let settlement = U256::from(u64::from(10_000 - share) * 1_000);
    Context {
        now: NOW,
        price: 1.0,
        price_at: NOW,
        price_degraded: false,
        balances_at: NOW,
        staleness_secs: 10,
        settlement,
        corridor,
        available_settlement: settlement,
        available_corridor: corridor,
        reserved_settlement: U256::ZERO,
        reserved_corridor: U256::ZERO,
        settlement_decimals: 6,
        corridor_decimals: 6,
        max_sell: U256::MAX,
        base_buy_bps: base.0,
        base_sell_bps: base.1,
    }
}

fn bps(s: Option<Spread>) -> Option<u32> {
    s.map(|s| match s {
        Spread::Bps(b) => b,
        Spread::Abs(_) => unreachable!("modules always hand RFQ whole bps"),
    })
}

fn case(rng: &mut Rng) -> Case {
    let config = config(rng);
    let share = share(rng, &config);
    let price = rng.pick(&[0.000_73, 0.000_25, 0.052, 1.0, 17.3, 3_400.0]) * (0.5 + rng.unit());
    let buy = spread(rng, price, 9_999, 0.3);
    // A sell offset of 1/price or more leaves no ask price; keep that edge in.
    let sell = spread(rng, price, 20_000, 1.2);
    let book = book(buy, sell);
    let base = base_spreads(&book, price);
    // Two samples SPAN apart, in either order; none while warming up. Round
    // feed prices (1.0 → 1.002) are where float noise sits on a ceil boundary.
    let prices = if rng.chance(20) {
        None
    } else {
        let low = if rng.chance(30) {
            rng.pick(&[1.0, 1000.0, 0.5])
        } else {
            price * (0.5 + rng.unit())
        };
        let high = if rng.chance(15) {
            low
        } else if rng.chance(30) {
            low * (1.0 + f64::from(rng.range(1, 50)) / 10_000.0)
        } else {
            low * (1.0 + rng.unit() * rng.pick(&[0.001, 0.01, 0.15]))
        };
        Some(if rng.chance(50) {
            (low, high)
        } else {
            (high, low)
        })
    };
    let history: Vec<PricePoint> = prices
        .map(|(first, last)| {
            vec![
                PricePoint {
                    timestamp: NOW - SPAN,
                    price: first,
                },
                PricePoint {
                    timestamp: NOW,
                    price: last,
                },
            ]
        })
        .unwrap_or_default();
    let d = evaluate(&config, &context(share, base), &history);
    assert!(!d.blocked, "{:?}", d.reasons);
    assert_eq!(d.inventory_bps, Some(share));
    // Live changes the book only with a quote module on (`Engine::module_book`).
    let quoted = if config.inventory.enabled || config.spreads.enabled {
        let effective = apply(&book, &d);
        [bps(effective.buy_spread), bps(effective.sell_spread)]
    } else {
        [base.0, base.1]
    };
    Case {
        config,
        spreads: serde_json::json!({"buy": spread_json(buy), "sell": spread_json(sell)}),
        price,
        share,
        window: prices.map(|(a, b)| Window {
            low: a.min(b),
            high: a.max(b),
            extra_bps: d.volatility_bps,
        }),
        quoted,
    }
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src/moduleSpreadParity.json")
}

#[test]
fn the_panel_fixture_matches_what_the_bot_quotes() {
    let mut rng = Rng(SEED);
    // One case per line. Compared as text: floats are written shortest
    // round-trip and read back exactly by JSON.parse on the web side.
    let lines: Vec<String> = (0..CASES)
        .map(|_| serde_json::to_string(&case(&mut rng)).unwrap())
        .collect();
    let expected = format!("[\n{}\n]\n", lines.join(",\n"));
    if std::env::var_os("STITCH_WRITE_PARITY").is_some() {
        std::fs::write(fixture(), expected).unwrap();
        return;
    }
    let saved = std::fs::read_to_string(fixture())
        .expect("run with STITCH_WRITE_PARITY=1 to create the fixture");
    assert!(
        saved == expected,
        "the evaluator changed; regenerate with STITCH_WRITE_PARITY=1 and rerun the web tests"
    );
}
