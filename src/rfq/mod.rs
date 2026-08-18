// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The RFQ responder — Stitch's second, additive leg (dual-run pilot).
//!
//! The ladder keeps resting signed orders in the public book exactly as
//! before; this module ALSO answers the venue's private quote requests over a
//! WebSocket (`/v2/maker/stream`): publish indicative levels every second,
//! and reply to each `quoteRequest` with a firm, taker-bound, Permit2-signed
//! `LimitOrder` within the venue's reply budget (~750 ms hard, <400 ms
//! target).
//!
//! Kill switch: the whole module is spawned from `run()` only when
//! `[rfq].enabled = true` and the bot has at least one pool. Anything
//! less and no code here executes — a disabled config is behaviorally
//! identical to a build without the module.
//!
//! Shared with the ladder: the same feed URLs, the same
//! `quote::bid_price`/`ask_price` spreads, the same [`crate::eip712`] Permit2
//! digest and the same order-bytes encoder — one pricing and signing story,
//! two distribution channels. RFQ caps staleness at
//! [`crate::config::RFQ_MAX_STALENESS_SECS`] (60s) so a 900s ladder template
//! cannot keep firm quotes on a 14-minute-old print; the ladder still uses
//! `[feed].staleness_secs` as written. Deliberately NOT shared: the tick loop
//! (RFQ runs its own 1 s cadence and its own price cache so a slow ladder tick
//! can't blow the reply budget) and the nonce ledger (RFQ nonces live in a
//! disjoint namespace, see [`nonce`]).

pub mod math;
pub mod nonce;
pub mod order;
pub mod reserve;
pub mod responder;
pub mod session;
pub mod time;
pub mod wire;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Context as _;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, error, info, warn};

use crate::closer::executor::{encode_allowance, encode_balance_of};
use crate::config::{rfq_staleness_secs, Config};
use crate::eip712::permit2_digest;
use crate::feed::{HttpFeed, PriceFeed, Quote};
use crate::rpc::Wallet;
use crate::signer::DynSigner;
use crate::taker::encode_order_bytes;
use crate::tick::{is_price_usable, is_stale, unix_now};

use nonce::rfq_nonce;
use order::{build_order, RfqOrderSpec};
use reserve::{Reservations, RESERVATIONS_FILE};
use responder::{
    book_from_pool, decide_quote, levels_for, wallet_tokens, CorridorBook, InventoryView,
};
use session::AuthedSession;
use time::{format_iso_ms, parse_iso_ms, unix_ms_now};
use wire::{
    MakerFrame, QuoteRejectFrame, QuoteRequestFrame, QuoteResponseFrame, RejectReason, VenueFrame,
};

/// Everything the responder task needs, resolved once at spawn so the hot
/// path never re-reads config or environment.
pub struct RfqRuntime {
    url: String,
    api_key: String,
    maker_id: String,
    chain_id: u64,
    permit2: Address,
    reactor: Address,
    validation_contract: Address,
    staleness_secs: u64,
    rpc_url: String,
    indexer_url: String,
    books: Vec<CorridorBook>,
    signer: DynSigner,
    /// `rfq-reservations.json` next to stitch.toml. None only when the process
    /// has no config dir (env-only key); then the ledger is memory-only.
    reservations_path: Option<std::path::PathBuf>,
}

/// How old a wallet reading may be before a `max` side goes dark.
/// The refresh loop runs every second; 3s covers one missed tick plus RPC
/// slop. Fail closed: a stale or missing reading is no inventory.
const INVENTORY_TTL_SECS: u64 = 3;

/// Latest `min(balance, Permit2 allowance)` per token, shared between the
/// refresh loop and the session task. Quote path only reads — never waits
/// on RPC, so a slow node can't blow the reply budget.
#[derive(Clone, Default)]
struct InventoryCache(Arc<RwLock<HashMap<Address, (U256, u64)>>>);

impl InventoryCache {
    fn view(&self, now_secs: u64) -> InventoryView {
        let Ok(map) = self.0.read() else {
            return InventoryView::default();
        };
        InventoryView::new(
            map.iter()
                .filter(|(_, (_, at))| now_secs.saturating_sub(*at) <= INVENTORY_TTL_SECS)
                .map(|(token, (amount, _))| (*token, *amount))
                .collect(),
        )
    }

    fn set(&self, token: Address, funded: U256, at: u64) {
        if let Ok(mut map) = self.0.write() {
            map.insert(token, (funded, at));
        }
    }
}

/// Spawn the responder if — and only if — the config turns it on. Every
/// failure here refuses to spawn (fail closed) and leaves the ladder alone.
///
/// `config_dir` is the folder holding `stitch.toml`, so a panel-written
/// `rfq-api.key` sitting next to it is found even when the process env was
/// baked at container-create time (a later Settings save only restarts).
pub fn maybe_spawn(
    cfg: &Config,
    signer: DynSigner,
    dry_run: bool,
    config_dir: Option<&Path>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.rfq_active() {
        return None;
    }
    if dry_run {
        info!("RFQ responder configured but skipped: --dry-run never sends firm quotes");
        return None;
    }
    let rfq = cfg.rfq.as_ref()?; // rfq_active() implies Some
    let api_key = match load_rfq_api_key(&rfq.api_key_env, config_dir) {
        Ok(k) => k,
        Err(e) => {
            error!(
                error = %format!("{e:#}"),
                "RFQ responder NOT started: the maker API key is missing"
            );
            return None;
        }
    };
    let runtime = match build_runtime(cfg, rfq, api_key, signer, config_dir) {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %format!("{e:#}"), "RFQ responder NOT started: invalid configuration");
            return None;
        }
    };
    info!(
        url = %runtime.url,
        maker_id = %runtime.maker_id,
        corridors = ?runtime.books.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
        "starting RFQ responder (dual-run: the ladder is unchanged)"
    );
    Some(tokio::spawn(run(runtime)))
}

