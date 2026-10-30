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

/// One policy in the quote-module group, for asking what mode it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    Inventory,
    Spreads,
    Rebalance,
    Liquidity,
    Curve,
    ProfitUnwind,
}
impl Module {
    pub const ALL: [Module; 6] = [
        Module::Inventory,
        Module::Spreads,
        Module::Rebalance,
        Module::Liquidity,
        Module::Curve,
        Module::ProfitUnwind,
    ];
}

/// First-party policies. The root `modules_enabled` flag gates all runtime and UI access.
/// Each module runs Off, Shadow (Preview) or Live on its own: its table's `mode`,
/// or the shared `mode` here when it names none. Defaults observe only;
/// selecting Live is an explicit operator action.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ModulesConfig {
    pub mode: Mode,
    pub inventory: InventoryConfig,
    pub spreads: SpreadsConfig,
    pub rebalance: RebalanceConfig,
    pub liquidity: LiquidityConfig,
    pub curve: CurveConfig,
    pub profit_unwind: ProfitUnwindConfig,
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
            profit_unwind: ProfitUnwindConfig::default(),
        }
    }
}
impl ModulesConfig {
    /// `module`'s switch and the mode its table names, if any.
    fn switch(&self, module: Module) -> (bool, Option<Mode>) {
        match module {
            Module::Inventory => (self.inventory.enabled, self.inventory.mode),
            Module::Spreads => (self.spreads.enabled, self.spreads.mode),
            Module::Rebalance => (self.rebalance.enabled, self.rebalance.mode),
            Module::Liquidity => (self.liquidity.enabled, self.liquidity.mode),
            Module::Curve => (self.curve.enabled, self.curve.mode),
            Module::ProfitUnwind => (self.profit_unwind.enabled, self.profit_unwind.mode),
        }
    }

    fn set_switch(&mut self, module: Module, enabled: bool, mode: Option<Mode>) {
        let (on, own) = match module {
            Module::Inventory => (&mut self.inventory.enabled, &mut self.inventory.mode),
            Module::Spreads => (&mut self.spreads.enabled, &mut self.spreads.mode),
            Module::Rebalance => (&mut self.rebalance.enabled, &mut self.rebalance.mode),
            Module::Liquidity => (&mut self.liquidity.enabled, &mut self.liquidity.mode),
            Module::Curve => (&mut self.curve.enabled, &mut self.curve.mode),
            Module::ProfitUnwind => (
                &mut self.profit_unwind.enabled,
                &mut self.profit_unwind.mode,
            ),
        };
        *on = enabled;
        *own = mode;
    }

    /// The mode `module` runs in: Off while switched off, otherwise its own
    /// `mode`, falling back to the shared one. While profit unwind follows
    /// the shared mode its older `live` switch still applies, so a shared
    /// Live without it previews, as it always has.
    pub fn mode_of(&self, module: Module) -> Mode {
        let (enabled, own) = self.switch(module);
        match (enabled, own, self.mode) {
            (false, _, _) => Mode::Off,
            (true, Some(mode), _) => mode,
            (true, None, Mode::Live)
                if module == Module::ProfitUnwind && !self.profit_unwind.live =>
            {
                Mode::Shadow
            }
            (true, None, shared) => shared,
        }
    }

    /// `module` runs, in Preview or Live.
    pub fn runs(&self, module: Module) -> bool {
        self.mode_of(module) != Mode::Off
    }

    /// `module` acts: it changes quotes, sends transactions or allows sales.
    pub fn is_live(&self, module: Module) -> bool {
        self.mode_of(module) == Mode::Live
    }

    /// Whether the module runtime runs at all. A shared mode other than Off
    /// keeps it recording even with every module off, as it always has; the
    /// panel saves Off once nothing runs ([`Self::normalized`]).
    pub fn runtime_on(&self) -> bool {
        self.mode != Mode::Off || Module::ALL.into_iter().any(|m| self.runs(m))
    }

