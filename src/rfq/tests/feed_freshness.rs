// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Feed timestamps through the real HTTP, RFQ and NAV adapters. All I/O is local.
use super::*;
use axum::{http::StatusCode, routing::get, Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn serve_feed(body: String, status: StatusCode) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route("/", get(move || async move { (status, body) }));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

#[tokio::test]
async fn invalid_http_timestamps_clear_the_cache_and_cannot_fall_back() {
    let now = unix_now();
    let timestamps = [
        "0".into(),
        (now + 60).to_string(),
        (now * 1_000).to_string(),
        u64::MAX.to_string(),
        "18446744073709551616".into(), // u64 overflow
        "-1".into(),
        "1.5".into(),
        format!("\"{now}\""),
        "null".into(),
    ];
    for timestamp in timestamps {
        let (url, server) = serve_feed(
            format!(r#"{{"price":1.0,"timestamp":{timestamp}}}"#),
            StatusCode::OK,
        )
        .await;
        let feed = HttpFeed::new(&url);
        let prices = PriceCache::default();
        let mut engine = test_engine();
        engine.books[0].feed_url = url.clone();
        prices.set(
            url.clone(),
            Quote {
                price: 1.0,
                timestamp: now,
            },
        );
        assert!(!engine.level_frames(&prices, unix_now_ms()).is_empty());
        // Repeated successful HTTP responses must not keep a bad mark usable.
        for _ in 0..2 {
            let fetched = feed.fetch().await;
            assert!(fetched.is_err(), "timestamp {timestamp} was accepted");
            on_feed_fetch(&prices, &url, fetched);
            assert!(prices.get(&url).is_none());
            assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
            assert!(matches!(
                engine
                    .respond(exact_input_request("bad-feed"), &prices)
                    .await,
                MakerFrame::QuoteReject(QuoteRejectFrame {
                    reason: RejectReason::StaleFeed,
                    ..
                })
            ));
        }
        assert!(engine.reservations.is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn fresh_http_seconds_preserve_price_and_timestamp_and_failure_clears_them() {
    let now = unix_now();
    let (url, server) = serve_feed(
        json!({"price": 1.23456789, "timestamp": now}).to_string(),
        StatusCode::OK,
    )
    .await;
    let fetched = HttpFeed::new(&url).fetch().await.unwrap();
    assert_eq!(
        fetched,
        Quote {
            price: 1.23456789,
            timestamp: now
        }
    );
    assert!(!is_stale(fetched.timestamp, now, 0));
    server.abort();

    let (url, server) = serve_feed("unavailable".into(), StatusCode::SERVICE_UNAVAILABLE).await;
    let prices = PriceCache::default();
    prices.set(url.clone(), fetched.clone());
    on_feed_fetch(&prices, &url, HttpFeed::new(&url).fetch().await);
    assert!(prices.get(&url).is_none());
    on_feed_fetch(&prices, &url, Ok(fetched.clone()));
    assert_eq!(prices.get(&url), Some(fetched));
    server.abort();
}

#[tokio::test]
async fn old_http_seconds_are_rejected_at_use_without_retimestamping() {
    let now = unix_now();
    let timestamp = now - 241;
    let (url, server) = serve_feed(
        json!({"price": 1.0, "timestamp": timestamp}).to_string(),
        StatusCode::OK,
    )
    .await;
    let fetched = HttpFeed::new(&url).fetch().await.unwrap();
    assert_eq!(fetched.timestamp, timestamp);
    let mut engine = test_engine();
    engine.books[0].feed_url = url.clone();
    let prices = PriceCache::default();
    on_feed_fetch(&prices, &url, Ok(fetched));
    assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
    assert!(matches!(
        engine
            .respond(exact_input_request("held-old"), &prices)
            .await,
        MakerFrame::QuoteReject(QuoteRejectFrame {
            reason: RejectReason::StaleFeed,
            ..
        })
    ));
    server.abort();
}

#[tokio::test]
async fn rfq_rechecks_held_timestamps_with_modules_off_shadow_live_and_rebalance_only() {
    use crate::modules::Mode;
    for (mode, rebalance_only) in [
        (None, false),
        (Some(Mode::Shadow), false),
        (Some(Mode::Live), false),
        (Some(Mode::Live), true),
    ] {
        let mut engine = test_engine();
        let dir = mode.map(|mode| enable_modules(&mut engine, mode, 7_000_000_000, 3_000_000_000));
        // Exercise the policy's fallback when modules observe or only rebalance.
        if mode == Some(Mode::Shadow) {
            *engine.modules.as_ref().unwrap().balances.write().unwrap() = None;
        }
        if rebalance_only {
            let modules = engine.modules.as_mut().unwrap();
            modules.config.inventory.enabled = false;
            modules.config.rebalance.enabled = true;
            *modules.balances.write().unwrap() = None;
        }
        let now = unix_now();
        let prices = fresh_prices();
        for timestamp in [now + 60, now * 1_000, u64::MAX, 0, now - 241] {
            prices.set(
                "http://feed".into(),
                Quote {
                    price: 1.0,
                    timestamp,
                },
            );
            assert!(
                engine.level_frames(&prices, unix_now_ms()).is_empty(),
                "{mode:?}: {timestamp}"
            );
            assert!(
                matches!(
                    engine
                        .respond(exact_input_request("held-invalid"), &prices)
                        .await,
                    MakerFrame::QuoteReject(QuoteRejectFrame {
                        reason: RejectReason::StaleFeed,
                        ..
                    })
                ),
                "{mode:?}: {timestamp}"
            );
            assert!(engine.reservations.is_empty());
        }
        let prices = fresh_prices();
        assert!(!engine.level_frames(&prices, unix_now_ms()).is_empty());
        assert!(matches!(
            engine.respond(exact_input_request("fresh"), &prices).await,
            MakerFrame::QuoteResponse(_)
        ));
        if let Some(dir) = dir {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[test]
fn rfq_levels_obey_exact_age_and_future_boundaries_after_clock_changes() {
    let mut engine = test_engine();
    let now = unix_now();
    let prices = PriceCache::default();
    for (timestamp, available) in [
        (now, true),
        (now - 240, true),
        (now - 241, false),
        (now + 1, false),
    ] {
        prices.set(
            "http://feed".into(),
            Quote {
                price: 1.0,
                timestamp,
            },
        );
        assert_eq!(
            !engine.level_frames(&prices, now * 1_000).is_empty(),
            available
        );
    }
    prices.set(
        "http://feed".into(),
        Quote {
            price: 1.0,
            timestamp: now,
        },
    );
    assert!(!engine.level_frames(&prices, now * 1_000).is_empty());
    assert!(engine.level_frames(&prices, (now - 1) * 1_000).is_empty());
    assert!(engine.level_frames(&prices, (now + 241) * 1_000).is_empty());
}

#[tokio::test]
async fn valid_cached_http_sample_can_resume_after_clock_recovers() {
    use crate::modules::Mode;
    for mode in [None, Some(Mode::Shadow), Some(Mode::Live)] {
        let mut engine = test_engine();
        let dir = mode.map(|mode| enable_modules(&mut engine, mode, 7_000_000_000, 3_000_000_000));
        let now = unix_now();
        let (url, server) = serve_feed(
            json!({"price": 1.0, "timestamp": now}).to_string(),
            StatusCode::OK,
        )
        .await;
        engine.books[0].feed_url = url.clone();
        let prices = PriceCache::default();
        on_feed_fetch(&prices, &url, HttpFeed::new(&url).fetch().await);
        assert!(!engine.level_frames(&prices, now * 1_000).is_empty());
        assert!(engine.level_frames(&prices, (now - 1) * 1_000).is_empty());
        // This sample passed ingestion. The wall-clock policy can use it again
        // after clock recovery, provided it is still within the book's age limit.
        assert!(!engine.level_frames(&prices, now * 1_000).is_empty());
        assert!(engine.reservations.is_empty());
        assert!(matches!(
            engine
                .respond(exact_input_request("clock-recovered"), &prices)
                .await,
            MakerFrame::QuoteResponse(_)
        ));
        assert!(engine.level_frames(&prices, (now + 241) * 1_000).is_empty());
        server.abort();
        if let Some(dir) = dir {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

async fn nav_rpc() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    let app = Router::new().route("/", axum::routing::post(move |Json(request): Json<Value>| {
        let counter = counter.clone();
        async move {
            assert_eq!(request["method"], "eth_call");
            counter.fetch_add(1, Ordering::SeqCst);
            let data = alloy_primitives::hex::decode(request["params"][0]["data"].as_str().unwrap()).unwrap();
            let words = if data == encode_free_settlement() { vec![10_000_000] }
                else if data == encode_free_corridor() { vec![4_000_000_000_000_000_000u64] }
                else if data == encode_last_settled_nav() { vec![12_345] }
                else if data == encode_settlement_decimals() { vec![6] }
                else if data == encode_corridor_decimals() { vec![18] }
                else if data == encode_epochs(U256::from(7)) { vec![2, 0, 1, 0, 0, 0, 1] }
                else { panic!("unexpected NAV view") };
            let bytes: Vec<u8> = words.into_iter().flat_map(|v| U256::from(v).to_be_bytes::<32>()).collect();
            Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": alloy_primitives::hex::encode_prefixed(bytes)}))
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, reads, task)
}

fn nav_engine_and_request() -> (Engine, AttestRequestFrame) {
    let mut engine = test_engine();
    let vault = "0x2222222222222222222222222222222222222222";
    engine.vault = Some(vault.parse().unwrap());
    *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
        settlement: DEBT.parse().unwrap(),
        corridor: COLLATERAL.parse().unwrap(),
        max_input_settlement: U256::MAX,
        max_input_corridor: U256::MAX,
        max_lifetime_secs: 120,
    });
    engine.configured = engine.books.clone();
    let VenueFrame::AttestRequest(mut req) = attest_request(vault, engine.chain_id) else {
        unreachable!()
    };
    let now = unix_now();
    req.reply_by = format_iso_ms(unix_now_ms() + 30_000);
    req.attestation.valid_after = (now - 1).to_string();
    req.attestation.valid_until = (now + 300).to_string();
    (engine, req)
}

#[tokio::test]
async fn nav_cosign_rejects_invalid_feed_before_rpc_and_signs_valid_seconds() {
    let (mut engine, req) = nav_engine_and_request();
    // NAV must still be guarded when the venue did not seat this corridor.
    engine.books.clear();
    let (url, reads, server) = nav_rpc().await;
    engine.rpc = Rpc::new(&url);
    let now = unix_now();
    let prices = PriceCache::default();
    for timestamp in [now + 60, now * 1_000, u64::MAX, 0, now - 241] {
        prices.set(
            "http://feed".into(),
            Quote {
                price: 1.5,
                timestamp,
            },
        );
        let reply = engine.cosign(req.clone(), &prices).await;
        assert!(
            matches!(
                reply,
                MakerFrame::AttestReject(AttestRejectFrame {
                    reason: AttestRejectReason::StaleFeed,
                    ..
                })
            ),
            "{timestamp}: {reply:?}"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
    prices.set(
        "http://feed".into(),
        Quote {
            price: 1.5,
            timestamp: unix_now(),
        },
    );
    let reply = engine.cosign(req.clone(), &prices).await;
    let MakerFrame::AttestResponse(reply) = reply else {
        panic!("{reply:?}")
    };
    assert_eq!(reads.load(Ordering::SeqCst), 6);
    let signature: [u8; 65] = alloy_primitives::hex::decode(reply.signature)
        .unwrap()
        .try_into()
        .unwrap();
    let payload = nav_attestation_payload(&NavAttestation::parse(&req.attestation).unwrap());
    assert_eq!(
        recover_address(payload.digest(), &signature).unwrap(),
        engine.signer.address()
    );
    server.abort();
}

#[tokio::test]
async fn invalid_http_fetch_cannot_reuse_fresh_cache_for_nav_and_recovers_on_valid_fetch() {
    let now = unix_now();
    let body = Arc::new(RwLock::new(json!({"price": 1.5, "timestamp": now})));
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
    let feed_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let feed = HttpFeed::new(&url);
    let (mut engine, req) = nav_engine_and_request();
    engine.books[0].feed_url = url.clone();
    engine.configured = engine.books.clone();
    let (rpc_url, reads, rpc_server) = nav_rpc().await;
    engine.rpc = Rpc::new(&rpc_url);
    let prices = PriceCache::default();

    on_feed_fetch(&prices, &url, feed.fetch().await);
    assert!(!engine.level_frames(&prices, now * 1_000).is_empty());
    // A successfully ingested sample may resume after clock recovery.
    assert!(engine.level_frames(&prices, (now - 1) * 1_000).is_empty());
    assert!(matches!(
        engine.cosign(req.clone(), &prices).await,
        MakerFrame::AttestResponse(_)
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 6);

    for timestamp in [now + 60, now * 1_000, 0, u64::MAX] {
        // Seed a still-fresh fallback before EVERY invalid HTTP response.
        *body.write().unwrap() = json!({"price": 1.5, "timestamp": unix_now()});
        on_feed_fetch(&prices, &url, feed.fetch().await);
        assert!(prices.get(&url).is_some());
        *body.write().unwrap() = json!({"price": 1.5, "timestamp": timestamp});
        let fetched = feed.fetch().await;
        assert!(fetched.is_err());
        on_feed_fetch(&prices, &url, fetched);
        assert!(prices.get(&url).is_none());
        assert!(engine.level_frames(&prices, now * 1_000).is_empty());
        // Even advancing past a rejected future timestamp cannot revive it.
        assert!(engine.level_frames(&prices, (now + 60) * 1_000).is_empty());
        assert!(matches!(
            engine.cosign(req.clone(), &prices).await,
            MakerFrame::AttestReject(AttestRejectFrame {
                reason: AttestRejectReason::StaleFeed,
                ..
            })
        ));
        assert_eq!(reads.load(Ordering::SeqCst), 6);
    }

    *body.write().unwrap() = json!({"price": 1.5, "timestamp": unix_now()});
    on_feed_fetch(&prices, &url, feed.fetch().await);
    assert!(matches!(
        engine.cosign(req, &prices).await,
        MakerFrame::AttestResponse(_)
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 12);
    feed_server.abort();
    rpc_server.abort();
}