/// Resolve the maker API key without ever logging it.
///
/// Order: `{NAME}_FILE` (preferred, same as the wallet), then `{NAME}`, then
/// `rfq-api.key` next to the config. The last is what the panel writes.
fn load_rfq_api_key(api_key_env: &str, config_dir: Option<&Path>) -> anyhow::Result<String> {
    let file_env = format!("{api_key_env}_FILE");
    if let Ok(key) = crate::signer::read_env_secret(&file_env, api_key_env) {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let Some(dir) = config_dir else {
        anyhow::bail!("set {file_env} or {api_key_env}");
    };
    let path = dir.join(crate::setup::RFQ_API_KEY_FILE);
    let key = std::fs::read_to_string(&path).map_err(|_| {
        anyhow::anyhow!("set {file_env} or {api_key_env}, or place rfq-api.key next to stitch.toml")
    })?;
    let key = key.trim().to_string();
    anyhow::ensure!(
        !key.is_empty(),
        "rfq-api.key is empty; paste a new key in Settings"
    );
    Ok(key)
}

fn build_runtime(
    cfg: &Config,
    rfq: &crate::config::RfqConfig,
    api_key: String,
    signer: DynSigner,
    config_dir: Option<&Path>,
) -> anyhow::Result<RfqRuntime> {
    let books = cfg
        .pools
        .iter()
        .map(|p| book_from_pool(p, &cfg.feed.url))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    anyhow::ensure!(!books.is_empty(), "no pools to quote over RFQ");
    Ok(RfqRuntime {
        url: rfq.url.clone(),
        api_key,
        maker_id: rfq.maker_id.clone(),
        chain_id: cfg.chain_id,
        permit2: cfg.permit2.parse().context("invalid permit2 address")?,
        reactor: cfg.reactor.parse().context("invalid reactor address")?,
        validation_contract: rfq
            .validation_contract
            .parse()
            .context("invalid [rfq].validation_contract")?,
        staleness_secs: rfq_staleness_secs(cfg.feed.staleness_secs),
        rpc_url: cfg.rpc_url.clone(),
        indexer_url: cfg.indexer_url.clone(),
        books,
        signer,
        reservations_path: config_dir.map(|dir| dir.join(RESERVATIONS_FILE)),
    })
}

fn same_addr(value: &str, addr: Address) -> bool {
    value.parse::<Address>().is_ok_and(|parsed| parsed == addr)
}

fn pair_matches(book: &CorridorBook, token_a: &str, token_b: &str) -> bool {
    (same_addr(token_a, book.collateral) && same_addr(token_b, book.debt))
        || (same_addr(token_a, book.debt) && same_addr(token_b, book.collateral))
}

fn book_for_request<'a>(
    books: &'a [CorridorBook],
    req: &QuoteRequestFrame,
) -> Option<&'a CorridorBook> {
    books
        .iter()
        .find(|b| pair_matches(b, &req.sell_token, &req.buy_token))
        .or_else(|| books.iter().find(|b| b.slug == req.corridor_id))
}

/// Map a configured pool onto a venue-assigned slug. Token match wins so a
/// leftover `rfq_corridor` typo cannot hide a pair the venue already routed.
fn bind_assigned_book(
    book: &CorridorBook,
    accepted: &wire::SessionAcceptedFrame,
    chain_id: u64,
) -> Option<CorridorBook> {
    let from_pair = accepted
        .corridor_pairs
        .iter()
        .find(|p| p.chain_id == chain_id && pair_matches(book, &p.collateral_token, &p.debt_token));
    let slug = if let Some(pair) = from_pair {
        Some(pair.slug.clone())
    } else if !book.slug.is_empty() && accepted.corridors.iter().any(|c| c == &book.slug) {
        Some(book.slug.clone())
    } else {
        None
    };
    match slug {
        Some(slug) => {
            let mut bound = book.clone();
            bound.slug = slug;
            Some(bound)
        }
        None => {
            if book.slug.is_empty() {
                warn!(
                    collateral = %book.collateral,
                    debt = %book.debt,
                    "pool not assigned by the venue; skipping"
                );
            } else {
                warn!(
                    corridor = %book.slug,
                    "configured rfq_corridor not assigned by the venue; skipping"
                );
            }
            None
        }
    }
}

