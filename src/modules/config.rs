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
    pub liquidity: LiquidityConfig,
    pub curve: CurveConfig,
}
impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Shadow,
            inventory: InventoryConfig::default(),
            spreads: SpreadsConfig::default(),
            rebalance: RebalanceConfig::default(),
            liquidity: LiquidityConfig::default(),
            curve: CurveConfig::default(),
        }
    }
}
impl ModulesConfig {
    /// What can run on this bot's funds. Inventory balancing, dynamic spreads,
    /// spot rebalancing and the liquidity schedule read a vault's NAV and act
    /// through it, so on a bot that trades from its own wallet they are off
    /// whatever the file says; only the inventory curve runs there. On a vault
    /// every module can run.
    pub fn runnable(&self, on_vault: bool) -> Self {
        if on_vault {
            return self.clone();
        }
        let mut wallet = self.clone();
        wallet.inventory.enabled = false;
        wallet.spreads.enabled = false;
        wallet.rebalance.enabled = false;
        wallet.liquidity.enabled = false;
        wallet
    }

    /// The vault-only modules switched on in the file, by name: what
    /// [`Self::runnable`] turns off on a wallet bot.
    pub fn vault_only_enabled(&self) -> Vec<&'static str> {
        [
            (self.inventory.enabled, "inventory balancing"),
            (self.spreads.enabled, "dynamic spreads"),
            (self.rebalance.enabled, "spot rebalancing"),
            (self.liquidity.enabled, "the liquidity schedule"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}
/// The inventory curve ([`crate::pricing::curve`]): Swap quotes lean on the
/// bot's own share of the corridor currency and each quote pays the average
/// lean across the stretch of holdings it moves. Runs on a wallet bot and on a
/// vault; off unless switched on. The two offsets are in the corridor's soft
/// currency (NGN for cNGN), measured from the reference book on every quote;
/// positive widens the curve, negative narrows it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CurveConfig {
    pub enabled: bool,
    /// Soft currency below the reference mid where the curve starts, at no
    /// corridor holdings. Negative starts it above the mid.
    pub start_below_mid: f64,
    /// Soft currency above the reference buy where it ends, holding nothing
    /// else. Negative ends it below the buy.
    pub end_above_buy: f64,
    /// Exponent on the holdings share.
    pub shape: f64,
}
impl Default for CurveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start_below_mid: 0.0,
            end_above_buy: 0.0,
            shape: crate::pricing::curve::DEFAULT_SHAPE,
        }
    }
}
impl CurveConfig {
    pub fn params(&self) -> crate::pricing::curve::CurveParams {
        crate::pricing::curve::CurveParams {
            start_below_mid: self.start_below_mid,
            end_above_buy: self.end_above_buy,
            shape: self.shape,
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
/// Manual sales are opt-in per request. Dealer execution requires an explicit method.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RebalanceMethod {
    #[default]
    Manual,
    Dealer,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RebalanceConfig {
    pub enabled: bool,
    pub method: RebalanceMethod,
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
            method: RebalanceMethod::Manual,
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
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}
impl Weekday {
    /// Days after Monday, the start of a schedule week.
    pub fn index(self) -> u64 {
        self as u64
    }
}
/// Moves the vault's `minLiquidSettlement` on a UTC market-hours schedule.
///
/// Only settlement kept liquid up to that floor can back a bundled fill; the
/// rest earns yield in the adapter and is reachable only by a single-order fill
/// through the VaultOrderExecutor. Both floors are shares of free settlement.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LiquidityConfig {
    pub enabled: bool,
    /// Start of market hours, `HH:MM` UTC.
    pub open_utc: String,
    /// End of market hours, `HH:MM` UTC. Earlier than `open_utc` runs past midnight.
    pub close_utc: String,
    /// UTC weekdays a market-hours window opens on.
    pub days: Vec<Weekday>,
    pub market_floor_bps: u32,
    pub off_hours_floor_bps: u32,
    /// Gaps under this share of free settlement are left alone, so NAV drift
    /// and small fills do not each cost a transaction.
    pub tolerance_bps: u32,
    /// How the bot gets operator-admin authority over the floor. Required for Live.
    pub authority: Option<RolesAuthority>,
}
impl Default for LiquidityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            open_utc: "08:00".into(),
            close_utc: "17:00".into(),
            days: vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ],
            market_floor_bps: 5000,
            off_hours_floor_bps: 0,
            tolerance_bps: 500,
            authority: None,
        }
    }
}
impl LiquidityConfig {
    /// Minutes after midnight UTC for `open_utc` and `close_utc`.
    pub fn window_minutes(&self) -> Result<(u64, u64)> {
        Ok((parse_hhmm(&self.open_utc)?, parse_hhmm(&self.close_utc)?))
    }
}
/// A Zodiac Roles Modifier (v2) enabled on the operator-admin Safe. The bot's
/// signer holds `role_key`, scoped by the operator to `setMinLiquidSettlement`
/// on this vault, so the bot can move the floor and nothing else.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RolesAuthority {
    pub roles_modifier: String,
    /// `bytes32` role key, `0x` + 64 hex.
    pub role_key: String,
}
impl RolesAuthority {
    pub fn modifier(&self) -> Result<Address> {
        let address = self
            .roles_modifier
            .starts_with("0x")
            .then(|| self.roles_modifier.parse::<Address>().ok())
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("liquidity roles_modifier must be a 0x address"))?;
        ensure!(
            !address.is_zero(),
            "liquidity roles_modifier cannot be zero"
        );
        Ok(address)
    }
    pub fn key(&self) -> Result<alloy_primitives::B256> {
        let raw = self.role_key.strip_prefix("0x").unwrap_or_default();
        ensure!(
            raw.len() == 64 && raw.chars().all(|c| c.is_ascii_hexdigit()),
            "liquidity role_key must be 0x followed by 64 hex characters"
        );
        let key = self
            .role_key
            .parse::<alloy_primitives::B256>()
            .map_err(|_| anyhow::anyhow!("liquidity role_key is not bytes32"))?;
        ensure!(!key.is_zero(), "liquidity role_key cannot be zero");
        Ok(key)
    }
}
fn parse_hhmm(raw: &str) -> Result<u64> {
    let parsed = raw
        .split_once(':')
        .filter(|(h, m)| h.len() == 2 && m.len() == 2)
        .and_then(|(h, m)| Some((h.parse::<u64>().ok()?, m.parse::<u64>().ok()?)))
        .filter(|(h, m)| *h < 24 && *m < 60);
    let (h, m) = parsed.ok_or_else(|| anyhow::anyhow!("liquidity times must be HH:MM (UTC)"))?;
    Ok(h * 60 + m)
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
        self.validate_liquidity()?;
        let c = &self.curve;
        ensure!(
            [c.start_below_mid, c.end_above_buy]
                .iter()
                .all(|v| v.is_finite()),
            "modules curve offsets must be a finite amount of the soft currency (positive \
             widens the curve, negative narrows it)"
        );
        ensure!(
            c.shape.is_finite() && (0.5..=4.0).contains(&c.shape),
            "modules curve shape must be between 0.5 and 4"
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
impl ModulesConfig {
    fn validate_liquidity(&self) -> Result<()> {
        let l = &self.liquidity;
        let (open, close) = l.window_minutes()?;
        ensure!(
            open != close,
            "liquidity open_utc and close_utc must differ"
        );
        let mut days = l.days.clone();
        days.sort();
        days.dedup();
        ensure!(
            !l.days.is_empty() && days.len() == l.days.len(),
            "liquidity days must list at least one weekday, each once"
        );
        ensure!(
            l.market_floor_bps <= 10_000 && l.off_hours_floor_bps <= l.market_floor_bps,
            "liquidity floors must satisfy off_hours_floor_bps <= market_floor_bps <= 10000"
        );
        ensure!(
            (1..=5000).contains(&l.tolerance_bps),
            "liquidity tolerance_bps must be 1..5000"
        );
        if let Some(authority) = &l.authority {
            authority.modifier()?;
            authority.key()?;
        }
        ensure!(
            !(l.enabled && self.mode == Mode::Live && l.authority.is_none()),
            "liquidity schedule in Live mode needs [modules.liquidity.authority]: the Roles \
             modifier and role key that let this bot change the floor"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(liquidity: LiquidityConfig) -> ModulesConfig {
        ModulesConfig {
            liquidity,
            ..ModulesConfig::default()
        }
    }

    fn authority() -> Option<RolesAuthority> {
        Some(RolesAuthority {
            roles_modifier: "0x00000000000000000000000000000000000000cc".into(),
            role_key: format!("0x{}", "11".repeat(32)),
        })
    }

    #[test]
    fn the_default_schedule_validates() {
        ModulesConfig::default().validate().unwrap();
    }

    #[test]
    fn existing_configs_without_a_liquidity_table_still_parse() {
        let parsed: ModulesConfig = toml::from_str("mode = \"live\"\n").unwrap();
        assert_eq!(parsed.liquidity, LiquidityConfig::default());
        assert!(!parsed.liquidity.enabled);
    }

    #[test]
    fn the_liquidity_table_round_trips_through_toml() {
        let config = with(LiquidityConfig {
            enabled: true,
            authority: authority(),
            ..LiquidityConfig::default()
        });
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[liquidity]"), "{text}");
        assert_eq!(toml::from_str::<ModulesConfig>(&text).unwrap(), config);
    }

    #[test]
    fn times_must_be_hh_mm_and_differ() {
        for (open, close) in [
            ("8:00", "17:00"),
            ("24:00", "17:00"),
            ("08:60", "17:00"),
            ("08:00", "08:00"),
        ] {
            let c = with(LiquidityConfig {
                open_utc: open.into(),
                close_utc: close.into(),
                ..LiquidityConfig::default()
            });
            assert!(c.validate().is_err(), "{open}-{close}");
        }
        with(LiquidityConfig {
            open_utc: "22:00".into(),
            close_utc: "06:00".into(),
            ..LiquidityConfig::default()
        })
        .validate()
        .unwrap();
    }

    #[test]
    fn days_floors_and_tolerance_are_bounded() {
        let bad = [
            LiquidityConfig {
                days: vec![],
                ..LiquidityConfig::default()
            },
            LiquidityConfig {
                days: vec![Weekday::Mon, Weekday::Mon],
                ..LiquidityConfig::default()
            },
            LiquidityConfig {
                market_floor_bps: 10_001,
                ..LiquidityConfig::default()
            },
            LiquidityConfig {
                market_floor_bps: 1_000,
                off_hours_floor_bps: 2_000,
                ..LiquidityConfig::default()
            },
            LiquidityConfig {
                tolerance_bps: 0,
                ..LiquidityConfig::default()
            },
            LiquidityConfig {
                tolerance_bps: 5_001,
                ..LiquidityConfig::default()
            },
        ];
        for liquidity in bad {
            assert!(with(liquidity.clone()).validate().is_err(), "{liquidity:?}");
        }
    }

    #[test]
    fn live_needs_a_well_formed_authority() {
        let enabled = LiquidityConfig {
            enabled: true,
            ..LiquidityConfig::default()
        };
        let live = |liquidity| ModulesConfig {
            mode: Mode::Live,
            ..with(liquidity)
        };
        assert!(live(enabled.clone()).validate().is_err());
        with(enabled.clone()).validate().unwrap(); // Preview needs none.
        live(LiquidityConfig {
            authority: authority(),
            ..enabled.clone()
        })
        .validate()
        .unwrap();
        for (modifier, key) in [
            (
                "0x0000000000000000000000000000000000000000",
                "11".repeat(32),
            ),
            ("0xnot", "11".repeat(32)),
            ("00000000000000000000000000000000000000cc", "11".repeat(32)),
            (
                "0x00000000000000000000000000000000000000cc",
                "00".repeat(32),
            ),
            (
                "0x00000000000000000000000000000000000000cc",
                "11".repeat(31),
            ),
        ] {
            let c = live(LiquidityConfig {
                authority: Some(RolesAuthority {
                    roles_modifier: modifier.into(),
                    role_key: format!("0x{key}"),
                }),
                ..enabled.clone()
            });
            assert!(c.validate().is_err(), "{modifier} {key}");
        }
    }
}
