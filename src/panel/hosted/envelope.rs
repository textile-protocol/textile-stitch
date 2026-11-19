// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The quoting envelope: which settings a hosted panel may change, and within
//! what bounds.
//!
//! The values are agreed with the client and written by the host's Terraform to
//! a read-only JSON file (`STITCH_PANEL_ENVELOPE`). The panel compares the
//! settings it read before a save with the settings the edited file would have,
//! so the check doesn't depend on which fields a client chose to send:
//!
//! - every field outside the quoting set must come out unchanged (RPCs, feed,
//!   chain and pool, vault, RFQ, book/taker switches, TWAP, lean, modules)
//! - a quoting field that changes must have a bound in the envelope, and the new
//!   value must sit inside it. A field with no bound can't be changed at all, so
//!   a missing or empty envelope file means no quoting edits.
//!
//! Units: spreads and the minimum spread in basis points (decimals allowed),
//! times in seconds, `max_orders` a count, sizing amounts in the token's atomic
//! units as decimal strings. Bounds are inclusive.

use anyhow::{ensure, Result};
use serde::Deserialize;

use crate::setup::{SettingsView, SideSizing, SpreadEdit, SpreadKind};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Bound<T> {
    pub min: T,
    pub max: T,
}

/// Atomic amounts are strings in JSON, like everywhere else in the panel.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AmountBound {
    pub min: String,
    pub max: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    #[serde(default)]
    pub spread_bps: Option<Bound<f64>>,
    #[serde(default)]
    pub min_spread_bps: Option<Bound<f64>>,
    #[serde(default)]
    pub ttl_secs: Option<Bound<u64>>,
    #[serde(default)]
    pub refresh_threshold_bps: Option<Bound<u32>>,
    #[serde(default)]
    pub tick_interval_secs: Option<Bound<u64>>,
    #[serde(default)]
    pub max_orders: Option<Bound<u64>>,
    #[serde(default)]
    pub order_size: Option<AmountBound>,
    #[serde(default)]
    pub total_liquidity: Option<AmountBound>,
    #[serde(default)]
    pub min_slice_debt: Option<AmountBound>,
}

impl Envelope {
    pub fn parse(text: &str) -> Result<Self> {
        let envelope: Envelope = serde_json::from_str(text)?;
        envelope.validate()?;
        Ok(envelope)
    }

    fn validate(&self) -> Result<()> {
        for (name, b) in [
            ("spread_bps", &self.spread_bps),
            ("min_spread_bps", &self.min_spread_bps),
        ] {
            if let Some(b) = b {
                ensure!(
                    b.min.is_finite() && b.max.is_finite() && 0.0 <= b.min && b.min <= b.max,
                    "{name}: need 0 <= min <= max"
                );
            }
        }
        for (name, b) in [
            ("ttl_secs", &self.ttl_secs),
            ("tick_interval_secs", &self.tick_interval_secs),
            ("max_orders", &self.max_orders),
        ] {
            if let Some(b) = b {
                ensure!(b.min <= b.max, "{name}: need min <= max");
            }
        }
        if let Some(b) = &self.refresh_threshold_bps {
            ensure!(b.min <= b.max, "refresh_threshold_bps: need min <= max");
        }
        for (name, b) in [
            ("order_size", &self.order_size),
            ("total_liquidity", &self.total_liquidity),
            ("min_slice_debt", &self.min_slice_debt),
        ] {
            if let Some(b) = b {
                let (min, max) = (atomic(&b.min), atomic(&b.max));
                ensure!(
                    matches!((min, max), (Some(min), Some(max)) if min <= max),
                    "{name}: min and max must be atomic-unit integers with min <= max"
                );
            }
        }
        Ok(())
    }
}

