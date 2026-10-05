// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Conditional historical replay. Prices are observed in order; each portfolio
//! evolves independently. Imported customer limits and dealer depth determine
//! simulated fills. No module has access to later events or live execution.
use super::*;
use anyhow::{ensure, Result};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub version: u32,
    pub chain_id: u64,
    pub corridor_token: String,
    pub settlement_token: String,
    pub corridor_decimals: u8,
    pub settlement_decimals: u8,
    #[serde(with = "atomic")]
    pub initial_settlement: U256,
    #[serde(with = "atomic")]
    pub initial_corridor: U256,
    #[serde(with = "atomic")]
    pub max_order_settlement: U256,
    #[serde(with = "atomic")]
    pub max_order_corridor: U256,
    #[serde(with = "atomic")]
    pub reserve_settlement: U256,
    #[serde(with = "atomic")]
    pub reserve_corridor: U256,
    /// Total modeled execution costs, in settlement atomic units per fill.
    #[serde(with = "atomic")]
    pub cost_per_trade: U256,
    pub events: Vec<Event>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// When this information became available (not the source's publication time).
    pub at: u64,
    pub price_at: u64,
    pub price: f64,
    #[serde(default)]
    pub trade: Option<Trade>,
    #[serde(default)]
    pub dealer: Option<Liquidity>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trade {
    /// Maker side: true means vault buys corridor currency.
    pub vault_buys: bool,
    #[serde(with = "atomic")]
    pub corridor_amount: U256,
    /// Counterparty acceptance threshold, settlement per corridor. For a vault
    /// buy this is their minimum; for a vault sale this is their maximum.
    pub limit_price: f64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Liquidity {
    #[serde(with = "atomic")]
    pub max_corridor: U256,
    /// Net executable proceeds per corridor token; fees/slippage included.
    pub net_price: f64,
}
#[derive(Debug, Serialize)]
pub struct Metrics {
    #[serde(with = "atomic")]
    pub ending_nav: U256,
    pub pnl: String,
    pub return_pct: f64,
    pub max_drawdown_bps: u32,
    pub customer_fills: u32,
    pub rebalance_fills: u32,
    #[serde(with = "atomic")]
    pub execution_costs: U256,
    pub max_inventory_bps: u32,
}
#[derive(Debug, Serialize)]
pub struct EquityPoint {
    pub at: u64,
    #[serde(with = "atomic")]
    pub baseline: U256,
    #[serde(with = "atomic")]
    pub candidate: U256,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub version: u32,
    pub config: ModulesConfig,
    pub baseline: Metrics,
    pub candidate: Metrics,
    pub equity: Vec<EquityPoint>,
    pub assumptions: Vec<String>,
    pub events: usize,
    pub dealer_observations: usize,
}
struct Portfolio {
    settlement: U256,
    corridor: U256,
    fills: u32,
    hedges: u32,
    costs: U256,
    peak: U256,
    drawdown: u32,
    max_inventory: u32,
    next_hedge: u64,
}
impl Portfolio {
    fn new(d: &Dataset) -> Self {
        Self {
            settlement: d.initial_settlement,
            corridor: d.initial_corridor,
            fills: 0,
            hedges: 0,
            costs: U256::ZERO,
            peak: U256::ZERO,
            drawdown: 0,
            max_inventory: 0,
            next_hedge: 0,
        }
    }
    fn nav(&self, d: &Dataset, price: f64) -> Result<U256> {
        let c = math::debt_for_collateral(
            price,
            self.corridor,
            d.settlement_decimals,
            d.corridor_decimals,
        );
        ensure!(
            self.corridor.is_zero() || !c.is_zero(),
            "portfolio cannot be valued at this price"
        );
        self.settlement
            .checked_add(c)
            .ok_or_else(|| anyhow::anyhow!("portfolio NAV overflow"))
    }
    fn mark(&mut self, d: &Dataset, price: f64) -> Result<U256> {
        let nav = self.nav(d, price)?;
        self.peak = self.peak.max(nav);
        self.drawdown = self
            .drawdown
            .max(ratio_bps(self.peak - nav, self.peak).unwrap_or(0));
        let value = math::debt_for_collateral(
            price,
            self.corridor,
            d.settlement_decimals,
            d.corridor_decimals,
        );
        self.max_inventory = self.max_inventory.max(ratio_bps(value, nav).unwrap_or(0));
        Ok(nav)
    }
    fn context(&self, d: &Dataset, event: &Event, book: &CorridorBook) -> Context {
        let (base_buy_bps, base_sell_bps) = base_spreads(book, event.price);
        Context {
            now: event.at,
            price: event.price,
            price_at: event.price_at,
            balances_at: event.at,
            staleness_secs: book.staleness_secs,
            settlement: self.settlement,
            corridor: self.corridor,
            available_settlement: self
                .settlement
                .saturating_sub(d.reserve_settlement)
                .min(d.max_order_settlement),
            available_corridor: self
                .corridor
                .saturating_sub(d.reserve_corridor)
                .min(d.max_order_corridor),
            reserved_settlement: U256::ZERO,
            reserved_corridor: U256::ZERO,
            settlement_decimals: d.settlement_decimals,
            corridor_decimals: d.corridor_decimals,
            max_sell: d.max_order_corridor,
            base_buy_bps,
            base_sell_bps,
        }
    }
    fn trade(
        &mut self,
        d: &Dataset,
        event: &Event,
        book: &CorridorBook,
        config: Option<&ModulesConfig>,
        history: &[PricePoint],
    ) -> Result<()> {
        let ctx = self.context(d, event, book);
        let decision = config.map(|c| evaluate(c, &ctx, history));
        let effective = decision
            .as_ref()
            .filter(|_| config.is_some_and(|c| c.inventory.enabled || c.spreads.enabled))
            .map(|v| apply(book, v))
            .unwrap_or_else(|| book.clone());
        if let Some(trade) = &event.trade {
            let spread = if trade.vault_buys {
                effective.buy_spread
            } else {
                effective.sell_spread
            };
            let capacity = if trade.vault_buys {
                effective.buy_capacity_debt
            } else {
                effective.sell_capacity_collateral
            };
            if let (Some(spread), Some(capacity)) = (spread, capacity) {
                let price = if trade.vault_buys {
                    bid_price(event.price, spread)
                } else {
                    ask_price(event.price, spread)
                };
                let accepted = if trade.vault_buys {
                    price >= trade.limit_price
                } else {
                    price <= trade.limit_price
                };
                let amount = math::debt_for_collateral(
                    price,
                    trade.corridor_amount,
                    d.settlement_decimals,
                    d.corridor_decimals,
                );
                let enough = if trade.vault_buys {
                    amount <= ctx.available_settlement
                        && decision.as_ref().is_none_or(|v| amount <= v.buy_limit)
                        && config.is_none_or(|c| {
                            post_buy_allowed(c, &ctx, amount, trade.corridor_amount)
                        })
                } else {
                    trade.corridor_amount <= ctx.available_corridor
                };
                let pool_cap = match capacity {
                    crate::config::RfqCapacity::Wallet => true,
                    crate::config::RfqCapacity::Exact(cap) => {
                        if trade.vault_buys {
                            amount <= cap
                        } else {
                            trade.corridor_amount <= cap
                        }
                    }
                };
                if accepted && enough && pool_cap && !amount.is_zero() {
                    self.fill(d, trade.vault_buys, trade.corridor_amount, amount, false)?;
                }
            }
        }
        // Re-evaluate after the customer trade: never spend a stale portfolio.
        if let (Some(cfg), Some(liquidity)) = (config, &event.dealer) {
            let ctx = self.context(d, event, book);
            let decision = evaluate(cfg, &ctx, history);
            if cfg.rebalance.enabled && event.at >= self.next_hedge && !decision.blocked {
                let amount = decision.rebalance_sell.min(liquidity.max_corridor);
                let fair = math::debt_for_collateral(
                    event.price,
                    amount,
                    d.settlement_decimals,
                    d.corridor_decimals,
                );
                let proceeds = math::debt_for_collateral(
                    liquidity.net_price,
                    amount,
                    d.settlement_decimals,
                    d.corridor_decimals,
                );
                if !amount.is_zero()
                    && !proceeds.is_zero()
                    && proceeds
                        >= fair.saturating_sub(fraction(fair, cfg.rebalance.max_slippage_bps))
                {
                    if self.fill(d, false, amount, proceeds, true)? {
                        self.next_hedge = event.at.saturating_add(cfg.rebalance.cooldown_secs);
                    }
                }
            }
        }
        Ok(())
    }
    fn fill(
        &mut self,
        d: &Dataset,
        buys: bool,
        amount: U256,
        proceeds: U256,
        hedge: bool,
    ) -> Result<bool> {
        let (s, c) = if buys {
            (
                self.settlement.checked_sub(proceeds),
                self.corridor.checked_add(amount),
            )
        } else {
            (
                self.settlement.checked_add(proceeds),
                self.corridor.checked_sub(amount),
            )
        };
        let (Some(s), Some(c)) = (s, c) else {
            anyhow::bail!("trade amount overflow");
        };
        let Some(s) = s.checked_sub(d.cost_per_trade) else {
            return Ok(false);
        };
        if s < d.reserve_settlement || c < d.reserve_corridor {
            return Ok(false);
        }
        self.settlement = s;
        self.corridor = c;
        self.costs = self
            .costs
            .checked_add(d.cost_per_trade)
            .ok_or_else(|| anyhow::anyhow!("cost overflow"))?;
        if hedge {
            self.hedges += 1;
        } else {
            self.fills += 1;
        }
        Ok(true)
    }
    fn metrics(&self, d: &Dataset, start: U256, price: f64) -> Result<Metrics> {
        let end = self.nav(d, price)?;
        let (negative, delta) = if end >= start {
            (false, end - start)
        } else {
            (true, start - end)
        };
        let pct = delta.to_string().parse::<f64>()? / start.to_string().parse::<f64>()? * 100.0;
        Ok(Metrics {
            ending_nav: end,
            pnl: format!("{}{delta}", if negative { "-" } else { "" }),
            return_pct: if negative { -pct } else { pct },
            max_drawdown_bps: self.drawdown,
            customer_fills: self.fills,
            rebalance_fills: self.hedges,
            execution_costs: self.costs,
            max_inventory_bps: self.max_inventory,
        })
    }
}

pub fn run(d: &Dataset, config: &ModulesConfig, book: &CorridorBook) -> Result<Report> {
    config.validate()?;
    ensure!(
        d.version == VERSION && (2..=10_000).contains(&d.events.len()),
        "replay requires version 1 and 2..10000 events"
    );
    ensure!(
        d.corridor_decimals == book.collateral_decimals
            && d.settlement_decimals == book.debt_decimals
            && d.corridor_decimals <= 18
            && d.settlement_decimals <= 18,
        "replay token precision does not match the bot"
    );
    ensure!(
        d.corridor_token.parse::<alloy_primitives::Address>()? == book.collateral
            && d.settlement_token.parse::<alloy_primitives::Address>()? == book.debt,
        "replay token pair does not match the bot"
    );
    ensure!(
        !d.max_order_corridor.is_zero() && !d.max_order_settlement.is_zero(),
        "replay requires nonzero vault order caps"
    );
    let mut prior = 0;
    for e in &d.events {
        ensure!(
            e.at >= prior
                && e.price_at <= e.at
                && e.at - e.price_at <= book.staleness_secs
                && e.price.is_finite()
                && e.price > 0.0,
            "events must be ordered and prices fresh, finite, positive and known at event time"
        );
        if let Some(t) = &e.trade {
            ensure!(
                t.limit_price.is_finite() && t.limit_price > 0.0 && !t.corridor_amount.is_zero(),
                "invalid historical trade"
            );
        }
        if let Some(l) = &e.dealer {
            ensure!(
                l.net_price.is_finite() && l.net_price > 0.0,
                "invalid historical dealer price"
            );
        }
        prior = e.at;
    }
    let mut baseline = Portfolio::new(d);
    let mut candidate = Portfolio::new(d);
    let start = baseline.mark(d, d.events[0].price)?;
    candidate.mark(d, d.events[0].price)?;
    ensure!(!start.is_zero(), "initial NAV must be positive");
    let mut history: Vec<PricePoint> = vec![];
    let mut equity = vec![];
    for event in &d.events {
        if history.last().is_none_or(|p| event.price_at > p.timestamp) {
            history.push(PricePoint {
                timestamp: event.price_at,
                price: event.price,
            });
        }
        history.retain(|p| event.at - p.timestamp <= config.spreads.window_secs);
        if history.len() > 3601 {
            history.drain(..history.len() - 3601);
        }
        baseline.trade(d, event, book, None, &history)?;
        candidate.trade(d, event, book, Some(config), &history)?;
        equity.push(EquityPoint {
            at: event.at,
            baseline: baseline.mark(d, event.price)?,
            candidate: candidate.mark(d, event.price)?,
        });
    }
    let last = d.events.last().expect("validated nonempty").price;
    Ok(Report { version: VERSION, config: config.clone(), baseline: baseline.metrics(d, start, last)?, candidate: candidate.metrics(d, start, last)?, equity,
        events: d.events.len(), dealer_observations: d.events.iter().filter(|e| e.dealer.is_some()).count(),
        assumptions: vec![
            "Conditional simulation, not realized performance or a prediction. Customer activity is held fixed; changed prices may change real demand.".into(),
            "Customer fills require the imported acceptance price and sufficient portfolio funds. Historical fills alone do not establish those acceptance prices.".into(),
            "Spot sales require imported executable dealer prices and depth; none are invented when those observations are absent.".into(),
            "Immediate settlement, no concurrent reservations, no latency or failed transactions. Enter net dealer prices and execution costs. This can overstate achievable returns.".into(),
            "Independent portfolios start with the same holdings. No deposits, withdrawals, yield accrual or management/performance fees are modeled. NAV uses the same historical mark for both.".into(),
        ] })
}
