// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The RFQ responder — Stitch's private-quote leg.
//!
//! Answers the venue's private quote requests over a WebSocket
//! (`/v2/maker/stream`): publish indicative levels every second, and reply to
//! each `quoteRequest` with a firm, taker-bound, Permit2-signed `LimitOrder`
//! within the venue's reply budget (~750 ms hard, <400 ms target). The public
//! ladder is a separate switch (`book_enabled`); RFQ-only bots skip it.
//!
//! Kill switch: the whole module is spawned from `run()` only when
//! `[rfq].enabled = true` and the bot has at least one pool. Anything
//! less and no code here executes — a disabled config is behaviorally
//! identical to a build without the module.
//!
//! Shared with the ladder: the same feed URLs, the same
//! `quote::bid_price`/`ask_price` spreads, the same [`crate::protocol::eip712`] Permit2
//! digest and the same order-bytes encoder — one pricing and signing story,
//! two distribution channels. RFQ tightens staleness per feed (see
//! [`crate::config::rfq_staleness_secs`]) so a 900s ladder template cannot
//! keep firm quotes on a 14-minute-old print; the ladder still uses
//! `[feed].staleness_secs` as written. Deliberately NOT shared: the tick loop
//! (RFQ runs its own 1 s cadence and its own price cache so a slow ladder tick
//! can't blow the reply budget), the nonce ledger (RFQ nonces live in a
//! disjoint namespace, see [`nonce`]) and — the point of dual-run —
//! **inventory**. RFQ quotes `min(balance, Permit2 allowance)` in full, minus
//! its own in-flight quotes ([`reserve`]) and, when the token is the chain's
//! gas coin, the gas reserve
//! ([`crate::chain::gas_reserve`]); a ladder holding the whole
//! wallet on the book does not shrink a firm quote, and a firm quote does not
//! shrink the next ladder. Two channels pledging one balance means a fill can
//! revert when both land at once, which beats halving the depth of both.

pub mod iso8601;
pub mod math;
mod modules;
pub mod nonce;
pub mod order;
pub mod reserve;
pub mod responder;
pub mod session;
pub mod wire;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Context as _;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, error, info, warn};

use crate::book::taker::encode_order_bytes;
use crate::chain::gas_caps::GasCaps;
use crate::chain::gas_reserve::GasReserve;
use crate::chain::multicall::{
    decode_uint, encode_get_eth_balance, Batcher, Call, CANONICAL_MULTICALL3,
};
use crate::chain::rpc::{transaction_may_still_land, Rpc, Wallet};
use crate::chain::snapshot::{record, Publisher, RecordedRead};
use crate::closer::executor::{encode_allowance, encode_balance_of};
use crate::config::{rfq_staleness_secs_for_pool, Config};
use crate::pricing::feed::{HttpFeed, PriceFeed, Quote};
use crate::pricing::tick::{is_price_usable, is_stale};
use crate::protocol::attest::{
    check_attestation, check_window, price_wad, LiveVault, NavAttestation,
};
use crate::protocol::typed_data::{nav_attestation_payload, permit2_payload};
use crate::signer::DynSigner;
use crate::time::unix_now;

use crate::protocol::vault::{
    address_from_word, apply_vault_order_policy, clamp_vault_deadline, encode_close_only,
    encode_close_redeem_epoch, encode_closed_redeem_epoch_id, encode_corridor_asset,
    encode_corridor_decimals, encode_epochs, encode_free_corridor, encode_free_settlement,
    encode_last_settled_nav, encode_liquid_settlement, encode_max_order_input_corridor,
    encode_max_order_input_settlement, encode_max_order_lifetime, encode_paused,
    encode_quotable_corridor, encode_quotable_settlement, encode_redemption_epoch_duration,
    encode_settlement_asset, encode_settlement_decimals, encode_trading_epoch,
    quotable_settlement_for_route, trading_nonce, vault_nonce_low, RedeemEpochView,
    VaultQuotePolicy,
};
use crate::time::unix_now_ms;
use iso8601::{format_iso_ms, parse_iso_ms};
use nonce::rfq_nonce;
use order::{build_order, RfqOrderSpec};
use reserve::{Reservations, RESERVATIONS_FILE};
use responder::{
    book_from_pool, decide_quote, levels_for, wallet_tokens, CorridorBook, InventoryView,
};
use session::AuthedSession;
use wire::{
    AttestRejectFrame, AttestRejectReason, AttestRequestFrame, AttestResponseFrame,
    CloseRedeemAckFrame, CloseRedeemRejectFrame, CloseRedeemRejectReason, CloseRedeemRequestFrame,
    MakerFrame, QuoteRejectFrame, QuoteRequestFrame, QuoteResponseFrame, RejectReason, VenueFrame,
};

/// How long a redeem-epoch close may spend waiting for its receipt. Well past
/// the venue's reply budget on purpose — the ack has already gone back, and
/// this is only the window in which a fee bump can still rescue the nonce.
const CLOSE_REDEEM_RECEIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Everything the responder task needs, resolved once at spawn so the hot
/// path never re-reads config or environment.
pub struct RfqRuntime {
    url: String,
    api_key: String,
    maker_id: String,
    /// This bot's name on the venue; `None` keeps the pre-instance behaviour.
    instance_id: Option<String>,
    chain_id: u64,
    permit2: Address,
    reactor: Address,
    validation_contract: Address,
    rpc_url: String,
    /// `[gas]` ceilings for every wallet the responder builds.
    gas_caps: GasCaps,
    /// Gas held out of the wallet's funded figure when a quoted token is the
    /// chain's gas coin.
    gas_reserve: GasReserve,
    books: Vec<CorridorBook>,
    signer: DynSigner,
    /// `rfq-reservations.json` next to stitch.toml. None only when the process
    /// has no config dir (env-only key); then the ledger is memory-only.
    reservations_path: Option<std::path::PathBuf>,
    /// The folder holding stitch.toml, where the bot leaves its latest chain
    /// reads for the panel ([`crate::chain::snapshot`]). None for an env-only
    /// process, which then publishes nothing.
    config_dir: Option<std::path::PathBuf>,
    /// OperatorVault this bot quotes for. `None` is the EOA sign+fund path.
    vault: Option<Address>,
    /// VaultOrderExecutor to list beside the taker on vault orders. Always
    /// `None` without a vault.
    vault_order_executor: Option<Address>,
    /// Live `tradingEpoch()` for vault nonces. Unused when `vault` is None.
    trading_epoch: Arc<RwLock<u64>>,
    /// Per-order caps and lifetime. Unused when `vault` is None.
    vault_policy: Arc<RwLock<Option<VaultQuotePolicy>>>,
    /// Redeem epochs with a close in flight. Process-scoped so reconnecting
    /// the maker stream cannot forget a transaction task that is still alive.
    closing: Arc<Mutex<HashMap<U256, u64>>>,
    modules: Option<crate::modules::runtime::Runtime>,
}

/// How old a wallet reading may be before a `max` side goes dark.
/// Fail closed: a stale or missing reading is no inventory.
const INVENTORY_TTL_SECS: u64 = 3;

/// How often the refresh loop re-reads the wallet.
///
/// Every tick is RPC we pay for and nothing here moves between blocks except
/// when we trade, so the cadence is set by the TTL above rather than by how
/// fresh we could be: one full cycle plus a round trip has to land inside
/// [`INVENTORY_TTL_SECS`] or a side goes dark between refreshes. 2s leaves a
/// second of slack and halves what the loop cost at 1s.
const INVENTORY_REFRESH_SECS: u64 = 2;

// A refresh slower than the TTL takes every `max` side dark between cycles.
// Checked here rather than in a test: the two numbers are only ever changed
// together, and this refuses to build instead of failing later.
const _: () = assert!(INVENTORY_REFRESH_SECS < INVENTORY_TTL_SECS);

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
        instance_id = ?runtime.instance_id,
        corridors = ?runtime.books.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
        "starting RFQ responder"
    );
    Some(tokio::spawn(run(runtime)))
}