/// Refuse `after` unless it differs from `before` only in quoting fields, each
/// inside the envelope.
pub fn check(
    before: &SettingsView,
    after: &SettingsView,
    envelope: &Envelope,
) -> Result<(), String> {
    check_frozen(before, after)?;
    for (side, b, a) in [
        ("buy", &before.buy, &after.buy),
        ("sell", &before.sell, &after.sell),
    ] {
        check_spread(side, b, a, envelope.spread_bps)?;
    }
    check_min_spread(
        &before.min_spread_bps,
        &after.min_spread_bps,
        envelope.min_spread_bps,
    )?;
    check_number("TTL", before.ttl_secs, after.ttl_secs, envelope.ttl_secs)?;
    check_number(
        "refresh threshold",
        before.refresh_threshold_bps,
        after.refresh_threshold_bps,
        envelope.refresh_threshold_bps,
    )?;
    check_number(
        "tick interval",
        before.tick_interval_secs,
        after.tick_interval_secs,
        envelope.tick_interval_secs,
    )?;
    for (side, b, a) in [
        ("buy", &before.buy_sizing, &after.buy_sizing),
        ("sell", &before.sell_sizing, &after.sell_sizing),
    ] {
        check_sizing(side, b, a, envelope)?;
    }
    Ok(())
}

/// Everything except the quoting fields, so a field added to [`SettingsView`]
/// later is frozen on hosted until someone decides otherwise.
fn frozen(view: &SettingsView) -> SettingsView {
    SettingsView {
        buy: SpreadEdit::default(),
        sell: SpreadEdit::default(),
        min_spread_bps: String::new(),
        buy_sizing: SideSizing::default(),
        sell_sizing: SideSizing::default(),
        ttl_secs: 0,
        refresh_threshold_bps: 0,
        tick_interval_secs: 0,
        ..view.clone()
    }
}

fn check_frozen(before: &SettingsView, after: &SettingsView) -> Result<(), String> {
    if frozen(before) == frozen(after) {
        return Ok(());
    }
    let (b, a) = (before, after);
    let named = [
        ("the RPC URL", b.rpc_url != a.rpc_url),
        (
            "the fallback RPCs",
            b.rpc_fallback_urls != a.rpc_fallback_urls,
        ),
        ("the price feed", b.feed_url != a.feed_url),
        (
            "the pool or chain",
            b.pair != a.pair || b.pools != a.pools || b.pool_index != a.pool_index,
        ),
        ("the vault", b.vault_address != a.vault_address),
        (
            "RFQ settings",
            b.rfq_enabled != a.rfq_enabled
                || b.rfq_url != a.rfq_url
                || b.rfq_maker_id != a.rfq_maker_id
                || b.rfq_validation_contract != a.rfq_validation_contract
                || b.rfq_corridor != a.rfq_corridor,
        ),
        (
            "book or taker switches",
            b.book_enabled != a.book_enabled || b.taker_enabled != a.taker_enabled,
        ),
        (
            "TWAP settings",
            b.twap_window_secs != a.twap_window_secs
                || b.twap_max_deviation_bps != a.twap_max_deviation_bps,
        ),
        (
            "inventory lean",
            b.lean_enabled != a.lean_enabled
                || b.lean_shadow != a.lean_shadow
                || b.lean_floor_bps != a.lean_floor_bps
                || b.lean_base_bps != a.lean_base_bps
                || b.lean_wide_bps != a.lean_wide_bps,
        ),
        ("modules", b.modules_enabled != a.modules_enabled),
    ];
    let what = named
        .iter()
        .find(|(_, changed)| *changed)
        .map(|(name, _)| *name)
        .unwrap_or("a setting outside the quoting envelope");
    Err(format!(
        "this hosted panel can't change {what}. Only spreads, sizing, TTL, refresh threshold \
         and tick interval can change here, inside the agreed envelope. Anything else goes \
         through the host's administrators."
    ))
}

fn out_of_bounds(
    what: &str,
    value: impl std::fmt::Display,
    min: impl std::fmt::Display,
    max: impl std::fmt::Display,
) -> String {
    format!("{what} {value} is outside the agreed envelope ({min} to {max}).")
}

fn no_bound(what: &str) -> String {
    format!("the agreed envelope has no bound for {what}, so this hosted panel can't change it.")
}

