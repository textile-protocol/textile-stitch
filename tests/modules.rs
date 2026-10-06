// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
use alloy_primitives::{address, U256};
use stitch_bot::{
    config::RfqCapacity,
    modules::{config::DealerConfig, dealer, replay::*, *},
    pricing::quote::Spread,
    rfq::responder::CorridorBook,
};

fn u(v: u64) -> U256 {
    U256::from(v)
}
fn config() -> ModulesConfig {
    let mut c = ModulesConfig::default();
    c.spreads.enabled = false;
    c
}
fn context() -> Context {
    Context {
        now: 100,
        price: 1.0,
        price_at: 100,
        balances_at: 100,
        staleness_secs: 10,
        settlement: u(40_000_000),
        corridor: u(60_000_000),
        available_settlement: u(40_000_000),
        available_corridor: u(60_000_000),
        reserved_settlement: U256::ZERO,
        reserved_corridor: U256::ZERO,
        settlement_decimals: 6,
        corridor_decimals: 6,
        max_sell: u(10_000_000),
        base_buy_bps: Some(20),
        base_sell_bps: Some(20),
    }
}
fn book() -> CorridorBook {
    CorridorBook {
        slug: "test".into(),
        collateral: address!("0000000000000000000000000000000000000001"),
        debt: address!("0000000000000000000000000000000000000002"),
        collateral_decimals: 6,
        debt_decimals: 6,
        buy_spread: Some(Spread::Bps(20)),
        sell_spread: Some(Spread::Bps(20)),
        buy_capacity_debt: Some(RfqCapacity::Wallet),
        sell_capacity_collateral: Some(RfqCapacity::Wallet),
        feed_url: "http://localhost/price".into(),
        staleness_secs: 10,
    }
}
#[test]
fn inventory_disables_accumulation_at_cap_and_cheapens_unloading() {
    let d = evaluate(&config(), &context(), &[]);
    assert_eq!(d.inventory_bps, Some(6000));
    assert_eq!(d.buy_bps, None);
    assert_eq!(d.sell_bps, Some(5));
    let mut ctx = context();
    ctx.corridor = u(40_000_000);
    ctx.settlement = u(60_000_000);
    let d = evaluate(&config(), &ctx, &[]);
    assert!(d.buy_bps.unwrap() > 20);
    assert!(d.sell_bps.unwrap() < 20);
}
#[test]
fn reservations_do_not_disappear_from_exposure_and_block_new_risk() {
    let mut ctx = context();
    ctx.corridor = u(40_000_000);
    ctx.settlement = u(60_000_000);
    ctx.reserved_settlement = u(1);
    let mut cfg = config();
    cfg.rebalance.enabled = true;
    let d = evaluate(&cfg, &ctx, &[]);
    assert_eq!(d.inventory_bps, Some(4000));
    assert!(d.buy_bps.is_none());
    ctx.corridor = u(80_000_000);
    ctx.reserved_corridor = u(10_000_000);
    let d = evaluate(&cfg, &ctx, &[]);
    assert!(d.rebalance_sell.is_zero());
    assert!(d.inventory_bps.unwrap() > 5000);
}
#[test]
fn stale_future_and_missing_value_fail_closed() {
    for ctx in [
        Context {
            price_at: 89,
            ..context()
        },
        Context {
            price_at: 101,
            ..context()
        },
        Context {
            balances_at: 96,
            ..context()
        },
        Context {
            price: f64::NAN,
            ..context()
        },
        Context {
            settlement: U256::ZERO,
            corridor: U256::ZERO,
            ..context()
        },
    ] {
        let d = evaluate(&config(), &ctx, &[]);
        assert!(d.blocked);
        assert!(d.buy_bps.is_none());
        assert!(d.sell_bps.is_none());
        assert!(d.rebalance_sell.is_zero());
    }
}
#[test]
fn warmup_then_bounded_volatility_never_uses_a_future_point() {
    let mut cfg = config();
    cfg.spreads.enabled = true;
    assert!(evaluate(&cfg, &context(), &[]).blocked);
    let history = [
        PricePoint {
            timestamp: 50,
            price: 1.0,
        },
        PricePoint {
            timestamp: 100,
            price: 0.99,
        },
    ];
    let d = evaluate(&cfg, &context(), &history);
    assert!(!d.blocked);
    assert_eq!(d.volatility_bps, 100);
    let extended = [
        history[0],
        history[1],
        PricePoint {
            timestamp: 101,
            price: 100.0,
        },
    ];
    assert_eq!(d, evaluate(&cfg, &context(), &extended));
}

