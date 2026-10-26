// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! `[price_guard]` through the responder: feed fetches into the cache, then
//! firm quotes, levels and the module status. No I/O beyond a temp dir.
use super::*;
use crate::pricing::band::{order_within, price_wad, Order};

const URL: &str = "http://feed";

/// Feed actual reads through the price loop's shared admission path.
fn fetch_all(prices: &PriceCache, prints: &[(f64, u64)]) {
    let now = unix_now();
    // Model polls spaced like these fixture samples, without sleeping.
    for (price, ago) in prints {
        on_feed_fetch(
            tokio::time::Instant::now() - std::time::Duration::from_secs(*ago),
            prices,
            URL,
            Ok(Quote {
                price: *price,
                timestamp: now - ago,
                degraded: false,
                reference: None,
            }),
        );
    }
}

async fn reason(engine: &mut Engine, prices: &PriceCache, rfq_id: &str) -> Option<RejectReason> {
    match engine.respond(exact_input_request(rfq_id), prices).await {
        MakerFrame::QuoteReject(r) => Some(r.reason),
        MakerFrame::QuoteResponse(_) => None,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn startup_quotes_nothing_until_a_later_poll_confirms_the_first() {
    use tokio::time::{advance, Duration, Instant};

    let mut engine = test_engine();
    let prices = PriceCache::default();
    let tick = prices.clone();
    let now = unix_now();
    let sample = Quote {
        price: 1.0,
        timestamp: now,
        degraded: false,
        reference: None,
    };
    on_feed_fetch(Instant::now(), &prices, URL, Ok(sample.clone()));
    tick.admit(Instant::now(), URL, sample.clone(), now);
    assert_eq!(
        reason(&mut engine, &prices, "warm-1").await,
        Some(RejectReason::StaleFeed)
    );
    assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
    advance(Duration::from_millis(999)).await;
    on_feed_fetch(Instant::now(), &prices, URL, Ok(sample.clone()));
    assert_eq!(
        reason(&mut engine, &prices, "warm-early").await,
        Some(RejectReason::StaleFeed)
    );
    assert!(engine.reservations.is_empty());
    advance(Duration::from_millis(1)).await;
    // A later poll may confirm a cron feed's unchanged sample.
    on_feed_fetch(Instant::now(), &prices, URL, Ok(sample));
    assert_eq!(reason(&mut engine, &prices, "warm-2").await, None);
    assert!(!engine.level_frames(&prices, unix_now_ms()).is_empty());
}

#[tokio::test]
async fn a_10x_or_decimal_shift_print_is_refused_not_quoted_and_publishes_no_levels() {
    for glitch in [10.0, 0.1, 1_000.0, 0.000_001] {
        let mut engine = test_engine();
        let prices = PriceCache::default();
        fetch_all(&prices, &[(1.0, 3), (1.0, 2)]);
        assert_eq!(reason(&mut engine, &prices, "before").await, None);
        let now = unix_now();
        on_feed_fetch(
            tokio::time::Instant::now(),
            &prices,
            URL,
            Ok(Quote {
                price: glitch,
                timestamp: now - 1,
                degraded: false,
                reference: None,
            }),
        );
        assert_eq!(
            reason(&mut engine, &prices, "glitch").await,
            Some(RejectReason::StaleFeed),
            "{glitch}"
        );
        assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
        let before = engine.reservations.len();
        // The feed comes back: quoting resumes without waiting.
        on_feed_fetch(
            tokio::time::Instant::now(),
            &prices,
            URL,
            Ok(Quote {
                price: 1.001,
                timestamp: now,
                degraded: false,
                reference: None,
            }),
        );
        assert_eq!(reason(&mut engine, &prices, "after").await, None);
        assert_eq!(engine.reservations.len(), before + 1);
    }
}

#[tokio::test]
async fn a_held_print_says_why_in_the_module_status() {
    use crate::modules::{runtime::STATUS_FILE, Mode};
    let mut engine = test_engine();
    let dir = enable_modules(&mut engine, Mode::Shadow, 7_000_000_000, 3_000_000_000);
    let prices = PriceCache::default();
    fetch_all(&prices, &[(1.0, 3), (1.0, 2)]);
    on_feed_fetch(
        tokio::time::Instant::now(),
        &prices,
        URL,
        Ok(Quote {
            price: 10.0,
            timestamp: unix_now() - 1,
            degraded: false,
            reference: None,
        }),
    );
    assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
    let mut status = serde_json::Value::Null;
    for _ in 0..100 {
        status =
            serde_json::from_str(&std::fs::read_to_string(dir.join(STATUS_FILE)).unwrap()).unwrap();
        if status["quote_status"]["state"] == "suspect_price" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(status["quote_status"]["state"], "suspect_price", "{status}");
    let message = status["quote_status"]["message"].as_str().unwrap();
    assert!(message.contains("jumped 900.00%"), "{message}");
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn a_held_print_refuses_nav_co_signing_before_any_chain_read() {
    use super::feed_freshness::{nav_engine_and_request, nav_rpc};
    let (mut engine, req) = nav_engine_and_request();
    let (rpc_url, reads, server) = nav_rpc().await;
    engine.rpc = Rpc::new(&rpc_url);
    let prices = PriceCache::default();
    // The fixture's attestation is priced at 1.5.
    fetch_all(&prices, &[(1.5, 3), (1.5, 2)]);
    on_feed_fetch(
        tokio::time::Instant::now(),
        &prices,
        URL,
        Ok(Quote {
            price: 15.0,
            timestamp: unix_now() - 1,
            degraded: false,
            reference: None,
        }),
    );
    assert!(matches!(
        engine.cosign(req.clone(), &prices).await,
        MakerFrame::AttestReject(AttestRejectFrame {
            reason: AttestRejectReason::StaleFeed,
            ..
        })
    ));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    on_feed_fetch(
        tokio::time::Instant::now(),
        &prices,
        URL,
        Ok(Quote {
            price: 1.5,
            timestamp: unix_now(),
            degraded: false,
            reference: None,
        }),
    );
    assert!(matches!(
        engine.cosign(req, &prices).await,
        MakerFrame::AttestResponse(_)
    ));
    server.abort();
}

/// Co-sign the fixture's attestation (priced at 1.5) through a signer that
/// waits, admit `during` while it waits, then let it finish. `during` is
/// (price, seconds old, degraded).
async fn cosign_while_signing(during: (f64, u64, bool)) -> MakerFrame {
    use super::feed_freshness::{nav_engine_and_request, nav_rpc};
    let (mut engine, req) = nav_engine_and_request();
    let (rpc_url, _, server) = nav_rpc().await;
    engine.rpc = Rpc::new(&rpc_url);
    let custody = Arc::new(GatedSigner {
        inner: engine.signer.clone(),
        started: tokio::sync::Notify::new(),
        resume: tokio::sync::Semaphore::new(0),
    });
    engine.signer = custody.clone();
    let prices = PriceCache::default();
    fetch_all(&prices, &[(1.5, 3), (1.5, 2)]);
    let feed = prices.clone();
    let (reply, ()) = tokio::join!(engine.cosign(req, &prices), async {
        custody.started.notified().await;
        on_feed_fetch(
            tokio::time::Instant::now(),
            &feed,
            URL,
            Ok(Quote {
                price: during.0,
                timestamp: unix_now() - during.1,
                degraded: during.2,
                reference: None,
            }),
        );
        custody.resume.add_permits(1);
    });
    server.abort();
    reply
}

#[tokio::test]
async fn a_nav_co_signature_is_withheld_if_the_print_changes_while_signing() {
    let rejected = |reply: &MakerFrame| match reply {
        MakerFrame::AttestReject(AttestRejectFrame { reason, .. }) => Some(*reason),
        _ => None,
    };
    // A 10x print is held, so the cache has no price when the signer returns.
    let reply = cosign_while_signing((15.0, 1, false)).await;
    assert_eq!(
        rejected(&reply),
        Some(AttestRejectReason::StaleFeed),
        "{reply:?}"
    );
    let reply = cosign_while_signing((1.5, 1, true)).await;
    assert_eq!(
        rejected(&reply),
        Some(AttestRejectReason::DegradedFeed),
        "{reply:?}"
    );
    // 1.55 is no jump, but it is past the 1 % the attested price may sit
    // from our own.
    let reply = cosign_while_signing((1.55, 1, false)).await;
    assert_eq!(
        rejected(&reply),
        Some(AttestRejectReason::Price),
        "{reply:?}"
    );
    let reply = cosign_while_signing((1.5, 1, false)).await;
    assert!(matches!(reply, MakerFrame::AttestResponse(_)), "{reply:?}");
}

#[test]
fn a_failed_fetch_during_a_hold_shows_the_outage_not_the_jump() {
    let prices = PriceCache::default();
    fetch_all(&prices, &[(1.0, 3), (1.0, 2)]);
    on_feed_fetch(
        tokio::time::Instant::now(),
        &prices,
        URL,
        Ok(Quote {
            price: 10.0,
            timestamp: unix_now() - 1,
            degraded: false,
            reference: None,
        }),
    );
    assert!(prices.hold(URL).is_some());
    on_feed_fetch(
        tokio::time::Instant::now(),
        &prices,
        URL,
        Err(anyhow::anyhow!("feed down")),
    );
    assert!(prices.hold(URL).is_none());
    assert!(prices.get(URL).is_none());
}

/// The signed order inside a quote response, as the band sees it. The
/// `encode_order_bytes` layout puts the input amount at word 3 and the sole
/// output's amount second to last, after the variable validation data.
fn signed_order(reply: &MakerFrame, bid: bool) -> Order {
    let MakerFrame::QuoteResponse(r) = reply else {
        panic!("{reply:?}")
    };
    let bytes = alloy_primitives::hex::decode(&r.encoded_order).unwrap();
    let word = |i: usize| U256::from_be_slice(&bytes[i * 32..(i + 1) * 32]);
    let input = word(3);
    assert_eq!(
        input.to_string(),
        r.buy_amount,
        "buyAmount is the signed input"
    );
    Order {
        bid,
        input,
        output: word(bytes.len() / 32 - 2),
        debt_decimals: 6,
        collateral_decimals: 6,
    }
}

#[tokio::test]
async fn real_quotes_never_trip_even_a_one_bps_band() {
    // Stitch's spreads are never negative and its rounding at a feeable
    // size costs well under a basis point, so on its own pricing paths the
    // band is a backstop that never fires.
    let wad = price_wad(1.0).unwrap();
    for spread in [Spread::Bps(0), Spread::Bps(200)] {
        for bid in [true, false] {
            for exact_input in [true, false] {
                let mut engine = test_engine();
                engine.price_guard.max_loss_bps = 1;
                engine.books[0].buy_spread = Some(spread);
                engine.books[0].sell_spread = Some(spread);
                let mut req = exact_input_request(&format!("{spread:?}-{bid}-{exact_input}"));
                if !bid {
                    std::mem::swap(&mut req.sell_token, &mut req.buy_token);
                }
                if !exact_input {
                    req.buy_amount = req.sell_amount.take().map(|_| "777777777".into());
                }
                let reply = engine.respond(req, &fresh_prices()).await;
                let order = signed_order(&reply, bid);
                assert!(
                    order_within(&order, wad, 1),
                    "{spread:?} bid={bid} exact_input={exact_input}: {order:?}"
                );
            }
        }
    }
}

/// A print of 1/3 rounds to 0.333333333 in the 1e9-scaled amount math, a
/// hair under the print's WAD form. An ask built there gives the maker
/// slightly less than the print says, which only a 0 bps band can see. So a
/// 0 bps band refuses through the real pricing path; 1 bps signs.
fn third_prices() -> PriceCache {
    let prices = PriceCache::default();
    prices.set(
        "http://feed".into(),
        Quote {
            price: 1.0 / 3.0,
            timestamp: unix_now(),
            degraded: false,
            reference: None,
        },
    );
    prices
}

fn ask_request(rfq_id: &str) -> QuoteRequestFrame {
    let mut req = exact_input_request(rfq_id);
    std::mem::swap(&mut req.sell_token, &mut req.buy_token);
    req
}

#[tokio::test]
async fn a_built_order_outside_the_band_is_rejected_unsigned_and_reserves_nothing() {
    let mut engine = test_engine();
    engine.books[0].sell_spread = Some(Spread::Bps(0));
    engine.price_guard.max_loss_bps = 0;
    let reply = engine.respond(ask_request("tight"), &third_prices()).await;
    assert!(
        matches!(
            reply,
            MakerFrame::QuoteReject(QuoteRejectFrame {
                reason: RejectReason::Busy,
                ..
            })
        ),
        "{reply:?}"
    );
    assert!(engine.reservations.is_empty(), "nothing was signed");
    engine.price_guard.max_loss_bps = 1;
    let reply = engine.respond(ask_request("loose"), &third_prices()).await;
    assert!(matches!(reply, MakerFrame::QuoteResponse(_)), "{reply:?}");
}

#[test]
fn a_level_outside_the_band_is_not_published() {
    let asks = |engine: &mut Engine| -> usize {
        let frames = engine.level_frames(&third_prices(), unix_now_ms());
        let [MakerFrame::Levels(levels)] = frames.as_slice() else {
            panic!("{frames:?}")
        };
        levels.asks.len()
    };
    let mut engine = test_engine();
    engine.books[0].sell_spread = Some(Spread::Bps(0));
    engine.price_guard.max_loss_bps = 0;
    assert_eq!(asks(&mut engine), 0);
    engine.price_guard.max_loss_bps = 1;
    assert_eq!(asks(&mut engine), 1);
}

#[tokio::test]
async fn a_dealer_rebalance_outside_the_band_is_not_signed_or_reserved() {
    // The fixture's dealer answers at the slippage floor, 50 bps under fair.
    let RebalanceFixture {
        mut engine,
        prices,
        dir,
        received,
        server,
    } = rebalance_fixture().await;
    engine.price_guard.max_loss_bps = 10;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        engine.module_tick(&prices, unix_now());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        received.lock().unwrap().is_none(),
        "nothing reached the dealer"
    );
    assert!(engine.reservations.is_empty());
    assert!(
        engine.pending_rebalance.is_none(),
        "nothing went to the signer"
    );
    server.abort();
    std::fs::remove_dir_all(dir).ok();
}

/// Answer an ask (the maker sells collateral at the book's 200 bps) through a
/// signer that waits, change the feed while it waits, then let it finish.
/// `during` is the print admitted meanwhile: (price, seconds old, degraded).
async fn respond_while_signing(
    max_loss_bps: u32,
    during: (f64, u64, bool),
) -> (MakerFrame, Engine) {
    let mut engine = test_engine();
    engine.price_guard.max_loss_bps = max_loss_bps;
    let custody = Arc::new(GatedSigner {
        inner: engine.signer.clone(),
        started: tokio::sync::Notify::new(),
        resume: tokio::sync::Semaphore::new(0),
    });
    engine.signer = custody.clone();
    let prices = PriceCache::default();
    fetch_all(&prices, &[(1.0, 3), (1.0, 2)]);
    let feed = prices.clone();
    let (reply, ()) = tokio::join!(engine.respond(ask_request("slow"), &prices), async {
        custody.started.notified().await;
        let now = unix_now();
        on_feed_fetch(
            tokio::time::Instant::now(),
            &feed,
            URL,
            Ok(Quote {
                price: during.0,
                timestamp: now - during.1,
                degraded: during.2,
                reference: None,
            }),
        );
        custody.resume.add_permits(1);
    });
    (reply, engine)
}

#[tokio::test]
async fn a_feed_that_jumps_while_a_slow_signer_works_withholds_the_quote() {
    let (reply, engine) = respond_while_signing(1_500, (10.0, 1, false)).await;
    assert!(
        matches!(
            reply,
            MakerFrame::QuoteReject(QuoteRejectFrame {
                reason: RejectReason::StaleFeed,
                ..
            })
        ),
        "{reply:?}"
    );
    assert!(engine.reservations.is_empty(), "the signature never left");
}

#[tokio::test]
async fn a_print_that_moves_the_order_out_of_band_while_signing_withholds_it() {
    // Signed 2 % over a print of 1.0 under a 1 % band. A 5 % move is not a
    // jump, but against 1.05 the order sells 3 % cheap.
    let (reply, engine) = respond_while_signing(100, (1.05, 1, false)).await;
    assert!(
        matches!(
            reply,
            MakerFrame::QuoteReject(QuoteRejectFrame {
                reason: RejectReason::Busy,
                ..
            })
        ),
        "{reply:?}"
    );
    assert!(engine.reservations.is_empty());
    // The same move against a 15 % band still signs.
    let (reply, _) = respond_while_signing(1_500, (1.05, 1, false)).await;
    assert!(matches!(reply, MakerFrame::QuoteResponse(_)), "{reply:?}");
}

#[tokio::test]
async fn a_print_the_feed_flags_degraded_while_signing_withholds_the_quote() {
    // Same price, still in band, but the source no longer stands behind it.
    let (reply, engine) = respond_while_signing(1_500, (1.0, 1, true)).await;
    assert!(
        matches!(
            reply,
            MakerFrame::QuoteReject(QuoteRejectFrame {
                reason: RejectReason::DegradedFeed,
                ..
            })
        ),
        "{reply:?}"
    );
    assert!(engine.reservations.is_empty(), "the signature never left");
}

#[tokio::test]
async fn a_tick_loop_hold_withholds_an_in_flight_rfq_signature_and_levels() {
    let mut engine = test_engine();
    let custody = Arc::new(GatedSigner {
        inner: engine.signer.clone(),
        started: tokio::sync::Notify::new(),
        resume: tokio::sync::Semaphore::new(0),
    });
    engine.signer = custody.clone();
    let prices = PriceCache::default();
    fetch_all(&prices, &[(1.0, 3), (1.0, 2)]);
    let tick = prices.clone();
    let trusted_at = prices.get(URL).unwrap().timestamp;
    let (reply, ()) = tokio::join!(engine.respond(ask_request("tick-hold"), &prices), async {
        custody.started.notified().await;
        // The tick producer calls shared admission directly. A favorable
        // move for this ask would pass the amount band; the jump must stop it.
        tick.admit(
            tokio::time::Instant::now(),
            URL,
            Quote {
                price: 0.1,
                timestamp: trusted_at,
                degraded: false,
                reference: None,
            },
            unix_now(),
        );
        custody.resume.add_permits(1);
    });
    assert!(matches!(
        reply,
        MakerFrame::QuoteReject(QuoteRejectFrame {
            reason: RejectReason::StaleFeed,
            ..
        })
    ));
    assert!(engine.reservations.is_empty());
    assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
    tick.admit(
        tokio::time::Instant::now(),
        URL,
        Quote {
            price: 1.0,
            timestamp: trusted_at + 1,
            degraded: false,
            reference: None,
        },
        unix_now(),
    );
    assert!(!engine.level_frames(&prices, unix_now_ms()).is_empty());
}

#[tokio::test]
async fn a_print_stale_for_rfq_still_passes_the_jump_gate_before_the_ladder_can_read_it() {
    use crate::pricing::cache::FeedPolicy;
    use crate::pricing::tick::usable;

    let mut engine = test_engine();
    engine.books[0].staleness_secs = 60;
    let prices = PriceCache::new(
        None,
        FeedPolicy {
            jump: crate::config::PriceGuardConfig::default().jump(),
            staleness_secs: 900,
        },
    );
    let tick = prices.clone();
    fetch_all(&prices, &[(1.0, 500), (1.0, 400)]);
    let quote = tick.get(URL).unwrap();
    assert!(usable(&quote, unix_now(), 900).is_ok());
    // Admission did not widen RFQ's own freshness gate.
    assert_eq!(
        reason(&mut engine, &prices, "old-rfq").await,
        Some(RejectReason::StaleFeed)
    );
    assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());

    // These are actual RFQ fetches, older than its 60s window but usable by
    // the ladder. Skipping the guard here would leak a raw 10x price to it.
    fetch_all(&prices, &[(10.0, 300), (10.0, 299)]);
    assert_eq!(tick.get(URL), None);
    assert!(matches!(
        tick.hold(URL),
        Some(crate::pricing::jump::Hold::Jumped { .. })
    ));
    fetch_all(&prices, &[(1.0, 1)]);
    assert_eq!(tick.get(URL).unwrap().price, 1.0);
    assert_eq!(reason(&mut engine, &prices, "recovered-rfq").await, None);
}
