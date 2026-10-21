// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Liquidity schedule: where the vault's liquid floor should sit right now,
//! and the one step that moves the vault toward it.
//!
//! The vault stakes everything above `minLiquidSettlement` (the floor) in its
//! yield adapter. The venue only lets a vault order join a multi-maker bundle
//! when it fits under the floor, because anything above it can be staked
//! between admission and broadcast. So the floor trades bundle capacity
//! against yield. This module sets it to one share of free settlement during
//! market hours and another outside them.
//!
//! Pure. `rfq::liquidity` reads the vault, sends the transactions and applies
//! the quote cap; nothing here touches a signer or an RPC client.
use super::config::LiquidityConfig;
use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

const DAY_MINUTES: u64 = 24 * 60;
const WEEK_MINUTES: u64 = 7 * DAY_MINUTES;
/// 1970-01-01 was a Thursday: three days after the Monday a schedule week starts on.
const EPOCH_WEEKDAY_OFFSET: u64 = 3;

/// Minute of the UTC week, Monday 00:00 = 0.
pub fn week_minute(unix: u64) -> u64 {
    (unix / 60 + EPOCH_WEEKDAY_OFFSET * DAY_MINUTES) % WEEK_MINUTES
}

/// Market-hours windows as `(start minute of week, length in minutes)`.
/// Validation keeps `open != close`, so every window is shorter than a day and
/// windows opening on different days never overlap or touch.
fn windows(cfg: &LiquidityConfig) -> Vec<(u64, u64)> {
    let Ok((open, close)) = cfg.window_minutes() else {
        return vec![];
    };
    let length = (close + DAY_MINUTES - open) % DAY_MINUTES;
    cfg.days
        .iter()
        .map(|day| (day.index() * DAY_MINUTES + open, length))
        .collect()
}

fn inside(minute: u64, (start, length): (u64, u64)) -> bool {
    (minute + WEEK_MINUTES - start) % WEEK_MINUTES < length
}

pub fn market_hours(cfg: &LiquidityConfig, now: u64) -> bool {
    let minute = week_minute(now);
    windows(cfg).into_iter().any(|w| inside(minute, w))
}

/// Unix time of the next open or close after `now`.
pub fn next_change(cfg: &LiquidityConfig, now: u64) -> Option<u64> {
    let minute = week_minute(now);
    windows(cfg)
        .into_iter()
        .flat_map(|(start, length)| [start, (start + length) % WEEK_MINUTES])
        .map(|boundary| (boundary + WEEK_MINUTES - minute) % WEEK_MINUTES)
        .filter(|ahead| *ahead > 0)
        .min()
        .map(|ahead| (now / 60 + ahead) * 60)
}

/// The vault views a step depends on, read in one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultLiquidity {
    /// `minLiquidSettlement`.
    pub floor: U256,
    /// `liquidSettlement`: what a Permit2 pull can reach now.
    pub liquid: U256,
    /// `freeSettlement`: liquid plus the unreserved adapter position.
    pub free: U256,
    pub has_adapter: bool,
    pub paused: bool,
    pub close_only: bool,
}

/// Settlement this bot's live quotes have promised, and the quote cap that was
/// in force when the engine measured it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commitments {
    pub committed: U256,
    pub cap_in_effect: Option<U256>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Floor and liquid balance already match the schedule.
    Hold,
    /// `setMinLiquidSettlement(target)` through the operator's role.
    RaiseFloor(U256),
    /// `setMinLiquidSettlement(target)`. Only once live quotes fit under it.
    LowerFloor(U256),
    /// Lowering is due, but quotes admitted against the current floor are
    /// still live. New quotes are capped at the target until they expire.
    Drain,
    /// `prepareSettlement(needed)`: pull staked settlement back up to the floor.
    Recall(U256),
    /// Nothing can be decided: missing reads, or a vault state the floor does
    /// not apply to.
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Hold,
    RaiseFloor,
    LowerFloor,
    Drain,
    Recall,
    Wait,
}