/// Reconnect-forever driver: one authenticated session at a time, exponential
/// backoff 1s → 30s on any failure, reset after each successful acceptance.
async fn run(rt: RfqRuntime) {
    // The dependency graph compiles two rustls crypto providers (ring via this
    // module, aws-lc-rs via other deps); pick ring explicitly or the TLS
    // connector refuses to guess at runtime. First install wins; harmless if
    // something else got there first.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let prices = PriceCache::default();
    let mut feed_urls: Vec<String> = rt.books.iter().map(|b| b.feed_url.clone()).collect();
    feed_urls.sort();
    feed_urls.dedup();
    for url in feed_urls {
        tokio::spawn(price_loop(url, prices.clone()));
    }

    // Every RFQ side (Exact cap or live wallet) refreshes funded amounts
    // off the quote path. Exact is a cap on top of the wallet, not a bypass.
    let inventory = InventoryCache::default();
    let tokens = wallet_tokens(&rt.books);
    if !tokens.is_empty() {
        let wallet = Wallet::new(&rt.rpc_url, rt.signer.clone(), rt.chain_id);
        let indexer = crate::indexer::Indexer::from_base_url(&rt.indexer_url);
        tokio::spawn(inventory_loop(
            wallet,
            indexer,
            rt.chain_id,
            rt.permit2,
            tokens,
            inventory.clone(),
        ));
    }

    // The reservation ledger outlives sessions AND process restarts: every
    // quote signed before a disconnect or a panel save stays fillable until
    // its deadline + skew. A fresh in-memory ledger would re-advertise
    // inventory already committed (audit M-04). A corrupt file refuses to
    // quote rather than start empty over live signatures.
    let mut reservations = match &rt.reservations_path {
        Some(path) => match Reservations::load(path, unix_now()) {
            Ok(ledger) => ledger,
            Err(e) => {
                error!(
                    error = %format!("{e:#}"),
                    path = %path.display(),
                    "RFQ responder stopping: reservation ledger unreadable"
                );
                return;
            }
        },
        None => {
            warn!("RFQ reservations are memory-only: no config dir to persist them");
            Reservations::new()
        }
    };
    let mut backoff_secs = 1u64;
    loop {
        match session::connect_and_auth(&rt.url, &rt.api_key, &rt.maker_id, &rt.signer).await {
            Ok(authed) => {
                backoff_secs = 1;
                let (err, ledger) = session_loop(
                    &rt,
                    &prices,
                    &inventory,
                    authed,
                    std::mem::take(&mut reservations),
                )
                .await;
                reservations = ledger;
                warn!(
                    error = %format!("{err:#}"),
                    "RFQ session ended (superseded, closed, or failed); reconnecting"
                );
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "RFQ connect/auth failed");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

/// One authenticated session: 1 s level publishing + request dispatch, until
/// the stream dies or the venue goes silent past its own heartbeat timeout.
/// Takes the cross-session reservation ledger and always hands it back.
async fn session_loop(
    rt: &RfqRuntime,
    prices: &PriceCache,
    inventory: &InventoryCache,
    authed: AuthedSession,
    reservations: Reservations,
) -> (anyhow::Error, Reservations) {
    match session_loop_inner(rt, prices, inventory, authed, reservations).await {
        (Ok(()), ledger) => (anyhow::anyhow!("venue closed the stream"), ledger),
        (Err(e), ledger) => (e, ledger),
    }
}

async fn session_loop_inner(
    rt: &RfqRuntime,
    prices: &PriceCache,
    inventory: &InventoryCache,
    authed: AuthedSession,
    reservations: Reservations,
) -> (anyhow::Result<()>, Reservations) {
    let AuthedSession {
        mut stream,
        accepted,
    } = authed;

    // Bind each pool to a venue slug: tokens first, then a configured label.
    let books: Vec<CorridorBook> = rt
        .books
        .iter()
        .filter_map(|b| bind_assigned_book(b, &accepted, rt.chain_id))
        .collect();
    if books.is_empty() {
        info!("venue assigned no corridors yet; staying connected");
    }

    let mut engine = Engine {
        books,
        reservations,
        inventory: inventory.clone(),
        counter: 0,
        chain_id: rt.chain_id,
        permit2: rt.permit2,
        reactor: rt.reactor,
        validation_contract: rt.validation_contract,
        staleness_secs: rt.staleness_secs,
        signer: rt.signer.clone(),
    };

    // Venue liveness: it pings on heartbeat_interval; nothing at all for the
    // whole timeout means the link is dead even if TCP hasn't noticed.
    let heartbeat_timeout =
        std::time::Duration::from_millis(accepted.heartbeat_timeout_ms.max(1_000));
    let mut last_rx = tokio::time::Instant::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // The `?`s live in an inner block so the ledger is handed back to the
    // reconnect driver on every exit path.
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    anyhow::ensure!(
                        last_rx.elapsed() < heartbeat_timeout,
                        "venue silent for {:?} (heartbeat timeout)", last_rx.elapsed()
                    );
                    let now_ms = unix_ms_now();
                    engine.reservations.prune(now_ms / 1_000);
                    for frame in engine.level_frames(prices, now_ms) {
                        stream
                            .send(Message::text(serde_json::to_string(&frame)?))
                            .await
                            .context("sending levels")?;
                    }
                }
                msg = stream.next() => {
                    let msg = msg.context("venue closed the stream")??;
                    last_rx = tokio::time::Instant::now();
                    match msg {
                        Message::Text(text) => {
                            let frame = match serde_json::from_str::<VenueFrame>(text.as_str()) {
                                Ok(f) => f,
                                Err(e) => {
                                    debug!(error = %e, raw = %text, "unparseable venue frame; ignoring");
                                    continue;
                                }
                            };
                            if let Some(reply) = engine.dispatch(frame, prices).await {
                                stream
                                    .send(Message::text(serde_json::to_string(&reply)?))
                                    .await
                                    .context("sending quote reply")?;
                            }
                        }
                        Message::Ping(payload) => {
                            stream.send(Message::Pong(payload)).await.context("sending pong")?;
                        }
                        Message::Close(reason) => {
                            warn!(?reason, "venue sent close (possibly a superseding session)");
                            anyhow::bail!("venue closed the session: {reason:?}");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    .await;
    (result, engine.reservations)
}

/// Quoting state for one session. Owns the nonce counter and (for the
/// session's lifetime) the reservation ledger — the ledger itself is handed
/// back to the reconnect driver when the session ends, because signed quotes
/// outlive sockets. Everything price-shaped delegates to [`responder`].
struct Engine {
    books: Vec<CorridorBook>,
    reservations: Reservations,
    inventory: InventoryCache,
    counter: u64,
    chain_id: u64,
    permit2: Address,
    reactor: Address,
    validation_contract: Address,
    staleness_secs: u64,
    signer: DynSigner,
}

impl Engine {
    /// Levels for every corridor with a fresh feed. A stale/missing feed
    /// publishes nothing — the venue's >5 s gap rule takes the corridor dark,
    /// which is exactly the stale-feed behavior we want.
    fn level_frames(&self, prices: &PriceCache, now_ms: u64) -> Vec<MakerFrame> {
        let now_secs = now_ms / 1_000;
        let inventory = self.inventory.view(now_secs);
        self.books
            .iter()
            .filter_map(|book| {
                let quote = prices.get(&book.feed_url)?;
                if is_stale(quote.timestamp, now_secs, self.staleness_secs)
                    || !is_price_usable(quote.price)
                {
                    return None;
                }
                Some(MakerFrame::Levels(levels_for(
                    book,
                    quote.price,
                    self.reservations.reserved(&book.slug, true, now_secs),
                    self.reservations.reserved(&book.slug, false, now_secs),
                    format_iso_ms(now_ms),
                    &inventory,
                )))
            })
            .collect()
    }

    /// Handle one venue frame; `Some` is a reply to send.
    async fn dispatch(&mut self, frame: VenueFrame, prices: &PriceCache) -> Option<MakerFrame> {
        match frame {
            VenueFrame::QuoteRequest(req) => Some(self.respond(req, prices).await),
            VenueFrame::QuoteResult(r) => {
                // Informational only. A losing quote's reservation still holds
                // until its deadline + skew — the signed order is out there.
                info!(rfq_id = %r.rfq_id, result = %r.result, "quote result");
                None
            }
            VenueFrame::QuoteExpired(e) => {
                // The taker's accept window lapsed without a submit. Drop the
                // claim now so the next request on this side is not sized
                // against a quote the venue has already un-counted.
                if self.reservations.release(&e.rfq_id) {
                    info!(rfq_id = %e.rfq_id, "quote expired unaccepted; inventory released");
                } else {
                    debug!(rfq_id = %e.rfq_id, "quote expired unaccepted; no local reservation");
                }
                None
            }
            VenueFrame::Challenge(_) | VenueFrame::SessionAccepted(_) => {
                warn!("unexpected session frame mid-stream (venue restart or supersede?)");
                None
            }
        }
    }

    /// Firm-quote path. Every early exit is a reject frame so the venue never
    /// waits out the reply deadline on our account.
    async fn respond(&mut self, req: QuoteRequestFrame, prices: &PriceCache) -> MakerFrame {
        let reject = |reason| {
            MakerFrame::QuoteReject(QuoteRejectFrame {
                rfq_id: req.rfq_id.clone(),
                reason,
            })
        };

        let Some(book) = book_for_request(&self.books, &req) else {
            warn!(corridor = %req.corridor_id, "quote request for an unknown corridor");
            return reject(RejectReason::Busy);
        };
        let book = book.clone();
        if req.chain_id != self.chain_id {
            warn!(
                req_chain = req.chain_id,
                our_chain = self.chain_id,
                "chain id mismatch"
            );
            return reject(RejectReason::Busy);
        }

        let now_ms = unix_ms_now();
        let now_secs = now_ms / 1_000;
        let Some(quote) = prices.get(&book.feed_url) else {
            return reject(RejectReason::StaleFeed);
        };
        if is_stale(quote.timestamp, now_secs, self.staleness_secs) || !is_price_usable(quote.price)
        {
            return reject(RejectReason::StaleFeed);
        }

        // Deadline first: a request whose maxExpiresAt is unreadable or
        // already past can never yield a valid order, so it fails before any
        // pricing. The order lives exactly to the venue's maxExpiresAt
        // (floored to seconds, so never past it); the quote's own expiry is
        // the shorter of the TTL and the *floored* deadline — clamping to the
        // raw millisecond maxExpiresAt would let expiresAt outlive the signed
        // deadline by up to 999ms and the venue rejects that as
        // quote_outlives_order.
        let Some(max_expires_ms) = parse_iso_ms(&req.max_expires_at) else {
            warn!(raw = %req.max_expires_at, "unparseable maxExpiresAt");
            return reject(RejectReason::Busy);
        };
        // A stalled socket can deliver a request already past its replyBy:
        // the venue would only classify our reply as late, so signing and
        // reserving would pin inventory for the whole TTL for nothing.
        let Some(reply_by_ms) = parse_iso_ms(&req.reply_by) else {
            warn!(raw = %req.reply_by, "unparseable replyBy");
            return reject(RejectReason::Busy);
        };
        if now_ms >= reply_by_ms {
            return reject(RejectReason::Busy);
        }
        let deadline_secs = max_expires_ms / 1_000;
        if deadline_secs <= now_secs {
            return reject(RejectReason::Busy);
        }
        let expires_ms = (now_ms + req.quote_ttl_ms).min(deadline_secs * 1_000);
        let Ok(taker) = req.taker.parse::<Address>() else {
            warn!(raw = %req.taker, "unparseable taker address");
            return reject(RejectReason::Busy);
        };

        let plan = match decide_quote(
            &book,
            &req,
            quote.price,
            self.reservations.reserved(&book.slug, true, now_secs),
            self.reservations.reserved(&book.slug, false, now_secs),
            &self.inventory.view(now_secs),
        ) {
            Ok(plan) => plan,
            Err(reason) => return reject(reason),
        };

        let maker = self.signer.address();
        let nonce = rfq_nonce(now_ms, self.counter);
        self.counter += 1;
        let order = build_order(&RfqOrderSpec {
            reactor: self.reactor,
            maker,
            nonce,
            deadline_secs,
            input_token: plan.input_token,
            input_amount: plan.input,
            output_token: plan.output_token,
            output_amount: plan.output,
            validation_contract: self.validation_contract,
            taker,
        });
        let digest = permit2_digest(&order, self.permit2, self.chain_id);
        let signature = match self.signer.sign_digest(digest).await {
            Ok(sig) => sig,
            Err(e) => {
                error!(error = %format!("{e:#}"), rfq_id = %req.rfq_id, "signing failed");
                return reject(RejectReason::Busy);
            }
        };

        // The reservation starts the moment the signed order exists — even if
        // the send fails, the signature may have left the process.
        self.reservations.reserve(
            req.rfq_id.clone(),
            book.slug.clone(),
            plan.bid,
            plan.input,
            deadline_secs,
        );

        MakerFrame::QuoteResponse(QuoteResponseFrame {
            rfq_id: req.rfq_id,
            sell_amount: plan.sell_amount.to_string(),
            buy_amount: plan.buy_amount.to_string(),
            fee_amount: plan.fee.to_string(),
            expires_at: format_iso_ms(expires_ms),
            encoded_order: alloy_primitives::hex::encode_prefixed(encode_order_bytes(&order)),
            signature: alloy_primitives::hex::encode_prefixed(signature),
            signer: maker.to_string(),
        })
    }
}

/// Latest feed quote per URL, shared between the fetch loops and the session
/// task. `std::sync::RwLock` — nothing holds it across an await.
#[derive(Clone, Default)]
pub struct PriceCache(Arc<RwLock<HashMap<String, Quote>>>);

impl PriceCache {
    pub fn get(&self, url: &str) -> Option<Quote> {
        self.0.read().ok()?.get(url).cloned()
    }

    pub fn set(&self, url: String, quote: Quote) {
        if let Ok(mut map) = self.0.write() {
            map.insert(url, quote);
        }
    }

    pub fn invalidate(&self, url: &str) {
        if let Ok(mut map) = self.0.write() {
            map.remove(url);
        }
    }
}

/// Apply one fetch to the RFQ cache. A live quote replaces the last print; a
/// failed fetch drops it so the next `quoteRequest` is `StaleFeed`, not a
/// held mid. The ladder tick still keeps last-print + staleness.
fn on_feed_fetch(cache: &PriceCache, url: &str, result: anyhow::Result<Quote>) {
    match result {
        Ok(quote) => cache.set(url.to_string(), quote),
        Err(e) => {
            debug!(feed = %url, error = %e, "rfq feed fetch failed");
            cache.invalidate(url);
        }
    }
}

/// Refresh one feed URL every second. A failed fetch invalidates the cached
/// quote — RFQ fails closed instead of holding the last print.
async fn price_loop(url: String, cache: PriceCache) {
    let feed = HttpFeed::new(&url);
    loop {
        on_feed_fetch(&cache, &url, feed.fetch().await);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// What RFQ may still pledge: funded wallet minus live book commitments.
/// The venue subtracts the same v1 rows in `reserveReply`; quoting the raw
/// balance is what produced `invalid` / `insufficient_funding` in dual-run.
fn quotable_inventory(funded: U256, committed: U256) -> U256 {
    funded.saturating_sub(committed)
}

/// `min(ERC20 balance, Permit2 allowance)` minus indexer-side live book
/// input. Allowance is the fill-time constraint: a balance the wallet hasn't
/// approved is not quotable. Book commitments are the dual-run constraint:
/// the ladder already signed those tokens away.
async fn read_funded(
    wallet: &Wallet,
    indexer: &crate::indexer::Indexer,
    chain_id: u64,
    permit2: Address,
    token: Address,
) -> anyhow::Result<(U256, U256)> {
    let owner = wallet.address();
    let balance = wallet
        .read_uint(token, &Bytes::from(encode_balance_of(owner)))
        .await
        .context("reading RFQ token balance")?;
    let allowance = wallet
        .read_uint(token, &Bytes::from(encode_allowance(owner, permit2)))
        .await
        .context("reading RFQ Permit2 allowance")?;
    let funded = balance.min(allowance);
    let committed = indexer
        .committed_input(chain_id, &owner.to_string(), &token.to_string())
        .await
        .context("reading RFQ committed input")?
        .parse::<U256>()
        .context("parsing RFQ committed input")?;
    Ok((funded, committed))
}

/// Refresh quotable amounts for every `max` token, once a second. A failed
/// read leaves the previous value in place; the TTL then fails the side
/// closed instead of quoting a stale high balance forever.
async fn inventory_loop(
    wallet: Wallet,
    indexer: crate::indexer::Indexer,
    chain_id: u64,
    permit2: Address,
    tokens: Vec<Address>,
    cache: InventoryCache,
) {
    loop {
        let now = unix_now();
        for token in &tokens {
            match read_funded(&wallet, &indexer, chain_id, permit2, *token).await {
                Ok((funded, committed)) => {
                    let quotable = quotable_inventory(funded, committed);
                    if !funded.is_zero() && quotable.is_zero() {
                        warn!(
                            token = %token,
                            funded = %funded,
                            committed = %committed,
                            "rfq inventory fully committed to the book; this side stays dark until the ladder releases it"
                        );
                    }
                    cache.set(*token, quotable, now);
                }
                Err(e) => warn!(
                    token = %token,
                    error = %format!("{e:#}"),
                    "rfq inventory refresh failed; last reading kept until TTL"
                ),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::wire::QuoteExpiredFrame;
    use super::*;
    use crate::config::RfqCapacity;
    use crate::quote::Spread;
    use crate::signer::{recover_address, LocalSigner};
    use crate::tick::unix_now;
    use alloy_primitives::U256;
    use k256::ecdsa::SigningKey;

    const COLLATERAL: &str = "0x0000000000000000000000000000000000000001";
    const DEBT: &str = "0x0000000000000000000000000000000000000002";

    fn test_engine() -> Engine {
        let key = SigningKey::from_slice(
            &alloy_primitives::hex::decode(
                "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
            )
            .unwrap(),
        )
        .unwrap();
        Engine {
            books: vec![CorridorBook {
                slug: "cngn-usdc".into(),
                collateral: COLLATERAL.parse().unwrap(),
                debt: DEBT.parse().unwrap(),
                collateral_decimals: 6,
                debt_decimals: 6,
                buy_spread: Some(Spread::Bps(200)),
                sell_spread: Some(Spread::Bps(200)),
                buy_capacity_debt: Some(RfqCapacity::Exact(U256::from(1_500_000_000u64))),
                sell_capacity_collateral: Some(RfqCapacity::Exact(U256::from(1_500_000_000u64))),
                feed_url: "http://feed".into(),
            }],
            reservations: Reservations::new(),
            inventory: {
                let cache = InventoryCache::default();
                let now = unix_now();
                cache.set(DEBT.parse().unwrap(), U256::from(10_000_000_000u64), now);
                cache.set(
                    COLLATERAL.parse().unwrap(),
                    U256::from(10_000_000_000u64),
                    now,
                );
                cache
            },
            counter: 0,
            chain_id: 8453,
            permit2: "0x000000000022D473030F116dDEE9F6B43aC78BA3"
                .parse()
                .unwrap(),
            reactor: "0x00000000000000000000000000000000000000e1"
                .parse()
                .unwrap(),
            validation_contract: "0x00000000000000000000000000000000000000f1"
                .parse()
                .unwrap(),
            staleness_secs: 900,
            signer: Arc::new(LocalSigner::new(key)),
        }
    }

    fn fresh_prices() -> PriceCache {
        let prices = PriceCache::default();
        prices.set(
            "http://feed".into(),
            Quote {
                price: 1.0,
                timestamp: unix_now(),
            },
        );
        prices
    }

    fn exact_input_request(rfq_id: &str) -> QuoteRequestFrame {
        let deadline = unix_ms_now() + 120_000;
        QuoteRequestFrame {
            rfq_id: rfq_id.into(),
            corridor_id: "cngn-usdc".into(),
            chain_id: 8453,
            sell_token: COLLATERAL.into(),
            buy_token: DEBT.into(),
            sell_amount: Some("1000000000".into()),
            buy_amount: None,
            taker: "0x0000000000000000000000000000000000000003".into(),
            reply_by: format_iso_ms(unix_ms_now() + 750),
            quote_ttl_ms: 5_000,
            max_expires_at: format_iso_ms(deadline),
            fee_bps: 1,
        }
    }

    #[tokio::test]
    async fn a_firm_quote_signs_a_taker_bound_order_and_reserves_inventory() {
        let mut engine = test_engine();
        let prices = fresh_prices();

        let reply = engine.respond(exact_input_request("rfq_1"), &prices).await;
        let MakerFrame::QuoteResponse(resp) = reply else {
            panic!("expected a firm quote, got {reply:?}");
        };
        // Exact-input contract: the cap echoes, the fee is the golden 1 bps fit.
        assert_eq!(resp.sell_amount, "1000000000");
        assert_eq!(resp.fee_amount, "99990");
        assert_eq!(resp.buy_amount, "979902009");
        assert_eq!(
            resp.signer,
            engine.signer.address().to_string(),
            "signer field is the funding wallet"
        );

        // The signature is a real Permit2 witness sig by the funding wallet
        // over an order whose bytes we can decode enough to check the nonce
        // namespace (word 4 of the tuple head area holds the nonce).
        let sig: [u8; 65] = alloy_primitives::hex::decode(&resp.signature)
            .unwrap()
            .try_into()
            .unwrap();
        let order_bytes = alloy_primitives::hex::decode(&resp.encoded_order).unwrap();
        // abi.encode(LimitOrder): [0]=tuple offset, [1]=info offset, then the
        // OrderInfo block starts at word 6: reactor, swapper, nonce, deadline…
        let nonce_word: [u8; 32] = order_bytes[8 * 32..9 * 32].try_into().unwrap();
        let nonce = U256::from_be_bytes::<32>(nonce_word);
        assert_ne!(
            nonce & (U256::from(1u8) << nonce::RFQ_NONCE_BIT),
            U256::ZERO,
            "RFQ orders mint namespaced nonces"
        );
        let _ = sig;

        // The quote's input is reserved: a second identical request used to
        // inventory-reject (2 × 979902009 > 1.5e9). It now quotes the leftover
        // so the venue can bundle this slice with other makers.
        let reply = engine.respond(exact_input_request("rfq_2"), &prices).await;
        let MakerFrame::QuoteResponse(resp) = reply else {
            panic!("expected a leftover quote, got {reply:?}");
        };
        assert_eq!(resp.buy_amount, "520097991");
        assert_eq!(engine.reservations.len(), 2);
    }

    #[test]
    fn a_failed_fetch_invalidates_the_rfq_cache() {
        let cache = PriceCache::default();
        cache.set(
            "http://feed".into(),
            Quote {
                price: 1.0,
                timestamp: unix_now(),
            },
        );
        assert!(cache.get("http://feed").is_some());

        on_feed_fetch(
            &cache,
            "http://feed",
            Err(anyhow::anyhow!("feed host down")),
        );
        assert!(
            cache.get("http://feed").is_none(),
            "a failed fetch must drop the last print, not hold it"
        );

        on_feed_fetch(
            &cache,
            "http://feed",
            Ok(Quote {
                price: 2.0,
                timestamp: unix_now(),
            }),
        );
        assert_eq!(cache.get("http://feed").unwrap().price, 2.0);
    }

    fn test_book() -> CorridorBook {
        CorridorBook {
            slug: String::new(),
            collateral: COLLATERAL.parse().unwrap(),
            debt: DEBT.parse().unwrap(),
            collateral_decimals: 6,
            debt_decimals: 6,
            buy_spread: Some(Spread::Bps(200)),
            sell_spread: Some(Spread::Bps(200)),
            buy_capacity_debt: Some(RfqCapacity::Exact(U256::from(1u64))),
            sell_capacity_collateral: Some(RfqCapacity::Exact(U256::from(1u64))),
            feed_url: "http://feed".into(),
        }
    }

    fn accepted(
        corridors: &[&str],
        pairs: Vec<wire::CorridorPairFrame>,
    ) -> wire::SessionAcceptedFrame {
        wire::SessionAcceptedFrame {
            maker_id: "mk".into(),
            signing_address: "0x00".into(),
            heartbeat_interval_ms: 1_000,
            heartbeat_timeout_ms: 5_000,
            corridors: corridors.iter().map(|s| (*s).to_string()).collect(),
            corridor_pairs: pairs,
        }
    }

    #[test]
    fn bind_requires_pair_and_chain_not_list_cardinality() {
        let book = test_book();
        let one_unrelated = accepted(
            &["nvda-usdg-robinhood"],
            vec![wire::CorridorPairFrame {
                slug: "nvda-usdg-robinhood".into(),
                chain_id: 4663,
                collateral_token: "0x0000000000000000000000000000000000000099".into(),
                debt_token: "0x0000000000000000000000000000000000000098".into(),
            }],
        );
        assert!(
            bind_assigned_book(&book, &one_unrelated, 8453).is_none(),
            "one assigned corridor is not a pair match"
        );

        let tokens_wrong_chain = accepted(
            &["cngn-usdc-base"],
            vec![wire::CorridorPairFrame {
                slug: "cngn-usdc-base".into(),
                chain_id: 56,
                collateral_token: COLLATERAL.into(),
                debt_token: DEBT.into(),
            }],
        );
        assert!(bind_assigned_book(&book, &tokens_wrong_chain, 8453).is_none());

        let tokens_ok = accepted(
            &["cngn-usdc-base"],
            vec![wire::CorridorPairFrame {
                slug: "cngn-usdc-base".into(),
                chain_id: 8453,
                collateral_token: COLLATERAL.into(),
                debt_token: DEBT.into(),
            }],
        );
        let bound = bind_assigned_book(&book, &tokens_ok, 8453).unwrap();
        assert_eq!(bound.slug, "cngn-usdc-base");
    }

    #[tokio::test]
    async fn quote_expired_releases_inventory_so_the_next_request_can_fill() {
        let mut engine = test_engine();
        let prices = fresh_prices();
        let first = engine.respond(exact_input_request("rfq_1"), &prices).await;
        assert!(matches!(first, MakerFrame::QuoteResponse(_)));
        assert_eq!(engine.reservations.len(), 1);

        let none = engine
            .dispatch(
                VenueFrame::QuoteExpired(QuoteExpiredFrame {
                    rfq_id: "rfq_1".into(),
                }),
                &prices,
            )
            .await;
        assert!(none.is_none());
        assert!(engine.reservations.is_empty());

        let second = engine.respond(exact_input_request("rfq_2"), &prices).await;
        let MakerFrame::QuoteResponse(resp) = second else {
            panic!("expected a full-size quote after expiry release, got {second:?}");
        };
        assert_eq!(resp.buy_amount, "979902009");
        assert_eq!(engine.reservations.len(), 1);
    }

    #[tokio::test]
    async fn stale_or_missing_feeds_reject_and_publish_no_levels() {
        let mut engine = test_engine();

        // No price at all yet.
        let empty = PriceCache::default();
        let reply = engine.respond(exact_input_request("rfq_1"), &empty).await;
        let MakerFrame::QuoteReject(rej) = reply else {
            panic!("expected reject, got {reply:?}");
        };
        assert_eq!(rej.reason, RejectReason::StaleFeed);
        assert!(engine.level_frames(&empty, unix_ms_now()).is_empty());

        // A price far older than the staleness window.
        let stale = PriceCache::default();
        stale.set(
            "http://feed".into(),
            Quote {
                price: 1.0,
                timestamp: unix_now().saturating_sub(3_600),
            },
        );
        let reply = engine.respond(exact_input_request("rfq_2"), &stale).await;
        let MakerFrame::QuoteReject(rej) = reply else {
            panic!("expected reject, got {reply:?}");
        };
        assert_eq!(rej.reason, RejectReason::StaleFeed);
        assert!(engine.level_frames(&stale, unix_ms_now()).is_empty());
        assert!(engine.reservations.is_empty(), "rejects reserve nothing");
    }

    #[tokio::test]
    async fn wrong_chain_or_corridor_rejects_busy() {
        let mut engine = test_engine();
        let prices = fresh_prices();

        let mut req = exact_input_request("rfq_1");
        req.chain_id = 1;
        let MakerFrame::QuoteReject(rej) = engine.respond(req, &prices).await else {
            panic!("expected reject");
        };
        assert_eq!(rej.reason, RejectReason::Busy);

        let mut req = exact_input_request("rfq_2");
        req.sell_token = "0x0000000000000000000000000000000000000099".into();
        req.buy_token = "0x0000000000000000000000000000000000000098".into();
        req.corridor_id = "kes-usdt".into();
        let MakerFrame::QuoteReject(rej) = engine.respond(req, &prices).await else {
            panic!("expected reject");
        };
        assert_eq!(rej.reason, RejectReason::Busy);

        // A mistyped slug still quotes when the tokens match the book.
        let mut req = exact_input_request("rfq_3");
        req.corridor_id = "kes-usdt".into();
        let MakerFrame::QuoteResponse(_) = engine.respond(req, &prices).await else {
            panic!("tokens should bind the book");
        };
    }

    #[tokio::test]
    async fn deadlines_never_exceed_max_expires_at() {
        let mut engine = test_engine();
        let prices = fresh_prices();

        // Sub-second maxExpiresAt on purpose: the signed deadline floors to
        // whole seconds, and the quote expiry must stay within *that*, not
        // the raw millisecond bound (quote_outlives_order otherwise).
        let mut req = exact_input_request("rfq_1");
        req.max_expires_at = format_iso_ms(unix_ms_now() + 4_500);
        let max_expires_ms = parse_iso_ms(&req.max_expires_at).unwrap();
        let MakerFrame::QuoteResponse(resp) = engine.respond(req, &prices).await else {
            panic!("expected a firm quote");
        };
        let expires_ms = parse_iso_ms(&resp.expires_at).unwrap();
        assert!(
            expires_ms <= max_expires_ms,
            "quote expiry within the bound"
        );

        // Decode the signed deadline (OrderInfo word 9: reactor, swapper,
        // nonce, deadline) and pin the validator's invariant directly.
        let order_bytes = alloy_primitives::hex::decode(&resp.encoded_order).unwrap();
        let deadline_word: [u8; 32] = order_bytes[9 * 32..10 * 32].try_into().unwrap();
        let deadline_secs = U256::from_be_bytes::<32>(deadline_word).to::<u64>();
        assert!(
            expires_ms <= deadline_secs * 1_000,
            "quote expiry ({expires_ms}) must not outlive the signed deadline ({deadline_secs}s)"
        );

        // An already-expired maxExpiresAt is unquotable.
        let mut req = exact_input_request("rfq_2");
        req.max_expires_at = "2020-01-01T00:00:00.000Z".into();
        let MakerFrame::QuoteReject(rej) = engine.respond(req, &prices).await else {
            panic!("expected reject");
        };
        assert_eq!(rej.reason, RejectReason::Busy);

        // A request delivered past its replyBy (stalled socket) can only be
        // classified late by the venue — never sign or reserve for it.
        let reservations_before = engine.reservations.len();
        let mut req = exact_input_request("rfq_3");
        req.reply_by = format_iso_ms(unix_ms_now() - 1_000);
        let MakerFrame::QuoteReject(rej) = engine.respond(req, &prices).await else {
            panic!("expected reject");
        };
        assert_eq!(rej.reason, RejectReason::Busy);
        assert_eq!(
            engine.reservations.len(),
            reservations_before,
            "late requests reserve nothing"
        );
    }

    #[test]
    fn the_signature_recovers_to_the_funding_wallet() {
        // Sanity-tie the engine's signer to the recover path used on-chain.
        let engine = test_engine();
        let digest = alloy_primitives::keccak256(b"probe");
        let sig = futures_executor_block_on_sign(&engine.signer, digest);
        assert_eq!(
            recover_address(digest, &sig).unwrap(),
            engine.signer.address()
        );
    }

    /// Tiny helper: LocalSigner's sign is synchronous under the hood, so a
    /// current-thread block_on is enough for a unit test.
    fn futures_executor_block_on_sign(
        signer: &DynSigner,
        digest: alloy_primitives::B256,
    ) -> [u8; 65] {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(signer.sign_digest(digest))
            .unwrap()
    }

    #[test]
    fn inventory_cache_drops_stale_readings() {
        let cache = InventoryCache::default();
        let token: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        cache.set(token, U256::from(100u64), 1_000);

        assert_eq!(
            cache.view(1_000 + INVENTORY_TTL_SECS).funded(token),
            Some(U256::from(100u64)),
            "a reading at the TTL edge is still usable"
        );
        assert_eq!(
            cache.view(1_000 + INVENTORY_TTL_SECS + 1).funded(token),
            None,
            "one second past the TTL fails closed"
        );
        assert_eq!(
            cache.view(1_000).funded(
                "0x0000000000000000000000000000000000000002"
                    .parse()
                    .unwrap()
            ),
            None,
            "an unread token is not inventable"
        );
    }

    #[test]
    fn quotable_inventory_subtracts_the_live_book() {
        assert_eq!(
            quotable_inventory(U256::from(100u64), U256::from(40u64)),
            U256::from(60u64)
        );
        assert_eq!(
            quotable_inventory(U256::from(100u64), U256::from(100u64)),
            U256::ZERO,
            "fully pledged to the ladder is no RFQ inventory"
        );
        assert_eq!(
            quotable_inventory(U256::from(50u64), U256::from(80u64)),
            U256::ZERO,
            "over-committed book never goes negative"
        );
    }

    #[test]
    fn rfq_api_key_falls_back_to_the_sibling_file() {
        let dir = std::env::temp_dir().join(format!(
            "stitch-rfq-key-{}-{}",
            std::process::id(),
            "sibling"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rfq-api.key"), "  tx_live_from_file  \n").unwrap();
        let key = load_rfq_api_key("STITCH_RFQ_API_KEY_UNSET_FOR_TEST", Some(&dir)).unwrap();
        assert_eq!(key, "tx_live_from_file");
        std::fs::remove_dir_all(&dir).ok();
    }
}