    /// The same modes, in the shape earlier releases wrote whenever that
    /// shape can hold them, so a bot that predates per-module modes keeps
    /// reading the file. That shape is one shared mode, with profit unwind's
    /// `live = false` as the one way to preview beside a shared Live. Only
    /// modes it can't express (say inventory balancing Live beside dynamic
    /// spreads in Preview) keep Preview shared and name Live per module, and
    /// earlier releases refuse those. With nothing running the shared mode is
    /// Off. A switched-off module names no mode.
    pub fn normalized(&self) -> Self {
        let modes = Module::ALL.map(|m| (m, self.mode_of(m)));
        let unwind = self.mode_of(Module::ProfitUnwind);
        let running = |skip_unwind: bool| {
            modes
                .iter()
                .filter(move |(m, mode)| {
                    *mode != Mode::Off && !(skip_unwind && *m == Module::ProfitUnwind)
                })
                .map(|(_, mode)| *mode)
        };
        let all_are = |mode: Mode, skip_unwind: bool| running(skip_unwind).all(|m| m == mode);
        let shared = match running(false).next() {
            None => Mode::Off,
            Some(first) if all_are(first, false) => first,
            Some(_) if unwind == Mode::Shadow && all_are(Mode::Live, true) => Mode::Live,
            Some(_) => Mode::Shadow,
        };
        let mut c = self.clone();
        c.mode = shared;
        c.profit_unwind.live = unwind == Mode::Live;
        for (module, mode) in modes {
            c.set_switch(module, mode != Mode::Off, None);
            // What it runs in without a mode of its own.
            let follows = c.mode_of(module);
            if mode != Mode::Off && mode != follows {
                c.set_switch(module, true, Some(mode));
            }
        }
        c
    }

    /// The quote policies that change quotes: inventory balancing and dynamic
    /// spreads each switched on only while Live. Quotes are decided with this
    /// when one of the two runs Live and the other in Preview, so the Preview
    /// one is recorded without reaching a quote.
    pub fn live_quotes(&self) -> Self {
        let mut c = self.clone();
        c.inventory.enabled = self.is_live(Module::Inventory);
        c.spreads.enabled = self.is_live(Module::Spreads);
        c
    }

    /// One of inventory balancing and dynamic spreads runs Live and the other
    /// in Preview, so what quotes go out at isn't the recorded decision.
    pub fn quote_modes_differ(&self) -> bool {
        let (inventory, spreads) = (
            self.mode_of(Module::Inventory),
            self.mode_of(Module::Spreads),
        );
        inventory != Mode::Off && spreads != Mode::Off && inventory != spreads
    }