fn check_spread(
    side: &str,
    before: &SpreadEdit,
    after: &SpreadEdit,
    bound: Option<Bound<f64>>,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let what = format!("the {side} spread");
    let bound = bound.ok_or_else(|| no_bound(&what))?;
    if after.kind != SpreadKind::Bps {
        return Err(format!("{what} must be in basis points on a hosted panel."));
    }
    let value: f64 = after
        .value
        .trim()
        .parse()
        .map_err(|_| format!("{what} {:?} isn't a number of basis points.", after.value))?;
    if !(bound.min..=bound.max).contains(&value) {
        return Err(out_of_bounds(
            &format!("{what} (bps)"),
            value,
            bound.min,
            bound.max,
        ));
    }
    Ok(())
}

fn check_min_spread(before: &str, after: &str, bound: Option<Bound<f64>>) -> Result<(), String> {
    if before.trim() == after.trim() {
        return Ok(());
    }
    let what = "the minimum spread";
    let bound = bound.ok_or_else(|| no_bound(what))?;
    let value: f64 = after
        .trim()
        .parse()
        .map_err(|_| format!("{what} can't be removed or left blank on a hosted panel."))?;
    if !(bound.min..=bound.max).contains(&value) {
        return Err(out_of_bounds(
            "the minimum spread (bps)",
            value,
            bound.min,
            bound.max,
        ));
    }
    Ok(())
}

fn check_number<T: PartialOrd + Copy + std::fmt::Display>(
    what: &str,
    before: T,
    after: T,
    bound: Option<Bound<T>>,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let bound = bound.ok_or_else(|| no_bound(what))?;
    if after < bound.min || after > bound.max {
        return Err(out_of_bounds(what, after, bound.min, bound.max));
    }
    Ok(())
}

fn check_sizing(
    side: &str,
    before: &SideSizing,
    after: &SideSizing,
    envelope: &Envelope,
) -> Result<(), String> {
    let amounts = [
        (
            "order size",
            &before.order_size,
            &after.order_size,
            &envelope.order_size,
        ),
        (
            "total liquidity",
            &before.total_liquidity,
            &after.total_liquidity,
            &envelope.total_liquidity,
        ),
        (
            "minimum slice",
            &before.min_slice_debt,
            &after.min_slice_debt,
            &envelope.min_slice_debt,
        ),
    ];
    for (name, b, a, bound) in amounts {
        if b.trim() == a.trim() {
            continue;
        }
        let what = format!("the {side} {name}");
        let bound = bound.as_ref().ok_or_else(|| no_bound(&what))?;
        let value = atomic(a).ok_or_else(|| {
            format!("{what} must be a whole number of atomic units on a hosted panel (not blank or \"max\").")
        })?;
        let (min, max) = (
            atomic(&bound.min).unwrap_or(0),
            atomic(&bound.max).unwrap_or(0),
        );
        if value < min || value > max {
            return Err(out_of_bounds(&what, value, min, max));
        }
    }
    if before.max_orders.trim() != after.max_orders.trim() {
        let what = format!("the {side} max orders");
        let bound = envelope.max_orders.ok_or_else(|| no_bound(&what))?;
        let value: u64 = after
            .max_orders
            .trim()
            .parse()
            .map_err(|_| format!("{what} must be a whole number on a hosted panel."))?;
        if value < bound.min || value > bound.max {
            return Err(out_of_bounds(&what, value, bound.min, bound.max));
        }
    }
    Ok(())
}

