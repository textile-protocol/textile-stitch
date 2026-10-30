// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The panel works out each module's run mode, and the file it saves, in
//! TypeScript (`web/src/moduleModes.ts`). This test runs seeded configs through
//! the bot's own `mode_of` and `normalized` and pins the results in
//! `web/src/moduleModeParity.json`, which the web tests replay. A change on
//! either side fails one of the two suites.
//!
//! Regenerate after an intended change with
//! `STITCH_WRITE_PARITY=1 cargo test --test module_mode_parity`.
use serde::Serialize;
use stitch_bot::modules::{Mode, Module, ModulesConfig};

const CASES: usize = 240;
const SEED: u64 = 0x6d0d_e5ed;

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
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.next() as usize % items.len()]
    }
}

const MODES: [Mode; 3] = [Mode::Off, Mode::Shadow, Mode::Live];

/// Any switch and any mode a hand-edited file could hold, older shapes included.
fn config(rng: &mut Rng) -> ModulesConfig {
    let mut c = ModulesConfig {
        mode: rng.pick(&MODES),
        ..ModulesConfig::default()
    };
    fn switch(rng: &mut Rng) -> (bool, Option<Mode>) {
        (
            rng.pick(&[true, true, false]),
            rng.pick(&[
                None,
                None,
                Some(Mode::Off),
                Some(Mode::Shadow),
                Some(Mode::Live),
            ]),
        )
    }
    (c.inventory.enabled, c.inventory.mode) = switch(rng);
    (c.spreads.enabled, c.spreads.mode) = switch(rng);
    (c.rebalance.enabled, c.rebalance.mode) = switch(rng);
    (c.liquidity.enabled, c.liquidity.mode) = switch(rng);
    (c.curve.enabled, c.curve.mode) = switch(rng);
    (c.profit_unwind.enabled, c.profit_unwind.mode) = switch(rng);
    c.profit_unwind.live = rng.pick(&[true, false]);
    c
}

/// The part of a config the modes come from. Tables in `Module::ALL` order.
#[derive(Serialize)]
struct Switches {
    mode: Mode,
    enabled: [bool; 6],
    own: [Option<Mode>; 6],
    /// Profit unwind's older `live`.
    live: bool,
}
fn switches(c: &ModulesConfig) -> Switches {
    Switches {
        mode: c.mode,
        enabled: [
            c.inventory.enabled,
            c.spreads.enabled,
            c.rebalance.enabled,
            c.liquidity.enabled,
            c.curve.enabled,
            c.profit_unwind.enabled,
        ],
        own: [
            c.inventory.mode,
            c.spreads.mode,
            c.rebalance.mode,
            c.liquidity.mode,
            c.curve.mode,
            c.profit_unwind.mode,
        ],
        live: c.profit_unwind.live,
    }
}

#[derive(Serialize)]
struct Case {
    config: Switches,
    modes: [Mode; 6],
    normalized: Switches,
    quote_modes_differ: bool,
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src/moduleModeParity.json")
}

#[test]
fn the_panel_fixture_matches_the_bots_module_modes() {
    let mut rng = Rng(SEED);
    let cases: Vec<String> = (0..CASES)
        .map(|_| {
            let config = config(&mut rng);
            let case = Case {
                config: switches(&config),
                modes: Module::ALL.map(|m| config.mode_of(m)),
                normalized: switches(&config.normalized()),
                quote_modes_differ: config.quote_modes_differ(),
            };
            serde_json::to_string(&case).unwrap()
        })
        .collect();
    let expected = format!("[\n{}\n]\n", cases.join(",\n"));
    if std::env::var_os("STITCH_WRITE_PARITY").is_some() {
        std::fs::write(fixture(), expected).unwrap();
        return;
    }
    let saved = std::fs::read_to_string(fixture())
        .expect("run with STITCH_WRITE_PARITY=1 to create the fixture");
    assert!(
        saved == expected,
        "module modes changed; regenerate with STITCH_WRITE_PARITY=1 and rerun the web tests"
    );
}