    /// What can run on this bot's funds. Inventory balancing, dynamic spreads,
    /// spot rebalancing and the liquidity schedule read a vault's NAV and act
    /// through it, so on a bot that trades from its own wallet they are off
    /// whatever the file says; only the inventory curve and profit unwind run
    /// there. On a vault every module can run.
    ///
    /// Normalized, so `enabled` means the module runs in Preview or Live and
    /// the runtime asks [`Self::mode_of`] only whether it is Live.
    pub fn runnable(&self, on_vault: bool) -> Self {
        let mut c = self.clone();
        if !on_vault {
            c.inventory.enabled = false;
            c.spreads.enabled = false;
            c.rebalance.enabled = false;
            c.liquidity.enabled = false;
        }
        c.normalized()
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
    /// This module's own run mode. Absent, it runs in the shared
    /// `[modules].mode`. Off while `enabled` is false either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
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
            mode: None,
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
/// Profit unwind ([`crate::pricing::unwind`]): when the feed has moved so the
/// corridor currency the bot holds is worth more than when it came in, the
/// price lifts by a slice of that gain, so takers bringing dollars clear the
/// pile faster and the bot keeps the rest. Its own module: it lifts whatever
/// price goes out, the inventory curve's or the flat one. Off unless switched
/// on, and only quotes in Live with `live` set. The two amounts are in the
/// corridor's soft currency, like the curve's offsets.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfitUnwindConfig {
    pub enabled: bool,
    /// This module's own run mode. Absent, it runs in the shared
    /// `[modules].mode`. Off while `enabled` is false either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Quote with the lift while following a shared Live mode. Off, that
    /// previews: the bot tracks the cost, computes the lift and logs the
    /// price it would quote, and quotes without it. From before each module
    /// had its own mode, which wins when set; the panel keeps this in step
    /// with it ([`ModulesConfig::normalized`]).
    pub live: bool,
    /// Share of the gain passed back as a better price, 0 to 1.
    pub share: f64,
    /// The most profit unwind ever lifts the price, soft currency per dollar.
    pub cap_ngn: f64,
    /// Gains smaller than this, soft currency per dollar, move nothing.
    pub min_gain_ngn: f64,
    /// The corridor-currency share, in percent, where profit unwind starts.
    /// Below it nothing moves; above it the lift grows to full at 100%.
    pub start_pct: f64,
    /// How often the cost the lift is sized on is frozen again. The gain
    /// itself follows every price.
    pub refresh_secs: u64,
}
impl Default for ProfitUnwindConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: None,
            live: false,
            share: 0.5,
            cap_ngn: 2.0,
            min_gain_ngn: 0.7,
            start_pct: 50.0,
            refresh_secs: 900,
        }
    }
}
impl ProfitUnwindConfig {
    pub fn params(&self) -> crate::pricing::unwind::UnwindParams {
        crate::pricing::unwind::UnwindParams {
            share: self.share,
            cap_ngn: self.cap_ngn,
            min_gain_ngn: self.min_gain_ngn,
            start_pct: self.start_pct,
            refresh_secs: self.refresh_secs,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct InventoryConfig {
    pub enabled: bool,
    /// This module's own run mode. Absent, it runs in the shared
    /// `[modules].mode`. Off while `enabled` is false either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    pub target_bps: u32,
    pub max_bps: u32,
    pub max_skew_bps: u32,
    pub spread_floor_bps: u32,
}
impl Default for InventoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: None,
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
    /// This module's own run mode. Absent, it runs in the shared
    /// `[modules].mode`. Off while `enabled` is false either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
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
            mode: None,
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
    /// This module's own run mode. Absent, it runs in the shared
    /// `[modules].mode`. Off while `enabled` is false either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
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
            mode: None,
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
    /// This module's own run mode. Absent, it runs in the shared
    /// `[modules].mode`. Off while `enabled` is false either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
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
            mode: None,
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
        let u = &self.profit_unwind;
        ensure!(
            u.share.is_finite() && (0.0..=1.0).contains(&u.share),
            "modules profit_unwind share must be between 0 and 1"
        );
        ensure!(
            [u.cap_ngn, u.min_gain_ngn]
                .iter()
                .all(|v| v.is_finite() && *v >= 0.0),
            "modules profit_unwind cap_ngn and min_gain_ngn must be zero or more"
        );
        ensure!(
            u.start_pct.is_finite() && (0.0..=99.0).contains(&u.start_pct),
            "modules profit_unwind start_pct must be between 0 and 99"
        );
        ensure!(
            u.refresh_secs >= 60,
            "modules profit_unwind refresh_secs must be at least 60"
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
            !(self.is_live(Module::Liquidity) && l.authority.is_none()),
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

    /// Every module set to `modes[i]`, each written as its own table's mode.
    fn each(modes: [Mode; 6]) -> ModulesConfig {
        let mut c = ModulesConfig {
            inventory: InventoryConfig::default(),
            ..ModulesConfig::default()
        };
        for (module, mode) in Module::ALL.into_iter().zip(modes) {
            c.set_switch(module, mode != Mode::Off, Some(mode));
        }
        c
    }

    fn modes(c: &ModulesConfig) -> [Mode; 6] {
        Module::ALL.map(|m| c.mode_of(m))
    }

    #[test]
    fn a_file_without_module_modes_reads_as_it_always_has() {
        let c: ModulesConfig = toml::from_str(
            "mode = \"live\"\n[inventory]\nenabled = true\n[spreads]\nenabled = false\n\
             [profit_unwind]\nenabled = true\nlive = false\n",
        )
        .unwrap();
        assert_eq!(c.mode_of(Module::Inventory), Mode::Live);
        assert_eq!(c.mode_of(Module::Spreads), Mode::Off);
        // Live without its own switch has always previewed.
        assert_eq!(c.mode_of(Module::ProfitUnwind), Mode::Shadow);
        let off = ModulesConfig {
            mode: Mode::Off,
            ..c.clone()
        };
        assert_eq!(modes(&off), [Mode::Off; 6]);
        assert!(!off.runtime_on());
        // A shared mode keeps the runtime recording with every module off.
        let mut idle = ModulesConfig::default();
        for module in Module::ALL {
            idle.set_switch(module, false, None);
        }
        assert!(idle.runtime_on());
    }

    #[test]
    fn a_module_mode_overrides_the_shared_one_and_off_still_wins() {
        let c: ModulesConfig = toml::from_str(
            "mode = \"shadow\"\n[inventory]\nenabled = true\nmode = \"live\"\n\
             [rebalance]\nenabled = false\nmode = \"live\"\n[spreads]\nenabled = true\n\
             mode = \"off\"\n[profit_unwind]\nenabled = true\nlive = true\nmode = \"live\"\n",
        )
        .unwrap();
        assert_eq!(c.mode_of(Module::Inventory), Mode::Live);
        assert_eq!(c.mode_of(Module::Rebalance), Mode::Off);
        assert_eq!(c.mode_of(Module::Spreads), Mode::Off);
        assert_eq!(c.mode_of(Module::ProfitUnwind), Mode::Live);
        assert!(c.runtime_on());
        let all_off = ModulesConfig {
            mode: Mode::Off,
            ..c
        };
        assert!(all_off.runtime_on(), "a Live module runs the runtime");
    }

    #[test]
    fn normalizing_keeps_every_module_mode_and_settles_on_one_shape() {
        let all = [Mode::Off, Mode::Shadow, Mode::Live];
        for n in 0..3usize.pow(6) {
            let wanted = std::array::from_fn(|i| all[n / 3usize.pow(i as u32) % 3]);
            let c = each(wanted).normalized();
            assert_eq!(modes(&c), wanted, "{wanted:?}");
            assert_eq!(c.normalized(), c, "idempotent for {wanted:?}");
            let running: Vec<_> = wanted.into_iter().filter(|m| *m != Mode::Off).collect();
            let uniform = running.windows(2).all(|w| w[0] == w[1]);
            let others: Vec<_> = wanted[..5].iter().filter(|m| **m != Mode::Off).collect();
            // Everything else Live and profit unwind previewing: the older
            // shape holds that as a shared Live with `live = false`.
            let unwind_previews = wanted[5] == Mode::Shadow
                && !others.is_empty()
                && others.iter().all(|m| **m == Mode::Live);
            let own = Module::ALL.map(|m| c.switch(m).1);
            if uniform {
                assert_eq!(c.mode, running.first().copied().unwrap_or(Mode::Off));
                assert_eq!(own, [None; 6], "the shape older bots read: {wanted:?}");
            } else if unwind_previews {
                assert_eq!(c.mode, Mode::Live);
                assert_eq!(own, [None; 6], "the shape older bots read: {wanted:?}");
            } else {
                assert_eq!(c.mode, Mode::Shadow);
                for (mode, own) in wanted.into_iter().zip(own) {
                    assert_eq!(own, (mode == Mode::Live).then_some(Mode::Live));
                }
            }
            assert_eq!(c.profit_unwind.live, wanted[5] == Mode::Live);
            assert_eq!(
                toml::from_str::<ModulesConfig>(&toml::to_string(&c).unwrap()).unwrap(),
                c
            );
        }
    }

    /// A Live bot with profit unwind still previewing, as earlier releases
    /// saved it, stays in that shape: a rolled-back bot can still read it.
    #[test]
    fn a_live_bot_previewing_profit_unwind_keeps_the_older_shape() {
        let file = "mode = \"live\"\n[curve]\nenabled = true\n[inventory]\nenabled = false\n\
                    [profit_unwind]\nenabled = true\nlive = false\n";
        let c: ModulesConfig = toml::from_str(file).unwrap();
        let saved = c.normalized();
        assert_eq!(modes(&saved), modes(&c));
        let text = toml::to_string(&saved).unwrap();
        assert_eq!(text.matches("mode =").count(), 1, "{text}");
        assert!(text.contains("live = false"), "{text}");
    }

    #[test]
    fn a_uniform_config_writes_no_module_modes() {
        let mut c = ModulesConfig {
            mode: Mode::Live,
            ..ModulesConfig::default()
        };
        c.inventory.mode = Some(Mode::Live);
        let text = toml::to_string(&c.normalized()).unwrap();
        assert!(text.starts_with("mode = \"live\""), "{text}");
        assert_eq!(text.matches("mode =").count(), 1, "{text}");
    }

    #[test]
    fn a_wallet_bot_runs_only_the_wallet_modules_normalized() {
        let c = each([
            Mode::Live,
            Mode::Shadow,
            Mode::Live,
            Mode::Shadow,
            Mode::Shadow,
            Mode::Shadow,
        ]);
        let wallet = c.runnable(false);
        assert_eq!(
            modes(&wallet),
            [
                Mode::Off,
                Mode::Off,
                Mode::Off,
                Mode::Off,
                Mode::Shadow,
                Mode::Shadow
            ]
        );
        assert_eq!(wallet.mode, Mode::Shadow);
        assert_eq!(Module::ALL.map(|m| wallet.switch(m).1), [None; 6]);
        assert_eq!(modes(&c.runnable(true)), modes(&c));
    }

    #[test]
    fn only_live_pricing_modules_decide_quotes() {
        let mixed = each([
            Mode::Live,
            Mode::Shadow,
            Mode::Off,
            Mode::Off,
            Mode::Off,
            Mode::Off,
        ]);
        assert!(mixed.quote_modes_differ());
        let quotes = mixed.live_quotes();
        assert!(quotes.inventory.enabled && !quotes.spreads.enabled);
        for same in [Mode::Shadow, Mode::Live] {
            let c = each([same, same, Mode::Off, Mode::Off, Mode::Off, Mode::Off]);
            assert!(!c.quote_modes_differ());
        }
        let alone = each([
            Mode::Live,
            Mode::Off,
            Mode::Off,
            Mode::Off,
            Mode::Off,
            Mode::Off,
        ]);
        assert!(!alone.quote_modes_differ());
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
                                                   // Its own Live mode needs one too; another module's doesn't.
        let own = |mode| {
            let mut c = with(LiquidityConfig {
                mode: Some(mode),
                ..enabled.clone()
            });
            c.inventory.mode = Some(Mode::Live);
            c
        };
        assert!(own(Mode::Live).validate().is_err());
        own(Mode::Shadow).validate().unwrap();
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