/// True when a maker credential is configured for this bot, checking every
/// source [`load_rfq_api_key`] accepts. `env` looks a variable up in the
/// environment the bot will actually start with — the panel passes the bot's
/// own `stitch.env` plus the inherited process env, which is what the process
/// and Docker runtimes hand the child.
///
/// Lives next to `load_rfq_api_key` so the panel's Start guard cannot drift
/// from what the runtime accepts — same order, same standard. In particular a
/// variable that *names* a file is only a credential if that file reads back
/// non-blank: `read_env_secret` errors on an unreadable path and the loader
/// falls straight past it, so a guard that accepted the bare variable would
/// wave through a bot that then can't spawn its responder.
///
/// Only the process runtime consults the environment (see the Start guard), and
/// there the panel and the bot share a filesystem view, so reading the path here
/// answers the same question the child will ask.
pub fn api_key_configured(
    api_key_env: &str,
    config_dir: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> bool {
    let value = |name: &str| {
        env(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    // `read_env_secret` *returns* out of the `_FILE` branch the moment that
    // variable holds a non-blank path — success or failure. So a set `_FILE`
    // is the whole env answer: it never falls back to the raw variable, only
    // onward to the sibling file. Mirror that exactly, or the guard accepts a
    // raw key the loader will never reach.
    if let Some(path) = value(&format!("{api_key_env}_FILE")) {
        // The process runtime gives the child the bot directory as its working
        // directory, so a relative path in `stitch.env` resolves there — not
        // against wherever the panel happens to be running. Resolving it the
        // panel's way would both refuse a key the bot can read and accept a
        // same-named file next to the panel that the bot cannot.
        let path = Path::new(&path);
        let resolved = match (path.is_relative(), config_dir) {
            (true, Some(dir)) => dir.join(path),
            _ => path.to_path_buf(),
        };
        if file_holds_secret(&resolved) {
            return true;
        }
    } else if value(api_key_env).is_some() {
        return true;
    }
    config_dir.is_some_and(|dir| file_holds_secret(&dir.join(crate::setup::RFQ_API_KEY_FILE)))
}

/// A secret file counts only when it reads back non-blank — the same bar
/// `load_rfq_api_key` applies before it will use one.
fn file_holds_secret(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| !s.trim().is_empty())
}

/// Where a generated instance id is remembered, next to stitch.toml.
pub const INSTANCE_ID_FILE: &str = "rfq-instance-id";

/// This bot's name on the venue.
///
/// Order: `[rfq].instance_id`, then `rfq-instance-id` beside the config,
/// generating and persisting one on first use. Persisted rather than fresh per
/// process on purpose — the venue supersedes a session only when the same id
/// reconnects, so a stable id means a restart reclaims its own socket at once,
/// while a fresh one each time would leave the old session to time out and let
/// restarts pile sockets up.
///
/// `None` when there is nowhere to persist to (an env-only deployment) and
/// nothing was configured: the venue then falls back to one session per
/// credential chain, which is the behaviour that predates instance ids. Better
/// that than an id that changes on every restart.
fn resolve_instance_id(configured: Option<&str>, config_dir: Option<&Path>) -> Option<String> {
    if let Some(name) = configured.map(str::trim).filter(|n| !n.is_empty()) {
        return Some(name.to_string());
    }
    let path = config_dir?.join(INSTANCE_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Some(existing);
        }
    }
    let generated = format!("stitch-{:016x}", rand::rngs::OsRng.next_u64());
    match std::fs::write(&path, format!("{generated}\n")) {
        Ok(()) => {
            info!(
                path = %path.display(),
                instance_id = %generated,
                "generated this bot's RFQ instance id"
            );
            Some(generated)
        }
        Err(e) => {
            // Unwritable config dir: an id we cannot remember is worse than
            // none, because it would change on every restart.
            warn!(
                path = %path.display(),
                error = %e,
                "could not persist an RFQ instance id; falling back to one session per chain"
            );
            None
        }
    }
}

/// Resolve the maker API key without ever logging it.
///
/// Order: `{NAME}_FILE` (preferred, same as the wallet), then `{NAME}`, then
/// `rfq-api.key` next to the config. The last is what the panel writes.
pub(crate) fn load_rfq_api_key(
    api_key_env: &str,
    config_dir: Option<&Path>,
) -> anyhow::Result<String> {
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
        .map(|p| book_from_pool(p, &cfg.feed.url, rfq_staleness_secs_for_pool(&cfg.feed, p)))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    anyhow::ensure!(!books.is_empty(), "no pools to quote over RFQ");
    Ok(RfqRuntime {
        url: rfq.url.clone(),
        api_key,
        maker_id: rfq.maker_id.clone(),
        instance_id: resolve_instance_id(rfq.instance_id.as_deref(), config_dir),
        chain_id: cfg.chain_id,
        permit2: cfg.permit2.parse().context("invalid permit2 address")?,
        reactor: cfg.reactor.parse().context("invalid reactor address")?,
        validation_contract: rfq
            .validation_contract
            .parse()
            .context("invalid [rfq].validation_contract")?,
        rpc_url: cfg.rpc_url.clone(),
        gas_caps: cfg.gas_caps(),
        gas_reserve: cfg.gas_reserve(),
        books,
        signer,
        reservations_path: config_dir.map(|dir| dir.join(RESERVATIONS_FILE)),
        config_dir: config_dir.map(Path::to_path_buf),
        vault: cfg
            .vault
            .as_ref()
            .map(|v| v.address.parse().context("invalid [vault].address"))
            .transpose()?,
        vault_order_executor: cfg
            .vault
            .as_ref()
            .and_then(|v| v.order_executor.as_deref())
            .map(|a| a.parse().context("invalid [vault].order_executor"))
            .transpose()?,
        trading_epoch: Arc::new(RwLock::new(0)),
        vault_policy: Arc::new(RwLock::new(None)),
        closing: Arc::new(Mutex::new(HashMap::new())),
        modules: if cfg.modules_enabled && cfg.modules.mode != crate::modules::Mode::Off {
            Some(crate::modules::runtime::Runtime::new(
                cfg.modules.clone(),
                config_dir.context("modules need a persistent config directory")?,
            )?)
        } else {
            None
        },
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

/// The wallet *this* session's orders are attributed to, or `None` when the
/// venue has not said unambiguously.
///
/// The venue only guarantees `(chainId, fundingWallet)` unique, so one maker
/// can hold several slots on a chain and the list is not a lookup by chain
/// alone. The authenticated signer is what picks the slot — it is the key that
/// just proved itself in the handshake — so bind on it. Taking the first slot
/// for the chain instead would refuse a perfectly valid second bot with a
/// mismatch that is really the bot reading someone else's row.
///
/// Falling back to a lone slot keeps the check armed against a venue that
/// predates `signingAddress`. Several slots and none naming this signer is the
/// one case with no answer, and an unarmed check beats a wrong refusal: the
/// venue already vouched for the session, so refusing here would take a
/// working maker off the market over a list the bot cannot read.
fn venue_funding_wallet(accepted: &wire::SessionAcceptedFrame, chain_id: u64) -> Option<Address> {
    let on_chain: Vec<&wire::MakerWalletFrame> = accepted
        .funding_wallets
        .iter()
        .filter(|w| w.chain_id == chain_id)
        .collect();
    if let Ok(signer) = accepted.signing_address.parse::<Address>() {
        let bound = on_chain.iter().find(|w| {
            w.signing_address
                .as_deref()
                .and_then(|s| s.parse::<Address>().ok())
                == Some(signer)
        });
        if let Some(w) = bound {
            return w.funding_wallet.parse().ok();
        }
    }
    match on_chain.as_slice() {
        [only] => only.funding_wallet.parse().ok(),
        _ => None,
    }
}

/// Why this session must not quote, or `None` when it may.
///
/// The venue attributes every order to the funding wallet on the maker record
/// the credential names — not to `[vault].address`. Those are set in different
/// places: the wallet is fixed when the bot enrolls, the vault address is a
/// line in stitch.toml. Point `[vault].address` at a newly deployed vault
/// without re-enrolling and the bot keeps its old maker id, so it reads the
/// new vault's inventory and publishes it as the old vault's — levels carry no
/// vault, so nothing downstream can tell. A `restricted=<new vault>` swap then
/// answers `no_restricted_liquidity` against a bot that looks perfectly
/// healthy, while an unrestricted fill would be signed for a vault whose
/// balance was never the one quoted.
///
/// A vault maker therefore refuses the session outright rather than quoting
/// someone else's inventory. An EOA maker has nothing to compare — its funding
/// wallet is the signing key — and a venue that does not send the field at all
/// leaves the check unarmed rather than breaking the older pairing.
fn vault_session_mismatch(
    accepted: &wire::SessionAcceptedFrame,
    chain_id: u64,
    vault: Option<Address>,
) -> Option<String> {
    let vault = vault?;
    let funding = venue_funding_wallet(accepted, chain_id)?;
    if funding == vault {
        return None;
    }
    Some(format!(
        "maker {} funds from {funding} on chain {chain_id}, but [vault].address is {vault}. \
         Quoting would publish this vault's inventory as {funding}'s. Re-enroll the bot \
         against the vault (Connect in the Stitch panel) and put the maker id it issues in \
         [rfq].maker_id.",
        accepted.maker_id
    ))
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
    if !tokens.is_empty() || rt.vault.is_some() {
        let wallet =
            Wallet::new(&rt.rpc_url, rt.signer.clone(), rt.chain_id).with_gas_caps(rt.gas_caps);
        tokio::spawn(inventory_loop(
            wallet,
            rt.permit2,
            tokens,
            rt.gas_reserve,
            inventory.clone(),
            rt.vault,
            rt.vault_order_executor,
            rt.trading_epoch.clone(),
            rt.vault_policy.clone(),
            rt.closing.clone(),
            rt.modules.clone(),
            rt.config_dir
                .as_deref()
                .map(|dir| Publisher::spawn(dir, rt.chain_id)),
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
    let mut backoff = Backoff::default();
    let mut supersedes: u32 = 0;
    loop {
        match session::connect_and_auth(
            &rt.url,
            &rt.api_key,
            &rt.maker_id,
            rt.instance_id.as_deref(),
            &rt.signer,
        )
        .await
        {
            Ok(authed) => {
                let started = std::time::Instant::now();
                let (err, ledger) = session_loop(
                    &rt,
                    &prices,
                    &inventory,
                    authed,
                    std::mem::take(&mut reservations),
                )
                .await;
                reservations = ledger;
                // Acceptance alone is not health. A duplicate identity is
                // accepted and then closed inside a second, every second, so
                // clearing the backoff here pinned the bot to a 1 Hz retry
                // loop against a venue that will keep closing it.
                let lived = started.elapsed();
                if is_healthy_session(lived) {
                    backoff.reset();
                }
                supersedes = next_supersede_streak(supersedes, lived, &err);
                if session::is_handover(&err) {
                    info!(
                        detail = %format!("{err:#}"),
                        "RFQ venue handed this session over; reconnecting immediately"
                    );
                } else if session::is_superseded(&err) {
                    log_supersede(supersedes, &rt.maker_id, &err);
                } else {
                    warn!(
                        error = %format!("{err:#}"),
                        "RFQ session ended (closed or failed); reconnecting"
                    );
                }
                backoff.note(&err);
            }
            Err(e) => {
                // A failed attempt is an ending too, and has to run through the
                // same transition: a handshake that fails between two short
                // supersedes breaks the streak, exactly as a session that ran
                // would. Zero duration because nothing ever served.
                supersedes = next_supersede_streak(supersedes, std::time::Duration::ZERO, &e);
                if session::is_handover(&e) {
                    debug!(detail = %format!("{e:#}"), "venue redirected us; retrying immediately");
                } else if session::is_superseded(&e) {
                    // Taken over before the session was even accepted — same
                    // duplicate-identity story, so say the same thing.
                    log_supersede(supersedes, &rt.maker_id, &e);
                } else {
                    warn!(error = %format!("{e:#}"), "RFQ connect/auth failed");
                }
                backoff.note(&e);
            }
        }
        if let Some(modules) = &rt.modules {
            modules.quote_status(crate::modules::runtime::QuoteState::WaitingForSession,
                "RFQ session disconnected or authentication incomplete. Check the RFQ connection and bot logs.", unix_now());
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

/// A session must last this long to count as healthy enough to clear the
/// reconnect backoff. The duplicate-identity loop cycles in about a second, and
/// a real session lasts hours, so anything under this is "we are being closed
/// as fast as we connect" rather than "we were serving and something broke".
const HEALTHY_SESSION: std::time::Duration = std::time::Duration::from_secs(10);

/// Consecutive supersedes before this stops looking like a failover and starts
/// looking like a misconfiguration worth an error line.
const SUPERSEDE_ALERT_THRESHOLD: u32 = 3;

fn is_healthy_session(lived: std::time::Duration) -> bool {
    lived >= HEALTHY_SESSION
}

/// Consecutive supersedes of sessions that never got going — the flap streak.
///
/// A session that *ran* before being taken over resets it, because that is a
/// standby failover doing its job, and three of those spread over days is not a
/// flap. Only "accepted, then closed before it could serve" counts, which is the
/// shape a second process quoting as the same identity produces. Any other way
/// of ending (a handover, a dead socket, a silent venue) also clears it: the
/// alert is for the clean-cut duplicate case, not a general error tally.
fn next_supersede_streak(previous: u32, lived: std::time::Duration, err: &anyhow::Error) -> u32 {
    if session::is_superseded(err) && !is_healthy_session(lived) {
        previous.saturating_add(1)
    } else {
        0
    }
}

/// One supersede is the standby handoff. A run of them is two processes sharing
/// one bot identity, which the operator has to fix — nothing the bot retries
/// will resolve it, so say what to look for.
///
/// The advice has to name the thing that actually collides. The venue supersedes
/// on `(maker id, funding wallet, instance id)`; sharing a wallet or a chain is
/// fine and supported, so telling someone to split API keys per chain would not
/// stop a same-chain collision.
fn log_supersede(count: u32, maker_id: &str, err: &anyhow::Error) {
    if count >= SUPERSEDE_ALERT_THRESHOLD {
        error!(
            count,
            maker_id,
            error = %format!("{err:#}"),
            "another process keeps taking this RFQ session over — two bots are using one \
             instance id. Give each its own [rfq].instance_id, or its own config directory \
             so each generates its own rfq-instance-id"
        );
    } else {
        warn!(
            error = %format!("{err:#}"),
            "RFQ session superseded by another session; reconnecting"
        );
    }
}

/// Reconnect pacing, with a fast lane for handovers.
///
/// Two different failures wear the same shape here. A venue that is genuinely
/// down wants exponential backoff, so a fleet of bots doesn't hammer it back
/// into the ground. A venue that is *moving* — a deploy handing sockets from
/// the outgoing task to the warm incoming one — wants no delay at all: the
/// replacement is already accepting, and every second spent sleeping is a
/// second the corridor reports no makers for no reason. Backing off through a
/// deploy is most of what used to make one cost minutes.
///
/// The fast lane is bounded in attempts rather than time, because the thing it
/// is riding out is "the ALB handed me the wrong one of N tasks", which is a
/// couple of retries, not a wait.
struct Backoff {
    secs: u64,
    fast_attempts: u32,
}

/// Immediate retries allowed after a handover before normal backoff resumes.
/// Generous for a two-task overlap; still finite, so a venue stuck refusing
/// every socket can't be spun on forever.
const HANDOVER_FAST_RETRIES: u32 = 20;

/// Pause between fast-lane retries. Long enough not to spin, short enough that
/// a handover is invisible next to a WebSocket handshake.
const HANDOVER_RETRY_MS: u64 = 250;

impl Default for Backoff {
    fn default() -> Self {
        Self {
            secs: 1,
            fast_attempts: 0,
        }
    }
}

impl Backoff {
    /// A session that ran clears both lanes.
    fn reset(&mut self) {
        self.secs = 1;
        self.fast_attempts = 0;
    }

    /// Record why the last attempt ended, opening or closing the fast lane.
    fn note(&mut self, err: &anyhow::Error) {
        if session::is_handover(err) {
            self.fast_attempts = self.fast_attempts.saturating_add(1);
        } else {
            self.fast_attempts = 0;
        }
    }

    /// How long to wait before the next attempt, advancing the backoff.
    fn next_delay(&mut self) -> std::time::Duration {
        if self.fast_attempts > 0 && self.fast_attempts <= HANDOVER_FAST_RETRIES {
            return std::time::Duration::from_millis(HANDOVER_RETRY_MS);
        }
        let delay = std::time::Duration::from_secs(self.secs);
        self.secs = (self.secs * 2).min(30);
        delay
    }
}

/// Venue caps post-auth inbound frames at 40/s. Reconnect replay can
/// deliver many `quoteExpired`s back-to-back. Debounce those into one
/// levels flush after this quiet window so the last release is in the
/// snapshot, and we do not emit one book per expiry.
const LEVELS_REPUBLISH_COALESCE: std::time::Duration = std::time::Duration::from_millis(25);
const LEVELS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// Venue `SESSION_MSGS_PER_SEC` is 40, counted by `admitMessage` on
/// every JSON frame (levels, quoteResponse, quoteReject). A rolling
/// window of those sends, not the last batch, is what we budget against.
const LEVELS_RATE_BUDGET: usize = 40;

#[derive(Clone, Copy, Debug)]
struct OutboundBatch {
    at: tokio::time::Instant,
    frames: usize,
}

#[derive(Clone, Debug, Default)]
struct RateWindow {
    batches: Vec<OutboundBatch>,
}

/// Interval tick publishes only when nothing is waiting on the expiry
/// debounce and we have not done an *expiry* flush in the last second.
/// Ordinary interval sends do not stamp `last` — that would skip the
/// next tick whenever serialize/send takes any time and collapse the
/// book to a ~2s cadence. `None` means no expiry flush yet, so the
/// first tick must emit.
fn should_emit_interval_levels(last: Option<tokio::time::Instant>, trailing_pending: bool) -> bool {
    !trailing_pending && last.map(|t| t.elapsed() >= LEVELS_INTERVAL).unwrap_or(true)
}

fn trailing_due(at: Option<tokio::time::Instant>) -> bool {
    at.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
}

/// Drop corridors that actually went out. A dark sibling must stay
/// pending, but a successful send must not be resent on the next
/// expiry or the pending set grows until a flush blows the 40/s cap.
fn drop_emitted_pending(pending: &mut HashSet<String>, emitted: &[String]) {
    for slug in emitted {
        pending.remove(slug);
    }
}

/// Any nonempty expiry send stamps the interval suppressor. A dark
/// sibling still pending must not look like "no flush happened".
fn recorded_levels_flush(emitted: &[String]) -> Option<tokio::time::Instant> {
    (!emitted.is_empty()).then(tokio::time::Instant::now)
}

/// How long to push the next interval tick after we skip one because
/// an expiry flush was too recent. Without this the interval keeps its
/// old phase and the book goes dark for an extra full period.
fn interval_delay_after_expiry_flush(
    last: Option<tokio::time::Instant>,
) -> Option<std::time::Duration> {
    let remaining = LEVELS_INTERVAL.saturating_sub(last?.elapsed());
    (remaining > std::time::Duration::ZERO).then_some(remaining)
}

fn prune_outbound(window: &mut RateWindow) {
    window.batches.retain(|b| b.at.elapsed() < LEVELS_INTERVAL);
}

fn record_outbound(window: &mut RateWindow, frames: usize) {
    prune_outbound(window);
    if frames > 0 {
        window.batches.push(OutboundBatch {
            at: tokio::time::Instant::now(),
            frames,
        });
    }
}

fn outbound_used(window: &RateWindow) -> usize {
    window
        .batches
        .iter()
        .filter(|b| b.at.elapsed() < LEVELS_INTERVAL)
        .map(|b| b.frames)
        .sum()
}

/// Skip only when this pending send plus every JSON frame still inside
/// the 1s window would trip the 40/s cap. Last-batch snapshots miss
/// earlier expiry flushes and quote replies in the same second.
fn should_skip_trailing_for_rate_budget(window: &RateWindow, pending: usize) -> bool {
    outbound_used(window) + pending > LEVELS_RATE_BUDGET
}

/// When we defer a trailing flush, fire it as soon as enough oldest
/// batches age out for `pending` to fit. Dropping `trailing_at` would
/// leave only the interval tick, which inbound can starve under `biased`.
fn next_trailing_after_budget(window: &RateWindow, pending: usize) -> Option<tokio::time::Instant> {
    if pending == 0 || pending > LEVELS_RATE_BUDGET {
        return None;
    }
    let mut live: Vec<OutboundBatch> = window
        .batches
        .iter()
        .copied()
        .filter(|b| b.at.elapsed() < LEVELS_INTERVAL)
        .collect();
    live.sort_by_key(|b| b.at);
    let mut used: usize = live.iter().map(|b| b.frames).sum();
    if used + pending <= LEVELS_RATE_BUDGET {
        return None;
    }
    for b in live {
        used = used.saturating_sub(b.frames);
        if used + pending <= LEVELS_RATE_BUDGET {
            return Some(b.at + LEVELS_INTERVAL);
        }
    }
    None
}

async fn send_session_frame(
    stream: &mut session::WsStream,
    frame: &MakerFrame,
    outbound: &mut RateWindow,
) -> anyhow::Result<()> {
    stream
        .send(Message::text(serde_json::to_string(frame)?))
        .await
        .context("sending session frame")?;
    record_outbound(outbound, 1);
    Ok(())
}

/// Corridors that would actually go out. Dark / stale feeds are in
/// `pending` but emit nothing, so the rate budget must not count them.
fn ready_level_slugs(
    engine: &mut Engine,
    prices: &PriceCache,
    only: Option<&HashSet<String>>,
) -> Vec<String> {
    engine
        .level_frames(prices, unix_now_ms())
        .into_iter()
        .filter_map(|frame| {
            let MakerFrame::Levels(lvl) = frame else {
                return None;
            };
            if only.is_some_and(|set| !set.contains(&lvl.corridor_id)) {
                return None;
            }
            Some(lvl.corridor_id)
        })
        .collect()
}

/// Sends current books. `only` limits the send to those slugs so an
/// expiry flush does not re-emit every sibling and trip the 40/s cap
/// after a normal interval tick. Returns the corridor ids that went out.
async fn send_level_frames(
    stream: &mut session::WsStream,
    engine: &mut Engine,
    prices: &PriceCache,
    only: Option<&HashSet<String>>,
    outbound: &mut RateWindow,
) -> anyhow::Result<Vec<String>> {
    let frames = engine.level_frames(prices, unix_now_ms());
    let mut emitted = Vec::with_capacity(frames.len());
    for frame in frames {
        let MakerFrame::Levels(lvl) = &frame else {
            continue;
        };
        if only.is_some_and(|set| !set.contains(&lvl.corridor_id)) {
            continue;
        }
        emitted.push(lvl.corridor_id.clone());
        send_session_frame(stream, &frame, outbound).await?;
    }
    Ok(emitted)
}

async fn flush_expired_levels(
    stream: &mut session::WsStream,
    engine: &mut Engine,
    prices: &PriceCache,
    pending: &mut HashSet<String>,
    last_levels_flush: &mut Option<tokio::time::Instant>,
    outbound: &mut RateWindow,
) -> anyhow::Result<Option<tokio::time::Instant>> {
    if pending.is_empty() {
        return Ok(None);
    }
    let ready = ready_level_slugs(engine, prices, Some(pending));
    if let Some(at) = next_trailing_for_dark_pending(pending, &ready) {
        return Ok(Some(at));
    }
    if should_skip_trailing_for_rate_budget(outbound, ready.len()) {
        return Ok(next_trailing_after_budget(outbound, ready.len()));
    }
    let emitted = send_level_frames(stream, engine, prices, Some(pending), outbound).await?;
    drop_emitted_pending(pending, &emitted);
    if let Some(at) = recorded_levels_flush(&emitted) {
        *last_levels_flush = Some(at);
    }
    Ok(None)
}

/// Pending corridors whose feed is still dark keep a trailing retry.
/// Clearing it after a skipped interval tick would miss a feed that
/// recovers inside the venue's 1s republish wait.
fn next_trailing_for_dark_pending(
    pending: &HashSet<String>,
    ready: &[String],
) -> Option<tokio::time::Instant> {
    if pending.is_empty() || !ready.is_empty() {
        return None;
    }
    Some(tokio::time::Instant::now() + LEVELS_REPUBLISH_COALESCE)
}

/// Replay can deliver `quoteExpired` for a quote this process already
/// released. Arm the trailing flush only when a known corridor dropped.
fn trailing_after_quote_expired(
    pending: &mut HashSet<String>,
    slug: Option<String>,
) -> Option<tokio::time::Instant> {
    let slug = slug?;
    pending.insert(slug);
    Some(tokio::time::Instant::now() + LEVELS_REPUBLISH_COALESCE)
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

    if let Some(issue) = vault_session_mismatch(&accepted, rt.chain_id, rt.vault) {
        error!("{issue}");
        return (Err(anyhow::anyhow!("{issue}")), reservations);
    }

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
        configured: rt.books.clone(),
        reservations,
        inventory: inventory.clone(),
        counter: 0,
        chain_id: rt.chain_id,
        permit2: rt.permit2,
        reactor: rt.reactor,
        validation_contract: rt.validation_contract,
        signer: rt.signer.clone(),
        vault: rt.vault,
        vault_order_executor: rt.vault_order_executor,
        trading_epoch: rt.trading_epoch.clone(),
        vault_policy: rt.vault_policy.clone(),
        nonce_salt: rand::random(),
        rpc: Rpc::new(&rt.rpc_url),
        rpc_url: rt.rpc_url.clone(),
        gas_caps: rt.gas_caps,
        closing: rt.closing.clone(),
        modules: rt.modules.clone(),
        pending_rebalance: None,
    };
    // Upgrade path: a ledger written before `input_token` existed loads as
    // tokenless. Stamp every bound book now, while the quoted pool is still
    // here, so a later remove cannot drop that claim from the shared total.
    engine.tag_loaded_books();

    // Venue liveness: it pings on heartbeat_interval; nothing at all for the
    // whole timeout means the link is dead even if TCP hasn't noticed.
    let heartbeat_timeout =
        std::time::Duration::from_millis(accepted.heartbeat_timeout_ms.max(1_000));
    let mut last_rx = tokio::time::Instant::now();
    let mut last_levels_flush: Option<tokio::time::Instant> = None;
    let mut outbound = RateWindow::default();
    let mut trailing_at: Option<tokio::time::Instant> = None;
    let mut pending_republish: HashSet<String> = HashSet::new();
    let mut interval = tokio::time::interval(LEVELS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // The `?`s live in an inner block so the ledger is handed back to the
    // reconnect driver on every exit path.
    let result: anyhow::Result<()> = async {
        loop {
            // Once the debounce is due, flush before reading more inbound.
            // A continuously ready `stream.next()` would otherwise starve
            // the trailing timer under `biased` and the venue's 1s wait
            // would finish on a dropped book.
            if trailing_due(trailing_at) {
                trailing_at = None;
                trailing_at = flush_expired_levels(
                    &mut stream,
                    &mut engine,
                    prices,
                    &mut pending_republish,
                    &mut last_levels_flush,
                    &mut outbound,
                )
                .await?;
                continue;
            }
            tokio::select! {
                // Without `biased;`, Tokio randomizes ready branches.
                // Inbound first so a ready `quoteExpired` beats a ready
                // levels tick. The other way around published
                // reservation-reduced depth after the venue had already
                // dropped the snapshot. A *due* trailing flush is handled
                // above, so inbound only wins until the debounce fires.
                biased;
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
                            let expired_corridor = match &frame {
                                VenueFrame::QuoteExpired(e) => engine
                                    .reservations
                                    .corridor(&e.rfq_id)
                                    .map(str::to_owned),
                                _ => None,
                            };
                            if let Some(reply) = engine.dispatch(frame, prices).await {
                                send_session_frame(&mut stream, &reply, &mut outbound).await?;
                            }
                            if let Some(at) =
                                trailing_after_quote_expired(&mut pending_republish, expired_corridor)
                            {
                                trailing_at = Some(at);
                            }
                        }
                        Message::Ping(payload) => {
                            stream.send(Message::Pong(payload)).await.context("sending pong")?;
                        }
                        Message::Close(reason) => {
                            if session::handover_close(
                                reason.as_ref().map(|f| u16::from(f.code)).unwrap_or(0),
                            ) {
                                info!(?reason, "venue handing this session over; reconnecting now");
                            } else {
                                // The reconnect driver classifies and logs this
                                // close (supersede vs plain failure) with the
                                // code and reason in the error; a warn here just
                                // doubled every line.
                                debug!(?reason, "venue closed the session");
                            }
                            return Err(session::close_error(&reason));
                        }
                        _ => {}
                    }
                }
                _ = async {
                    match trailing_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    trailing_at = None;
                    trailing_at = flush_expired_levels(
                        &mut stream,
                        &mut engine,
                        prices,
                        &mut pending_republish,
                        &mut last_levels_flush,
                        &mut outbound,
                    )
                    .await?;
                }
                _ = interval.tick() => {
                    anyhow::ensure!(
                        last_rx.elapsed() < heartbeat_timeout,
                        "venue silent for {:?} (heartbeat timeout)", last_rx.elapsed()
                    );
                    engine.reservations.prune(unix_now_ms() / 1_000);
                    // The first tick is immediate. After an expiry flush,
                    // emitting every book again in the same second is
                    // 2×books and trips the 40/s cap. Ordinary interval
                    // sends do not stamp last_levels_flush, so the next
                    // tick still fires on cadence.
                    if !should_emit_interval_levels(last_levels_flush, trailing_at.is_some()) {
                        if let Some(delay) =
                            interval_delay_after_expiry_flush(last_levels_flush)
                        {
                            interval.reset_after(delay);
                        }
                        continue;
                    }
                    let emitted =
                        send_level_frames(&mut stream, &mut engine, prices, None, &mut outbound)
                            .await?;
                    drop_emitted_pending(&mut pending_republish, &emitted);
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
    /// Every pool as configured, including the ones the venue did not assign.
    ///
    /// `books` only holds what is quotable this session. A pool the venue
    /// dropped still owns any live claim signed under it, so its slug has to
    /// stay visible to tagging and to shared-token accounting — otherwise that
    /// claim goes missing from the corridors that are still quoting.
    configured: Vec<CorridorBook>,
    reservations: Reservations,
    inventory: InventoryCache,
    counter: u64,
    chain_id: u64,
    permit2: Address,
    reactor: Address,
    validation_contract: Address,
    signer: DynSigner,
    vault: Option<Address>,
    /// Listed beside the taker on every vault order when set — see
    /// `[vault].order_executor`.
    vault_order_executor: Option<Address>,
    trading_epoch: Arc<RwLock<u64>>,
    vault_policy: Arc<RwLock<Option<VaultQuotePolicy>>>,
    /// Per-process namespace for vault nonces. Two bots sharing one vault can
    /// sign in the same millisecond with equal counters; without this the
    /// nonces collide and the venue rejects the second reply `nonce_reserved`.
    nonce_salt: u64,
    /// For the vault reads behind attestation co-signing. Off the quote
    /// path: a co-sign request is one per settled epoch, not per RFQ.
    rpc: Rpc,
    /// Kept alongside `rpc` so a redeem close can build its own [`Wallet`]:
    /// the broadcast outlives the frame that asked for it, and nothing that
    /// sends a transaction may borrow the session loop.
    rpc_url: String,
    /// `[gas]` ceilings for that [`Wallet`].
    gas_caps: GasCaps,
    /// Process-scoped redeem epochs with a close in flight. The venue asks
    /// again every tick until the chain says Closed, and a second close would
    /// be a second nonce spent on a revert.
    closing: Arc<Mutex<HashMap<U256, u64>>>,
    modules: Option<crate::modules::runtime::Runtime>,
    pending_rebalance: Option<modules::PendingRebalance>,
}

impl Engine {
    fn sync_vault_epoch(&mut self) {
        if self.vault.is_none() {
            return;
        }
        let Ok(epoch) = self.trading_epoch.read() else {
            return;
        };
        let epoch = *epoch;
        // `trading_epoch` starts at 0 until the first inventory RPC. Treat
        // that sentinel as "not loaded" so a restart does not clear the
        // on-disk ledger when the real epoch (always ≥ 1) arrives.
        if epoch == 0 {
            return;
        }
        // The ledger persists the epoch its claims were signed under, so a
        // bump that happened while the process was down is caught the same
        // way as a live one.
        self.reservations.sync_vault_epoch(epoch);
    }

    /// Write `input_token` onto tokenless rows that match any book we know.
    ///
    /// Both lists, because they name a pool differently: `books` carries the
    /// venue-assigned slug a quote was signed under, `configured` the
    /// `rfq_corridor` label from the config. A pool the venue no longer assigns
    /// is missing from `books` entirely, and its label is the only handle left
    /// on a claim it still owns.
    fn tag_loaded_books(&mut self) {
        let tags: Vec<(String, String, String)> = self
            .books
            .iter()
            .chain(&self.configured)
            .filter(|b| !b.slug.is_empty())
            .map(|b| {
                (
                    b.slug.clone(),
                    format!("{:#x}", b.debt),
                    format!("{:#x}", b.collateral),
                )
            })
            .collect();
        self.reservations.tag_books(
            tags.iter()
                .map(|(slug, debt, collat)| (slug.as_str(), debt.as_str(), collat.as_str())),
        );
    }

    /// Every slug that names a pool we can attribute a claim to.
    fn known_slugs(&self) -> impl Iterator<Item = &str> {
        self.books
            .iter()
            .chain(&self.configured)
            .map(|b| b.slug.as_str())
            .filter(|s| !s.is_empty())
    }

    /// In-flight claim on the token this side pays. Tagged reservations
    /// count even after their pool is removed; untagged (pre-token) ones
    /// still need a live book whose slug shares this token.
    fn reserved_on(&self, book: &CorridorBook, bid: bool, now_secs: u64) -> U256 {
        let token = if bid { book.debt } else { book.collateral };
        let slugs = self
            .books
            .iter()
            .chain(&self.configured)
            .filter_map(|other| {
                let other_token = if bid { other.debt } else { other.collateral };
                (other_token == token).then_some(other.slug.as_str())
            });
        // A claim we can't pin to any book may have spent this token, and its
        // amount is in units we can't convert — the row's side names a leg of a
        // book we can't identify. Saturating takes every book dark until it is
        // tagged or expires, which is the only honest answer:
        // `available_capacity` subtracts this from the funded balance, so MAX
        // leaves nothing to publish and nothing to sign.
        if self
            .reservations
            .has_unattributable_claim(self.known_slugs(), now_secs)
        {
            return U256::MAX;
        }
        self.reservations
            .reserved_paying(&format!("{token:#x}"), slugs, bid, now_secs)
    }

    /// Levels for every corridor with a fresh feed. A stale/missing feed
    /// publishes nothing — the venue's >5 s gap rule takes the corridor dark,
    /// which is exactly the stale-feed behavior we want.
    fn level_frames(&mut self, prices: &PriceCache, now_ms: u64) -> Vec<MakerFrame> {
        use crate::modules::runtime::QuoteState;
        self.sync_vault_epoch();
        let now_secs = now_ms / 1_000;
        let inventory = self.inventory.view(now_secs);
        let policy = self.vault_policy.read().ok().and_then(|g| *g);
        if self.vault.is_some() && policy.is_none() {
            if let Some(modules) = &self.modules {
                modules.quote_status(
                    QuoteState::WaitingForVault,
                    "Waiting for vault policy reads from RPC",
                    now_secs,
                );
            }
            return Vec::new();
        }
        if self.books.is_empty() {
            if let Some(modules) = &self.modules {
                modules.quote_status(
                    QuoteState::NoCorridor,
                    "The RFQ venue has not accepted a matching corridor for this bot",
                    now_secs,
                );
            }
        }
        self.module_tick(prices, now_secs);
        let order_caps: Vec<(Address, U256)> = policy
            .map(|policy| {
                vec![
                    (policy.settlement, policy.max_input_settlement),
                    (policy.corridor, policy.max_input_corridor),
                ]
            })
            .unwrap_or_default();
        self.books
            .iter()
            .filter_map(|book| {
                if let Some(policy) = policy {
                    if !policy.matches_pair(book.debt, book.collateral) {
                        if let Some(modules) = &self.modules {
                            modules.quote_status(QuoteState::NoCorridor, "The configured tokens do not match this vault", now_secs);
                        }
                        return None;
                    }
                }
                let Some(quote) = prices.get(&book.feed_url) else {
                    if let Some(modules) = &self.modules {
                        modules.quote_status(QuoteState::WaitingForPrice, "Waiting for the first reference price. Check the price feed and bot logs.", now_secs);
                    }
                    return None;
                };
                if is_stale(quote.timestamp, now_secs, book.staleness_secs)
                    || !is_price_usable(quote.price)
                {
                    if let Some(modules) = &self.modules {
                        modules.quote_status(QuoteState::StalePrice, "Reference price is stale, future-dated or invalid. Check the price feed.", now_secs);
                    }
                    return None;
                }
                let effective = self.module_book(book, &quote, now_secs)?;
                Some(MakerFrame::Levels(levels_for(
                    &effective,
                    quote.price,
                    self.reservations.reserved(&book.slug, true, now_secs),
                    self.reservations.reserved(&book.slug, false, now_secs),
                    self.reserved_on(book, true, now_secs),
                    self.reserved_on(book, false, now_secs),
                    format_iso_ms(now_ms),
                    &inventory,
                    &order_caps,
                )))
            })
            .collect()
    }

    /// Handle one venue frame; `Some` is a reply to send.
    async fn dispatch(&mut self, frame: VenueFrame, prices: &PriceCache) -> Option<MakerFrame> {
        match frame {
            VenueFrame::QuoteRequest(req) => Some(self.respond(req, prices).await),
            VenueFrame::AttestRequest(req) => Some(self.cosign(req, prices).await),
            VenueFrame::CloseRedeemRequest(req) => Some(self.close_redeem(req).await),
            VenueFrame::QuoteResult(r) => {
                if r.rfq_id.starts_with("module-rebalance:") {
                    return None;
                }
                // selected stays reserved until quoteExpired or the deadline.
                // Everything else is a signature the taker will never submit:
                // losers are not handed out, and no_quote / invalid / late
                // never produce an executable order.
                match r.result.as_str() {
                    "selected" => {
                        info!(rfq_id = %r.rfq_id, result = %r.result, "quote result");
                    }
                    "no_quote" | "lost_price" | "invalid" | "late" => {
                        if self.reservations.release(&r.rfq_id) {
                            info!(
                                rfq_id = %r.rfq_id,
                                result = %r.result,
                                "quote result released inventory"
                            );
                        } else {
                            debug!(
                                rfq_id = %r.rfq_id,
                                result = %r.result,
                                "quote result; no local reservation"
                            );
                        }
                    }
                    _ => {
                        info!(rfq_id = %r.rfq_id, result = %r.result, "quote result");
                    }
                }
                None
            }
            VenueFrame::QuoteExpired(e) => {
                if e.rfq_id.starts_with("module-rebalance:") {
                    return None;
                }
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
        self.sync_vault_epoch();
        let reject = |reason| {
            MakerFrame::QuoteReject(QuoteRejectFrame {
                rfq_id: req.rfq_id.clone(),
                reason,
            })
        };

        let manual_id = crate::modules::manual::request_id(&req.rfq_id);
        if req.rfq_id.starts_with("module-rebalance:") && manual_id.is_none() {
            return reject(RejectReason::Busy);
        }
        let manual_sale = if let Some(id) = manual_id {
            let Some(modules) = &self.modules else {
                return reject(RejectReason::Busy);
            };
            match crate::modules::manual::read(&modules.dir, id) {
                Ok(sale)
                    if sale.chain_id == self.chain_id
                        && Some(sale.vault) == self.vault
                        && req.taker.parse::<Address>().ok() == Some(sale.taker)
                        && req.sell_token.parse::<Address>().ok()
                            == Some(sale.settlement_token)
                        && req.buy_token.parse::<Address>().ok() == Some(sale.corridor_token)
                        && req.sell_amount.is_none()
                        && req
                            .buy_amount
                            .as_deref()
                            .and_then(|v| v.parse::<U256>().ok())
                            == Some(sale.corridor_amount) =>
                {
                    Some(sale)
                }
                _ => return reject(RejectReason::Busy),
            }
        } else {
            None
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

        let now_ms = unix_now_ms();
        let now_secs = now_ms / 1_000;
        let Some(quote) = prices.get(&book.feed_url) else {
            return reject(RejectReason::StaleFeed);
        };
        // The book's own window, not the runtime's: this is the firm-quote
        // gate, so it must match the one the levels were published under.
        if is_stale(quote.timestamp, now_secs, book.staleness_secs) || !is_price_usable(quote.price)
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
        let mut deadline_secs = max_expires_ms / 1_000;
        if deadline_secs <= now_secs {
            return reject(RejectReason::Busy);
        }
        if self.vault.is_some() {
            let Some(policy) = self.vault_policy.read().ok().and_then(|g| *g) else {
                warn!("vault policy not loaded yet");
                return reject(RejectReason::Busy);
            };
            let Some(clamped) =
                clamp_vault_deadline(now_secs, deadline_secs, policy.max_lifetime_secs)
            else {
                warn!("vault maxOrderLifetime leaves no usable deadline");
                return reject(RejectReason::Busy);
            };
            deadline_secs = clamped;
        }
        if let Some(sale) = &manual_sale {
            let modules = self.modules.as_ref().expect("manual runtime");
            deadline_secs = deadline_secs
                .min(sale.expires_at)
                .min(now_secs + modules.config.rebalance.order_lifetime_secs);
            if deadline_secs <= now_secs {
                return reject(RejectReason::Busy);
            }
        }
        let expires_ms = (now_ms + req.quote_ttl_ms).min(deadline_secs * 1_000);
        let Ok(taker) = req.taker.parse::<Address>() else {
            warn!(raw = %req.taker, "unparseable taker address");
            return reject(RejectReason::Busy);
        };

        let Some(effective_book) = self.module_book(&book, &quote, now_secs) else {
            return reject(RejectReason::Busy);
        };
        let plan = match decide_quote(
            &effective_book,
            &req,
            quote.price,
            self.reservations.reserved(&book.slug, true, now_secs),
            self.reservations.reserved(&book.slug, false, now_secs),
            self.reserved_on(&book, true, now_secs),
            self.reserved_on(&book, false, now_secs),
            &self.inventory.view(now_secs),
        ) {
            Ok(plan) => plan,
            Err(reason) => return reject(reason),
        };
        if let Some(modules) = &self.modules {
            if modules.config.mode == crate::modules::Mode::Live
                && modules.config.inventory.enabled
                && plan.bid
            {
                let Some(context) = self.module_context(&book, &quote, now_secs) else {
                    return reject(RejectReason::Busy);
                };
                if !crate::modules::post_buy_allowed(
                    &modules.config,
                    &context,
                    plan.input,
                    plan.output,
                ) {
                    return reject(RejectReason::Size);
                }
            }
        }
        let plan = if self.vault.is_some() {
            let Some(policy) = self.vault_policy.read().ok().and_then(|g| *g) else {
                return reject(RejectReason::Busy);
            };
            if !policy.matches_pair(plan.input_token, plan.output_token) {
                warn!(
                    input = %plan.input_token,
                    output = %plan.output_token,
                    "vault quote is not the settlement/corridor pair"
                );
                return reject(RejectReason::Busy);
            }
            let cap = policy.max_input_for(plan.input_token);
            if plan.input > cap {
                warn!(
                    input = %plan.input,
                    cap = %cap,
                    "vault quote exceeds maxOrderInput"
                );
                return reject(RejectReason::Busy);
            }
            plan
        } else {
            plan
        };

        if let Some(sale) = &manual_sale {
            let modules = self.modules.as_ref().expect("manual runtime");
            let Some(ctx) = self.module_context(&book, &quote, now_secs) else {
                return reject(RejectReason::Busy);
            };
            if plan.bid
                || sale
                    .allows(&modules.config, &ctx, plan.input, plan.output)
                    .is_err()
            {
                modules.status("Manual sale waiting: check fresh balances, outstanding quotes, size and minimum proceeds");
                return reject(RejectReason::Size);
            }
        }
        let maker = self.vault.unwrap_or_else(|| self.signer.address());
        let nonce = if let Some(sale) = &manual_sale {
            let modules = self.modules.as_ref().expect("manual runtime");
            let epoch = self.trading_epoch.read().ok().map(|g| *g).unwrap_or(0);
            let result = crate::modules::manual::pace(
                &modules.dir,
                &sale.id,
                now_secs,
                modules.config.rebalance.cooldown_secs,
            )
            .and_then(|()| crate::modules::manual::nonce(&modules.dir, &sale.id, epoch));
            match result {
                Ok(nonce) => nonce,
                Err(_) => return reject(RejectReason::Busy),
            }
        } else if self.vault.is_some() {
            let epoch = self.trading_epoch.read().ok().map(|g| *g).unwrap_or(0);
            if epoch == 0 {
                warn!("vault tradingEpoch not loaded yet");
                return reject(RejectReason::Busy);
            }
            trading_nonce(
                epoch,
                vault_nonce_low(self.nonce_salt, now_ms, self.counter),
            )
        } else {
            rfq_nonce(now_ms, self.counter)
        };
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
            order_executor: self.vault_order_executor,
        });
        // Bound the signature by what is left of the reply budget.
        //
        // `replyBy` was checked on the way in, but pricing has taken time since,
        // and on an MPC backend signing is a network round trip rather than a
        // microsecond of local ECDSA. Two things go wrong without a bound. A
        // signature that lands after `replyBy` produces a quote the venue has
        // already stopped listening for — yet the reservation below would still
        // pin that inventory for the whole TTL, against nothing. And because
        // `run_connected` awaits `dispatch` inline, a backend sitting on its own
        // poll timeout would stall the socket for every *other* RFQ and level
        // update too, turning one slow signature into a dead session.
        //
        // Dropping the future cancels our wait, not the provider's work: a
        // remote signer may still finish and bill the request. That is the right
        // trade — the alternative is holding the socket for a signature we can
        // no longer use. Local signing never reaches the timeout.
        let budget_ms = reply_by_ms.saturating_sub(unix_now_ms());
        if budget_ms == 0 {
            warn!(rfq_id = %req.rfq_id, "reply budget spent before signing");
            return reject(RejectReason::Busy);
        }
        let payload = permit2_payload(&order, self.permit2, self.chain_id);
        let signing = tokio::time::timeout(
            std::time::Duration::from_millis(budget_ms),
            self.signer.sign_typed(&payload),
        );
        let signature = match signing.await {
            Ok(Ok(sig)) => sig,
            Ok(Err(e)) => {
                error!(error = %format!("{e:#}"), rfq_id = %req.rfq_id, "signing failed");
                return reject(RejectReason::Busy);
            }
            Err(_) => {
                warn!(
                    budget_ms,
                    rfq_id = %req.rfq_id,
                    "signing did not finish inside the reply budget"
                );
                return reject(RejectReason::Busy);
            }
        };

        if let Some(sale) = &manual_sale {
            let modules = self.modules.as_ref().expect("manual runtime");
            if crate::modules::manual::read(&modules.dir, &sale.id)
                .ok()
                .as_ref()
                != Some(sale)
                || sale.validate(unix_now()).is_err()
                || crate::modules::manual::nonce(
                    &modules.dir,
                    &sale.id,
                    self.trading_epoch.read().ok().map(|g| *g).unwrap_or(0),
                )
                .ok()
                    != Some(nonce)
            {
                return reject(RejectReason::Busy);
            }
            modules.status("Manual sale quoted; awaiting buyer settlement");
        }

        // The reservation starts the moment the signed order exists — even if
        // the send fails, the signature may have left the process.
        self.reservations.reserve_paying(
            req.rfq_id.clone(),
            book.slug.clone(),
            plan.bid,
            plan.input,
            deadline_secs,
            Some(format!("{:#x}", plan.input_token)),
        );

        if self.modules.is_some() && self.reservations.flush().is_err() {
            error!("module-enabled quote withheld: reservation persistence failed");
            return reject(RejectReason::Busy);
        }
        MakerFrame::QuoteResponse(QuoteResponseFrame {
            rfq_id: req.rfq_id,
            sell_amount: plan.sell_amount.to_string(),
            buy_amount: plan.buy_amount.to_string(),
            fee_amount: plan.fee.to_string(),
            expires_at: format_iso_ms(expires_ms),
            encoded_order: alloy_primitives::hex::encode_prefixed(encode_order_bytes(&order)),
            signature: alloy_primitives::hex::encode_prefixed(signature),
            signer: self.signer.address().to_string(),
        })
    }
}

impl Engine {
    /// Close a redeem epoch for the vault this bot signs for.
    ///
    /// `closeRedeemEpoch` gates on `msg.sender`. This key may close whenever
    /// it likes; the venue's keeper is on the permissionless path and waits
    /// `redemptionEpochDuration + valuationTimeout` (audit v0.2 L-02), which
    /// on a five-minute redeem epoch is still a day. So the venue asks.
    ///
    /// The checks below are the bot's own. Closing bumps the trading epoch,
    /// which kills every order signed under it and holds the vault close-only
    /// until settlement — a frame is not enough to spend that on, so the
    /// epoch is read from the chain and has to be an open redeem epoch with
    /// shares in it, past its own duration, with no other close outstanding.
    ///
    /// The ack goes back before the transaction does. A nonce, a gas
    /// estimate and a receipt run far past any reply budget, and this runs
    /// inline in the session loop, so the broadcast moves to its own task and
    /// the venue reads the outcome off the chain on its next tick.
    async fn close_redeem(&self, req: CloseRedeemRequestFrame) -> MakerFrame {
        let reject = |reason| {
            MakerFrame::CloseRedeemReject(CloseRedeemRejectFrame {
                request_id: req.request_id.clone(),
                reason,
            })
        };
        let Some(vault) = self.vault else {
            return reject(CloseRedeemRejectReason::WrongVault);
        };
        let asked = req.vault.parse::<Address>().ok();
        if asked != Some(vault) || req.chain_id != self.chain_id {
            return reject(CloseRedeemRejectReason::WrongVault);
        }
        if !self.signer.can_sign_transactions() {
            warn!(
                signer = %self.signer.address(),
                "signer cannot broadcast a redeem epoch close"
            );
            return reject(CloseRedeemRejectReason::Busy);
        }
        let Ok(epoch_id) = req.epoch_id.parse::<U256>() else {
            warn!(request_id = %req.request_id, raw = %req.epoch_id, "unparseable epoch id");
            return reject(CloseRedeemRejectReason::NotDue);
        };
        // Same rule as the co-sign path: nothing here may outlive the venue's
        // deadline, or a slow RPC holds quotes and heartbeats hostage.
        let Some(reply_by_ms) = parse_iso_ms(&req.reply_by) else {
            warn!(request_id = %req.request_id, raw = %req.reply_by, "unparseable replyBy");
            return reject(CloseRedeemRejectReason::Busy);
        };
        let Some(budget) = remaining_reply_budget(reply_by_ms, unix_now_ms()) else {
            return reject(CloseRedeemRejectReason::Busy);
        };

        let calls = [
            Call::new(vault, encode_epochs(epoch_id)),
            Call::new(vault, encode_redemption_epoch_duration()),
            Call::new(vault, encode_closed_redeem_epoch_id()),
            Call::new(vault, encode_trading_epoch()),
        ];
        let reader = Batcher::sequential();
        let words = match tokio::time::timeout(budget, reader.read(&self.rpc, &calls)).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                warn!(request_id = %req.request_id, error = %e, "vault read failed; not closing");
                return reject(CloseRedeemRejectReason::Busy);
            }
            Err(_) => {
                warn!(request_id = %req.request_id, "vault read ran past replyBy; not closing");
                return reject(CloseRedeemRejectReason::Busy);
            }
        };
        let word = |i: usize| words.get(i).and_then(|o| o.as_ref());
        let (Some(raw_epoch), Some(duration), Some(outstanding), Some(trading_epoch)) = (
            word(0),
            word(1).map(decode_uint),
            word(2).map(decode_uint),
            word(3).map(decode_uint).map(|epoch| epoch.to::<u64>()),
        ) else {
            return reject(CloseRedeemRejectReason::Busy);
        };
        let Some(view) = RedeemEpochView::decode(raw_epoch.as_ref()) else {
            warn!(request_id = %req.request_id, "vault does not speak the epoch ABI");
            return reject(CloseRedeemRejectReason::Busy);
        };
        // One closed redeem epoch at a time: the contract reverts with
        // RedeemEpochOutstanding, so there is nothing to spend a nonce on.
        if outstanding != U256::ZERO || !view.closable_now(duration.saturating_to(), unix_now()) {
            release_close_claim(&self.closing, epoch_id);
            info!(
                request_id = %req.request_id,
                epoch = %epoch_id,
                state = view.state,
                units = %view.units,
                outstanding = %outstanding,
                "refused to close redeem epoch"
            );
            return reject(CloseRedeemRejectReason::NotDue);
        }

        // Gas is the operator's, and a close with none is a broadcast that
        // fails after the venue has been told it is handled.
        let Some(budget) = remaining_reply_budget(reply_by_ms, unix_now_ms()) else {
            return reject(CloseRedeemRejectReason::Busy);
        };
        match tokio::time::timeout(budget, self.rpc.get_balance(self.signer.address())).await {
            Ok(Ok(balance)) if balance == U256::ZERO => {
                warn!(
                    signer = %self.signer.address(),
                    "no native balance to close a redeem epoch"
                );
                return reject(CloseRedeemRejectReason::Unfunded);
            }
            Ok(Ok(_)) => {}
            // A balance we could not read is not a balance of zero.
            Ok(Err(e)) => {
                debug!(error = %format!("{e:#}"), "gas balance read failed; closing anyway");
            }
            Err(_) => {
                warn!("gas balance read ran past replyBy; not closing");
                return reject(CloseRedeemRejectReason::Busy);
            }
        }

        // Do not acknowledge after the venue's timer has already discarded
        // the request, even if the balance future completed on the boundary.
        if remaining_reply_budget(reply_by_ms, unix_now_ms()).is_none() {
            return reject(CloseRedeemRejectReason::Busy);
        }

        if !self.claim_close(epoch_id, trading_epoch) {
            return reject(CloseRedeemRejectReason::InFlight);
        }

        let wallet = Wallet::new(&self.rpc_url, self.signer.clone(), self.chain_id)
            .with_gas_caps(self.gas_caps);
        let closing = self.closing.clone();
        let request_id = req.request_id.clone();
        tokio::spawn(async move {
            let data = Bytes::from(encode_close_redeem_epoch(epoch_id));
            let sent = wallet
                .send_and_wait(vault, data, U256::ZERO, CLOSE_REDEEM_RECEIPT_TIMEOUT)
                .await;
            let release = match sent {
                Ok(_) => {
                    info!(request_id = %request_id, epoch = %epoch_id, "closed redeem epoch");
                    true
                }
                Err(e) => {
                    let may_land = transaction_may_still_land(&e);
                    warn!(
                    request_id = %request_id,
                    epoch = %epoch_id,
                    claim_held = may_land,
                    error = %format!("{e:#}"),
                    "redeem epoch close failed"
                    );
                    !may_land
                }
            };
            if release {
                release_close_claim(&closing, epoch_id);
            }
        });

        MakerFrame::CloseRedeemAck(CloseRedeemAckFrame {
            request_id: req.request_id,
        })
    }

    /// Take the close for `epoch_id`, or `false` if this bot already has one
    /// in flight. A poisoned lock counts as held: a close nobody is tracking
    /// is worse than one that waits for the next ask.
    fn claim_close(&self, epoch_id: U256, trading_epoch: u64) -> bool {
        let claimed = self
            .closing
            .lock()
            .map(|mut held| held.insert(epoch_id, trading_epoch).is_none())
            .unwrap_or(false);
        if claimed {
            // Closing bumps tradingEpoch and makes the vault close-only. Stop
            // publishing and signing against the pre-close snapshot now; the
            // inventory loop keeps this dark while the claim is held, then a
            // post-close refresh installs the new epoch and policy.
            if let Ok(mut policy) = self.vault_policy.write() {
                *policy = None;
            }
        }
        claimed
    }

    /// Countersign a NAV attestation for the vault this bot signs for.
    ///
    /// The bot's key is one of the two the vault needs to settle an epoch, so
    /// this is a signature over value, not a formality: the figures are
    /// checked against the bot's own read of the vault and the price against
    /// its own feed before anything is signed. See `protocol::attest`.
    async fn cosign(&self, req: AttestRequestFrame, prices: &PriceCache) -> MakerFrame {
        let reject = |reason| {
            MakerFrame::AttestReject(AttestRejectFrame {
                request_id: req.request_id.clone(),
                reason,
            })
        };
        let Some(vault) = self.vault else {
            return reject(AttestRejectReason::WrongVault);
        };
        let asked = req.vault.parse::<Address>().ok();
        if asked != Some(vault) || req.chain_id != self.chain_id {
            return reject(AttestRejectReason::WrongVault);
        }
        let att = match NavAttestation::parse(&req.attestation) {
            Ok(att) => att,
            Err(e) => {
                warn!(request_id = %req.request_id, error = %e, "unparseable attestation");
                return reject(AttestRejectReason::Figures);
            }
        };
        if att.vault != vault
            || att.chain_id != U256::from(self.chain_id)
            || att.epoch_id.to_string() != req.epoch_id
        {
            return reject(AttestRejectReason::Figures);
        }
        // The venue stops waiting at replyBy, and this runs inline in the
        // session loop, so nothing below may outlive it: a stalled RPC or a
        // slow MPC signer would otherwise hold quotes and heartbeats hostage.
        let Some(reply_by_ms) = parse_iso_ms(&req.reply_by) else {
            warn!(request_id = %req.request_id, raw = %req.reply_by, "unparseable replyBy");
            return reject(AttestRejectReason::Busy);
        };
        let budget = || std::time::Duration::from_millis(reply_by_ms.saturating_sub(unix_now_ms()));
        if budget().is_zero() {
            return reject(AttestRejectReason::Busy);
        }
        // Everything below checks the figures at this moment. A long or
        // future-dated window would let the risk key use this signature long
        // after that, so it is refused before any read.
        if let Err(reason) = check_window(&att, unix_now()) {
            info!(
                request_id = %req.request_id,
                valid_after = %att.valid_after,
                valid_until = %att.valid_until,
                "attestation window refused; not co-signing"
            );
            return reject(reason);
        }

        // Our own mark for the corridor leg: the book on exactly the vault's
        // pair, oriented the way the attestation prices it (settlement per
        // corridor is debt per collateral). From every configured pool, not
        // just the ones the venue seats this session on: the feed loops run
        // for all of them, and a vault whose corridor the venue unassigned
        // still has epochs to settle.
        let policy = self.vault_policy.read().ok().and_then(|p| *p);
        let Some(policy) = policy else {
            return reject(AttestRejectReason::Busy);
        };
        let Some(book) = self
            .configured
            .iter()
            .find(|b| b.debt == policy.settlement && b.collateral == policy.corridor)
        else {
            return reject(AttestRejectReason::StaleFeed);
        };
        let Some(quote) = prices.get(&book.feed_url) else {
            return reject(AttestRejectReason::StaleFeed);
        };
        if is_stale(quote.timestamp, unix_now(), book.staleness_secs)
            || !is_price_usable(quote.price)
        {
            return reject(AttestRejectReason::StaleFeed);
        }

        let calls = [
            Call::new(vault, encode_free_settlement()),
            Call::new(vault, encode_free_corridor()),
            Call::new(vault, encode_last_settled_nav()),
            Call::new(vault, encode_settlement_decimals()),
            Call::new(vault, encode_corridor_decimals()),
            Call::new(vault, encode_epochs(att.epoch_id)),
        ];
        let reader = Batcher::sequential();
        let read = tokio::time::timeout(budget(), reader.read(&self.rpc, &calls));
        let words = match read.await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                warn!(request_id = %req.request_id, error = %e, "vault read failed; not co-signing");
                return reject(AttestRejectReason::Busy);
            }
            Err(_) => {
                warn!(request_id = %req.request_id, "vault read ran past replyBy; not co-signing");
                return reject(AttestRejectReason::Busy);
            }
        };
        let word = |i: usize| words.get(i).and_then(|o| o.as_ref()).map(decode_uint);
        let (
            Some(free_settlement),
            Some(free_corridor),
            Some(last_settled_nav),
            Some(sd),
            Some(cd),
        ) = (word(0), word(1), word(2), word(3), word(4))
        else {
            return reject(AttestRejectReason::Busy);
        };
        let (Ok(settlement_decimals), Ok(corridor_decimals)) = (u8::try_from(sd), u8::try_from(cd))
        else {
            return reject(AttestRejectReason::Busy);
        };
        // Warp only attests closed epochs; the strategy key holds itself to the
        // same rule rather than signing for an epoch that is still taking requests.
        let Some(epoch) = words
            .get(5)
            .and_then(|o| o.as_ref())
            .and_then(|raw| RedeemEpochView::decode(raw.as_ref()))
        else {
            return reject(AttestRejectReason::Busy);
        };
        if !epoch.is_closed() {
            info!(request_id = %req.request_id, epoch = %req.epoch_id, state = epoch.state, "epoch not closed; not co-signing");
            return reject(AttestRejectReason::NotClosed);
        }
        let live = LiveVault {
            free_settlement,
            free_corridor,
            last_settled_nav,
            settlement_decimals,
            corridor_decimals,
        };
        if let Err(reason) = check_attestation(&att, &live, price_wad(quote.price)) {
            info!(
                request_id = %req.request_id,
                epoch = %req.epoch_id,
                ?reason,
                version = att.version(),
                nav = %att.nav_or_derived(&live),
                attested_price = %att.corridor_asset_price,
                own_price = quote.price,
                "refused to co-sign attestation"
            );
            return reject(reason);
        }

        // Dropping the future cancels our wait, not the provider's work, same
        // as the quote path: better a refused co-sign than a socket held for a
        // signature the venue has already stopped waiting for.
        let payload = nav_attestation_payload(&att);
        let signing = tokio::time::timeout(budget(), self.signer.sign_typed(&payload));
        let signature = match signing.await {
            Ok(Ok(sig)) => sig,
            Ok(Err(e)) => {
                warn!(request_id = %req.request_id, error = %e, "signer failed on attestation");
                return reject(AttestRejectReason::Busy);
            }
            Err(_) => {
                warn!(request_id = %req.request_id, "signer ran past replyBy; not co-signing");
                return reject(AttestRejectReason::Busy);
            }
        };
        info!(
            request_id = %req.request_id,
            epoch = %req.epoch_id,
            version = att.version(),
            nav = %att.nav_or_derived(&live),
            "co-signed attestation"
        );
        MakerFrame::AttestResponse(AttestResponseFrame {
            request_id: req.request_id,
            signature: alloy_primitives::hex::encode_prefixed(signature),
            signer: self.signer.address().to_string(),
        })
    }
}

fn release_close_claim(closing: &Mutex<HashMap<U256, u64>>, epoch_id: U256) {
    if let Ok(mut held) = closing.lock() {
        held.remove(&epoch_id);
    }
}

/// Drop ambiguous close claims once the chain's trading epoch proves their
/// transaction landed. Returns whether any close is still unresolved.
fn reconcile_close_claims(closing: &Mutex<HashMap<U256, u64>>, trading_epoch: u64) -> bool {
    closing
        .lock()
        .map(|mut held| {
            held.retain(|_, submitted_at| *submitted_at == trading_epoch);
            !held.is_empty()
        })
        .unwrap_or(true)
}

fn remaining_reply_budget(reply_by_ms: u64, now_ms: u64) -> Option<std::time::Duration> {
    let remaining_ms = reply_by_ms.saturating_sub(now_ms);
    (remaining_ms > 0).then(|| std::time::Duration::from_millis(remaining_ms))
}

fn refreshed_vault_policy(
    close_in_flight: bool,
    policy: VaultQuotePolicy,
) -> Option<VaultQuotePolicy> {
    (!close_in_flight).then_some(policy)
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

/// What RFQ may pledge: `min(ERC20 balance, Permit2 allowance)`. Allowance is
/// the fill-time constraint — a balance the wallet hasn't approved is not
/// quotable. The live ladder is deliberately not subtracted: on a dual-run
/// corridor both channels quote the whole wallet, and the venue's
/// `reserveReply` counts firm quotes against firm quotes only. In-flight RFQ
/// quotes are still netted off, in [`reserve`].
///
/// Two reads per token, every token in one batch. A bot seated on five
/// corridors holds six tokens, which was twelve round trips a second before
/// this was a batch.
fn funded_calls(owner: Address, permit2: Address, tokens: &[Address]) -> Vec<Call> {
    tokens
        .iter()
        .flat_map(|token| {
            [
                Call::new(*token, encode_balance_of(owner)),
                Call::new(*token, encode_allowance(owner, permit2)),
            ]
        })
        .collect()
}

/// `min(balance, allowance)` per token, in `tokens` order, with the gas
/// reserve taken off the balance of the chain's gas coin (see
/// [`GasReserve::funded`]). A token whose pair of reads did not both come back
/// is `None`: it keeps its last reading until the TTL drops it, rather than
/// reading as zero and taking the side dark on one reverting token.
fn decode_funded(
    tokens: &[Address],
    results: &[Option<Bytes>],
    gas_reserve: &GasReserve,
) -> Vec<(Address, Option<U256>)> {
    tokens
        .iter()
        .enumerate()
        .map(|(i, token)| {
            let funded = match (results.get(i * 2), results.get(i * 2 + 1)) {
                (Some(Some(balance)), Some(Some(allowance))) => {
                    Some(gas_reserve.funded(*token, decode_uint(balance), decode_uint(allowance)))
                }
                _ => None,
            };
            (*token, funded)
        })
        .collect()
}

/// Refresh quotable amounts for every `max` token on a timer. A failed read
/// leaves the previous value in place; the TTL then fails the side closed
/// instead of quoting a stale high balance forever.
///
/// Every cycle is one batched request where it used to be one per view (see
/// [`crate::chain::multicall`]). The batcher is probed once and then reused;
/// a probe that cannot reach the node reads one call at a time for that cycle
/// and tries again on the next, rather than pinning the process to the
/// expensive path because the node blipped at startup.
///
/// A vault running modules also needs the vault's free balances. Where
/// Multicall3 exists those two views ride in this same batch, so modules cost
/// no request of their own; `allowFailure` keeps a reverting module view from
/// taking quote inventory dark. Without Multicall3 they would be two more
/// sequential calls in front of every refresh, so that chain gets the separate
/// [`modules::balance_loop`] instead.
#[allow(clippy::too_many_arguments)]
async fn inventory_loop(
    wallet: Wallet,
    permit2: Address,
    tokens: Vec<Address>,
    gas_reserve: GasReserve,
    cache: InventoryCache,
    vault: Option<Address>,
    vault_order_executor: Option<Address>,
    trading_epoch: Arc<RwLock<u64>>,
    vault_policy: Arc<RwLock<Option<VaultQuotePolicy>>>,
    closing: Arc<Mutex<HashMap<U256, u64>>>,
    modules: Option<crate::modules::runtime::Runtime>,
    snapshot: Option<Publisher>,
) {
    let mut vault_pair: Option<(Address, Address, Address, u64)> = None;
    let mut batcher: Option<Batcher> = None;
    loop {
        if batcher.is_none() {
            match Batcher::detect(wallet.rpc()).await {
                Ok(resolved) => {
                    info!(
                        batched = resolved.is_batched(),
                        "rfq inventory reads resolved"
                    );
                    if let (false, Some(address), Some(modules)) =
                        (resolved.is_batched(), vault, modules.clone())
                    {
                        tokio::spawn(modules::balance_loop(
                            wallet.rpc().clone(),
                            address,
                            modules,
                        ));
                    }
                    batcher = Some(resolved);
                }
                Err(e) => debug!(
                    error = %format!("{e:#}"),
                    "could not probe for Multicall3; reading one call at a time this cycle"
                ),
            }
        }
        let reader = batcher.unwrap_or_else(Batcher::sequential);

        if vault_pair.is_none() {
            if let Some(address) = vault {
                match read_vault_assets(wallet.rpc(), reader, address).await {
                    Ok(pair) => vault_pair = Some((address, pair.0, pair.1, pair.2)),
                    Err(e) => warn!(
                        error = %format!("{e:#}"),
                        "vault asset read failed; RFQ inventory stays dark"
                    ),
                }
            }
        }
        if let Some((address, settlement, corridor, max_lifetime)) = vault_pair {
            let module_balances = modules.as_ref().filter(|_| reader.is_batched());
            let also = snapshot_calls(
                snapshot.as_ref(),
                reader,
                wallet.address(),
                &[settlement, corridor],
            );
            match read_vault_inventory(
                wallet.rpc(),
                reader,
                permit2,
                address,
                vault_order_executor,
                settlement,
                corridor,
                module_balances.is_some(),
                also,
            )
            .await
            {
                Ok(VaultReading {
                    inventory: (settlement_qty, corridor_qty, epoch, max_settlement, max_corridor),
                    free,
                    recorded,
                }) => {
                    // Stamp after the RPC batch. A pre-read clock can already
                    // exceed INVENTORY_TTL_SECS on a slow endpoint, which would
                    // keep both sides dark forever.
                    let now = unix_now();
                    cache.set(settlement, settlement_qty, now);
                    cache.set(corridor, corridor_qty, now);
                    if let Some(snapshot) = &snapshot {
                        snapshot.publish(now, recorded);
                    }
                    if let (Some(modules), Some((free_settlement, free_corridor))) =
                        (module_balances, free)
                    {
                        if let Ok(mut slot) = modules.balances.write() {
                            *slot = Some(crate::modules::runtime::Balances {
                                settlement: free_settlement,
                                corridor: free_corridor,
                                at: now,
                            });
                        }
                    }
                    if let Ok(mut slot) = trading_epoch.write() {
                        *slot = epoch;
                    }
                    let close_in_flight = reconcile_close_claims(&closing, epoch);
                    if let Ok(mut slot) = vault_policy.write() {
                        *slot = refreshed_vault_policy(
                            close_in_flight,
                            VaultQuotePolicy {
                                settlement,
                                corridor,
                                max_input_settlement: max_settlement,
                                max_input_corridor: max_corridor,
                                max_lifetime_secs: max_lifetime,
                            },
                        );
                    }
                }
                Err(e) => warn!(
                    error = %format!("{e:#}"),
                    "vault inventory refresh failed; last reading kept until TTL"
                ),
            }
        } else if vault.is_none() {
            // Only the wallet path holds gas back: a vault's capital never
            // pays for the bot's transactions, the signer wallet does.
            let calls = [
                funded_calls(wallet.address(), permit2, &tokens),
                snapshot_calls(snapshot.as_ref(), reader, wallet.address(), &[]),
            ]
            .concat();
            match reader.read(wallet.rpc(), &calls).await {
                Ok(results) => {
                    let now = unix_now();
                    if let Some(snapshot) = &snapshot {
                        snapshot.publish(now, record(&calls, &results));
                    }
                    for (token, funded) in decode_funded(&tokens, &results, &gas_reserve) {
                        match funded {
                            Some(funded) => cache.set(token, funded, now),
                            // One token reverting must not cost the others
                            // their refresh, which is why the batch allows
                            // failures instead of failing whole.
                            None => warn!(
                                token = %token,
                                "rfq inventory read failed for this token; last reading kept until TTL"
                            ),
                        }
                    }
                }
                Err(e) => warn!(
                    error = %format!("{e:#}"),
                    "rfq inventory refresh failed; last reading kept until TTL"
                ),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(INVENTORY_REFRESH_SECS)).await;
    }
}

/// The vault's two assets and its order-lifetime cap — fixed at deploy, so
/// this is read once and kept.
fn vault_asset_calls(vault: Address) -> Vec<Call> {
    vec![
        Call::new(vault, encode_settlement_asset()),
        Call::new(vault, encode_corridor_asset()),
        Call::new(vault, encode_max_order_lifetime()),
    ]
}

/// Every answer is required: without all three there is no pair to quote, so
/// a missing one is an error rather than a zero.
fn decode_vault_assets(results: &[Option<Bytes>]) -> anyhow::Result<(Address, Address, u64)> {
    let word = |i: usize, what: &str| -> anyhow::Result<U256> {
        results
            .get(i)
            .and_then(|r| r.as_ref())
            .map(decode_uint)
            .ok_or_else(|| anyhow::anyhow!("reading vault {what}"))
    };
    let settlement = address_from_word(word(0, "settlementAsset")?);
    let corridor = address_from_word(word(1, "corridorAsset")?);
    anyhow::ensure!(
        !settlement.is_zero() && !corridor.is_zero(),
        "vault assets unset"
    );
    let max_lifetime = word(2, "maxOrderLifetime")?.to::<u64>();
    anyhow::ensure!(max_lifetime > 0, "vault maxOrderLifetime is zero");
    Ok((settlement, corridor, max_lifetime))
}

async fn read_vault_assets(
    rpc: &Rpc,
    reader: Batcher,
    vault: Address,
) -> anyhow::Result<(Address, Address, u64)> {
    let results = reader.read(rpc, &vault_asset_calls(vault)).await?;
    decode_vault_assets(&results)
}

/// The ten views a vault maker quotes off, in one batch. `settlement` and
/// `corridor` are the vault's own assets, already resolved — the two
/// allowances are the vault's Permit2 approvals on them.
fn vault_inventory_calls(
    permit2: Address,
    vault: Address,
    settlement: Address,
    corridor: Address,
) -> Vec<Call> {
    vec![
        Call::new(vault, encode_quotable_settlement()),
        Call::new(vault, encode_liquid_settlement()),
        Call::new(vault, encode_quotable_corridor()),
        Call::new(vault, encode_max_order_input_settlement()),
        Call::new(vault, encode_max_order_input_corridor()),
        Call::new(vault, encode_close_only()),
        Call::new(vault, encode_paused()),
        Call::new(vault, encode_trading_epoch()),
        Call::new(settlement, encode_allowance(vault, permit2)),
        Call::new(corridor, encode_allowance(vault, permit2)),
    ]
}

/// `(quotable settlement, quotable corridor, epoch, per-order caps)`.
///
/// Quotable prices liquid + yield-adapter holdings. validateEnvelope admits
/// settlement input only up to min(quotable, liquid) *at fill time*: with an
/// executor listed on the order, `VaultOrderExecutor.fill` recalls from the
/// adapter first, so the whole position is fillable and publishing only the
/// liquid part would hide a staked vault's inventory. Without one the fill is
/// a direct reactor call that cannot recall, so publishing the larger number
/// signs sizes the vault will reject.
///
/// Every view is required. A partial answer cannot be applied — quoting
/// without `paused` or `closeOnly` is quoting through a stop — so a missing
/// one fails the refresh and the TTL takes the vault dark.
fn decode_vault_inventory(
    results: &[Option<Bytes>],
    executor_routed: bool,
) -> anyhow::Result<(U256, U256, u64, U256, U256)> {
    const VIEWS: [&str; 10] = [
        "quotableSettlement",
        "liquidSettlement",
        "quotableCorridor",
        "maxOrderInputSettlement",
        "maxOrderInputCorridor",
        "closeOnly",
        "paused",
        "tradingEpoch",
        "settlement Permit2 allowance",
        "corridor Permit2 allowance",
    ];
    let word = |i: usize| -> anyhow::Result<U256> {
        results
            .get(i)
            .and_then(|r| r.as_ref())
            .map(decode_uint)
            .ok_or_else(|| anyhow::anyhow!("reading vault {}", VIEWS[i]))
    };
    let settlement_qty = quotable_settlement_for_route(word(0)?, word(1)?, executor_routed);
    let corridor_qty = word(2)?;
    let max_settlement = word(3)?;
    let max_corridor = word(4)?;
    let close_only = !word(5)?.is_zero();
    let paused = !word(6)?.is_zero();
    let epoch = word(7)?.to::<u64>();
    let settlement_allowance = word(8)?;
    let corridor_allowance = word(9)?;
    let (settlement_qty, corridor_qty) =
        apply_vault_order_policy(settlement_qty, corridor_qty, close_only, paused);
    Ok((
        settlement_qty.min(settlement_allowance),
        corridor_qty.min(corridor_allowance),
        epoch,
        max_settlement,
        max_corridor,
    ))
}

/// The vault's free balances for modules, appended after the ten quote views.
fn module_balance_calls(vault: Address) -> Vec<Call> {
    vec![
        Call::new(vault, encode_free_settlement()),
        Call::new(vault, encode_free_corridor()),
    ]
}

/// `(freeSettlement, freeCorridor)` off the tail of the batch, or `None` when
/// either view failed. Never an error: modules wait on a missing reading, and
/// quote inventory must not.
fn decode_module_balances(tail: &[Option<Bytes>]) -> Option<(U256, U256)> {
    match tail {
        [Some(settlement), Some(corridor)] => {
            Some((decode_uint(settlement), decode_uint(corridor)))
        }
        _ => None,
    }
}

/// Reads that only the panel uses, riding the batch the bot sends anyway: the
/// signer's gas balance and its balance of each of `tokens`. Nothing without
/// a snapshot to publish, and nothing on a chain without Multicall3, where
/// each would be a request of its own.
fn snapshot_calls(
    snapshot: Option<&Publisher>,
    reader: Batcher,
    owner: Address,
    tokens: &[Address],
) -> Vec<Call> {
    if snapshot.is_none() || !reader.is_batched() {
        return Vec::new();
    }
    std::iter::once(Call::new(
        CANONICAL_MULTICALL3,
        encode_get_eth_balance(owner),
    ))
    .chain(
        tokens
            .iter()
            .map(|token| Call::new(*token, encode_balance_of(owner))),
    )
    .collect()
}

/// One vault refresh: what to quote, the module balances when asked for, and
/// every read that answered, for the panel.
struct VaultReading {
    inventory: (U256, U256, u64, U256, U256),
    free: Option<(U256, U256)>,
    recorded: Vec<RecordedRead>,
}

/// The quote views, then the module balances when `with_module_balances`,
/// then `also` — in one batch either way.
#[allow(clippy::too_many_arguments)]
async fn read_vault_inventory(
    rpc: &Rpc,
    reader: Batcher,
    permit2: Address,
    vault: Address,
    order_executor: Option<Address>,
    settlement: Address,
    corridor: Address,
    with_module_balances: bool,
    also: Vec<Call>,
) -> anyhow::Result<VaultReading> {
    let quote_calls = vault_inventory_calls(permit2, vault, settlement, corridor);
    let quote_views = quote_calls.len();
    let module_calls = match with_module_balances {
        true => module_balance_calls(vault),
        false => Vec::new(),
    };
    let module_views = module_calls.len();
    let calls = [quote_calls, module_calls, also].concat();
    let results = reader.read(rpc, &calls).await?;
    let (quote, tail) = results.split_at(quote_views.min(results.len()));
    let inventory = decode_vault_inventory(quote, order_executor.is_some())?;
    let free = with_module_balances
        .then(|| decode_module_balances(&tail[..module_views.min(tail.len())]))
        .flatten();
    Ok(VaultReading {
        inventory,
        free,
        recorded: record(&calls, &results),
    })
}

#[cfg(test)]
mod tests {
    use super::wire::{QuoteExpiredFrame, QuoteResultFrame};
    use super::*;
    use crate::config::RfqCapacity;
    use crate::pricing::quote::Spread;
    use crate::signer::{recover_address, LocalSigner};
    use crate::time::unix_now;
    use alloy_primitives::{address, U256};
    use k256::ecdsa::SigningKey;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    const COLLATERAL: &str = "0x0000000000000000000000000000000000000001";
    const DEBT: &str = "0x0000000000000000000000000000000000000002";

    // --- inventory reads -------------------------------------------------
    //
    // These used to be one `eth_call` per view on a one-second loop. They are
    // one batch now, so what is worth pinning is that the batch is decoded
    // into the same numbers, in the right order, and that a hole in it is
    // never read as a zero — a zero here is "quote nothing", and a wrong zero
    // would take a funded maker off the market.

    fn word(v: u64) -> Option<Bytes> {
        Some(Bytes::from(U256::from(v).to_be_bytes::<32>().to_vec()))
    }

    #[test]
    fn funded_calls_are_a_balance_and_an_allowance_per_token_in_order() {
        let owner = address!("0000000000000000000000000000000000000009");
        let permit2 = address!("000000000000000000000000000000000000000a");
        let tokens = [COLLATERAL.parse().unwrap(), DEBT.parse().unwrap()];
        let calls = funded_calls(owner, permit2, &tokens);
        assert_eq!(calls.len(), 4, "two reads per token, batched");
        assert_eq!(calls[0].target, tokens[0]);
        assert_eq!(calls[1].target, tokens[0]);
        assert_eq!(calls[2].target, tokens[1]);
        assert_eq!(calls[0].data, encode_balance_of(owner));
        assert_eq!(calls[1].data, encode_allowance(owner, permit2));
    }

    #[test]
    fn funded_is_the_lesser_of_balance_and_allowance() {
        let tokens: Vec<Address> = vec![COLLATERAL.parse().unwrap(), DEBT.parse().unwrap()];
        // Token 0: approved for less than it holds. Token 1: the other way.
        let results = vec![word(100), word(40), word(7), word(900)];
        assert_eq!(
            decode_funded(&tokens, &results, &GasReserve::default()),
            vec![
                (tokens[0], Some(U256::from(40u64))),
                (tokens[1], Some(U256::from(7u64))),
            ]
        );
    }

    #[test]
    fn one_reverting_token_does_not_cost_the_others_their_reading() {
        let tokens: Vec<Address> = vec![COLLATERAL.parse().unwrap(), DEBT.parse().unwrap()];
        // Token 0's allowance call reverted; token 1 answered in full.
        let results = vec![word(100), None, word(7), word(900)];
        let decoded = decode_funded(&tokens, &results, &GasReserve::default());
        assert_eq!(decoded[0], (tokens[0], None), "keeps its last reading");
        assert_eq!(decoded[1], (tokens[1], Some(U256::from(7u64))));
    }

    #[test]
    fn a_truncated_batch_reads_as_missing_rather_than_zero() {
        let tokens: Vec<Address> = vec![COLLATERAL.parse().unwrap(), DEBT.parse().unwrap()];
        let decoded = decode_funded(&tokens, &[word(100), word(40)], &GasReserve::default());
        assert_eq!(decoded[1], (tokens[1], None));
    }

    #[test]
    fn the_gas_coin_is_quoted_net_of_the_gas_reserve() {
        const COIN: u64 = 1_000_000_000_000_000_000;
        let celo = crate::chain::gas_reserve::native_erc20(42220).unwrap();
        let usdt: Address = DEBT.parse().unwrap();
        let reserve = GasReserve::for_chain(42220);
        // CELO: 10 held, 5 kept for gas, approved without limit -> 5.
        // USDT: untouched by the reserve.
        let results = vec![word(10 * COIN), word(u64::MAX), word(100), word(u64::MAX)];
        assert_eq!(
            decode_funded(&[celo, usdt], &results, &reserve),
            vec![
                (celo, Some(U256::from(5 * COIN))),
                (usdt, Some(U256::from(100u64))),
            ]
        );
        // Under the reserve, the gas coin quotes nothing rather than wrapping.
        let drained = decode_funded(&[celo], &[word(2 * COIN), word(u64::MAX)], &reserve);
        assert_eq!(drained, vec![(celo, Some(U256::ZERO))]);
    }

    fn vault_inventory_results() -> Vec<Option<Bytes>> {
        vec![
            word(1_000),    // quotableSettlement
            word(10),       // liquidSettlement — most of it is staked
            word(2_000),    // quotableCorridor
            word(500),      // maxOrderInputSettlement
            word(600),      // maxOrderInputCorridor
            word(0),        // closeOnly
            word(0),        // paused
            word(4),        // tradingEpoch
            word(u64::MAX), // settlement Permit2 allowance
            word(u64::MAX), // corridor Permit2 allowance
        ]
    }

    /// A vault RPC for the inventory loop. `batched` deploys Multicall3; without
    /// it every view is its own `eth_call`. `module_views` answers
    /// `freeSettlement()`/`freeCorridor()`: `None` reverts inside a batch, and
    /// on the sequential path hangs forever, the way a stuck node would.
    /// Returns the URL plus counters for quote batches, module reads, and every
    /// request.
    async fn vault_rpc(
        batched: bool,
        module_views: Option<(u64, u64)>,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use crate::chain::multicall::{Call3, Result3};
        use alloy_sol_types::SolValue;
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let quote_reads = Arc::new(AtomicUsize::new(0));
        let module_reads = Arc::new(AtomicUsize::new(0));
        let (quote_count, module_count) = (quote_reads.clone(), module_reads.clone());
        // One answer per calldata, the same tables batched or not.
        let single = move |data: &[u8]| -> Option<Bytes> {
            let quote_views = [
                encode_quotable_settlement(),
                encode_liquid_settlement(),
                encode_quotable_corridor(),
                encode_max_order_input_settlement(),
                encode_max_order_input_corridor(),
                encode_close_only(),
                encode_paused(),
                encode_trading_epoch(),
            ];
            let results = vault_inventory_results();
            if data == encode_settlement_asset() {
                word(2)
            } else if data == encode_corridor_asset() {
                word(1)
            } else if data == encode_max_order_lifetime() {
                word(120)
            } else if let Some(i) = quote_views.iter().position(|v| v.as_slice() == data) {
                results[i].clone()
            } else if data == encode_free_settlement() {
                module_views.and_then(|(s, _)| word(s))
            } else if data == encode_free_corridor() {
                module_views.and_then(|(_, c)| word(c))
            } else {
                // The two Permit2 allowances.
                word(u64::MAX)
            }
        };
        let app = Router::new().route(
            "/",
            post(move |Json(req): Json<Value>| {
                let (quote_count, module_count) = (quote_count.clone(), module_count.clone());
                async move {
                    let result = if req["method"] == "eth_getCode" {
                        if batched { "0x01" } else { "0x" }.to_string()
                    } else {
                        let data = alloy_primitives::hex::decode(
                            req["params"][0]["data"].as_str().unwrap(),
                        )
                        .unwrap();
                        if batched {
                            let (calls,) =
                                <(Vec<Call3>,)>::abi_decode_params_validate(&data[4..]).unwrap();
                            match calls.len() {
                                3 => {}
                                10 | 12 => {
                                    quote_count.fetch_add(1, Ordering::SeqCst);
                                }
                                count => panic!("unexpected inventory batch of {count} calls"),
                            }
                            if calls.len() == 12 {
                                module_count.fetch_add(1, Ordering::SeqCst);
                            }
                            let results: Vec<Result3> = calls
                                .iter()
                                .map(|c| single(c.callData.as_ref()))
                                .map(|value| Result3 {
                                    success: value.is_some(),
                                    returnData: value.unwrap_or_default(),
                                })
                                .collect();
                            alloy_primitives::hex::encode_prefixed((results,).abi_encode_params())
                        } else {
                            if data == encode_trading_epoch() {
                                quote_count.fetch_add(1, Ordering::SeqCst);
                            }
                            if data == encode_free_settlement() {
                                module_count.fetch_add(1, Ordering::SeqCst);
                            }
                            match single(&data) {
                                Some(out) => alloy_primitives::hex::encode_prefixed(out),
                                // Keep module balances unavailable, as for a hung RPC.
                                None => std::future::pending::<String>().await,
                            }
                        }
                    };
                    Json(json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, quote_reads, module_reads, server)
    }

    /// An engine with modules on and nothing read yet.
    fn module_engine() -> (Engine, std::path::PathBuf) {
        let mut engine = test_engine();
        let dir = enable_modules(&mut engine, crate::modules::Mode::Shadow, 0, 0);
        engine.inventory = InventoryCache::default();
        *engine.trading_epoch.write().unwrap() = 0;
        *engine.vault_policy.write().unwrap() = None;
        *engine.modules.as_ref().unwrap().balances.write().unwrap() = None;
        (engine, dir)
    }

    fn spawn_inventory_loop(engine: &Engine, url: &str) -> tokio::task::JoinHandle<()> {
        tokio::spawn(inventory_loop(
            Wallet::new(url, engine.signer.clone(), engine.chain_id),
            engine.permit2,
            vec![],
            GasReserve::default(),
            engine.inventory.clone(),
            engine.vault,
            None,
            engine.trading_epoch.clone(),
            engine.vault_policy.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            engine.modules.clone(),
            None,
        ))
    }

    fn assert_quote_inventory_fresh(engine: &Engine) {
        let now = unix_now();
        let inventory = engine.inventory.view(now);
        assert_eq!(
            inventory.funded(DEBT.parse().unwrap()),
            Some(U256::from(10))
        );
        assert_eq!(
            inventory.funded(COLLATERAL.parse().unwrap()),
            Some(U256::from(2_000))
        );
        assert_eq!(*engine.trading_epoch.read().unwrap(), 4);
        let policy = engine.vault_policy.read().unwrap().unwrap();
        assert_eq!(policy.max_input_settlement, U256::from(500));
        assert_eq!(policy.max_input_corridor, U256::from(600));
    }

    /// A wallet bot publishes what it read for the panel, gas included, and
    /// the panel's extra reads ride the same batch: no request of their own.
    #[tokio::test]
    async fn the_inventory_batch_publishes_its_reads_for_the_panel() {
        use crate::chain::mock_node::{mock_rpc, MockChain};
        use std::sync::atomic::Ordering;
        let node = mock_rpc(
            MockChain::default()
                .balance(COLLATERAL, 5_000)
                .balance(DEBT, 7_000)
                .native(42)
                .with_multicall3(),
        )
        .await;
        let engine = test_engine();
        let dir =
            std::env::temp_dir().join(format!("stitch-rfq-snapshot-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens: Vec<Address> = vec![COLLATERAL.parse().unwrap(), DEBT.parse().unwrap()];
        let refresh = tokio::spawn(inventory_loop(
            Wallet::new(&node.url, engine.signer.clone(), engine.chain_id),
            engine.permit2,
            tokens.clone(),
            GasReserve::default(),
            engine.inventory.clone(),
            None,
            None,
            engine.trading_epoch.clone(),
            engine.vault_policy.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            None,
            Some(Publisher::spawn(&dir, engine.chain_id)),
        ));
        let owner = engine.signer.address();
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(s) = crate::chain::snapshot::load(&dir) {
                    return s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("snapshot published");
        refresh.abort();
        let _ = refresh.await;
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(snapshot.is_fresh(engine.chain_id, unix_now(), 5));
        assert_eq!(
            snapshot
                .lookup(tokens[1], &encode_balance_of(owner))
                .map(|b| decode_uint(&b)),
            Some(U256::from(7_000))
        );
        assert!(snapshot
            .lookup(tokens[0], &encode_allowance(owner, engine.permit2))
            .is_some());
        assert_eq!(
            snapshot
                .lookup(CANONICAL_MULTICALL3, &encode_get_eth_balance(owner))
                .map(|b| decode_uint(&b)),
            Some(U256::from(42))
        );
        // The Multicall3 probe and one batch: the panel's reads cost nothing.
        assert_eq!(node.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn module_balances_ride_the_quote_inventory_batch() {
        use std::sync::atomic::Ordering;
        let (url, quote_reads, module_reads, server) = vault_rpc(true, Some((700, 800))).await;
        let (engine, dir) = module_engine();
        let refresh = spawn_inventory_loop(&engine, &url);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        refresh.abort();
        let _ = refresh.await;
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();

        assert_quote_inventory_fresh(&engine);
        let balances = engine
            .modules
            .as_ref()
            .unwrap()
            .balances
            .read()
            .unwrap()
            .expect("module balances published from the inventory batch");
        assert_eq!(balances.settlement, U256::from(700));
        assert_eq!(balances.corridor, U256::from(800));
        // No 2-call module batch: `vault_rpc` panics on any batch size other
        // than assets (3) and inventory (10 or 12).
        assert_eq!(
            quote_reads.load(Ordering::SeqCst),
            module_reads.load(Ordering::SeqCst),
            "every inventory batch carries the module views"
        );
    }

    #[tokio::test]
    async fn a_reverting_module_view_does_not_take_quote_inventory_dark() {
        let (url, _, _, server) = vault_rpc(true, None).await;
        let (engine, dir) = module_engine();
        let refresh = spawn_inventory_loop(&engine, &url);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        refresh.abort();
        let _ = refresh.await;
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();

        assert_quote_inventory_fresh(&engine);
        assert!(engine
            .modules
            .as_ref()
            .unwrap()
            .balances
            .read()
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn without_multicall3_a_hung_module_read_does_not_block_quote_inventory() {
        use std::sync::atomic::Ordering;
        let (url, quote_reads, module_reads, server) = vault_rpc(false, None).await;
        let (engine, dir) = module_engine();
        let refresh = spawn_inventory_loop(&engine, &url);
        // Keep the module RPC hung for longer than quote inventory's TTL.
        // At least two further ordinary refreshes must complete during that time.
        tokio::time::sleep(std::time::Duration::from_secs(INVENTORY_TTL_SECS + 2)).await;
        refresh.abort();
        let _ = refresh.await;
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();

        assert!(quote_reads.load(Ordering::SeqCst) >= 3);
        assert_eq!(
            module_reads.load(Ordering::SeqCst),
            1,
            "the separate module loop reached its read once, and never overlapped"
        );
        assert_quote_inventory_fresh(&engine);
        assert!(engine
            .modules
            .as_ref()
            .unwrap()
            .balances
            .read()
            .unwrap()
            .is_none());
        assert!(
            engine
                .inventory
                .view(unix_now() + INVENTORY_TTL_SECS + 1)
                .funded(DEBT.parse().unwrap())
                .is_none(),
            "a hung module read must not extend quote inventory freshness"
        );
    }

    #[test]
    fn the_vault_batch_decodes_in_view_order() {
        let (settlement, corridor, epoch, max_s, max_c) =
            decode_vault_inventory(&vault_inventory_results(), true).unwrap();
        assert_eq!(
            settlement,
            U256::from(1_000u64),
            "executor-routed quotes the staked position"
        );
        assert_eq!(corridor, U256::from(2_000u64));
        assert_eq!(epoch, 4);
        assert_eq!(max_s, U256::from(500u64));
        assert_eq!(max_c, U256::from(600u64));

        // Without an executor the fill cannot recall from the adapter, so only
        // the idle balance is quotable.
        let (settlement, _, _, _, _) =
            decode_vault_inventory(&vault_inventory_results(), false).unwrap();
        assert_eq!(settlement, U256::from(10u64));
    }

    #[test]
    fn a_paused_vault_quotes_nothing_and_close_only_quotes_one_side() {
        let mut paused = vault_inventory_results();
        paused[6] = word(1);
        let (settlement, corridor, ..) = decode_vault_inventory(&paused, true).unwrap();
        assert_eq!((settlement, corridor), (U256::ZERO, U256::ZERO));

        let mut close_only = vault_inventory_results();
        close_only[5] = word(1);
        let (settlement, corridor, ..) = decode_vault_inventory(&close_only, true).unwrap();
        assert_eq!(settlement, U256::ZERO);
        assert_eq!(corridor, U256::from(2_000u64));
    }

    #[test]
    fn a_vault_publishes_no_more_than_it_has_approved_to_permit2() {
        let mut results = vault_inventory_results();
        results[8] = word(25); // settlement approved for 25 of its 1000
        let (settlement, corridor, ..) = decode_vault_inventory(&results, true).unwrap();
        assert_eq!(settlement, U256::from(25u64));
        assert_eq!(corridor, U256::from(2_000u64));
    }

    /// Quoting without `paused` is quoting through a stop, so a partial batch
    /// fails the whole refresh and the TTL takes the vault dark.
    #[test]
    fn a_missing_vault_view_fails_the_refresh() {
        for i in 0..10 {
            let mut results = vault_inventory_results();
            results[i] = None;
            assert!(
                decode_vault_inventory(&results, true).is_err(),
                "view #{i} missing must not decode"
            );
        }
        assert!(decode_vault_inventory(&[], true).is_err());
    }

    #[test]
    fn vault_assets_refuse_an_unset_pair_or_a_zero_lifetime() {
        let settlement = address!("0000000000000000000000000000000000000011");
        let corridor = address!("0000000000000000000000000000000000000012");
        let as_word = |a: Address| Some(Bytes::from(a.into_word().0.to_vec()));

        let ok = vec![as_word(settlement), as_word(corridor), word(300)];
        assert_eq!(
            decode_vault_assets(&ok).unwrap(),
            (settlement, corridor, 300)
        );

        let unset = vec![as_word(Address::ZERO), as_word(corridor), word(300)];
        assert!(decode_vault_assets(&unset).is_err());

        let no_lifetime = vec![as_word(settlement), as_word(corridor), word(0)];
        assert!(decode_vault_assets(&no_lifetime).is_err());

        assert!(decode_vault_assets(&[as_word(settlement)]).is_err());
    }

    fn handover(code: u16) -> anyhow::Error {
        anyhow::Error::new(session::VenueHandover {
            code,
            reason: "test".into(),
        })
    }

    #[test]
    fn only_the_redirect_codes_count_as_a_handover() {
        assert!(session::handover_close(4005), "not-the-engine redirects");
        assert!(session::handover_close(4006), "draining redirects");
        assert!(session::handover_close(4007), "stale seats redirect");
        // Everything in 4000-4004 is the maker's problem, not the task's:
        // reconnecting instantly would hammer the venue with a credential it
        // has already rejected.
        for code in [0u16, 1006, 4000, 4001, 4002, 4003, 4004] {
            assert!(
                !session::handover_close(code),
                "{code} must not open the fast lane"
            );
        }
    }

    #[test]
    fn is_handover_sees_through_added_context() {
        let wrapped = handover(4006).context("connecting to venue");
        assert!(session::is_handover(&wrapped));
        assert!(!session::is_handover(&anyhow::anyhow!(
            "503 Service Unavailable"
        )));
    }

    #[test]
    fn a_supersede_close_is_classified_as_one() {
        let err = session::close_error(&Some(CloseFrame {
            code: 4001u16.into(),
            reason: "superseded".into(),
        }));
        assert!(session::is_superseded(&err));
        // And not as a handover: reconnecting instantly against a venue that
        // keeps handing our identity to someone else is the flap loop.
        assert!(!session::is_handover(&err));
        assert!(session::is_superseded(&err.context("reading venue frame")));

        let unauthorized = session::close_error(&Some(CloseFrame {
            code: 4000u16.into(),
            reason: "unauthorized".into(),
        }));
        assert!(!session::is_superseded(&unauthorized));
    }

    fn superseded() -> anyhow::Error {
        session::close_error(&Some(CloseFrame {
            code: 4001u16.into(),
            reason: "superseded".into(),
        }))
    }

    const SHORT: std::time::Duration = std::time::Duration::from_secs(1);
    const LONG: std::time::Duration = std::time::Duration::from_secs(3_600);

    #[test]
    fn the_flap_streak_only_counts_sessions_that_never_got_going() {
        // The flap: accepted, superseded a second later, over and over.
        let mut streak = 0;
        for expected in 1..=4 {
            streak = next_supersede_streak(streak, SHORT, &superseded());
            assert_eq!(streak, expected);
        }
        assert!(streak >= SUPERSEDE_ALERT_THRESHOLD, "the alert fires");
    }

    #[test]
    fn a_standby_takeover_after_a_real_session_is_not_a_flap() {
        // Three isolated failovers over days, each after a session that served
        // for an hour. Counting those hit the duplicate-process alert with
        // nothing wrong — the streak has to reset when a session actually ran.
        let mut streak = 0;
        for _ in 0..3 {
            streak = next_supersede_streak(streak, LONG, &superseded());
            assert_eq!(streak, 0);
            assert!(streak < SUPERSEDE_ALERT_THRESHOLD);
        }
    }

    #[test]
    fn a_healthy_session_clears_a_streak_in_progress() {
        let mut streak = next_supersede_streak(0, SHORT, &superseded());
        streak = next_supersede_streak(streak, SHORT, &superseded());
        assert_eq!(streak, 2);
        streak = next_supersede_streak(streak, LONG, &superseded());
        assert_eq!(streak, 0, "the duplicate went away; start over");
    }

    #[test]
    fn any_other_ending_clears_the_streak() {
        let mut streak = next_supersede_streak(0, SHORT, &superseded());
        assert_eq!(streak, 1);
        streak = next_supersede_streak(streak, SHORT, &handover(4006));
        assert_eq!(streak, 0, "a deploy handover is not a duplicate identity");
        streak = next_supersede_streak(1, SHORT, &anyhow::anyhow!("503"));
        assert_eq!(streak, 0, "nor is a dead socket");
    }

    #[test]
    fn a_failed_handshake_breaks_the_streak_too() {
        // The reconnect driver runs its Err path through the same transition,
        // with a zero duration because nothing served. Two short supersedes, a
        // handshake failure, then one more supersede must not read as three in
        // a row.
        let mut streak = next_supersede_streak(0, SHORT, &superseded());
        streak = next_supersede_streak(streak, SHORT, &superseded());
        assert_eq!(streak, 2);
        streak = next_supersede_streak(
            streak,
            std::time::Duration::ZERO,
            &anyhow::anyhow!("connecting to venue: 503"),
        );
        assert_eq!(streak, 0);
        streak = next_supersede_streak(streak, SHORT, &superseded());
        assert_eq!(streak, 1, "the alert stays quiet");
        assert!(streak < SUPERSEDE_ALERT_THRESHOLD);
    }

    #[test]
    fn being_superseded_during_a_handshake_still_counts() {
        // Taken over before acceptance is the same duplicate-identity story, so
        // the zero duration must not exempt it.
        let streak = next_supersede_streak(2, std::time::Duration::ZERO, &superseded());
        assert_eq!(streak, 3);
    }

    #[test]
    fn only_a_session_that_lasted_clears_the_backoff() {
        // The flap loop: accepted, superseded ~1s later, over and over. Health
        // has to mean "it ran", or the backoff never grows and the bot retries
        // at 1 Hz forever.
        assert!(!is_healthy_session(std::time::Duration::from_millis(900)));
        assert!(!is_healthy_session(
            HEALTHY_SESSION - std::time::Duration::from_millis(1)
        ));
        assert!(is_healthy_session(HEALTHY_SESSION));
        assert!(is_healthy_session(std::time::Duration::from_secs(3_600)));
    }

    #[test]
    fn an_unreset_backoff_walks_a_supersede_loop_out_to_the_ceiling() {
        // Same shape as the reconnect driver when every session dies instantly:
        // note(err) with no reset in between.
        let mut b = Backoff::default();
        let mut delays = Vec::new();
        for _ in 0..7 {
            b.note(&anyhow::anyhow!(
                "venue closed the session: 4001 superseded"
            ));
            delays.push(b.next_delay().as_secs());
        }
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn a_handover_retries_immediately_while_a_failure_backs_off() {
        let mut b = Backoff::default();

        // A deploy handover: no sleeping. This is the whole point — the warm
        // replacement is already accepting sockets.
        b.note(&handover(4006));
        assert_eq!(b.next_delay(), std::time::Duration::from_millis(250));
        b.note(&handover(4005));
        assert_eq!(b.next_delay(), std::time::Duration::from_millis(250));

        // A real outage goes back to exponential, and the handover retries it
        // just made must not have eaten the budget.
        b.note(&anyhow::anyhow!("503"));
        assert_eq!(b.next_delay(), std::time::Duration::from_secs(1));
        b.note(&anyhow::anyhow!("503"));
        assert_eq!(b.next_delay(), std::time::Duration::from_secs(2));
    }

    #[test]
    fn the_fast_lane_is_finite() {
        let mut b = Backoff::default();
        for _ in 0..HANDOVER_FAST_RETRIES {
            b.note(&handover(4005));
            assert_eq!(b.next_delay(), std::time::Duration::from_millis(250));
        }
        // A venue refusing every socket must not be spun on forever.
        b.note(&handover(4005));
        assert_eq!(b.next_delay(), std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_session_that_ran_clears_both_lanes() {
        let mut b = Backoff::default();
        b.note(&anyhow::anyhow!("503"));
        let _ = b.next_delay();
        let _ = b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), std::time::Duration::from_secs(1));
    }

    /// A signer that takes longer than any reply budget, for pinning the
    /// timeout. Stands in for an MPC backend whose policy path needs a human,
    /// or whose provider is simply slow.
    struct SlowSigner {
        address: Address,
    }

    #[async_trait::async_trait]
    impl crate::signer::Signer for SlowSigner {
        async fn sign_digest(&self, _digest: alloy_primitives::B256) -> anyhow::Result<[u8; 65]> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            unreachable!("the reply budget must cut this off long before it returns")
        }

        fn address(&self) -> Address {
            self.address
        }
    }

    struct TypedOnlySigner {
        address: Address,
    }

    #[async_trait::async_trait]
    impl crate::signer::Signer for TypedOnlySigner {
        async fn sign_digest(&self, _digest: alloy_primitives::B256) -> anyhow::Result<[u8; 65]> {
            unreachable!("a typed-only signer must be rejected before transaction signing")
        }

        fn address(&self) -> Address {
            self.address
        }

        fn can_sign_transactions(&self) -> bool {
            false
        }
    }

    /// A signature that misses `replyBy` is worthless — the venue has stopped
    /// listening — but signing it anyway costs twice over: the reservation
    /// below pins inventory for the whole TTL against a quote nobody will take,
    /// and because `run_connected` awaits `dispatch` inline, the socket stalls
    /// for every other RFQ and level update while we wait.
    #[tokio::test]
    async fn a_signature_that_misses_the_reply_budget_rejects_and_reserves_nothing() {
        let mut engine = test_engine();
        let address = engine.signer.address();
        engine.signer = Arc::new(SlowSigner { address });
        let prices = fresh_prices();

        let mut req = exact_input_request("rfq_slow");
        // A realistic budget: the venue's pilot default.
        req.reply_by = format_iso_ms(unix_now_ms() + 750);

        let started = std::time::Instant::now();
        let MakerFrame::QuoteReject(rej) = engine.respond(req, &prices).await else {
            panic!("a signature that can't arrive in time must not become a quote");
        };
        assert_eq!(rej.reason, RejectReason::Busy);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the wait must be bounded by the reply budget, not the signer's own timeout \
             (took {:?})",
            started.elapsed()
        );
        assert!(
            engine.reservations.is_empty(),
            "a quote that was never sent must not pin inventory"
        );
    }

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
                staleness_secs: 240,
            }],
            configured: Vec::new(),
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
            signer: Arc::new(LocalSigner::new(key)),
            vault: None,
            vault_order_executor: None,
            trading_epoch: Arc::new(RwLock::new(0)),
            vault_policy: Arc::new(RwLock::new(None)),
            nonce_salt: 7,
            rpc: Rpc::new("http://127.0.0.1:1"),
            rpc_url: "http://127.0.0.1:1".into(),
            gas_caps: GasCaps::for_chain(8453),
            closing: Arc::new(Mutex::new(HashMap::new())),
            modules: None,
            pending_rebalance: None,
        }
    }

    fn close_redeem_request(vault: &str, chain_id: u64) -> VenueFrame {
        VenueFrame::CloseRedeemRequest(CloseRedeemRequestFrame {
            request_id: "cls_1".into(),
            chain_id,
            vault: vault.into(),
            epoch_id: "3".into(),
            reply_by: "2026-08-05T10:00:04.000Z".into(),
        })
    }

    /// A bot without a vault, or asked about another vault or chain, refuses
    /// before it touches the chain or its key — the close spends a nonce and
    /// kills every order signed under the trading epoch.
    #[tokio::test]
    async fn refuses_to_close_for_a_vault_it_does_not_sign_for() {
        let vault = "0x2222222222222222222222222222222222222222";
        let mut engine = test_engine();
        let reply = engine
            .dispatch(close_redeem_request(vault, 8453), &PriceCache::default())
            .await;
        let Some(MakerFrame::CloseRedeemReject(rej)) = reply else {
            panic!("expected a reject for a bot with no vault");
        };
        assert_eq!(rej.reason, CloseRedeemRejectReason::WrongVault);

        engine.vault = Some(vault.parse().unwrap());
        let other = "0x3333333333333333333333333333333333333333";
        let reply = engine
            .dispatch(close_redeem_request(other, 8453), &PriceCache::default())
            .await;
        let Some(MakerFrame::CloseRedeemReject(rej)) = reply else {
            panic!("expected a reject for another vault");
        };
        assert_eq!(rej.reason, CloseRedeemRejectReason::WrongVault);

        let reply = engine
            .dispatch(close_redeem_request(vault, 1), &PriceCache::default())
            .await;
        let Some(MakerFrame::CloseRedeemReject(rej)) = reply else {
            panic!("expected a reject for another chain");
        };
        assert_eq!(rej.reason, CloseRedeemRejectReason::WrongVault);
    }

    /// A replyBy already in the past is not worth an RPC: the venue has
    /// stopped listening, and it asks again next tick.
    #[tokio::test]
    async fn refuses_a_close_it_cannot_answer_in_time() {
        let vault = "0x2222222222222222222222222222222222222222";
        let mut engine = test_engine();
        engine.vault = Some(vault.parse().unwrap());
        let VenueFrame::CloseRedeemRequest(mut req) = close_redeem_request(vault, 8453) else {
            unreachable!()
        };
        req.reply_by = "2020-01-01T00:00:00.000Z".into();
        let reply = engine
            .dispatch(VenueFrame::CloseRedeemRequest(req), &PriceCache::default())
            .await;
        let Some(MakerFrame::CloseRedeemReject(rej)) = reply else {
            panic!("expected a reject for an expired budget");
        };
        assert_eq!(rej.reason, CloseRedeemRejectReason::Busy);
    }

    #[tokio::test]
    async fn refuses_a_close_when_the_signer_cannot_broadcast_transactions() {
        let vault = "0x2222222222222222222222222222222222222222";
        let mut engine = test_engine();
        engine.vault = Some(vault.parse().unwrap());
        engine.signer = Arc::new(TypedOnlySigner {
            address: engine.signer.address(),
        });

        let reply = engine
            .dispatch(close_redeem_request(vault, 8453), &PriceCache::default())
            .await;
        let Some(MakerFrame::CloseRedeemReject(rej)) = reply else {
            panic!("expected a reject for a signer that cannot send transactions");
        };
        assert_eq!(rej.reason, CloseRedeemRejectReason::Busy);
    }

    #[test]
    fn each_close_rpc_gets_only_the_reply_budget_that_remains() {
        assert_eq!(
            remaining_reply_budget(1_500, 1_250),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(remaining_reply_budget(1_500, 1_500), None);
        assert_eq!(remaining_reply_budget(1_500, 1_750), None);
    }

    #[test]
    fn inventory_refresh_cannot_relight_quotes_while_a_close_is_in_flight() {
        let policy = VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::MAX,
            max_lifetime_secs: 60,
        };
        assert!(refreshed_vault_policy(true, policy).is_none());
        let restored = refreshed_vault_policy(false, policy).expect("refresh restores policy");
        assert_eq!(restored.settlement, policy.settlement);
    }

    /// One close per epoch. The venue asks every tick until the chain says
    /// Closed; a second nonce would only buy a revert.
    #[test]
    fn claims_an_epoch_close_once() {
        let engine = test_engine();
        *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::MAX,
            max_lifetime_secs: 60,
        });
        let mut reconnected = test_engine();
        reconnected.closing = engine.closing.clone();

        assert!(engine.claim_close(U256::from(3), 7));
        assert!(engine.vault_policy.read().unwrap().is_none());
        assert!(!reconnected.claim_close(U256::from(3), 7));
        assert!(engine.claim_close(U256::from(4), 7));
    }

    #[test]
    fn an_epoch_bump_reconciles_an_ambiguous_close_claim() {
        let closing = Mutex::new(HashMap::from([(U256::from(3), 7)]));

        assert!(reconcile_close_claims(&closing, 7));
        assert!(!reconcile_close_claims(&closing, 8));
        assert!(closing.lock().unwrap().is_empty());
    }

    fn attest_request(vault: &str, chain_id: u64) -> VenueFrame {
        VenueFrame::AttestRequest(AttestRequestFrame {
            request_id: "att_1".into(),
            chain_id,
            vault: vault.into(),
            epoch_id: "7".into(),
            attestation: wire::NavAttestationWire {
                vault: vault.into(),
                chain_id: chain_id.to_string(),
                epoch_id: "7".into(),
                corridor_asset_price: "1500000000000000000".into(),
                nav: Some("16000000".into()),
                last_settled_nav: "12345".into(),
                free_settlement: "10000000".into(),
                free_corridor: "4000000000000000000".into(),
                valid_after: "1700000000".into(),
                valid_until: "1700003600".into(),
            },
            reply_by: "2026-08-05T10:00:04.000Z".into(),
        })
    }

    /// A bot without a vault, or asked about another vault or chain, refuses
    /// before it touches the chain or its key.
    #[tokio::test]
    async fn refuses_to_cosign_for_a_vault_it_does_not_sign_for() {
        let vault = "0x2222222222222222222222222222222222222222";
        let mut engine = test_engine();
        let reply = engine
            .dispatch(attest_request(vault, 8453), &PriceCache::default())
            .await;
        let Some(MakerFrame::AttestReject(rej)) = reply else {
            panic!("expected a reject: {reply:?}");
        };
        assert_eq!(rej.reason, AttestRejectReason::WrongVault);
        assert_eq!(rej.request_id, "att_1");

        engine.vault = Some(vault.parse().unwrap());
        let reply = engine
            .dispatch(attest_request(vault, 1), &PriceCache::default())
            .await;
        assert!(matches!(
            reply,
            Some(MakerFrame::AttestReject(AttestRejectFrame {
                reason: AttestRejectReason::WrongVault,
                ..
            }))
        ));
        // Right vault, but the venue's replyBy (2026-08-05 in the fixture) is
        // long gone: refused before any read or signing.
        let reply = engine
            .dispatch(attest_request(vault, 8453), &PriceCache::default())
            .await;
        assert!(matches!(
            reply,
            Some(MakerFrame::AttestReject(AttestRejectFrame {
                reason: AttestRejectReason::Busy,
                ..
            }))
        ));
    }

    #[test]
    fn a_shared_debt_token_shares_bid_reservations_across_pools() {
        let mut engine = test_engine();
        let mut sibling = engine.books[0].clone();
        sibling.slug = "wbrl-usdt-celo".into();
        sibling.collateral = "0x0000000000000000000000000000000000000003"
            .parse()
            .unwrap();
        engine.books.push(sibling);
        engine
            .reservations
            .reserve("rfq_cngn", "cngn-usdc", true, U256::from(400u64), 1_000);

        assert_eq!(
            engine.reserved_on(&engine.books[0], true, 0),
            U256::from(400u64)
        );
        assert_eq!(
            engine.reserved_on(&engine.books[1], true, 0),
            U256::from(400u64),
            "a wBRL bid must see the live cNGN USDT claim"
        );
        assert_eq!(engine.reserved_on(&engine.books[0], false, 0), U256::ZERO);
        assert_eq!(
            engine.reserved_on(&engine.books[1], false, 0),
            U256::ZERO,
            "asks pay different collaterals, so they stay separate"
        );
    }

    /// A pool the venue stopped assigning is absent from `books`, so its live
    /// claim used to be invisible to both tagging and the shared-token total.
    /// The configured list keeps its slug in view.
    #[test]
    fn a_configured_but_unassigned_pool_still_owns_its_claim() {
        let mut engine = test_engine();
        // Same USDT debt, so a bid on it competes for the same balance.
        let mut unassigned = engine.books[0].clone();
        unassigned.slug = "cngn-usdt-celo".into();
        unassigned.collateral = "0x0000000000000000000000000000000000000003"
            .parse()
            .unwrap();
        engine.configured.push(unassigned);
        engine.reservations.reserve(
            "rfq_dropped",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
        );

        assert_eq!(
            engine.reserved_on(&engine.books[0], true, 0),
            U256::from(400u64),
            "the quoting book must see the unassigned pool's live claim"
        );

        // And session start can stamp it, since the label is still known.
        engine.tag_loaded_books();
        assert_eq!(engine.reservations.live_tokenless_count(0), 0);
    }

    /// Nothing names the corridor a claim was signed under — not a live book,
    /// not the config. It spent *something*, so nothing may be sized against a
    /// balance it might have spent.
    #[test]
    fn an_unattributable_claim_takes_every_book_dark() {
        let mut engine = test_engine();
        engine.reservations.reserve(
            "rfq_orphan",
            "venue-slug-nobody-configured",
            true,
            U256::from(400u64),
            1_000,
        );

        // Nothing says which of our tokens it spent, or in what units, so both
        // sides saturate and the book goes dark until it expires.
        assert_eq!(engine.reserved_on(&engine.books[0], true, 0), U256::MAX);
        assert_eq!(engine.reserved_on(&engine.books[0], false, 0), U256::MAX);
    }

    #[test]
    fn a_removed_pool_still_counts_against_the_shared_token() {
        let mut engine = test_engine();
        let mut sibling = engine.books[0].clone();
        sibling.slug = "wbrl-usdt-celo".into();
        sibling.collateral = "0x0000000000000000000000000000000000000003"
            .parse()
            .unwrap();
        engine.books.push(sibling);
        engine.reservations.reserve_paying(
            "rfq_cngn",
            "cngn-usdc",
            true,
            U256::from(400u64),
            1_000,
            Some(DEBT),
        );
        engine.books.remove(0);

        assert_eq!(
            engine.reserved_on(&engine.books[0], true, 0),
            U256::from(400u64),
            "a leftover cNGN USDT signature must still shrink the wBRL bid"
        );
    }

    #[test]
    fn a_tokenless_claim_is_tagged_from_the_live_book_before_removal() {
        let mut engine = test_engine();
        let mut sibling = engine.books[0].clone();
        sibling.slug = "wbrl-usdt-celo".into();
        sibling.collateral = "0x0000000000000000000000000000000000000003"
            .parse()
            .unwrap();
        engine.books.push(sibling);
        engine
            .reservations
            .reserve("rfq_cngn", "cngn-usdc", true, U256::from(400u64), 1_000);
        engine.tag_loaded_books();
        engine.books.remove(0);

        assert_eq!(
            engine.reserved_on(&engine.books[0], true, 0),
            U256::from(400u64),
            "upgrade-era tokenless claims must be stamped before the pool can vanish"
        );
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
        let deadline = unix_now_ms() + 120_000;
        QuoteRequestFrame {
            rfq_id: rfq_id.into(),
            corridor_id: "cngn-usdc".into(),
            chain_id: 8453,
            sell_token: COLLATERAL.into(),
            buy_token: DEBT.into(),
            sell_amount: Some("1000000000".into()),
            buy_amount: None,
            taker: "0x0000000000000000000000000000000000000003".into(),
            reply_by: format_iso_ms(unix_now_ms() + 750),
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
            "EOA maker: signer field is the funding wallet"
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

    #[tokio::test]
    async fn a_vault_quote_funds_from_the_vault_and_signs_as_strategy() {
        let vault: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let mut engine = test_engine();
        engine.vault = Some(vault);
        *engine.trading_epoch.write().unwrap() = 3;
        *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::MAX,
            max_lifetime_secs: 120,
        });
        let prices = fresh_prices();

        let reply = engine
            .respond(exact_input_request("rfq_vault"), &prices)
            .await;
        let MakerFrame::QuoteResponse(resp) = reply else {
            panic!("expected a firm quote, got {reply:?}");
        };
        assert_eq!(
            resp.signer,
            engine.signer.address().to_string(),
            "frame.signer is the strategy EOA, not the vault"
        );
        let order_bytes = alloy_primitives::hex::decode(&resp.encoded_order).unwrap();
        let swapper_word: [u8; 32] = order_bytes[7 * 32..8 * 32].try_into().unwrap();
        let swapper = Address::from_slice(&swapper_word[12..]);
        assert_eq!(swapper, vault, "swapper is the vault");
        let nonce_word: [u8; 32] = order_bytes[8 * 32..9 * 32].try_into().unwrap();
        let nonce = U256::from_be_bytes::<32>(nonce_word);
        assert_eq!(nonce >> 128, U256::from(3u64), "nonce embeds tradingEpoch");
        assert_eq!(
            nonce & (U256::from(1u8) << nonce::RFQ_NONCE_BIT),
            U256::ZERO,
            "vault nonces do not use the EOA RFQ namespace bit"
        );
    }

    #[test]
    fn an_uninitialized_vault_epoch_does_not_clear_loaded_reservations() {
        let mut engine = test_engine();
        engine.vault = Some(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
        );
        engine
            .reservations
            .reserve("rfq_disk", "cngn-usdc", true, U256::from(100u64), 9_999_999);
        engine.sync_vault_epoch();
        assert_eq!(
            engine.reservations.len(),
            1,
            "epoch 0 is the pre-RPC sentinel, not a transition"
        );
        assert!(engine.reservations.vault_epoch().is_none());
        *engine.trading_epoch.write().unwrap() = 1;
        engine.sync_vault_epoch();
        assert_eq!(
            engine.reservations.len(),
            1,
            "first real epoch must keep the loaded ledger"
        );
        assert_eq!(engine.reservations.vault_epoch(), Some(1));
    }

    #[test]
    fn a_vault_epoch_bump_drops_live_reservations() {
        let mut engine = test_engine();
        engine.vault = Some(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
        );
        *engine.trading_epoch.write().unwrap() = 1;
        engine.sync_vault_epoch();
        engine
            .reservations
            .reserve("rfq_old", "cngn-usdc", true, U256::from(100u64), 9_999_999);
        assert_eq!(engine.reservations.len(), 1);
        *engine.trading_epoch.write().unwrap() = 2;
        engine.sync_vault_epoch();
        assert!(
            engine.reservations.is_empty(),
            "a new tradingEpoch must drop quotes signed under the last one"
        );
    }

    #[test]
    fn vault_level_caps_survive_an_open_reservation() {
        let mut engine = test_engine();
        engine.vault = Some(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
        );
        *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::from(1_000_000_000u64),
            max_lifetime_secs: 120,
        });
        engine.reservations.reserve(
            "rfq_open",
            "cngn-usdc",
            false,
            U256::from(400_000_000u64),
            unix_now() + 60,
        );
        let prices = fresh_prices();
        let MakerFrame::Levels(frame) = engine.level_frames(&prices, unix_now_ms())[0].clone()
        else {
            panic!("expected levels");
        };
        assert_eq!(
            frame.asks[0].size, "1000000000",
            "per-order cap applies after the reservation, not to the wallet first"
        );
    }

    const OTHER: &str = "0x0000000000000000000000000000000000000004";

    fn off_pair_book() -> CorridorBook {
        CorridorBook {
            slug: "other-usdc".into(),
            collateral: OTHER.parse().unwrap(),
            debt: DEBT.parse().unwrap(),
            collateral_decimals: 6,
            debt_decimals: 6,
            buy_spread: Some(Spread::Bps(200)),
            sell_spread: Some(Spread::Bps(200)),
            buy_capacity_debt: Some(RfqCapacity::Exact(U256::from(1_500_000_000u64))),
            sell_capacity_collateral: Some(RfqCapacity::Exact(U256::from(1_500_000_000u64))),
            feed_url: "http://feed".into(),
            staleness_secs: 240,
        }
    }

    #[test]
    fn vault_levels_skip_books_outside_the_pair() {
        let mut engine = test_engine();
        engine.vault = Some(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
        );
        engine.books.push(off_pair_book());
        let prices = fresh_prices();
        assert!(
            engine.level_frames(&prices, unix_now_ms()).is_empty(),
            "no policy yet → publish nothing"
        );
        *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::MAX,
            max_lifetime_secs: 120,
        });
        let frames = engine.level_frames(&prices, unix_now_ms());
        assert_eq!(frames.len(), 1, "off-pair book must stay unpublished");
        let MakerFrame::Levels(frame) = &frames[0] else {
            panic!("expected levels, got {frames:?}");
        };
        assert_eq!(frame.corridor_id, "cngn-usdc");
    }

    #[tokio::test]
    async fn vault_quote_rejects_a_book_that_shares_only_one_asset() {
        let mut engine = test_engine();
        engine.vault = Some(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
        );
        *engine.trading_epoch.write().unwrap() = 3;
        *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::MAX,
            max_lifetime_secs: 120,
        });
        engine.books.push(off_pair_book());
        engine.inventory.set(
            OTHER.parse().unwrap(),
            U256::from(10_000_000_000u64),
            unix_now(),
        );
        let mut req = exact_input_request("rfq_off");
        req.corridor_id = "other-usdc".into();
        req.sell_token = OTHER.into();
        req.buy_token = DEBT.into();
        let reply = engine.respond(req, &fresh_prices()).await;
        let MakerFrame::QuoteReject(rej) = reply else {
            panic!("expected reject, got {reply:?}");
        };
        assert_eq!(rej.reason, RejectReason::Busy);
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
            staleness_secs: 240,
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
            funding_wallets: Vec::new(),
        }
    }

    const A_VAULT: Address = address!("f23712874ab6f973c2761cbfc608529da81d7ae2");
    const ANOTHER_VAULT: Address = address!("4eae04aa25157d35684e958ed0fc22790729be60");

    /// One slot on `chain_id`, naming no signer — the shape an older venue sends.
    fn accepted_funding(chain_id: u64, wallet: &str) -> wire::SessionAcceptedFrame {
        let mut a = accepted(&[], Vec::new());
        a.funding_wallets = vec![wire::MakerWalletFrame {
            chain_id,
            funding_wallet: wallet.into(),
            signing_address: None,
        }];
        a
    }

    /// Several slots on one chain, each naming the signer bound to it, and a
    /// session authenticated as `signer`.
    fn accepted_slots(signer: &str, slots: &[(u64, &str, &str)]) -> wire::SessionAcceptedFrame {
        let mut a = accepted(&[], Vec::new());
        a.signing_address = signer.into();
        a.funding_wallets = slots
            .iter()
            .map(|(chain_id, funding, signing)| wire::MakerWalletFrame {
                chain_id: *chain_id,
                funding_wallet: (*funding).into(),
                signing_address: Some((*signing).to_string()),
            })
            .collect();
        a
    }

    #[test]
    fn a_vault_session_on_the_matching_funding_wallet_quotes() {
        let a = accepted_funding(56, "0xf23712874ab6f973C2761cbFc608529da81D7ae2");
        assert!(vault_session_mismatch(&a, 56, Some(A_VAULT)).is_none());
    }

    #[test]
    fn a_vault_repointed_without_re_enrolling_refuses_the_session() {
        // stitch.toml moved to the new vault; the maker id still funds from
        // the old one, so every level would be published as the old vault's.
        let a = accepted_funding(56, "0x4Eae04Aa25157D35684e958Ed0fC22790729bE60");
        let issue = vault_session_mismatch(&a, 56, Some(A_VAULT)).expect("mismatch");
        assert!(issue.contains("Re-enroll"));
        assert!(issue.to_lowercase().contains("4eae04aa"));
    }

    #[test]
    fn an_eoa_maker_has_nothing_to_compare() {
        let a = accepted_funding(56, "0x4Eae04Aa25157D35684e958Ed0fC22790729bE60");
        assert!(vault_session_mismatch(&a, 56, None).is_none());
    }

    #[test]
    fn a_venue_that_sends_no_binding_leaves_the_check_unarmed() {
        // Older venue: absent is "cannot check", not "mismatch" — refusing
        // would take every vault maker off the market on a rollback.
        let a = accepted(&[], Vec::new());
        assert!(vault_session_mismatch(&a, 56, Some(A_VAULT)).is_none());
        // Same for a binding that covers other chains only.
        let other = accepted_funding(8453, "0xf23712874ab6f973C2761cbFc608529da81D7ae2");
        assert!(vault_session_mismatch(&other, 56, Some(ANOTHER_VAULT)).is_none());
    }

    const SIGNER_A: &str = "0x1b67b8a0fADdE796fFdA75c7932AcBd0a54ca0d3";
    const SIGNER_B: &str = "0x000000000000000000000000000000000000BbBb";

    #[test]
    fn a_second_slot_on_the_chain_does_not_refuse_the_bot_bound_to_the_other() {
        // The venue only holds (chainId, fundingWallet) unique, so one maker
        // can carry both vaults on one chain. Taking the first slot for the
        // chain refused whichever bot happened to sort second.
        let a = accepted_slots(
            SIGNER_A,
            &[
                (56, "0x4Eae04Aa25157D35684e958Ed0fC22790729bE60", SIGNER_B),
                (56, "0xf23712874ab6f973C2761cbFc608529da81D7ae2", SIGNER_A),
            ],
        );
        assert!(vault_session_mismatch(&a, 56, Some(A_VAULT)).is_none());
    }

    #[test]
    fn the_mismatch_names_the_slot_this_signer_is_bound_to() {
        // Signer B funds from the old vault, so pointing its stitch.toml at
        // the new one is still a mismatch — and the message must name B's
        // wallet, not whichever slot came first.
        let a = accepted_slots(
            SIGNER_B,
            &[
                (56, "0xf23712874ab6f973C2761cbFc608529da81D7ae2", SIGNER_A),
                (56, "0x4Eae04Aa25157D35684e958Ed0fC22790729bE60", SIGNER_B),
            ],
        );
        let issue = vault_session_mismatch(&a, 56, Some(A_VAULT)).expect("mismatch");
        assert!(issue.to_lowercase().contains("4eae04aa"));
    }

    #[test]
    fn several_slots_and_no_signer_match_leaves_the_check_unarmed() {
        // Nothing says which slot is ours; refusing would take a maker the
        // venue already authenticated off the market.
        let mut a = accepted_slots(
            SIGNER_A,
            &[
                (56, "0x4Eae04Aa25157D35684e958Ed0fC22790729bE60", SIGNER_B),
                (56, "0x000000000000000000000000000000000000cCcC", SIGNER_B),
            ],
        );
        assert!(vault_session_mismatch(&a, 56, Some(A_VAULT)).is_none());
        // Same when the venue predates the per-slot signer entirely.
        for w in &mut a.funding_wallets {
            w.signing_address = None;
        }
        assert!(vault_session_mismatch(&a, 56, Some(A_VAULT)).is_none());
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

    #[test]
    fn interval_levels_emit_before_any_flush() {
        assert!(
            should_emit_interval_levels(None, false),
            "a new session must publish on the first interval tick"
        );
    }

    #[test]
    fn trailing_due_only_after_the_deadline() {
        assert!(!trailing_due(None), "no timer is not due");
        let later = tokio::time::Instant::now() + LEVELS_INTERVAL;
        assert!(!trailing_due(Some(later)), "a future deadline must wait");
        let past = tokio::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .expect("tokio clock allows a 1ms rewind");
        assert!(trailing_due(Some(past)));
    }

    #[test]
    fn empty_levels_send_does_not_suppress_interval() {
        assert!(
            should_emit_interval_levels(recorded_levels_flush(&[]), false),
            "sending no frames is not a flush; the next tick must still try"
        );
        assert!(
            !should_emit_interval_levels(recorded_levels_flush(&["cngn-usdc".into()]), false),
            "an actual send suppresses the immediate interval tick"
        );
    }

    #[test]
    fn partial_expiry_flush_still_suppresses_the_interval() {
        let mut pending = HashSet::from(["sent".to_string(), "dark".to_string()]);
        drop_emitted_pending(&mut pending, &["sent".into()]);
        assert_eq!(pending, HashSet::from(["dark".to_string()]));
        assert!(
            recorded_levels_flush(&["sent".into()]).is_some(),
            "21 published books plus a dark sibling must still stamp the rate budget"
        );
    }

    #[test]
    fn interval_reschedules_to_the_end_of_the_expiry_window() {
        assert!(
            interval_delay_after_expiry_flush(None).is_none(),
            "no expiry flush means the interval keeps its phase"
        );
        let just_now = tokio::time::Instant::now();
        let delay = interval_delay_after_expiry_flush(Some(just_now))
            .expect("a fresh expiry flush must delay the next tick");
        assert!(delay <= LEVELS_INTERVAL);
        assert!(delay > std::time::Duration::ZERO);
        let aged = tokio::time::Instant::now()
            .checked_sub(LEVELS_INTERVAL + std::time::Duration::from_millis(1))
            .expect("tokio clock allows a 1s rewind");
        assert!(interval_delay_after_expiry_flush(Some(aged)).is_none());
    }

    fn window_with(batches: &[OutboundBatch]) -> RateWindow {
        RateWindow {
            batches: batches.to_vec(),
        }
    }

    fn batch(at: tokio::time::Instant, frames: usize) -> OutboundBatch {
        OutboundBatch { at, frames }
    }

    #[test]
    fn trailing_skips_only_when_the_window_would_trip_the_cap() {
        assert!(
            !should_skip_trailing_for_rate_budget(&RateWindow::default(), 21),
            "reconnect replay with no prior send must still flush"
        );
        let now = tokio::time::Instant::now();
        let interval = window_with(&[batch(now, 21)]);
        assert!(
            !should_skip_trailing_for_rate_budget(&interval, 1),
            "one corrected corridor after a 21-book interval must still fit"
        );
        assert!(
            should_skip_trailing_for_rate_budget(&interval, 21),
            "a second full book in the same second trips the 40/s cap"
        );
        let aged = tokio::time::Instant::now()
            .checked_sub(LEVELS_INTERVAL + std::time::Duration::from_millis(1))
            .expect("tokio clock allows a 1s rewind");
        assert!(!should_skip_trailing_for_rate_budget(
            &window_with(&[batch(aged, 21), batch(aged, 21)]),
            21
        ));
    }

    #[test]
    fn rate_window_keeps_every_expiry_batch() {
        let now = tokio::time::Instant::now();
        let two = window_with(&[batch(now, 14), batch(now, 14)]);
        assert!(
            should_skip_trailing_for_rate_budget(&two, 14),
            "14 + 14 + 14 is 42 and must wait"
        );
        let last_only = window_with(&[batch(now, 14)]);
        assert!(
            !should_skip_trailing_for_rate_budget(&last_only, 14),
            "a last-batch snapshot would wrongly allow the third 14"
        );
    }

    #[test]
    fn rate_budget_counts_only_corridors_that_can_emit() {
        let mut engine = test_engine();
        let prices = fresh_prices();
        let pending = HashSet::from(["cngn-usdc".to_string(), "dark".to_string()]);
        let ready = ready_level_slugs(&mut engine, &prices, Some(&pending));
        assert_eq!(ready, vec!["cngn-usdc".to_string()]);
        let now = tokio::time::Instant::now();
        let almost_full = window_with(&[batch(now, 39)]);
        assert!(
            should_skip_trailing_for_rate_budget(&almost_full, pending.len()),
            "counting dark slugs would wrongly defer"
        );
        assert!(
            !should_skip_trailing_for_rate_budget(&almost_full, ready.len()),
            "one healthy corridor plus 39 must still fit"
        );
    }

    #[test]
    fn dark_pending_keeps_trailing_armed() {
        assert!(
            next_trailing_for_dark_pending(&HashSet::from(["cngn-usdc".into()]), &[]).is_some(),
            "a dark expiry must retry, not wait for the next interval"
        );
        assert!(next_trailing_for_dark_pending(&HashSet::new(), &[]).is_none());
        assert!(next_trailing_for_dark_pending(
            &HashSet::from(["cngn-usdc".into()]),
            &["cngn-usdc".into()]
        )
        .is_none());
    }

    #[test]
    fn no_op_quote_expired_does_not_arm_trailing() {
        let mut pending = HashSet::new();
        assert!(
            trailing_after_quote_expired(&mut pending, None).is_none(),
            "a replay notice with no local reservation must not block the first interval tick"
        );
        assert!(pending.is_empty());
        assert!(trailing_after_quote_expired(&mut pending, Some("cngn-usdc".into())).is_some());
        assert_eq!(pending, HashSet::from(["cngn-usdc".to_string()]));
    }

    #[test]
    fn rate_window_includes_quote_replies() {
        let now = tokio::time::Instant::now();
        let window = window_with(&[batch(now, 20), batch(now, 1)]);
        assert!(
            should_skip_trailing_for_rate_budget(&window, 20),
            "20 levels + 1 quote + 20 expiry is 41 and trips admitMessage"
        );
    }

    #[test]
    fn deferred_trailing_waits_until_enough_batches_age_out() {
        assert!(next_trailing_after_budget(&RateWindow::default(), 21).is_none());
        let now = tokio::time::Instant::now();
        let later = now + std::time::Duration::from_millis(10);
        let window = window_with(&[batch(now, 21), batch(later, 1)]);
        let due = next_trailing_after_budget(&window, 21).expect("21 + 21 needs room");
        assert_eq!(due, now + LEVELS_INTERVAL);
        let three = window_with(&[batch(now, 14), batch(later, 14)]);
        let due = next_trailing_after_budget(&three, 14).expect("42 needs the first 14 to age out");
        assert_eq!(due, now + LEVELS_INTERVAL);
    }

    #[test]
    fn interval_levels_skip_when_trailing_flush_pending() {
        let aged = tokio::time::Instant::now()
            .checked_sub(LEVELS_INTERVAL + std::time::Duration::from_millis(1))
            .expect("tokio clock allows a 1s rewind");
        assert!(
            !should_emit_interval_levels(Some(aged), true),
            "a pending expiry debounce must win over the interval tick"
        );
    }

    #[test]
    fn interval_levels_skip_when_quote_expired_just_flushed() {
        let just_now = tokio::time::Instant::now();
        assert!(
            !should_emit_interval_levels(Some(just_now), false),
            "an immediate interval tick must not double the quoteExpired book"
        );
        let aged = tokio::time::Instant::now()
            .checked_sub(LEVELS_INTERVAL + std::time::Duration::from_millis(1))
            .expect("tokio clock allows a 1s rewind");
        assert!(should_emit_interval_levels(Some(aged), false));
    }

    #[tokio::test]
    async fn quote_expired_restores_published_level_depth() {
        let mut engine = test_engine();
        let prices = fresh_prices();
        let now_ms = unix_now_ms();

        let MakerFrame::Levels(full) = engine.level_frames(&prices, now_ms)[0].clone() else {
            panic!("expected a levels frame before any quote");
        };
        let full_bid: u128 = full.bids[0].size.parse().unwrap();

        let _ = engine.respond(exact_input_request("rfq_1"), &prices).await;
        let MakerFrame::Levels(reserved) = engine.level_frames(&prices, now_ms)[0].clone() else {
            panic!("expected a levels frame while reserved");
        };
        let reserved_bid: u128 = reserved.bids[0].size.parse().unwrap();
        assert!(
            reserved_bid < full_bid,
            "an open quote must shrink published bid depth"
        );

        let _ = engine
            .dispatch(
                VenueFrame::QuoteExpired(QuoteExpiredFrame {
                    rfq_id: "rfq_1".into(),
                }),
                &prices,
            )
            .await;
        let MakerFrame::Levels(restored) = engine.level_frames(&prices, now_ms)[0].clone() else {
            panic!("expected a levels frame after quoteExpired");
        };
        assert_eq!(
            restored.bids[0].size.parse::<u128>().unwrap(),
            full_bid,
            "quoteExpired must put full depth back on the next levels frame"
        );
    }

    #[tokio::test]
    async fn quote_result_releases_inventory_except_selected() {
        let mut engine = test_engine();
        let prices = fresh_prices();

        let _ = engine.respond(exact_input_request("rfq_1"), &prices).await;
        assert_eq!(engine.reservations.len(), 1);
        let none = engine
            .dispatch(
                VenueFrame::QuoteResult(QuoteResultFrame {
                    rfq_id: "rfq_1".into(),
                    result: "lost_price".into(),
                }),
                &prices,
            )
            .await;
        assert!(none.is_none());
        assert!(
            engine.reservations.is_empty(),
            "lost_price must drop the reservation"
        );

        let _ = engine.respond(exact_input_request("rfq_2"), &prices).await;
        let _ = engine
            .dispatch(
                VenueFrame::QuoteResult(QuoteResultFrame {
                    rfq_id: "rfq_2".into(),
                    result: "no_quote".into(),
                }),
                &prices,
            )
            .await;
        assert!(engine.reservations.is_empty());

        let _ = engine.respond(exact_input_request("rfq_3"), &prices).await;
        let _ = engine
            .dispatch(
                VenueFrame::QuoteResult(QuoteResultFrame {
                    rfq_id: "rfq_3".into(),
                    result: "selected".into(),
                }),
                &prices,
            )
            .await;
        assert_eq!(
            engine.reservations.len(),
            1,
            "selected stays reserved until quoteExpired"
        );
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
        assert!(engine.level_frames(&empty, unix_now_ms()).is_empty());

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
        assert!(engine.level_frames(&stale, unix_now_ms()).is_empty());
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
        req.max_expires_at = format_iso_ms(unix_now_ms() + 4_500);
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
        req.reply_by = format_iso_ms(unix_now_ms() - 1_000);
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

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stitch-instance-{}-{}-{tag}",
            std::process::id(),
            unix_now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_configured_name_is_the_instance_id() {
        let dir = temp_dir("configured");
        assert_eq!(
            resolve_instance_id(Some("  bsc-cngn  "), Some(&dir)).as_deref(),
            Some("bsc-cngn"),
            "trimmed, so a stray space in stitch.toml is not a different bot"
        );
        assert!(
            !dir.join(INSTANCE_ID_FILE).exists(),
            "a configured name needs nothing persisted"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_generated_instance_id_survives_a_restart() {
        // The property that matters: the same bot reconnecting reclaims its own
        // venue session. A fresh id per process would leave the old socket to
        // time out and let restarts pile sockets up.
        let dir = temp_dir("generated");
        let first = resolve_instance_id(None, Some(&dir)).expect("generated");
        let second = resolve_instance_id(None, Some(&dir)).expect("reused");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(dir.join(INSTANCE_ID_FILE))
                .unwrap()
                .trim(),
            first
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_bots_generate_different_instance_ids() {
        // Separate config directories are separate bots, which is the whole
        // point: they must not collide and evict each other at the venue.
        let a = temp_dir("bot-a");
        let b = temp_dir("bot-b");
        assert_ne!(
            resolve_instance_id(None, Some(&a)),
            resolve_instance_id(None, Some(&b))
        );
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn an_empty_or_absent_source_falls_back_to_no_instance_id() {
        // No config dir and nothing configured: the venue's own fallback (one
        // session per credential chain) beats an id that changes every restart.
        assert_eq!(resolve_instance_id(None, None), None);
        assert_eq!(resolve_instance_id(Some("   "), None), None);

        // An empty file is treated as absent, and replaced.
        let dir = temp_dir("empty-file");
        std::fs::write(dir.join(INSTANCE_ID_FILE), "\n").unwrap();
        assert!(resolve_instance_id(None, Some(&dir)).is_some());
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn an_unstaged_credential_is_no_key_rather_than_a_hard_error() {
        // `deploy/stitch.service` picks the maker key up with `ImportCredential=`,
        // which stages nothing when the credential store has no match — so
        // `STITCH_RFQ_API_KEY_FILE` points at a path that does not exist. The
        // loader has to read that as "no key" so `maybe_spawn` logs, skips the
        // responder, and leaves the bot's other legs running. A bot still
        // waiting on `stitch connect`, or a limit-taker-only one, lands here.
        let dir = std::env::temp_dir().join(format!(
            "stitch-rfq-key-{}-{}",
            std::process::id(),
            "unstaged"
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let var = "STITCH_RFQ_API_KEY_UNSTAGED_CRED_TEST";
        // No sibling `rfq-api.key` either, which is the systemd layout.
        std::env::set_var(format!("{var}_FILE"), dir.join("never-staged"));
        let err = load_rfq_api_key(var, Some(&dir))
            .expect_err("an unstaged credential is not a usable key");
        assert!(err.to_string().contains(var), "{err}");

        // An empty staged file reads the same way, so a hand-rolled unit that
        // provisions a blank source degrades identically.
        let blank = dir.join("blank-credential");
        std::fs::write(&blank, "").unwrap();
        std::env::set_var(format!("{var}_FILE"), &blank);
        assert!(load_rfq_api_key(var, Some(&dir)).is_err());

        std::env::remove_var(format!("{var}_FILE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn api_key_configured_matches_what_the_loader_would_accept() {
        let dir = std::env::temp_dir().join(format!("stitch-rfq-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("maker.key");
        std::fs::write(&good, "tx_live_secret\n").unwrap();
        let blank = dir.join("blank.key");
        std::fs::write(&blank, "   \n").unwrap();
        let missing = dir.join("gone.key");
        let none = |_: &str| None;
        let only =
            |name: &'static str, value: String| move |n: &str| (n == name).then(|| value.clone());

        // Nothing anywhere.
        assert!(!api_key_configured("K", Some(&dir), none));

        // A readable, non-blank file the variable points at.
        assert!(api_key_configured(
            "K",
            None,
            only("K_FILE", good.display().to_string())
        ));

        // A path that doesn't resolve is not a credential: `read_env_secret`
        // errors and the loader falls past it, so accepting it here would let a
        // bot start and then decline to spawn its responder.
        assert!(!api_key_configured(
            "K",
            None,
            only("K_FILE", missing.display().to_string())
        ));
        assert!(!api_key_configured(
            "K",
            None,
            only("K_FILE", blank.display().to_string())
        ));

        // A set _FILE is the whole env answer: `read_env_secret` returns out of
        // that branch either way, so a broken path does NOT fall back to the raw
        // variable — only onward to the sibling file.
        let broken_plus_raw = |missing: String| {
            move |n: &str| match n {
                "K_FILE" => Some(missing.clone()),
                "K" => Some("tx_live_secret".to_string()),
                _ => None,
            }
        };
        assert!(!api_key_configured(
            "K",
            None,
            broken_plus_raw(missing.display().to_string())
        ));
        // ...and with a sibling present it resolves there, as the loader does.
        let sibling_dir = dir.join("with-sibling");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::write(
            sibling_dir.join(crate::setup::RFQ_API_KEY_FILE),
            "tx_live_sib\n",
        )
        .unwrap();
        assert!(api_key_configured(
            "K",
            Some(&sibling_dir),
            broken_plus_raw(missing.display().to_string())
        ));

        // A blank _FILE is not "set" at all — `read_env_secret` trims it and
        // does fall through to the raw variable there.
        let blank_file_plus_raw = |n: &str| match n {
            "K_FILE" => Some("   ".to_string()),
            "K" => Some("tx_live_secret".to_string()),
            _ => None,
        };
        assert!(api_key_configured("K", None, blank_file_plus_raw));

        // A relative path resolves against the bot directory, because that is
        // the child's working directory. Without a config dir there is nothing
        // to resolve against and it stays relative to the caller.
        let relative = only("K_FILE", "maker.key".to_string());
        assert!(api_key_configured("K", Some(&dir), relative));
        let elsewhere = dir.join("no-key-here");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(!api_key_configured(
            "K",
            Some(&elsewhere),
            only("K_FILE", "maker.key".to_string())
        ));

        // The raw variable alone, and blank values that don't count.
        assert!(api_key_configured("K", None, only("K", "tx_live".into())));
        assert!(!api_key_configured("K", None, only("K", "   ".into())));

        // The panel-written sibling, which must also be non-blank.
        std::fs::write(dir.join(crate::setup::RFQ_API_KEY_FILE), "tx_live_sib\n").unwrap();
        assert!(api_key_configured("K", Some(&dir), none));
        std::fs::write(dir.join(crate::setup::RFQ_API_KEY_FILE), "\n").unwrap();
        assert!(!api_key_configured("K", Some(&dir), none));

        std::fs::remove_dir_all(&dir).ok();
    }
    fn enable_modules(
        engine: &mut Engine,
        mode: crate::modules::Mode,
        settlement: u64,
        corridor: u64,
    ) -> std::path::PathBuf {
        use crate::modules::{
            runtime::{Balances, Runtime},
            ModulesConfig,
        };
        let dir = std::env::temp_dir().join(format!("stitch-rfq-module-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = ModulesConfig::default();
        cfg.mode = mode;
        cfg.spreads.enabled = false;
        let runtime = Runtime::new(cfg, &dir).unwrap();
        *runtime.balances.write().unwrap() = Some(Balances {
            settlement: U256::from(settlement),
            corridor: U256::from(corridor),
            at: unix_now(),
        });
        engine.modules = Some(runtime);
        engine.vault = Some(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
        );
        engine.reservations = Reservations::with_persist_path(dir.join(RESERVATIONS_FILE));
        *engine.trading_epoch.write().unwrap() = 3;
        *engine.vault_policy.write().unwrap() = Some(VaultQuotePolicy {
            settlement: DEBT.parse().unwrap(),
            corridor: COLLATERAL.parse().unwrap(),
            max_input_settlement: U256::MAX,
            max_input_corridor: U256::MAX,
            max_lifetime_secs: 120,
        });
        dir
    }
    #[tokio::test]
    async fn manual_sale_requires_local_authorization_and_survives_restart() {
        use crate::modules::{manual, Mode};
        let mut engine = test_engine();
        let dir = enable_modules(&mut engine, Mode::Live, 4_000_000_000, 6_000_000_000);
        engine.modules.as_mut().unwrap().config.rebalance.enabled = true;
        let sale = manual::Sale {
            id: "a".repeat(64),
            chain_id: 8453,
            vault: engine.vault.unwrap(),
            taker: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            corridor_token: COLLATERAL.parse().unwrap(),
            settlement_token: DEBT.parse().unwrap(),
            corridor_amount: U256::from(100_000_000u64),
            min_settlement: U256::from(99_500_000u64),
            expires_at: unix_now() + 3600,
            closed: false,
        };
        let mut req = exact_input_request(&format!("{}{}:rfq_1", manual::PREFIX, sale.id));
        req.sell_token = DEBT.into();
        req.buy_token = COLLATERAL.into();
        req.sell_amount = None;
        req.buy_amount = Some(sale.corridor_amount.to_string());
        let prices = fresh_prices();
        assert!(matches!(
            engine.respond(req.clone(), &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        manual::write(&dir, &sale).unwrap();
        let mut wrong_buyer = req.clone();
        wrong_buyer.taker = Address::repeat_byte(5).to_string();
        assert!(matches!(
            engine.respond(wrong_buyer, &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        let mut wrong_size = req.clone();
        wrong_size.buy_amount = Some("99999999".into());
        assert!(matches!(
            engine.respond(wrong_size, &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        let reply = engine.respond(req.clone(), &prices).await;
        let MakerFrame::QuoteResponse(first) = reply else {
            panic!("{reply:?}")
        };
        let bytes = alloy_primitives::hex::decode(&first.encoded_order).unwrap();
        let nonce = U256::from_be_slice(&bytes[8 * 32..9 * 32]);
        assert_eq!(nonce, manual::nonce(&dir, &sale.id, 3).unwrap());
        assert!(matches!(
            engine.respond(req.clone(), &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        // Simulate the reservation expiring; the same persisted nonce is reused.
        engine.reservations = Reservations::with_persist_path(dir.join("fresh-reservations.json"));
        req.rfq_id = format!("{}{}:rfq_2", manual::PREFIX, sale.id);
        let reply = engine.respond(req.clone(), &prices).await;
        let MakerFrame::QuoteResponse(next) = reply else {
            panic!("{reply:?}")
        };
        let next_bytes = alloy_primitives::hex::decode(&next.encoded_order).unwrap();
        assert_eq!(&bytes[8 * 32..9 * 32], &next_bytes[8 * 32..9 * 32]);
        manual::write(
            &dir,
            &manual::Sale {
                closed: true,
                ..sale
            },
        )
        .unwrap();
        engine.reservations = Reservations::new();
        assert!(matches!(
            engine.respond(req, &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn modules_shadow_preserves_firm_price_while_live_enforces_exposure() {
        let prices = fresh_prices();
        let mut shadow = test_engine();
        let _dir = enable_modules(
            &mut shadow,
            crate::modules::Mode::Shadow,
            1_000_000_000,
            9_000_000_000,
        );
        let MakerFrame::QuoteResponse(quote) =
            shadow.respond(exact_input_request("shadow"), &prices).await
        else {
            panic!("shadow must preserve base quotes");
        };
        assert_eq!(quote.buy_amount, "979902009");
        let mut live = test_engine();
        let _dir = enable_modules(
            &mut live,
            crate::modules::Mode::Live,
            1_000_000_000,
            9_000_000_000,
        );
        assert!(matches!(
            live.respond(exact_input_request("live"), &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        let levels = live.level_frames(&prices, unix_now_ms());
        let MakerFrame::Levels(levels) = &levels[0] else {
            panic!("expected levels");
        };
        assert!(
            levels.bids.is_empty(),
            "indicative levels must agree with firm quote veto"
        );
    }
    #[tokio::test]
    async fn modules_outstanding_buy_prevents_two_quotes_crossing_the_inventory_cap() {
        let mut engine = test_engine();
        let _dir = enable_modules(
            &mut engine,
            crate::modules::Mode::Live,
            7_000_000_000,
            3_000_000_000,
        );
        let prices = fresh_prices();
        assert!(matches!(
            engine.respond(exact_input_request("first"), &prices).await,
            MakerFrame::QuoteResponse(_)
        ));
        assert!(matches!(
            engine.respond(exact_input_request("second"), &prices).await,
            MakerFrame::QuoteReject(_)
        ));
    }

    fn rebalance_only_without_fresh_balances(stale: bool) -> (Engine, std::path::PathBuf) {
        let mut engine = test_engine();
        let dir = enable_modules(
            &mut engine,
            crate::modules::Mode::Live,
            2_000_000_000,
            8_000_000_000,
        );
        let modules = engine.modules.as_mut().unwrap();
        modules.config.inventory.enabled = false;
        modules.config.spreads.enabled = false;
        modules.config.rebalance.enabled = true;
        modules.config.rebalance.dealer = Some(crate::modules::config::DealerConfig {
            url: "http://127.0.0.1:1".into(),
            taker: "0x0000000000000000000000000000000000000003".into(),
            api_key_env: None,
        });
        if stale {
            modules.balances.write().unwrap().as_mut().unwrap().at = 0;
        } else {
            *modules.balances.write().unwrap() = None;
        }
        (engine, dir)
    }

    #[tokio::test]
    async fn rebalance_only_preserves_levels_without_fresh_module_balances() {
        for stale in [false, true] {
            let (mut engine, dir) = rebalance_only_without_fresh_balances(stale);
            let (mut baseline, _) = rebalance_only_without_fresh_balances(stale);
            baseline.modules = None;
            let prices = fresh_prices();
            let now_ms = unix_now_ms();
            let expected = baseline.level_frames(&prices, now_ms);
            assert!(!expected.is_empty());
            assert_eq!(
                serde_json::to_value(engine.level_frames(&prices, now_ms)).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "rebalance-only must preserve both quote sides (stale={stale})"
            );
            assert!(engine.pending_rebalance.is_none());
            assert!(engine.reservations.is_empty());
            assert!(
                !dir.join("modules-attempt.json").exists(),
                "rebalancing must still wait for fresh economic balances"
            );
        }
    }

    #[tokio::test]
    async fn rebalance_only_preserves_firm_quotes_without_fresh_module_balances() {
        for stale in [false, true] {
            for bid in [true, false] {
                let (mut engine, _) = rebalance_only_without_fresh_balances(stale);
                let (mut baseline, _) = rebalance_only_without_fresh_balances(stale);
                baseline.modules = None;
                let prices = fresh_prices();
                let mut request = exact_input_request("rebalance-only-customer");
                if !bid {
                    std::mem::swap(&mut request.sell_token, &mut request.buy_token);
                }
                let MakerFrame::QuoteResponse(expected) =
                    baseline.respond(request.clone(), &prices).await
                else {
                    panic!("baseline should quote both directions");
                };
                let reply = engine.respond(request, &prices).await;
                let MakerFrame::QuoteResponse(actual) = reply else {
                    panic!("rebalance-only should quote (stale={stale}, bid={bid}): {reply:?}");
                };
                assert_eq!(actual.sell_amount, expected.sell_amount);
                assert_eq!(actual.buy_amount, expected.buy_amount);
                assert_eq!(actual.fee_amount, expected.fee_amount);
            }
        }
    }

    #[tokio::test]
    async fn enabled_quote_strategies_still_require_module_balances() {
        for inventory in [true, false] {
            let (mut engine, _) = rebalance_only_without_fresh_balances(false);
            let cfg = &mut engine.modules.as_mut().unwrap().config;
            cfg.inventory.enabled = inventory;
            cfg.spreads.enabled = !inventory;
            let prices = fresh_prices();
            assert!(engine.level_frames(&prices, unix_now_ms()).is_empty());
            assert!(matches!(
                engine
                    .respond(exact_input_request("missing-balances"), &prices)
                    .await,
                MakerFrame::QuoteReject(_)
            ));
        }
    }

    #[tokio::test]
    async fn modules_stale_economic_balances_and_reversed_pair_stop_live_quotes() {
        let mut engine = test_engine();
        let _dir = enable_modules(
            &mut engine,
            crate::modules::Mode::Live,
            7_000_000_000,
            3_000_000_000,
        );
        let prices = fresh_prices();
        engine
            .modules
            .as_ref()
            .unwrap()
            .balances
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .at = unix_now() - 10;
        assert!(matches!(
            engine.respond(exact_input_request("stale"), &prices).await,
            MakerFrame::QuoteReject(_)
        ));
        engine
            .modules
            .as_ref()
            .unwrap()
            .balances
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .at = unix_now();
        let mut limits = engine.vault_policy.write().unwrap();
        let p = limits.as_mut().unwrap();
        std::mem::swap(&mut p.settlement, &mut p.corridor);
        drop(limits);
        assert!(matches!(
            engine
                .respond(exact_input_request("reversed"), &prices)
                .await,
            MakerFrame::QuoteReject(_)
        ));
    }
    #[tokio::test]
    async fn module_rebalance_reservations_survive_venue_release_frames_and_reload() {
        let mut engine = test_engine();
        let dir = enable_modules(
            &mut engine,
            crate::modules::Mode::Live,
            7_000_000_000,
            3_000_000_000,
        );
        let key = "module-rebalance:pending";
        engine.reservations.reserve_paying(
            key,
            "cngn-usdc",
            false,
            U256::from(100),
            unix_now() + 60,
            Some(COLLATERAL),
        );
        engine
            .dispatch(
                VenueFrame::QuoteExpired(QuoteExpiredFrame { rfq_id: key.into() }),
                &fresh_prices(),
            )
            .await;
        assert_eq!(engine.reservations.len(), 1);
        let loaded = Reservations::load(dir.join(RESERVATIONS_FILE), unix_now()).unwrap();
        assert_eq!(
            loaded.reserved("cngn-usdc", false, unix_now()),
            U256::from(100)
        );
    }

    struct RebalanceFixture {
        engine: Engine,
        prices: PriceCache,
        dir: std::path::PathBuf,
        received: Arc<Mutex<Option<serde_json::Value>>>,
        server: tokio::task::JoinHandle<()>,
    }

    async fn rebalance_fixture() -> RebalanceFixture {
        use crate::modules::{config::DealerConfig, dealer, runtime::Runtime, Mode};
        use axum::{routing::post, Json, Router};
        let received = Arc::new(Mutex::new(None::<serde_json::Value>));
        let seen = received.clone();
        let app = Router::new()
            .route(
                "/quote",
                post(|Json(r): Json<dealer::QuoteRequest>| async move {
                    Json(dealer::Quote {
                        request_id: r.request_id,
                        taker: "0x0000000000000000000000000000000000000003".into(),
                        sell_amount: r.sell_amount,
                        buy_amount: r.min_buy_amount,
                        expires_at: r.deadline,
                    })
                }),
            )
            .route(
                "/execute",
                post(move |Json(body): Json<serde_json::Value>| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().unwrap() = Some(body);
                        axum::http::StatusCode::ACCEPTED
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut engine = test_engine();
        let dir = enable_modules(&mut engine, Mode::Live, 2_000_000_000, 8_000_000_000);
        let previous = engine.modules.take().unwrap();
        let mut cfg = previous.config.clone();
        cfg.rebalance.enabled = true;
        cfg.rebalance.method = crate::modules::config::RebalanceMethod::Dealer;
        cfg.rebalance.dealer = Some(DealerConfig {
            url: format!("http://{addr}"),
            taker: "0x0000000000000000000000000000000000000003".into(),
            api_key_env: None,
        });
        let runtime = Runtime::new(cfg, &dir).unwrap();
        *runtime.balances.write().unwrap() = *previous.balances.read().unwrap();
        engine.modules = Some(runtime);
        RebalanceFixture {
            engine,
            prices: fresh_prices(),
            dir,
            received,
            server,
        }
    }

    #[tokio::test]
    async fn module_spot_rebalance_signs_only_after_reserving_and_never_counts_submission_as_fill()
    {
        let RebalanceFixture {
            mut engine,
            prices,
            dir,
            received,
            server,
        } = rebalance_fixture().await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                engine.module_tick(&prices, unix_now());
                if received.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dealer should receive one locally constructed order");
        let body = received.lock().unwrap().clone().unwrap();
        assert_eq!(body["chainId"], 8453);
        assert_eq!(body["strategySignature"].as_str().unwrap().len(), 132);
        let bytes = alloy_primitives::hex::decode(body["encodedOrder"].as_str().unwrap()).unwrap();
        assert_eq!(
            Address::from_slice(&bytes[7 * 32 + 12..8 * 32]),
            engine.vault.unwrap()
        );
        let loaded = Reservations::load(dir.join(RESERVATIONS_FILE), unix_now()).unwrap();
        assert_eq!(
            loaded.reserved("cngn-usdc", false, unix_now()),
            U256::from(200_000_000u64)
        );
        assert_eq!(
            engine
                .modules
                .as_ref()
                .unwrap()
                .balances
                .read()
                .unwrap()
                .unwrap()
                .corridor,
            U256::from(8_000_000_000u64),
            "submission must not manufacture a fill"
        );
        engine.module_tick(&prices, unix_now());
        assert_eq!(
            engine.reservations.len(),
            1,
            "no duplicate while the first order can still fill"
        );
        server.abort();
    }

    struct GatedSigner {
        inner: DynSigner,
        started: tokio::sync::Notify,
        resume: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl crate::signer::Signer for GatedSigner {
        async fn sign_digest(&self, digest: alloy_primitives::B256) -> anyhow::Result<[u8; 65]> {
            self.started.notify_one();
            self.resume.acquire().await.unwrap().forget();
            self.inner.sign_digest(digest).await
        }

        fn address(&self) -> Address {
            self.inner.address()
        }
    }

    async fn pending_rebalance_signer(
        engine: &mut Engine,
        prices: &PriceCache,
    ) -> Arc<GatedSigner> {
        let local = engine.signer.clone();
        let custody = Arc::new(GatedSigner {
            inner: local.clone(),
            started: tokio::sync::Notify::new(),
            resume: tokio::sync::Semaphore::new(0),
        });
        engine.signer = custody.clone();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while engine.pending_rebalance.is_none() {
                engine.module_tick(&prices, unix_now());
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            custody.started.notified().await;
        })
        .await
        .expect("rebalance must reach the custody signer");
        engine.signer = local;
        custody
    }
    #[tokio::test]
    async fn module_signature_is_withheld_when_customer_quote_arrives_during_signing() {
        let RebalanceFixture {
            mut engine,
            prices,
            dir,
            received,
            server,
        } = rebalance_fixture().await;
        let custody = pending_rebalance_signer(&mut engine, &prices).await;
        assert_eq!(
            engine.reservations.len(),
            1,
            "reserve before custody signing"
        );

        // The engine can quote a customer while custody approves the module order.
        let mut customer = exact_input_request("customer-during-rebalance-signing");
        customer.sell_token = DEBT.into();
        customer.buy_token = COLLATERAL.into();
        assert!(matches!(
            engine.respond(customer, &prices).await,
            MakerFrame::QuoteResponse(_)
        ));
        assert_eq!(engine.reservations.len(), 2);
        finish_rebalance_signing(&mut engine, &prices, &custody).await;
        assert!(
            received.lock().unwrap().is_none(),
            "dealer must not receive the overlapping order"
        );
        let loaded = Reservations::load(dir.join(RESERVATIONS_FILE), unix_now()).unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "withholding a signature does not release durable claims"
        );
        server.abort();
    }

    async fn finish_rebalance_signing(
        engine: &mut Engine,
        prices: &PriceCache,
        custody: &GatedSigner,
    ) {
        custody.resume.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while engine.pending_rebalance.is_some() {
                engine.module_tick(&prices, unix_now());
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("completed signature must return to the ledger owner");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn module_signature_is_rechecked_against_current_vault_state() {
        for changed in [
            "epoch",
            "order cap",
            "stale balances",
            "missing reservation",
        ] {
            let RebalanceFixture {
                mut engine,
                prices,
                received,
                server,
                ..
            } = rebalance_fixture().await;
            let custody = pending_rebalance_signer(&mut engine, &prices).await;
            match changed {
                "epoch" => *engine.trading_epoch.write().unwrap() += 1,
                "order cap" => {
                    engine
                        .vault_policy
                        .write()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .max_input_corridor = U256::from(1);
                }
                "stale balances" => {
                    engine
                        .modules
                        .as_ref()
                        .unwrap()
                        .balances
                        .write()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .at = 0;
                }
                "missing reservation" => engine.reservations = Reservations::new(),
                _ => unreachable!(),
            }
            finish_rebalance_signing(&mut engine, &prices, &custody).await;
            assert!(
                received.lock().unwrap().is_none(),
                "changed {changed} must withhold the signature"
            );
            server.abort();
        }
    }
}
