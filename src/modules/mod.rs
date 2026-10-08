// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! First-party policies. Pure decisions are shared by live RFQ and replay.
//! Modules never receive a signer, RPC client or authority to move funds.
pub mod config;
pub mod dealer;
pub mod history;
pub mod manual;
pub mod replay;
pub mod runtime;
mod strategies;

use crate::pricing::quote::{ask_price, bid_price};
use crate::pricing::tick::is_stale;
use crate::rfq::{math, responder::CorridorBook};
use alloy_primitives::U256;
pub use config::{Mode, ModulesConfig};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
/// Leads the decision reason while dynamic spreads quote at their cap because
/// the price window has not spanned `warmup_secs` yet. The panel matches on it.
pub const WARMUP_REASON: &str = "Warming up dynamic spreads";
pub const REGISTRY: [(&str, &str); 3] = [
    ("inventory", "Inventory balancing"),
    ("spreads", "Dynamic spreads"),
    ("rebalance", "Spot rebalancing"),
];

pub mod atomic {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &U256, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<U256, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricePoint {
    pub timestamp: u64,
    pub price: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Context {
    pub now: u64,
    pub price: f64,
    pub price_at: u64,
    pub balances_at: u64,
    pub staleness_secs: u64,
    pub settlement: U256,
    pub corridor: U256,
    pub available_settlement: U256,
    pub available_corridor: U256,
    pub reserved_settlement: U256,
    pub reserved_corridor: U256,
    pub settlement_decimals: u8,
    pub corridor_decimals: u8,
    pub max_sell: U256,
    pub base_buy_bps: Option<u32>,
    pub base_sell_bps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub version: u32,
    pub at: u64,
    pub inventory_bps: Option<u32>,
    pub buy_bps: Option<u32>,
    pub sell_bps: Option<u32>,
    #[serde(with = "atomic")]
    pub buy_limit: U256,
    #[serde(with = "atomic")]
    pub rebalance_sell: U256,
    pub volatility_bps: u32,
    pub reasons: Vec<String>,
    pub blocked: bool,
}

/// Side offsets, floors and vetoes compose here; execution remains in RFQ.
pub fn evaluate(config: &ModulesConfig, ctx: &Context, history: &[PricePoint]) -> Decision {
    let mut d = Decision {
        version: VERSION,
        at: ctx.now,
        inventory_bps: None,
        buy_bps: ctx.base_buy_bps,
        sell_bps: ctx.base_sell_bps,
        buy_limit: ctx.available_settlement,
        rebalance_sell: U256::ZERO,
        volatility_bps: 0,
        reasons: vec![],
        blocked: false,
    };
    let valid = ctx.price.is_finite()
        && ctx.price > 0.0
        && !is_stale(ctx.price_at, ctx.now, ctx.staleness_secs)
        && ctx.balances_at <= ctx.now
        && ctx.now - ctx.balances_at <= 3
        && ctx.corridor_decimals <= 18
        && ctx.settlement_decimals <= 18;
    if !valid {
        return blocked(d, "Fresh prices and vault balances required");
    }
    let c_value = math::debt_for_collateral(
        ctx.price,
        ctx.corridor,
        ctx.settlement_decimals,
        ctx.corridor_decimals,
    );
    if !ctx.corridor.is_zero() && c_value.is_zero() {
        return blocked(d, "Inventory cannot be valued at this price");
    }
    let Some(nav) = ctx.settlement.checked_add(c_value).filter(|v| !v.is_zero()) else {
        return blocked(d, "Vault has no usable NAV");
    };
    let Some(share) = ratio_bps(c_value, nav) else {
        return blocked(d, "Inventory exceeds supported numeric range");
    };
    d.inventory_bps = Some(share);
    if config.inventory.enabled {
        let proposal = strategies::inventory(&config.inventory, ctx, share);
        d.buy_bps = proposal.buy_bps;
        d.sell_bps = proposal.sell_bps;
        d.reasons.extend(proposal.reasons);
    }
    if config.spreads.enabled {
        // A cold window (a fresh start, or source timestamps too sparse to span
        // the warmup yet) quotes at the cap: the widest buffer this module can
        // ever add. Pulling both sides instead would take the maker off the
        // venue after every restart until the feed ticks past the warmup, which
        // on a 3-minute feed is up to 3 minutes of no liquidity.
        let measured = strategies::spread_extra(&config.spreads, ctx.now, history);
        let extra = measured.unwrap_or(config.spreads.max_extra_bps);
        d.volatility_bps = extra;
        let (buy_extra, sell_extra) = strategies::spread_additions(config, share, extra);
        d.buy_bps = d.buy_bps.map(|b| b.saturating_add(buy_extra).min(9999));
        d.sell_bps = d.sell_bps.map(|b| b.saturating_add(sell_extra));
        if measured.is_none() {
            d.reasons.push(format!(
                "{WARMUP_REASON}: holding the maximum buffer (buy +{buy_extra} bps, sell +{sell_extra} bps) until {}s of price history",
                config.spreads.warmup_secs
            ));
        } else if extra > 0 {
            d.reasons.push(if config.spreads.inventory_aware && config.inventory.enabled {
                format!("Inventory-aware volatility buffer: buy +{buy_extra} bps, sell +{sell_extra} bps before side limits")
            } else {
                format!("Market movement adds {extra} bps")
            });
        }
    }
    // A conservative headroom cap: do not spend more than the fair value of
    // remaining corridor headroom, discounted for our bid. The firm path also
    // checks exact post-trade balances, including rounding.
    if config.inventory.enabled {
        let headroom = fraction(nav, config.inventory.max_bps).saturating_sub(c_value);
        d.buy_limit = d.buy_limit.min(fraction(
            headroom,
            10_000u32.saturating_sub(d.buy_bps.unwrap_or(9999)),
        ));
        if !ctx.reserved_settlement.is_zero() {
            d.buy_bps = None;
            d.reasons
                .push("An inventory-increasing quote is still outstanding".into());
        }
    }
    if config.rebalance.enabled {
        let proposal = strategies::rebalance(
            &config.rebalance,
            &config.inventory,
            ctx,
            nav,
            c_value,
            share,
        );
        d.rebalance_sell = proposal.sell;
        d.reasons.extend(proposal.reasons);
    }
    d
}

fn blocked(mut d: Decision, reason: &str) -> Decision {
    d.blocked = true;
    d.buy_bps = None;
    d.sell_bps = None;
    d.rebalance_sell = U256::ZERO;
    d.reasons.push(reason.into());
    d
}
pub fn fraction(value: U256, bps: u32) -> U256 {
    math::fee_on(value, bps)
}
fn ratio_bps(value: U256, total: U256) -> Option<u32> {
    // U512 keeps even a maximal U256 balance from wrapping during a ratio.
    use alloy_primitives::U512;
    (!total.is_zero())
        .then(|| ((U512::from(value) * U512::from(10_000u32)) / U512::from(total)).to::<u32>())
}

pub fn base_spreads(book: &CorridorBook, price: f64) -> (Option<u32>, Option<u32>) {
    use crate::pricing::quote::Spread;
    let buy = book.buy_spread.map(|s| match s {
        Spread::Bps(b) => b,
        Spread::Abs(_) => ((1.0 - bid_price(price, s) / price) * 10_000.0)
            .max(0.0)
            .ceil() as u32,
    });
    let sell = book.sell_spread.map(|s| match s {
        Spread::Bps(b) => b,
        Spread::Abs(_) => ((ask_price(price, s) / price - 1.0) * 10_000.0)
            .max(0.0)
            .ceil() as u32,
    });
    (buy, sell)
}

pub fn apply(book: &CorridorBook, decision: &Decision) -> CorridorBook {
    use crate::{config::RfqCapacity, pricing::quote::Spread};
    let mut out = book.clone();
    out.buy_spread = decision.buy_bps.map(Spread::Bps);
    out.sell_spread = decision.sell_bps.map(Spread::Bps);
    if decision.buy_bps.is_none() {
        out.buy_capacity_debt = None;
    } else {
        out.buy_capacity_debt = out.buy_capacity_debt.map(|c| {
            RfqCapacity::Exact(match c {
                RfqCapacity::Exact(n) => n.min(decision.buy_limit),
                RfqCapacity::Wallet => decision.buy_limit,
            })
        });
    }
    if decision.sell_bps.is_none() {
        out.sell_capacity_collateral = None;
    }
    out
}

pub fn post_buy_allowed(config: &ModulesConfig, ctx: &Context, paid: U256, received: U256) -> bool {
    if !config.inventory.enabled {
        return true;
    }
    let Some(s) = ctx.settlement.checked_sub(paid) else {
        return false;
    };
    let Some(c) = ctx.corridor.checked_add(received) else {
        return false;
    };
    let value =
        math::debt_for_collateral(ctx.price, c, ctx.settlement_decimals, ctx.corridor_decimals);
    if value.is_zero() && !c.is_zero() {
        return false;
    }
    use alloy_primitives::U512;
    value.checked_add(s).is_some_and(|nav| {
        U512::from(value) * U512::from(10_000u32)
            <= U512::from(nav) * U512::from(config.inventory.max_bps)
    })
}