impl Step {
    pub fn kind(&self) -> StepKind {
        match self {
            Step::Hold => StepKind::Hold,
            Step::RaiseFloor(_) => StepKind::RaiseFloor,
            Step::LowerFloor(_) => StepKind::LowerFloor,
            Step::Drain => StepKind::Drain,
            Step::Recall(_) => StepKind::Recall,
            Step::Wait => StepKind::Wait,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub market_hours: bool,
    /// Floor the schedule wants now. `None` until the vault has been read.
    pub target: Option<U256>,
    pub step: Step,
    /// Most settlement the bot's live quotes may commit in total. Set while a
    /// lower is pending (the new floor), and while liquid settlement is short
    /// of the floor (what is liquid): the venue admits bundles up to the
    /// floor, but only liquid settlement can pay them.
    pub quote_cap: Option<U256>,
    pub message: String,
}

/// The quote cap any liquid shortfall needs: the venue admits bundles up to
/// the floor, but only liquid settlement can pay them, so while `liquid` is
/// under what the floor promises (at most `free`, since `prepareSettlement`
/// reverts above it) quotes promise no more than `liquid`. `None` when liquid
/// covers it. Tolerance plays no part: it only spares small recall
/// transactions, never a fill.
pub fn shortfall_cap(liquid: U256, floor: U256, free: U256) -> Option<U256> {
    (liquid < floor.min(free)).then_some(liquid)
}

/// `value * bps / 10_000`, rounded down.
fn share(value: U256, bps: u32) -> U256 {
    crate::rfq::math::fee_on(value, bps)
}

/// The next step toward the scheduled floor.
///
/// Raising is always safe: it only keeps more settlement liquid. Lowering is
/// not. A bundled order was admitted against the current floor, and once the
/// floor drops, any `allocateIdle` (the executor calls it after every fill)
/// can stake the settlement that order's Permit2 pull needs. So a lower waits
/// until a quote cap at the new floor is already in force and everything
/// still live fits under it; from then on no quote can promise more than the
/// new floor holds.
pub fn plan(
    cfg: &LiquidityConfig,
    now: u64,
    vault: Option<&VaultLiquidity>,
    commitments: Option<Commitments>,
) -> Plan {
    let market = market_hours(cfg, now);
    let waiting = |message: &str| Plan {
        market_hours: market,
        target: None,
        step: Step::Wait,
        quote_cap: None,
        message: message.into(),
    };
    let Some(v) = vault else {
        return waiting("Waiting for vault reads");
    };
    if !v.has_adapter {
        return waiting(
            "This vault has no yield adapter, so nothing is staked and the floor does nothing",
        );
    }
    if v.paused {
        return waiting("The vault is paused. Nothing is staked while it is paused");
    }
    if v.close_only {
        return waiting(
            "A redemption epoch is closed. Settling it recalls the whole position first",
        );
    }
    let bps = if market {
        cfg.market_floor_bps
    } else {
        cfg.off_hours_floor_bps
    };
    let target = share(v.free, bps);
    let tolerance = share(v.free, cfg.tolerance_bps);
    let planned = |step: Step, quote_cap: Option<U256>, message: &str| Plan {
        market_hours: market,
        target: Some(target),
        step,
        quote_cap,
        message: message.into(),
    };
    // Fills spend liquid first and nothing refills it, so liquid can sit
    // under the floor. Every shortfall caps quotes (`shortfall_cap`); only one
    // past the tolerance is worth a recall transaction.
    let liquid_cap = |floor: U256| shortfall_cap(v.liquid, floor, v.free);
    let min_cap = |a: U256, b: Option<U256>| Some(b.map_or(a, |b| a.min(b)));
    if target > v.floor.saturating_add(tolerance) {
        return planned(
            Step::RaiseFloor(target),
            liquid_cap(target),
            "Raising the floor so more settlement stays liquid for bundled fills",
        );
    }
    if v.floor > target.saturating_add(tolerance) {
        let cap = min_cap(target, liquid_cap(v.floor));
        return match commitments {
            None => planned(
                Step::Drain,
                cap,
                "Waiting for a reading of live quotes before lowering the floor",
            ),
            Some(c) if c.cap_in_effect.is_none_or(|in_force| in_force > target) => planned(
                Step::Drain,
                cap,
                "Capping new quotes at the new floor before lowering it",
            ),
            Some(c) if c.committed > target => planned(
                Step::Drain,
                cap,
                "Waiting for live quotes above the new floor to expire",
            ),
            Some(_) => planned(
                Step::LowerFloor(target),
                cap,
                "Lowering the floor so idle settlement earns yield",
            ),
        };
    }
    let needed = v.floor.min(v.free);
    if needed.saturating_sub(v.liquid) > tolerance {
        return planned(
            Step::Recall(needed),
            liquid_cap(v.floor),
            "Liquid settlement is under the floor; capping quotes at what is liquid and \
             recalling from the yield adapter",
        );
    }
    match liquid_cap(v.floor) {
        None => planned(
            Step::Hold,
            None,
            "Floor and liquid settlement match the schedule",
        ),
        cap => planned(
            Step::Hold,
            cap,
            "Liquid settlement is a little under the floor; capping quotes at what is \
             liquid rather than paying for a recall",
        ),
    }
}

/// One sent (or refused) transaction, for the panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionRecord {
    pub at: u64,
    pub action: String,
    pub ok: bool,
    pub detail: String,
}

/// What the panel shows. Amounts are atomic settlement units as decimal strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiquidityStatus {
    pub at: u64,
    pub market_hours: bool,
    pub next_change_at: Option<u64>,
    /// The bot's signer: holds the role and pays the gas.
    pub sender: String,
    pub operator_admin: Option<String>,
    /// The Roles modifier's `target()`. Must be the operator admin.
    pub authority_target: Option<String>,
    pub floor: Option<String>,
    pub target: Option<String>,
    pub liquid: Option<String>,
    pub free: Option<String>,
    pub committed: Option<String>,
    pub quote_cap: Option<String>,
    pub step: StepKind,
    pub message: String,
    pub in_flight: Option<String>,
    pub actions: Vec<ActionRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::Weekday;

