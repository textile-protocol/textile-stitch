// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use super::{config::*, *};

/// A policy proposal has no execution authority. The host composes it with
/// other proposals and enforces its own position and spending constraints.
pub struct QuoteProposal {
    pub buy_bps: Option<u32>,
    pub sell_bps: Option<u32>,
    pub reasons: Vec<String>,
}
pub struct RebalanceProposal {
    pub sell: U256,
    pub reasons: Vec<String>,
}

pub fn inventory(cfg: &InventoryConfig, ctx: &Context, share: u32) -> QuoteProposal {
    let distance = share.abs_diff(cfg.target_bps);
    let skew = (cfg.max_skew_bps.saturating_mul(distance) / (cfg.max_bps - cfg.target_bps))
        .min(cfg.max_skew_bps);
    let floor = cfg.spread_floor_bps;
    let (buy_bps, sell_bps) = if share >= cfg.target_bps {
        (
            ctx.base_buy_bps
                .filter(|_| share < cfg.max_bps)
                .map(|b| b.saturating_add(skew).max(floor).min(9999)),
            ctx.base_sell_bps.map(|b| b.saturating_sub(skew).max(floor)),
        )
    } else {
        (
            ctx.base_buy_bps.map(|b| b.saturating_sub(skew).max(floor)),
            ctx.base_sell_bps.map(|b| b.saturating_add(skew).max(floor)),
        )
    };
    QuoteProposal {
        buy_bps,
        sell_bps,
        reasons: vec![format!(
            "Corridor inventory {share} bps; target {} bps",
            cfg.target_bps
        )],
    }
}

pub fn spread_extra(cfg: &SpreadsConfig, now: u64, history: &[PricePoint]) -> Option<u32> {
    let points: Vec<_> = history
        .iter()
        .filter(|p| {
            p.timestamp <= now
                && now - p.timestamp <= cfg.window_secs
                && p.price.is_finite()
                && p.price > 0.0
        })
        .collect();
    let first = points.first()?;
    let last = points.last()?;
    if last.timestamp.saturating_sub(first.timestamp) < cfg.warmup_secs {
        return None;
    }
    let low = points.iter().map(|p| p.price).fold(f64::INFINITY, f64::min);
    let high = points.iter().map(|p| p.price).fold(0.0, f64::max);
    Some((((high / low - 1.0) * 10_000.0 * cfg.multiplier).ceil() as u32).min(cfg.max_extra_bps))
}

pub fn rebalance(
    cfg: &RebalanceConfig,
    inventory: &InventoryConfig,
    ctx: &Context,
    nav: U256,
    value: U256,
    share: u32,
) -> RebalanceProposal {
    if share < cfg.trigger_bps {
        return RebalanceProposal {
            sell: U256::ZERO,
            reasons: vec![],
        };
    }
    if !ctx.reserved_corridor.is_zero() || !ctx.reserved_settlement.is_zero() {
        return RebalanceProposal {
            sell: U256::ZERO,
            reasons: vec!["Rebalance waits for outstanding quotes to settle or expire".into()],
        };
    }
    let excess = value.saturating_sub(fraction(nav, inventory.target_bps));
    let budget = excess.min(fraction(nav, cfg.max_trade_bps));
    let sell = math::collateral_for_debt(
        ctx.price,
        budget,
        ctx.settlement_decimals,
        ctx.corridor_decimals,
    )
    .min(ctx.available_corridor)
    .min(ctx.max_sell);
    let reasons = if sell.is_zero() {
        vec![]
    } else {
        vec![if cfg.dealer.is_some() {
            "Excess corridor inventory: request a spot sale"
        } else {
            "Spot sale indicated; no dealer configured"
        }
        .into()]
    };
    RebalanceProposal { sell, reasons }
}