fn inventory_aware_config() -> ModulesConfig {
    let mut cfg = ModulesConfig::default();
    cfg.inventory.target_bps = 4000;
    cfg.inventory.max_bps = 8000;
    cfg.inventory.max_skew_bps = 40;
    cfg.spreads.inventory_aware = true;
    cfg.spreads.multiplier = 0.008;
    cfg
}
fn spread_history() -> [PricePoint; 2] {
    // An exact binary range avoids floating-point rounding at an integer bps
    // boundary: 1250 bps * .008 = a 10 bps common volatility buffer.
    [
        PricePoint {
            timestamp: 50,
            price: 1.125,
        },
        PricePoint {
            timestamp: 100,
            price: 1.0,
        },
    ]
}
fn inventory_context(share: u32) -> Context {
    Context {
        corridor: u(u64::from(share) * 10_000),
        settlement: u(u64::from(10_000 - share) * 10_000),
        ..context()
    }
}

#[test]
fn volatility_weighting_tracks_both_sides_of_target_and_preserves_the_inventory_ceiling() {
    let cfg = inventory_aware_config();
    for (share, buy, sell) in [
        (0, Some(5), Some(70)),
        (2000, Some(10), Some(50)),
        (4000, Some(30), Some(30)),
        (6000, Some(50), Some(10)),
        (8000, None, Some(5)),
        (9945, None, Some(5)),
        (10000, None, Some(5)),
    ] {
        let d = evaluate(&cfg, &inventory_context(share), &spread_history());
        assert!(!d.blocked);
        assert_eq!(d.inventory_bps, Some(share));
        assert_eq!(d.volatility_bps, 10);
        assert_eq!((d.buy_bps, d.sell_bps), (buy, sell), "share {share}");
        if share >= cfg.inventory.max_bps {
            assert!(d.buy_limit.is_zero());
        }
    }
}

#[test]
fn weighting_is_opt_in_and_requires_inventory_balancing() {
    let old: ModulesConfig = toml::from_str("[spreads]\nenabled = true").unwrap();
    assert!(!old.spreads.inventory_aware);
    let mut cfg = inventory_aware_config();
    cfg.spreads.inventory_aware = false;
    let ctx = inventory_context(6000);
    let symmetric = evaluate(&cfg, &ctx, &spread_history());
    assert_eq!(
        (symmetric.buy_bps, symmetric.sell_bps),
        (Some(50), Some(15))
    );
    cfg.spreads.inventory_aware = true;
    cfg.inventory.enabled = false;
    let no_inventory = evaluate(&cfg, &ctx, &spread_history());
    assert_eq!(
        (no_inventory.buy_bps, no_inventory.sell_bps),
        (Some(30), Some(30))
    );
    cfg.inventory.enabled = true;
    cfg.spreads.enabled = false;
    let no_spreads = evaluate(&cfg, &ctx, &[]);
    assert_eq!(
        (no_spreads.buy_bps, no_spreads.sell_bps),
        (Some(40), Some(5))
    );
    let restored: ModulesConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(cfg, restored);
}