    /// Monday 2026-10-05 00:00 UTC.
    const MONDAY: u64 = 1_791_158_400;
    const HOUR: u64 = 3600;

    fn cfg() -> LiquidityConfig {
        LiquidityConfig {
            enabled: true,
            ..LiquidityConfig::default()
        }
    }

    fn vault(floor: u64, liquid: u64, free: u64) -> VaultLiquidity {
        VaultLiquidity {
            floor: U256::from(floor),
            liquid: U256::from(liquid),
            free: U256::from(free),
            has_adapter: true,
            paused: false,
            close_only: false,
        }
    }

    fn capped(committed: u64, cap: Option<u64>) -> Option<Commitments> {
        Some(Commitments {
            committed: U256::from(committed),
            cap_in_effect: cap.map(U256::from),
        })
    }

    #[test]
    fn the_week_starts_on_monday_utc() {
        assert_eq!(week_minute(MONDAY), 0);
        assert_eq!(week_minute(MONDAY + 7 * 24 * HOUR), 0);
        assert_eq!(week_minute(MONDAY + 24 * HOUR + 60), DAY_MINUTES + 1);
    }

    #[test]
    fn default_schedule_is_weekday_business_hours() {
        let c = cfg();
        assert!(!market_hours(&c, MONDAY + 7 * HOUR + 59 * 60));
        assert!(market_hours(&c, MONDAY + 8 * HOUR));
        assert!(market_hours(&c, MONDAY + 16 * HOUR + 59 * 60));
        assert!(!market_hours(&c, MONDAY + 17 * HOUR));
        // Saturday noon.
        assert!(!market_hours(&c, MONDAY + 5 * 24 * HOUR + 12 * HOUR));
    }

    #[test]
    fn a_window_can_run_past_midnight_into_the_next_day() {
        let c = LiquidityConfig {
            open_utc: "22:00".into(),
            close_utc: "06:00".into(),
            days: vec![Weekday::Fri],
            ..cfg()
        };
        let friday = MONDAY + 4 * 24 * HOUR;
        assert!(!market_hours(&c, friday + 21 * HOUR));
        assert!(market_hours(&c, friday + 22 * HOUR));
        assert!(market_hours(&c, friday + 29 * HOUR)); // Saturday 05:00
        assert!(!market_hours(&c, friday + 30 * HOUR)); // Saturday 06:00
                                                        // Sunday's window would wrap into Monday: the week is circular.
        let sunday = LiquidityConfig {
            days: vec![Weekday::Sun],
            ..c
        };
        assert!(market_hours(&sunday, MONDAY + 2 * HOUR));
        assert!(market_hours(&sunday, MONDAY + 7 * 24 * HOUR - HOUR));
    }

    #[test]
    fn next_change_is_the_next_open_or_close() {
        let c = cfg();
        assert_eq!(next_change(&c, MONDAY + 30), Some(MONDAY + 8 * HOUR));
        assert_eq!(next_change(&c, MONDAY + 8 * HOUR), Some(MONDAY + 17 * HOUR));
        // Friday close to Monday open skips the weekend.
        let friday_evening = MONDAY + 4 * 24 * HOUR + 18 * HOUR;
        assert_eq!(
            next_change(&c, friday_evening),
            Some(MONDAY + 7 * 24 * HOUR + 8 * HOUR)
        );
    }

    #[test]
    fn market_hours_raise_the_floor_to_the_market_share() {
        let p = plan(&cfg(), MONDAY + 9 * HOUR, Some(&vault(0, 0, 1_000)), None);
        assert_eq!(p.step, Step::RaiseFloor(U256::from(500u64)));
        assert_eq!(p.target, Some(U256::from(500u64)));
        // Nothing is liquid yet: until a recall funds the new floor, quotes
        // promise no more than what is.
        assert_eq!(p.quote_cap, Some(U256::ZERO));
        // Already liquid enough: raising alone needs no cap.
        let funded = plan(&cfg(), MONDAY + 9 * HOUR, Some(&vault(0, 600, 1_000)), None);
        assert_eq!(funded.step, Step::RaiseFloor(U256::from(500u64)));
        assert_eq!(funded.quote_cap, None);
    }

