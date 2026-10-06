// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use alloy_primitives::Address;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Off,
    #[default]
    Shadow,
    Live,
}

/// First-party policies. The root `modules_enabled` flag gates all runtime and UI access.
/// Defaults observe only; selecting Live is an explicit operator action.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ModulesConfig {
    pub mode: Mode,
    pub inventory: InventoryConfig,
    pub spreads: SpreadsConfig,
    pub rebalance: RebalanceConfig,
}
impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Shadow,
            inventory: InventoryConfig::default(),
            spreads: SpreadsConfig::default(),
            rebalance: RebalanceConfig::default(),
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct InventoryConfig {
    pub enabled: bool,
    pub target_bps: u32,
    pub max_bps: u32,
    pub max_skew_bps: u32,
    pub spread_floor_bps: u32,
}
impl Default for InventoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            target_bps: 3000,
            max_bps: 6000,
            max_skew_bps: 50,
            spread_floor_bps: 5,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SpreadsConfig {
    pub enabled: bool,
    /// Reduce the volatility buffer on inventory-reducing trades.
    /// Requires enabled inventory balancing to take effect.
    pub inventory_aware: bool,
    pub window_secs: u64,
    pub warmup_secs: u64,
    pub multiplier: f64,
    pub max_extra_bps: u32,
}
impl Default for SpreadsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            inventory_aware: false,
            window_secs: 300,
            warmup_secs: 30,
            multiplier: 1.0,
            max_extra_bps: 100,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RebalanceConfig {
    pub enabled: bool,
    pub trigger_bps: u32,
    /// Maximum NAV fraction sold in a single attempt.
    pub max_trade_bps: u32,
    pub max_slippage_bps: u32,
    pub cooldown_secs: u64,
    pub order_lifetime_secs: u64,
    pub dealer: Option<DealerConfig>,
}
impl Default for RebalanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_bps: 5000,
            max_trade_bps: 200,
            max_slippage_bps: 50,
            cooldown_secs: 300,
            order_lifetime_secs: 60,
            dealer: None,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DealerConfig {
    pub url: String,
    /// Only this counterparty may fill a signed rebalance order.
    pub taker: String,
    /// Optional bearer credential; the secret itself is never in TOML or the panel.
    pub api_key_env: Option<String>,
}
impl ModulesConfig {
    pub fn validate(&self) -> Result<()> {
        let i = &self.inventory;
        ensure!(
            i.target_bps > 0 && i.target_bps < i.max_bps && i.max_bps < 10_000,
            "modules inventory requires 0 < target_bps < max_bps < 10000"
        );
        ensure!(
            i.max_skew_bps < 5000 && i.spread_floor_bps < 5000,
            "module spread limits must be below 5000 bps"
        );
        let s = &self.spreads;
        ensure!(
            (10..=3600).contains(&s.window_secs)
                && s.warmup_secs > 0
                && s.warmup_secs < s.window_secs,
            "modules spreads require 0 < warmup_secs < window_secs <= 3600 (window at least 10s)"
        );
        ensure!(
            s.multiplier.is_finite()
                && (0.0..=10.0).contains(&s.multiplier)
                && s.max_extra_bps < 5000,
            "invalid modules volatility spread limits"
        );
        let r = &self.rebalance;
        ensure!(
            !r.enabled || (r.trigger_bps > i.target_bps && r.trigger_bps <= i.max_bps),
            "rebalance trigger_bps must be above inventory target and at most max_bps"
        );
        ensure!(
            (1..=1000).contains(&r.max_trade_bps) && r.max_slippage_bps < 1000,
            "rebalance max_trade_bps must be 1..1000 and max_slippage_bps below 1000"
        );
        ensure!(
            (10..=300).contains(&r.order_lifetime_secs)
                && r.cooldown_secs >= r.order_lifetime_secs + 30,
            "rebalance cooldown must cover order lifetime plus 30s; lifetime must be 10..300s"
        );
        if let Some(d) = &r.dealer {
            crate::config::assert_feed_url(&d.url, "modules.rebalance.dealer.url")?;
            let url = url::Url::parse(&d.url)?;
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "dealer URL cannot contain credentials, query or fragment"
            );
            ensure!(
                !d.taker.parse::<Address>()?.is_zero(),
                "dealer taker cannot be zero"
            );
            if let Some(env) = &d.api_key_env {
                ensure!(
                    !env.is_empty() && env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "invalid dealer api_key_env"
                );
            }
        }
        Ok(())
    }
}