#[test]
fn weighted_buffers_stay_within_caps_and_keep_paused_sides_closed() {
    let mut cfg = inventory_aware_config();
    cfg.spreads.max_extra_bps = 6;
    let mut ctx = inventory_context(6000);
    let d = evaluate(&cfg, &ctx, &spread_history());
    assert_eq!((d.buy_bps, d.sell_bps), (Some(46), Some(8)));
    ctx.reserved_settlement = u(1);
    assert!(evaluate(&cfg, &ctx, &spread_history()).buy_bps.is_none());
    ctx.reserved_settlement = U256::ZERO;
    ctx.base_buy_bps = None;
    ctx.base_sell_bps = None;
    let d = evaluate(&cfg, &ctx, &spread_history());
    assert_eq!((d.buy_bps, d.sell_bps), (None, None));
    assert!(apply(&book(), &d).buy_capacity_debt.is_none());
    cfg.spreads.max_extra_bps = 0;
    let d = evaluate(&cfg, &inventory_context(6000), &spread_history());
    assert_eq!((d.buy_bps, d.sell_bps), (Some(40), Some(5)));
}

#[test]
fn higher_volatility_increases_the_inventory_bias_without_breaching_floors_or_caps() {
    let mut cfg = inventory_aware_config();
    let ctx = inventory_context(6000);
    let mut prior_bias = 0;
    for multiplier in [0.0, 0.004, 0.008, 0.016, 0.032, 1.0, 10.0] {
        cfg.spreads.multiplier = multiplier;
        let d = evaluate(&cfg, &ctx, &spread_history());
        let buy_extra = d.buy_bps.unwrap() - 40;
        let sell_extra = d.sell_bps.unwrap() - 5;
        assert!(buy_extra <= cfg.spreads.max_extra_bps);
        assert!(sell_extra <= cfg.spreads.max_extra_bps);
        let bias = buy_extra - sell_extra;
        assert!(bias >= prior_bias);
        prior_bias = bias;
        assert!(d.sell_bps.unwrap() >= cfg.inventory.spread_floor_bps);
    }
    assert!(evaluate(&cfg, &ctx, &[]).blocked);
    assert!(
        evaluate(
            &cfg,
            &Context {
                price_at: 89,
                ..ctx
            },
            &spread_history()
        )
        .blocked
    );
    let mut future = spread_history().to_vec();
    future.push(PricePoint {
        timestamp: 101,
        price: 100.0,
    });
    assert_eq!(
        evaluate(&cfg, &ctx, &future),
        evaluate(&cfg, &ctx, &spread_history())
    );
}
#[test]
fn rebalance_limits_trade_to_nav_fraction_and_quotable_inventory() {
    let mut cfg = config();
    cfg.rebalance.enabled = true;
    assert_eq!(evaluate(&cfg, &context(), &[]).rebalance_sell, u(2_000_000));
    let ctx = Context {
        available_corridor: u(100),
        ..context()
    };
    assert_eq!(evaluate(&cfg, &ctx, &[]).rebalance_sell, u(100));
    let ctx = Context {
        max_sell: u(50),
        ..ctx
    };
    assert_eq!(evaluate(&cfg, &ctx, &[]).rebalance_sell, u(50));
}
#[test]
fn exact_post_trade_check_rejects_even_sub_basis_point_limit_breaches() {
    let cfg = config();
    assert!(!post_buy_allowed(&cfg, &context(), u(1), u(1)));
    assert!(post_buy_allowed(&cfg, &context(), U256::ZERO, U256::ZERO));
    let ctx = Context {
        settlement: u(70_000_000),
        corridor: u(30_000_000),
        ..context()
    };
    assert!(post_buy_allowed(&cfg, &ctx, u(1_000_000), u(1_000_000)));
}
#[test]
fn offsets_are_exact_and_operator_disabled_sides_stay_disabled() {
    let mut b = book();
    assert_eq!(base_spreads(&b, 1.0), (Some(20), Some(20)));
    b.buy_spread = None;
    b.buy_capacity_debt = None;
    let ctx = Context {
        base_buy_bps: None,
        ..context()
    };
    let d = evaluate(&config(), &ctx, &[]);
    assert!(apply(&b, &d).buy_capacity_debt.is_none());
}
#[test]
fn strict_configs_reject_bad_limits_and_unknown_keys() {
    let mut c = config();
    c.inventory.target_bps = c.inventory.max_bps;
    assert!(c.validate().is_err());
    let mut c = config();
    c.rebalance.cooldown_secs = 1;
    assert!(c.validate().is_err());
    assert!(toml::from_str::<ModulesConfig>("modde = 'live'").is_err());
    let mut c = config();
    c.spreads.multiplier = f64::INFINITY;
    assert!(c.validate().is_err());
}
fn dealer_terms() -> (DealerConfig, dealer::QuoteRequest, dealer::Quote) {
    let cfg = DealerConfig {
        url: "http://localhost:3333".into(),
        taker: "0x0000000000000000000000000000000000000003".into(),
        api_key_env: None,
    };
    let request = dealer::QuoteRequest {
        request_id: "id".into(),
        chain_id: 1,
        vault: "vault".into(),
        sell_token: "sell".into(),
        buy_token: "buy".into(),
        sell_amount: u(100),
        min_buy_amount: u(99),
        deadline: 150,
    };
    let quote = dealer::Quote {
        request_id: "id".into(),
        taker: cfg.taker.clone(),
        sell_amount: u(100),
        buy_amount: u(99),
        expires_at: 145,
    };
    (cfg, request, quote)
}
#[test]
fn dealer_cannot_change_counterparty_size_price_or_deadline() {
    let (cfg, req, q) = dealer_terms();
    assert!(dealer::validate_quote(&cfg, &req, &q, 100).is_ok());
    for changed in [
        dealer::Quote {
            buy_amount: u(98),
            ..q.clone()
        },
        dealer::Quote {
            sell_amount: u(101),
            ..q.clone()
        },
        dealer::Quote {
            expires_at: 151,
            ..q.clone()
        },
        dealer::Quote {
            expires_at: 105,
            ..q.clone()
        },
        dealer::Quote {
            request_id: "another".into(),
            ..q.clone()
        },
        dealer::Quote {
            taker: "0x0000000000000000000000000000000000000004".into(),
            ..q
        },
    ] {
        assert!(dealer::validate_quote(&cfg, &req, &changed, 100).is_err());
    }
}
fn data() -> Dataset {
    let b = book();
    Dataset {
        version: 1,
        chain_id: 1,
        corridor_token: b.collateral.to_string(),
        settlement_token: b.debt.to_string(),
        corridor_decimals: 6,
        settlement_decimals: 6,
        initial_settlement: u(40_000_000),
        initial_corridor: u(60_000_000),
        max_order_settlement: u(100_000_000),
        max_order_corridor: u(100_000_000),
        reserve_settlement: U256::ZERO,
        reserve_corridor: U256::ZERO,
        cost_per_trade: U256::ZERO,
        events: vec![
            Event {
                at: 100,
                price_at: 100,
                price: 1.0,
                trade: None,
                dealer: Some(Liquidity {
                    max_corridor: u(100_000_000),
                    net_price: 1.0,
                }),
            },
            Event {
                at: 500,
                price_at: 500,
                price: 0.5,
                trade: None,
                dealer: None,
            },
        ],
    }
}
#[test]
fn replay_protects_against_a_drop_only_when_executable_liquidity_exists() {
    let mut cfg = config();
    cfg.rebalance.enabled = true;
    let d = data();
    let report = run(&d, &cfg, &book()).unwrap();
    assert_eq!(report.baseline.ending_nav, u(70_000_000));
    assert_eq!(report.candidate.ending_nav, u(71_000_000));
    assert_eq!(report.candidate.rebalance_fills, 1);
    let mut d = d;
    d.events[0].dealer = None;
    let report = run(&d, &cfg, &book()).unwrap();
    assert_eq!(report.candidate.ending_nav, report.baseline.ending_nav);
    assert_eq!(report.candidate.rebalance_fills, 0);
}
#[test]
fn replay_costs_can_erase_the_benefit_and_portfolios_do_not_share_balances() {
    let mut cfg = config();
    cfg.rebalance.enabled = true;
    let mut d = data();
    d.cost_per_trade = u(1_500_000);
    let report = run(&d, &cfg, &book()).unwrap();
    assert_eq!(report.baseline.ending_nav, u(70_000_000));
    assert!(report.candidate.ending_nav < report.baseline.ending_nav);
    assert_eq!(report.candidate.execution_costs, u(1_500_000));
}
#[test]
fn customer_acceptance_and_operator_side_switches_control_simulated_fills() {
    let mut d = data();
    d.events[0].dealer = None;
    d.events[0].trade = Some(Trade {
        vault_buys: true,
        corridor_amount: u(1_000_000),
        limit_price: 0.998,
    });
    let report = run(&d, &config(), &book()).unwrap();
    assert_eq!(report.baseline.customer_fills, 1);
    assert_eq!(report.candidate.customer_fills, 0);
    let mut b = book();
    b.buy_capacity_debt = None;
    let report = run(&d, &config(), &b).unwrap();
    assert_eq!(report.baseline.customer_fills, 0);
}