    #[test]
    fn an_unfunded_floor_caps_quotes_at_what_is_liquid_until_recalled() {
        // Fills spent liquid under a correct floor.
        let p = plan(
            &cfg(),
            MONDAY + 9 * HOUR,
            Some(&vault(500, 120, 1_000)),
            None,
        );
        assert_eq!(p.step, Step::Recall(U256::from(500u64)));
        assert_eq!(p.quote_cap, Some(U256::from(120u64)));
        // Short by less than the tolerance: no recall transaction, but a
        // bundle up to the floor still could not be paid, so quotes are capped.
        let close = plan(
            &cfg(),
            MONDAY + 9 * HOUR,
            Some(&vault(500, 499, 1_000)),
            None,
        );
        assert_eq!(
            (close.step, close.quote_cap),
            (Step::Hold, Some(U256::from(499u64)))
        );
        let funded = plan(
            &cfg(),
            MONDAY + 9 * HOUR,
            Some(&vault(500, 500, 1_000)),
            None,
        );
        assert_eq!((funded.step, funded.quote_cap), (Step::Hold, None));
    }

    #[test]
    fn a_gap_inside_the_tolerance_is_left_alone() {
        // 5% of 1_000 is 50: a floor of 460 against a 500 target holds.
        let p = plan(
            &cfg(),
            MONDAY + 9 * HOUR,
            Some(&vault(460, 460, 1_000)),
            None,
        );
        assert_eq!(p.step, Step::Hold);
        let p = plan(
            &cfg(),
            MONDAY + 9 * HOUR,
            Some(&vault(449, 449, 1_000)),
            None,
        );
        assert_eq!(p.step, Step::RaiseFloor(U256::from(500u64)));
    }

    #[test]
    fn a_correct_floor_with_drained_liquid_recalls_up_to_it() {
        let p = plan(
            &cfg(),
            MONDAY + 9 * HOUR,
            Some(&vault(500, 100, 1_000)),
            None,
        );
        assert_eq!(p.step, Step::Recall(U256::from(500u64)));
    }

    #[test]
    fn recall_never_asks_for_more_than_free_settlement() {
        // Free fell under the floor after a redemption: recall what exists.
        let c = LiquidityConfig {
            market_floor_bps: 10_000,
            ..cfg()
        };
        let p = plan(&c, MONDAY + 9 * HOUR, Some(&vault(1_000, 100, 960)), None);
        assert_eq!(p.step, Step::Recall(U256::from(960u64)));
    }

    #[test]
    fn off_hours_lower_only_after_a_cap_is_in_force_and_quotes_fit() {
        let night = MONDAY + 20 * HOUR;
        let v = vault(500, 500, 1_000);
        let no_reading = plan(&cfg(), night, Some(&v), None);
        assert_eq!(no_reading.step, Step::Drain);
        assert_eq!(no_reading.quote_cap, Some(U256::ZERO));

        let uncapped = plan(&cfg(), night, Some(&v), capped(0, None));
        assert_eq!(uncapped.step, Step::Drain, "no cap in force yet");

        let loose_cap = plan(&cfg(), night, Some(&v), capped(0, Some(100)));
        assert_eq!(loose_cap.step, Step::Drain, "cap above the new floor");

        let busy = plan(
            &LiquidityConfig {
                off_hours_floor_bps: 1_000,
                ..cfg()
            },
            night,
            Some(&v),
            capped(150, Some(100)),
        );
        assert_eq!(busy.step, Step::Drain, "live quotes above the new floor");

        let ready = plan(&cfg(), night, Some(&v), capped(0, Some(0)));
        assert_eq!(ready.step, Step::LowerFloor(U256::ZERO));
        assert_eq!(
            ready.quote_cap,
            Some(U256::ZERO),
            "cap holds until the chain shows it"
        );
    }

    #[test]
    fn halted_or_adapterless_vaults_wait() {
        let at = MONDAY + 9 * HOUR;
        for v in [
            VaultLiquidity {
                has_adapter: false,
                ..vault(0, 0, 1_000)
            },
            VaultLiquidity {
                paused: true,
                ..vault(0, 0, 1_000)
            },
            VaultLiquidity {
                close_only: true,
                ..vault(0, 0, 1_000)
            },
        ] {
            let p = plan(&cfg(), at, Some(&v), None);
            assert_eq!(p.step, Step::Wait);
            assert_eq!(p.quote_cap, None);
        }
        assert_eq!(plan(&cfg(), at, None, None).step, Step::Wait);
    }

    #[test]
    fn floors_round_down() {
        let c = LiquidityConfig {
            market_floor_bps: 3_333,
            tolerance_bps: 1,
            ..cfg()
        };
        let p = plan(&c, MONDAY + 9 * HOUR, Some(&vault(0, 0, 10_001)), None);
        assert_eq!(p.step, Step::RaiseFloor(U256::from(3_333u64)));
    }
}
