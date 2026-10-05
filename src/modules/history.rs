// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Read-only collection and conservative conversion to the shared replay engine.
use super::{
    atomic,
    replay::{Dataset, Event, Liquidity, Trade},
    U256,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct History {
    pub version: u32,
    pub chain_id: u64,
    pub vault: String,
    pub from: u64,
    pub to: u64,
    pub collected_at: u64,
    pub price_source: String,
    pub indexed_through: u64,
    pub snapshot: Snapshot,
    pub prices: Vec<Price>,
    pub trades: Vec<Fill>,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub block_number: String,
    pub block_hash: String,
    pub at: u64,
    pub settlement_token: String,
    pub corridor_token: String,
    pub settlement_decimals: u8,
    pub corridor_decimals: u8,
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
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Price {
    pub at: u64,
    pub price_at: u64,
    pub price: f64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub sold_token: String,
    #[serde(with = "atomic")]
    pub sold_amount: U256,
    pub bought_token: String,
    #[serde(with = "atomic")]
    pub bought_amount: U256,
    pub timestamp: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub from: u64,
    pub to: u64,
    #[serde(with = "atomic")]
    pub cost_per_trade: U256,
    /// None by default. Synthetic liquidity is never described as observed.
    pub dealer_scenario: Option<DealerScenario>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DealerScenario {
    pub slippage_bps: u32,
    #[serde(with = "atomic")]
    pub max_corridor_per_observation: U256,
}
impl Options {
    pub fn validate(&self, now: u64) -> Result<()> {
        ensure!(
            self.from > 0
                && self.to >= self.from.saturating_add(300)
                && self.to - self.from <= 30 * 86400
                && self.to <= now.saturating_sub(300),
            "Choose 5 minutes to 30 days, ending at least 5 minutes ago"
        );
        if let Some(d) = &self.dealer_scenario {
            ensure!(
                d.slippage_bps < 1000 && !d.max_corridor_per_observation.is_zero(),
                "Dealer scenario needs slippage below 1000 bps and positive depth"
            );
        }
        Ok(())
    }
}

pub async fn collect(cfg: &crate::config::Config, options: &Options) -> Result<History> {
    let vault = &cfg
        .vault
        .as_ref()
        .context("Historical collection requires an OperatorVault")?
        .address;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()?;
    let mut response = client.post(crate::venue::indexer::graphql_url_from_base(&cfg.indexer_url))
        .json(&serde_json::json!({
            "query": "query StitchHistory($chainId: Int!, $address: String!, $from: Int!, $to: Int!) { stitchSimulationHistory(chainId: $chainId, address: $address, from: $from, to: $to) }",
            "variables": { "chainId": cfg.chain_id, "address": vault, "from": options.from, "to": options.to }
        })).send().await.context("Cannot reach Textile's historical data service")?
        .error_for_status().context("Textile historical data service rejected the request")?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 8_000_000,
            "Historical response is too large; choose a shorter period"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(errors) = value.get("errors").and_then(|e| e.as_array()) {
        anyhow::bail!(
            "{}",
            errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    let history: History = serde_json::from_value(value["data"]["stitchSimulationHistory"].clone())
        .context("Textile returned an unsupported historical response; update the API and panel together")?;
    ensure!(
        history.version == 1
            && history.chain_id == cfg.chain_id
            && history.vault.eq_ignore_ascii_case(vault)
            && history.from == options.from
            && history.to == options.to,
        "Historical response does not match this vault and period"
    );
    Ok(history)
}

#[derive(Debug, Serialize)]
pub struct Coverage {
    pub observed_trades: usize,
    pub replayed_trades: usize,
    pub skipped_stale_trades: usize,
    pub price_observations: usize,
    pub fresh_seconds: u64,
    pub total_seconds: u64,
    pub max_price_gap_secs: u64,
}

/// Capture the baseline separately from the candidate modules for an auditable
/// export. Feed credentials and the rest of stitch.toml never leave the panel.
pub fn baseline_settings(book: &crate::rfq::responder::CorridorBook) -> serde_json::Value {
    use crate::{config::RfqCapacity, pricing::quote::Spread};
    let spread = |v: Option<Spread>| match v {
        Some(Spread::Bps(bps)) => serde_json::json!({ "bps": bps }),
        Some(Spread::Abs(value)) => serde_json::json!({ "corridor_per_settlement": value }),
        None => serde_json::Value::Null,
    };
    let capacity = |v: Option<RfqCapacity>| match v {
        Some(RfqCapacity::Wallet) => serde_json::json!("available_balance"),
        Some(RfqCapacity::Exact(value)) => serde_json::json!(value.to_string()),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "buy_spread": spread(book.buy_spread), "sell_spread": spread(book.sell_spread),
        "buy_capacity_settlement": capacity(book.buy_capacity_debt),
        "sell_capacity_corridor": capacity(book.sell_capacity_collateral),
    })
}

pub fn dataset(h: &History, o: &Options, staleness: u64) -> Result<(Dataset, Coverage)> {
    ensure!(
        h.from == o.from && h.to == o.to && h.to > h.from,
        "Historical period mismatch"
    );
    ensure!(
        h.snapshot.at < h.from && h.indexed_through >= h.to,
        "History does not cover the requested period"
    );
    ensure!(
        h.prices.len() <= 10000 && h.trades.len() <= 5000,
        "Historical data exceeds the collection limit"
    );
    let s = &h.snapshot;
    ensure!(
        s.corridor_decimals <= 18 && s.settlement_decimals <= 18,
        "Unsupported historical token precision"
    );
    let mut d = Dataset {
        version: 1,
        chain_id: h.chain_id,
        corridor_token: s.corridor_token.clone(),
        settlement_token: s.settlement_token.clone(),
        corridor_decimals: s.corridor_decimals,
        settlement_decimals: s.settlement_decimals,
        initial_settlement: s.initial_settlement,
        initial_corridor: s.initial_corridor,
        max_order_settlement: s.max_order_settlement,
        max_order_corridor: s.max_order_corridor,
        reserve_settlement: s.reserve_settlement,
        reserve_corridor: s.reserve_corridor,
        cost_per_trade: o.cost_per_trade,
        events: vec![],
    };
    let mut timeline = h
        .prices
        .iter()
        .enumerate()
        .map(|(i, p)| (p.at, 0, i))
        .collect::<Vec<_>>();
    for (i, t) in h.trades.iter().enumerate() {
        let at = t.timestamp.parse::<u64>()?;
        ensure!(
            (h.from..=h.to).contains(&at),
            "Trade outside requested period"
        );
        timeline.push((at, 2, i));
    }
    // Prices available at a second precede fills; same-second fills retain the
    // source's order. The source cannot reconstruct intra-block quote timing.
    timeline.push((h.from, 1, 0));
    timeline.push((h.to, 1, 0));
    timeline.sort_by_key(|&(at, kind, _)| (at, kind));
    let mut current: Option<&Price> = None;
    let mut coverage = Coverage {
        observed_trades: h.trades.len(),
        replayed_trades: 0,
        skipped_stale_trades: 0,
        price_observations: 0,
        fresh_seconds: 0,
        total_seconds: h.to - h.from,
        max_price_gap_secs: 0,
    };
    let mut fresh_until = h.from;
    let mut last_price = h.from;
    for (at, kind, index) in timeline {
        ensure!(at <= h.to, "Price outside requested period");
        if kind == 0 {
            let p = &h.prices[index];
            ensure!(
                p.price_at <= p.at && p.price.is_finite() && p.price > 0.0,
                "Invalid historical price"
            );
            // Late ingestion of an older observation cannot rewind the feed.
            if current.is_some_and(|c| c.price_at > p.price_at) {
                continue;
            }
            current = Some(p);
        }
        if at < h.from {
            continue;
        }
        let fresh = current.filter(|p| at - p.price_at <= staleness);
        if kind == 1 {
            ensure!(fresh.is_some(), "No fresh price at a period boundary. Choose a different period; missing prices are not forward-filled");
        }
        let Some(p) = fresh else {
            if kind == 2 {
                coverage.skipped_stale_trades += 1;
            }
            continue;
        };
        let end = p.price_at.saturating_add(staleness).min(h.to);
        coverage.fresh_seconds += end.saturating_sub(fresh_until.max(at));
        fresh_until = fresh_until.max(end);
        if kind == 0 {
            coverage.price_observations += 1;
            coverage.max_price_gap_secs = coverage.max_price_gap_secs.max(at - last_price);
            last_price = at;
        }
        let trade = if kind == 2 {
            let t = &h.trades[index];
            let buys = t.bought_token.eq_ignore_ascii_case(&s.corridor_token)
                && t.sold_token.eq_ignore_ascii_case(&s.settlement_token);
            let sells = t.sold_token.eq_ignore_ascii_case(&s.corridor_token)
                && t.bought_token.eq_ignore_ascii_case(&s.settlement_token);
            ensure!(
                buys || sells,
                "Historical swap contains a different token pair"
            );
            let (corridor, settlement) = if buys {
                (t.bought_amount, t.sold_amount)
            } else {
                (t.sold_amount, t.bought_amount)
            };
            ensure!(
                !corridor.is_zero() && !settlement.is_zero(),
                "Historical swap has zero amounts"
            );
            let limit = (settlement.to_string().parse::<f64>()?
                / 10f64.powi(s.settlement_decimals.into()))
                / (corridor.to_string().parse::<f64>()? / 10f64.powi(s.corridor_decimals.into()));
            coverage.replayed_trades += 1;
            Some(Trade {
                vault_buys: buys,
                corridor_amount: corridor,
                limit_price: limit,
            })
        } else {
            None
        };
        // Hypothetical depth replenishes only on new price observations, never
        // once per customer fill. It cannot multiply on same-second records.
        let dealer = if kind == 0 && d.events.last().is_none_or(|e| e.at != at) {
            o.dealer_scenario.as_ref().map(|v| Liquidity {
                max_corridor: v.max_corridor_per_observation,
                net_price: p.price * (1.0 - f64::from(v.slippage_bps) / 10000.0),
            })
        } else {
            None
        };
        d.events.push(Event {
            at,
            price_at: p.price_at,
            price: p.price,
            trade,
            dealer,
        });
    }
    coverage.max_price_gap_secs = coverage.max_price_gap_secs.max(h.to - last_price);
    ensure!(
        d.events.len() <= 10000,
        "Replay exceeds 10000 events. Choose a shorter period"
    );
    Ok((d, coverage))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (History, Options) {
        let history = serde_json::from_value(serde_json::json!({
            "version": 1, "chain_id": 56, "vault": "0xvault", "from": 1000, "to": 1600,
            "collected_at": 2000, "indexed_through": 1700, "price_source": "test",
            "snapshot": {
                "block_number": "12", "block_hash": "0xabc", "at": 999,
                "settlement_token": "0xstable", "corridor_token": "0xsoft",
                "settlement_decimals": 18, "corridor_decimals": 6,
                "initial_settlement": "1000000000000000000000", "initial_corridor": "1000000000",
                "max_order_settlement": "1000000000000000000000", "max_order_corridor": "1000000000",
                "reserve_settlement": "0", "reserve_corridor": "0"
            },
            "prices": [ { "at": 1000, "price_at": 1000, "price": 0.001 }, { "at": 1600, "price_at": 1600, "price": 0.0009 } ],
            "trades": []
        })).unwrap();
        (
            history,
            Options {
                from: 1000,
                to: 1600,
                cost_per_trade: U256::ZERO,
                dealer_scenario: None,
            },
        )
    }
    fn fill(at: u64, buys: bool) -> Fill {
        let (sold_token, sold_amount, bought_token, bought_amount) = if buys {
            (
                "0xstable",
                U256::from(1_000_000_000_000_000u64),
                "0xsoft",
                U256::from(1_000_000),
            )
        } else {
            (
                "0xsoft",
                U256::from(1_000_000),
                "0xstable",
                U256::from(1_000_000_000_000_000u64),
            )
        };
        Fill {
            sold_token: sold_token.into(),
            sold_amount,
            bought_token: bought_token.into(),
            bought_amount,
            timestamp: at.to_string(),
        }
    }
    #[test]
    fn trade_prices_use_maker_direction_and_both_token_precisions() {
        let (mut h, o) = fixture();
        h.trades = vec![fill(1001, true), fill(1002, false)];
        let (d, c) = dataset(&h, &o, 60).unwrap();
        let trades = d
            .events
            .iter()
            .filter_map(|e| e.trade.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(trades.len(), 2);
        assert!(trades[0].vault_buys);
        assert!(!trades[1].vault_buys);
        assert_eq!(trades[0].corridor_amount, U256::from(1_000_000));
        assert!((trades[0].limit_price - 0.001).abs() < 1e-12);
        assert_eq!(c.replayed_trades, 2);
        assert!(d.events.iter().all(|e| e.dealer.is_none()));
    }
    #[test]
    fn gaps_skip_trades_without_borrowing_a_future_price() {
        let (mut h, o) = fixture();
        h.trades = vec![fill(1300, true)];
        let (d, c) = dataset(&h, &o, 60).unwrap();
        assert_eq!(c.skipped_stale_trades, 1);
        assert_eq!(c.replayed_trades, 0);
        assert_eq!(c.fresh_seconds, 60);
        assert_eq!(c.max_price_gap_secs, 600);
        assert!(d.events.iter().all(|e| e.trade.is_none()));
    }
    #[test]
    fn late_ingestion_never_rewinds_the_feed() {
        let (mut h, o) = fixture();
        h.prices.push(Price {
            at: 1100,
            price_at: 900,
            price: 0.5,
        });
        h.trades = vec![fill(1101, true)];
        let (d, _) = dataset(&h, &o, 300).unwrap();
        let event = d.events.iter().find(|e| e.trade.is_some()).unwrap();
        assert_eq!(event.price, 0.001);
        assert_eq!(event.price_at, 1000);
    }
    #[test]
    fn incomplete_boundaries_and_unindexed_periods_fail() {
        let (mut h, o) = fixture();
        h.prices[0].at = 1001;
        assert!(dataset(&h, &o, 60)
            .unwrap_err()
            .to_string()
            .contains("boundary"));
        h.prices[0].at = 1000;
        h.prices.pop();
        assert!(dataset(&h, &o, 60)
            .unwrap_err()
            .to_string()
            .contains("boundary"));
        h.indexed_through = 1500;
        assert!(dataset(&h, &o, 60)
            .unwrap_err()
            .to_string()
            .contains("cover"));
    }
    #[test]
    fn dealer_scenario_does_not_replenish_for_each_same_second_trade_or_price() {
        let (mut h, mut o) = fixture();
        h.trades = vec![fill(1000, true), fill(1000, true)];
        h.prices.push(h.prices[0].clone());
        o.dealer_scenario = Some(DealerScenario {
            slippage_bps: 50,
            max_corridor_per_observation: U256::from(100),
        });
        let (d, _) = dataset(&h, &o, 60).unwrap();
        assert_eq!(d.events.iter().filter(|e| e.dealer.is_some()).count(), 2);
        let liquidity = d.events[0].dealer.as_ref().unwrap();
        assert_eq!(liquidity.max_corridor, U256::from(100));
        assert!((liquidity.net_price - 0.000995).abs() < 1e-12);
    }
    #[test]
    fn period_and_liquidity_inputs_are_bounded() {
        let (_, mut o) = fixture();
        o.validate(2000).unwrap();
        assert!(o.validate(1800).is_err());
        o.dealer_scenario = Some(DealerScenario {
            slippage_bps: 1000,
            max_corridor_per_observation: U256::from(100),
        });
        assert!(o.validate(2000).is_err());
        o.dealer_scenario = Some(DealerScenario {
            slippage_bps: 0,
            max_corridor_per_observation: U256::ZERO,
        });
        assert!(o.validate(2000).is_err());
    }
}
