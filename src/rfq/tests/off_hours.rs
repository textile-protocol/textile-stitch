// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Off-hours prints through the real RFQ engine and HTTP adapter: an
//! off-hours pool publishes wider, capped levels and firm quotes; a plain
//! pool never quotes one. All I/O is local.
use super::*;
use crate::pricing::off_hours::{OffHoursConfig, Session};
use axum::{routing::get, Json, Router};
use serde_json::json;

fn block(max_notional_debt: &str) -> OffHoursConfig {
    OffHoursConfig {
        offshore_extra_bps: 50,
        held_extra_bps: 100,
        held_extra_bps_per_hour: 0,
        held_max_extra_bps: 100,
        held_max_hours: 24.0,
        max_notional_debt: max_notional_debt.into(),
    }
}

fn prices(session: Session) -> PriceCache {
    let now = unix_now();
    let prices = PriceCache::default();
    prices.set(
        "http://feed".into(),
        Quote {
            price: 1.0,
            timestamp: now,
            degraded: false,
            reference: None,
            bid: None,
            ask: None,
            session,
            market_at: Some(now - 600),
            market_source: Some("icap-setfx".into()),
            spread: Default::default(),
        },
    );
    prices
}

fn levels(engine: &mut Engine, prices: &PriceCache) -> Option<LevelsFrame> {
    engine
        .level_frames(prices, unix_now_ms())
        .into_iter()
        .find_map(|f| match f {
            MakerFrame::Levels(l) => Some(l),
            _ => None,
        })
}

fn top_rate(levels: &[crate::rfq::wire::Level]) -> U256 {
    levels[0].rate_ray.parse().unwrap()
}

fn response(reply: MakerFrame) -> QuoteResponseFrame {
    match reply {
        MakerFrame::QuoteResponse(r) => r,
        other => panic!("expected a quote, got {other:?}"),
    }
}

#[tokio::test]
async fn an_off_hours_pool_quotes_a_held_print_wider_and_smaller() {
    let mut engine = test_engine();
    engine.books[0].off_hours = Some(block("5000000000"));
    let live = levels(&mut engine, &prices(Session::Live)).unwrap();
    let held = levels(&mut engine, &prices(Session::Held)).unwrap();
    assert!(top_rate(&held.bids) < top_rate(&live.bids));
    assert!(top_rate(&held.asks) > top_rate(&live.asks));

    // The taker sells 1,000 collateral: off-hours the maker pays less debt.
    let live_quote = response(
        engine
            .respond(exact_input_request("live"), &prices(Session::Live))
            .await,
    );
    let held_quote = response(
        engine
            .respond(exact_input_request("held"), &prices(Session::Held))
            .await,
    );
    let paid = |r: &QuoteResponseFrame| r.buy_amount.parse::<U256>().unwrap();
    assert!(paid(&held_quote) < paid(&live_quote));

    // With a 500 debt cap the same request is answered for at most 500.
    let mut capped = test_engine();
    capped.books[0].off_hours = Some(block("500000000"));
    let small = response(
        capped
            .respond(exact_input_request("capped"), &prices(Session::Held))
            .await,
    );
    assert!(paid(&small) <= U256::from(500_000_000u64));
    let held_levels = levels(&mut capped, &prices(Session::Held)).unwrap();
    let bid_collateral = held_levels
        .bids
        .iter()
        .map(|l| l.size.parse::<U256>().unwrap())
        .fold(U256::ZERO, |a, b| a + b);
    // 500 debt at a 0.97 bid buys at most ~515.5 collateral.
    assert!(
        bid_collateral <= U256::from(515_464_000u64),
        "{bid_collateral}"
    );
}

#[tokio::test]
async fn a_plain_pool_never_quotes_an_off_hours_or_unknown_print() {
    let mut engine = test_engine();
    for session in [Session::Offshore, Session::Held, Session::Unknown] {
        let p = prices(session);
        assert!(levels(&mut engine, &p).is_none(), "{session:?}");
        let reply = engine.respond(exact_input_request("off"), &p).await;
        assert!(
            matches!(
                reply,
                MakerFrame::QuoteReject(QuoteRejectFrame {
                    reason: RejectReason::DegradedFeed,
                    ..
                })
            ),
            "{session:?}"
        );
    }
    assert!(engine.reservations.is_empty());
}

#[tokio::test]
async fn a_held_print_past_max_hours_is_not_quoted_even_by_an_off_hours_pool() {
    let mut engine = test_engine();
    engine.books[0].off_hours = Some(OffHoursConfig {
        held_max_hours: 0.1,
        ..block("5000000000")
    });
    // marketAt is 600 s old; 0.1 h is 360 s.
    let p = prices(Session::Held);
    assert!(levels(&mut engine, &p).is_none());
    assert!(matches!(
        engine.respond(exact_input_request("old"), &p).await,
        MakerFrame::QuoteReject(QuoteRejectFrame {
            reason: RejectReason::DegradedFeed,
            ..
        })
    ));
    // Offshore isn't aged.
    assert!(levels(&mut engine, &prices(Session::Offshore)).is_some());
}

#[tokio::test]
async fn only_a_request_that_asked_accepts_an_off_hours_print_over_http() {
    let now = unix_now();
    let app = Router::new().route(
        "/price",
        get(move || async move {
            Json(json!({
                "pair": "copm-usdt", "chainId": 42220, "price": 0.00025,
                "timestamp": now, "session": "held", "marketAt": now - 3_600,
                "marketSource": "icap-setfx"
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://{}/price?pair=copm-usdt&chainId=42220",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    assert!(HttpFeed::new(&base).fetch().await.is_err());
    let asked = crate::pricing::off_hours::feed_request_url(&base, true);
    assert_eq!(asked, format!("{base}&offHours=1"));
    let quote = HttpFeed::new(&asked).fetch().await.unwrap();
    assert_eq!(quote.session, Session::Held);
    assert_eq!(quote.market_at, Some(now - 3_600));
    server.abort();
}