#[test]
fn replay_uses_inventory_weighting_to_price_a_sale_and_charges_its_execution_cost() {
    let mut cfg = inventory_aware_config();
    let mut d = data();
    d.cost_per_trade = u(100);
    d.events = spread_history()
        .iter()
        .map(|p| Event {
            at: p.timestamp,
            price_at: p.timestamp,
            price: p.price,
            trade: None,
            dealer: None,
        })
        .collect();
    d.events[1].trade = Some(Trade {
        vault_buys: false,
        corridor_amount: u(1_000_000),
        limit_price: 1.0012,
    });
    // The same live decision offers a 10 bps sale; a 12 bps customer limit
    // accepts it. The symmetric buffer instead asks 15 bps and misses it.
    assert_eq!(
        evaluate(&cfg, &inventory_context(6000), &spread_history()).sell_bps,
        Some(10)
    );
    let aware = run(&d, &cfg, &book()).unwrap();
    assert_eq!(aware.candidate.customer_fills, 1);
    assert_eq!(aware.candidate.execution_costs, u(100));
    assert!(aware.config.spreads.inventory_aware);
    cfg.spreads.inventory_aware = false;
    let symmetric = run(&d, &cfg, &book()).unwrap();
    assert_eq!(symmetric.candidate.customer_fills, 0);
    assert_eq!(aware.baseline.ending_nav, symmetric.baseline.ending_nav);
    assert_eq!(aware.candidate.rebalance_fills, 0);
}
#[test]
fn replay_refuses_future_prices_wrong_tokens_unsorted_events_and_wrong_versions() {
    let mut d = data();
    d.events[0].price_at = 101;
    assert!(run(&d, &config(), &book()).is_err());
    let mut d = data();
    d.events.reverse();
    assert!(run(&d, &config(), &book()).is_err());
    let mut d = data();
    d.corridor_token = d.settlement_token.clone();
    assert!(run(&d, &config(), &book()).is_err());
    let mut d = data();
    d.version = 2;
    assert!(run(&d, &config(), &book()).is_err());
}
#[tokio::test]
async fn dealer_pacing_survives_restart_and_corrupt_pacing_blocks_start() {
    let dir = std::env::temp_dir().join(format!("stitch-module-test-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut dealer, mut request, _) = dealer_terms();
    dealer.url = "http://127.0.0.1:1".into();
    let mut cfg = config();
    cfg.rebalance.dealer = Some(dealer);
    request.deadline = stitch_bot::time::unix_now() + 60;
    let rt = runtime::Runtime::new(cfg.clone(), &dir).unwrap();
    rt.begin_quote(request).unwrap();
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("modules-attempt.json")).unwrap())
            .unwrap();
    assert!(stored["next_attempt_at"].as_u64().unwrap() > stitch_bot::time::unix_now());
    assert!(runtime::Runtime::new(cfg.clone(), &dir).is_ok());
    std::fs::write(dir.join("modules-attempt.json"), "broken").unwrap();
    assert!(runtime::Runtime::new(cfg, &dir).is_err());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn recorded_spread_inputs_explain_the_decision_and_read_legacy_status() {
    let dir =
        std::env::temp_dir().join(format!("stitch-spread-telemetry-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = ModulesConfig::default();
    let rt = runtime::Runtime::new(cfg, &dir).unwrap();
    let path = dir.join(runtime::STATUS_FILE);
    async fn read_at(path: &std::path::Path, at: u64) -> serde_json::Value {
        for _ in 0..100 {
            let value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            if value["decisions"]
                .as_array()
                .and_then(|v| v.last())
                .is_some_and(|v| v["decision"]["at"] == at)
            {
                return value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("telemetry snapshot was not written");
    }
    let mut ctx = Context {
        settlement: u(70_000_000),
        corridor: u(30_000_000),
        ..context()
    };
    assert!(rt.decide(&ctx).blocked);
    let first = read_at(&path, 100).await;
    assert_eq!(
        first["decisions"][0]["inputs"]["spread_window"]["extra_bps"],
        serde_json::Value::Null
    );
    ctx.now = 140;
    ctx.price_at = 140;
    ctx.balances_at = 140;
    ctx.price = 1.01;
    let d = rt.decide(&ctx);
    assert!(!d.blocked);
    let mut value = read_at(&path, 140).await;
    let inputs = &value["decisions"][1]["inputs"];
    assert_eq!(inputs["spread_window"]["samples"], 2);
    assert_eq!(inputs["spread_window"]["history_secs"], 40);
    assert_eq!(inputs["spread_window"]["low"], 1.0);
    assert_eq!(inputs["spread_window"]["high"], 1.01);
    assert_eq!(inputs["spread_window"]["extra_bps"], d.volatility_bps);
    assert_eq!(inputs["base_buy_bps"], 20);
    assert_eq!(
        d.buy_bps.unwrap(),
        inputs["inventory_buy_bps"].as_u64().unwrap() as u32 + d.volatility_bps
    );
    // A burst before the writer can run must retain the final diagnostic.
    for at in 141..150 {
        rt.quote_status(runtime::QuoteState::WaitingForSession, "Disconnected", at);
    }
    for _ in 0..100 {
        let snapshot: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        if snapshot["quote_status"]["at"] == 149 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let snapshot: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(snapshot["quote_status"]["at"], 149);
    assert_eq!(snapshot["quote_status"]["state"], "waiting_for_session");
    value.as_object_mut().unwrap().remove("quote_status");
    value["config"]["spreads"]
        .as_object_mut()
        .unwrap()
        .remove("inventory_aware");
    for observation in value["decisions"].as_array_mut().unwrap() {
        observation.as_object_mut().unwrap().remove("inputs");
    }
    let legacy: runtime::Status = serde_json::from_value(value).unwrap();
    assert!(!legacy.config.spreads.inventory_aware);
    assert!(legacy.quote_status.is_none());
    assert!(legacy.decisions.iter().all(|o| o.inputs.is_none()));
    drop(rt);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn economic_inventory_includes_settlement_that_is_not_currently_quotable() {
    let ctx = Context {
        settlement: u(90_000_000),
        corridor: u(10_000_000),
        available_settlement: u(1_000_000),
        ..context()
    };
    let d = evaluate(&config(), &ctx, &[]);
    assert_eq!(d.inventory_bps, Some(1000));
    assert!(d.buy_limit <= ctx.available_settlement);
}

#[test]
fn different_token_precisions_produce_the_same_exposure_and_sale_size() {
    let mut cfg = config();
    cfg.rebalance.enabled = true;
    let ctx = Context {
        settlement_decimals: 18,
        settlement: U256::from(40u64) * U256::from(10u64).pow(U256::from(18u64)),
        ..context()
    };
    let d = evaluate(&cfg, &ctx, &[]);
    assert_eq!(d.inventory_bps, Some(6000));
    assert_eq!(d.rebalance_sell, u(2_000_000));
}
