// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Move-clamped prints through the real RFQ engine and HTTP adapter: levels
//! and firm quotes keep going out, widened from the same print; held,
//! fallback and reasonless prints stay dark; NAV co-signing still refuses.
//! All I/O is local.
use super::feed_freshness::{nav_engine_and_request, nav_rpc};
use super::*;
use crate::pricing::clamped::DegradedReason;
use axum::{routing::get, Json, Router};
use serde_json::json;
use std::sync::atomic::Ordering;

fn print(degraded: bool, reason: DegradedReason, gap: Option<f64>) -> PriceCache {
    let prices = PriceCache::default();
    prices.set(
        "http://feed".into(),
        Quote {
            price: 1.0,
            timestamp: unix_now(),
            degraded,
            degraded_reason: reason,
            clamp_gap_bps: gap,
            spread: Default::default(),
            reference: None,
            bid: None,
            ask: None,
            session: Default::default(),
            market_at: None,
            market_source: None,
        },
    );
    prices
}

fn live() -> PriceCache {
    print(false, DegradedReason::Missing, None)
}

fn clamped(gap: Option<f64>) -> PriceCache {
    print(true, DegradedReason::Clamped, gap)
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

/// A side's levels as `(size, rate)`, comparable.
fn side(levels: &[crate::rfq::wire::Level]) -> Vec<(String, String)> {
    levels
        .iter()
        .map(|l| (l.size.clone(), l.rate_ray.clone()))
        .collect()
}

fn response(reply: MakerFrame) -> QuoteResponseFrame {
    match reply {
        MakerFrame::QuoteResponse(r) => r,
        other => panic!("expected a quote, got {other:?}"),
    }
}

fn rejected_with(reply: &MakerFrame) -> Option<RejectReason> {
    match reply {
        MakerFrame::QuoteReject(QuoteRejectFrame { reason, .. }) => Some(*reason),
        _ => None,
    }
}

/// The test engine's 200/200 bps book with other spreads, on a live print.
fn engine_at(buy_bps: u32, sell_bps: u32) -> Engine {
    let mut engine = test_engine();
    engine.books[0].buy_spread = Some(Spread::Bps(buy_bps));
    engine.books[0].sell_spread = Some(Spread::Bps(sell_bps));
    engine
}

fn paid(r: &QuoteResponseFrame) -> U256 {
    r.buy_amount.parse().unwrap()
}

#[tokio::test]
async fn a_clamped_print_quotes_levels_and_firm_quotes_off_the_same_widened_book() {
    // 200 bps widened by the default 10 bps of its own price: the bid keeps
    // 0.98 × 0.999 = 0.97902 (209.8 → 210 bps) and the ask 1.02 × 1.001 =
    // 1.02102 (210.2 → 211 bps), rounded away from mid.
    let mut engine = test_engine();
    let mut reference = engine_at(210, 211);

    let wide = levels(&mut engine, &clamped(Some(4.56))).expect("a clamped print publishes");
    let same = levels(&mut reference, &live()).unwrap();
    assert_eq!(side(&wide.bids), side(&same.bids));
    assert_eq!(side(&wide.asks), side(&same.asks));
    let tight = levels(&mut engine, &live()).unwrap();
    assert_ne!(side(&wide.bids), side(&tight.bids));
    assert_ne!(side(&wide.asks), side(&tight.asks));

    // The firm quote on the same print matches the reference book too, so
    // the venue never sees levels tighter than the reply it would get.
    let clamped_reply = response(
        engine
            .respond(exact_input_request("clamped"), &clamped(Some(4.56)))
            .await,
    );
    let reference_reply = response(
        reference
            .respond(exact_input_request("reference"), &live())
            .await,
    );
    assert_eq!(paid(&clamped_reply), paid(&reference_reply));
    // A fresh engine: the reply above reserved part of this one's capacity.
    let live_reply = response(
        test_engine()
            .respond(exact_input_request("live"), &live())
            .await,
    );
    assert!(paid(&clamped_reply) < paid(&live_reply));
}

#[tokio::test]
async fn the_gap_floors_the_widening_and_the_knob_sets_it() {
    // A 25.3 bps gap beats the 10 bps default: 26 bps, so 0.98 × 0.9974 →
    // 226 bps and 1.02 × 1.0026 → 227 bps.
    let mut engine = test_engine();
    let mut reference = engine_at(226, 227);
    let got = levels(&mut engine, &clamped(Some(25.3))).unwrap();
    let want = levels(&mut reference, &live()).unwrap();
    assert_eq!(side(&got.bids), side(&want.bids));
    assert_eq!(side(&got.asks), side(&want.asks));
    // A pool configured wider widens by its own amount; no gap needed.
    let mut wider = test_engine();
    wider.books[0].clamped_extra_bps = 50;
    // 0.98 × 0.995 = 0.9751 → 249 bps; 1.02 × 1.005 = 1.0251 → 251 bps.
    let mut reference = engine_at(249, 251);
    let got = levels(&mut wider, &clamped(None)).unwrap();
    let want = levels(&mut reference, &live()).unwrap();
    assert_eq!(side(&got.bids), side(&want.bids));
    assert_eq!(side(&got.asks), side(&want.asks));
}

#[tokio::test]
async fn held_fallback_reasonless_and_unknown_prints_stay_dark() {
    let mut engine = test_engine();
    for (reason, gap) in [
        (DegradedReason::Held, None),
        (DegradedReason::Fallback, None),
        // Today's API, before it says why.
        (DegradedReason::Missing, None),
        (DegradedReason::Unknown, None),
        // A clamp whose gap can't be trusted.
        (DegradedReason::Clamped, Some(-1.0)),
        (DegradedReason::Clamped, Some(f64::NAN)),
        (DegradedReason::Clamped, Some(900.0)),
    ] {
        let p = print(true, reason, gap);
        assert!(levels(&mut engine, &p).is_none(), "{reason:?} {gap:?}");
        let reply = engine.respond(exact_input_request("dark"), &p).await;
        assert_eq!(
            rejected_with(&reply),
            Some(RejectReason::DegradedFeed),
            "{reason:?} {gap:?}"
        );
    }
    assert!(engine.reservations.is_empty(), "a refusal reserves nothing");
}

#[tokio::test]
async fn an_old_clamped_print_is_still_stale() {
    let mut engine = test_engine();
    let prices = clamped(Some(4.0));
    let old = Quote {
        timestamp: unix_now() - 241,
        ..prices.get("http://feed").unwrap()
    };
    prices.set("http://feed".into(), old);
    let reply = engine.respond(exact_input_request("old"), &prices).await;
    assert_eq!(rejected_with(&reply), Some(RejectReason::StaleFeed));
    assert!(levels(&mut engine, &prices).is_none());
}

#[tokio::test]
async fn nav_cosign_still_refuses_a_clamped_print_before_any_rpc_read() {
    let (mut engine, req) = nav_engine_and_request();
    let (url, reads, server) = nav_rpc().await;
    engine.rpc = Rpc::new(&url);
    let prices = PriceCache::default();
    prices.set(
        "http://feed".into(),
        Quote {
            price: 1.5,
            ..clamped(Some(4.0)).get("http://feed").unwrap()
        },
    );
    let reply = engine.cosign(req, &prices).await;
    assert!(
        matches!(
            reply,
            MakerFrame::AttestReject(AttestRejectFrame {
                reason: AttestRejectReason::DegradedFeed,
                ..
            })
        ),
        "{reply:?}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn a_module_vault_keeps_quoting_a_clamped_print() {
    use crate::modules::{runtime::QuoteState, Mode};
    for mode in [Mode::Shadow, Mode::Live] {
        let mut engine = test_engine();
        let dir = enable_modules(&mut engine, mode, 7_000_000_000, 3_000_000_000);
        let p = clamped(Some(4.0));
        assert!(levels(&mut engine, &p).is_some(), "{mode:?}");
        let status = engine
            .modules
            .as_ref()
            .unwrap()
            .current_quote_status()
            .unwrap();
        assert!(status.state == QuoteState::Evaluating, "{mode:?}");
        assert!(matches!(
            engine.respond(exact_input_request("clamped"), &p).await,
            MakerFrame::QuoteResponse(_)
        ));
        // A held print still pauses the policy.
        let held = print(true, DegradedReason::Held, None);
        assert!(levels(&mut engine, &held).is_none(), "{mode:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn the_reason_reaches_the_rfq_gate_over_http_and_unknown_fields_are_ignored() {
    let now = unix_now();
    let body = Arc::new(RwLock::new(json!({
        "pair": "cngn-usdt", "chainId": 56, "price": 1.0, "timestamp": now,
        "degraded": true, "degradedReason": "clamped", "clampGapBps": 4.56,
        "somethingNew": {"the": "API may add fields"}
    })));
    let served = body.clone();
    let app = Router::new().route(
        "/",
        get(move || {
            let served = served.clone();
            async move { Json(served.read().unwrap().clone()) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let feed = HttpFeed::new(&url);
    let mut engine = test_engine();
    engine.books[0].feed_url = url.clone();
    let prices = PriceCache::default();
    let admit = |prices: &PriceCache, quote: Quote| {
        // An earlier poll confirmed the price (see feed_freshness::ingest).
        prices.admit(
            tokio::time::Instant::now() - std::time::Duration::from_secs(1),
            &url,
            quote.clone(),
            unix_now(),
        );
        prices.admit(tokio::time::Instant::now(), &url, quote, unix_now());
    };

    admit(&prices, feed.fetch().await.unwrap());
    let cached = prices.get(&url).unwrap();
    assert_eq!(cached.degraded_reason, DegradedReason::Clamped);
    assert_eq!(cached.clamp_gap_bps, Some(4.56));
    assert!(levels(&mut engine, &prices).is_some());
    assert!(matches!(
        engine
            .respond(exact_input_request("clamped"), &prices)
            .await,
        MakerFrame::QuoteResponse(_)
    ));

    // The next sample freezes: dark again.
    *body.write().unwrap() = json!({
        "price": 1.0, "timestamp": unix_now(), "degraded": true, "degradedReason": "held"
    });
    admit(&prices, feed.fetch().await.unwrap());
    assert!(levels(&mut engine, &prices).is_none());
    let reply = engine.respond(exact_input_request("held"), &prices).await;
    assert_eq!(rejected_with(&reply), Some(RejectReason::DegradedFeed));
    server.abort();
}