fn atomic(raw: &str) -> Option<u128> {
    let raw = raw.trim();
    (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
        .then(|| raw.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE: &str = r#"{
        "spread_bps": {"min": 5, "max": 300},
        "min_spread_bps": {"min": 0, "max": 100},
        "ttl_secs": {"min": 30, "max": 600},
        "refresh_threshold_bps": {"min": 1, "max": 100},
        "tick_interval_secs": {"min": 1, "max": 60},
        "max_orders": {"min": 1, "max": 10},
        "order_size": {"min": "1000000", "max": "5000000000"}
    }"#;

    fn view() -> SettingsView {
        let toml = include_str!("../../../stitch.example.toml");
        crate::setup::read_settings(toml).expect("the example config reads")
    }

    fn envelope() -> Envelope {
        Envelope::parse(ENVELOPE).unwrap()
    }

    #[test]
    fn unknown_keys_and_inverted_bounds_are_refused() {
        assert!(Envelope::parse(r#"{"spread": {"min": 1, "max": 2}}"#).is_err());
        assert!(Envelope::parse(r#"{"spread_bps": {"min": 9, "max": 2}}"#).is_err());
        assert!(Envelope::parse(r#"{"order_size": {"min": "x", "max": "2"}}"#).is_err());
        assert_eq!(Envelope::parse("{}").unwrap(), Envelope::default());
    }

    #[test]
    fn the_shipped_example_parses_and_allows_nothing_until_filled_in() {
        let example =
            Envelope::parse(include_str!("../../../deploy/hosted-envelope.example.json")).unwrap();
        let before = view();
        let mut after = before.clone();
        after.ttl_secs = before.ttl_secs + 1;
        assert!(check(&before, &after, &example).is_err());
    }

    #[test]
    fn an_unchanged_save_passes_even_with_no_envelope() {
        assert!(check(&view(), &view(), &Envelope::default()).is_ok());
    }

    #[test]
    fn spreads_inside_the_envelope_pass_and_outside_fail() {
        let before = view();
        let mut after = before.clone();
        after.buy = SpreadEdit {
            kind: SpreadKind::Bps,
            value: "42.5".into(),
        };
        assert!(check(&before, &after, &envelope()).is_ok());
        after.buy.value = "301".into();
        assert!(check(&before, &after, &envelope())
            .unwrap_err()
            .contains("outside"));
        after.buy = SpreadEdit {
            kind: SpreadKind::Abs,
            value: "0.01".into(),
        };
        assert!(check(&before, &after, &envelope())
            .unwrap_err()
            .contains("basis points"));
    }

    #[test]
    fn a_field_without_a_bound_cannot_change() {
        let before = view();
        let mut after = before.clone();
        after.ttl_secs = before.ttl_secs + 1;
        assert!(check(&before, &after, &Envelope::default())
            .unwrap_err()
            .contains("no bound"));
        let mut after = before.clone();
        after.sell_sizing.total_liquidity = "123".into();
        assert!(check(&before, &after, &envelope())
            .unwrap_err()
            .contains("no bound"));
    }

    #[test]
    fn sizing_amounts_must_be_whole_atomic_units_inside_bounds() {
        let before = view();
        let mut after = before.clone();
        after.buy_sizing.order_size = "2000000".into();
        assert!(check(&before, &after, &envelope()).is_ok());
        after.buy_sizing.order_size = "max".into();
        assert!(check(&before, &after, &envelope()).is_err());
        after.buy_sizing.order_size = "5000000001".into();
        assert!(check(&before, &after, &envelope()).is_err());
    }

    #[test]
    fn rpcs_feed_rfq_vault_and_switches_are_frozen() {
        let before = view();
        let edits: Vec<Box<dyn Fn(&mut SettingsView)>> = vec![
            Box::new(|v| v.rpc_url = "https://evil.example".into()),
            Box::new(|v| v.rpc_fallback_urls = vec!["https://evil.example".into()]),
            Box::new(|v| v.feed_url = "https://evil.example".into()),
            Box::new(|v| v.vault_address = "0x0000000000000000000000000000000000000001".into()),
            Box::new(|v| v.rfq_enabled = !v.rfq_enabled),
            Box::new(|v| v.book_enabled = !v.book_enabled),
            Box::new(|v| v.taker_enabled = !v.taker_enabled),
            Box::new(|v| v.lean_enabled = !v.lean_enabled),
            Box::new(|v| v.twap_window_secs = "60".into()),
            Box::new(|v| v.modules_enabled = !v.modules_enabled),
        ];
        for edit in edits {
            let mut after = before.clone();
            edit(&mut after);
            let err = check(&before, &after, &envelope()).unwrap_err();
            assert!(err.contains("can't change"), "{err}");
        }
    }
}
