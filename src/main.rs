// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch entry point: read the feed, sign short-TTL limit orders, and post
//! them to the indexer — re-quoting as the market moves (the green leg). Each
//! tick also runs the blue leg: discover a pool's OPEN positions and close the
//! in-the-money front of the FIFO queue with `pool.fill()`, paying gas and
//! approving the debt asset as needed. The blue leg is active for any pool with
//! `closer_pool` configured and a `subgraph_url` set.
//!
//! Usage:
//!   STITCH_PRIVATE_KEY_FILE=stitch.key stitch --config stitch.toml
//!   STITCH_PRIVATE_KEY_FILE=stitch.key stitch --config stitch.toml --dry-run

use std::collections::HashMap;
use std::env::VarError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use anyhow::{anyhow, Context};
use k256::ecdsa::SigningKey;
use tracing::{info, warn};

use stitch_bot::approve::{run_approvals, unapproved_tokens, ApprovalMode};
use stitch_bot::banner::print_startup_banner;
use stitch_bot::cli::{parse, Command};
use stitch_bot::closer::discover::Discoverer;
use stitch_bot::closer::runner::{close_pool_once, CloseOutcome, CloserPool};
use stitch_bot::closer::strategy::{PoolParams, StrategyConfig};
use stitch_bot::config::{Config, PoolConfig};
use stitch_bot::feed::{HttpFeed, PriceFeed};
use stitch_bot::indexer::Indexer;
use stitch_bot::ladder::balanced_ladder;
use stitch_bot::quote::{ask_price, bid_price, buy_amounts_at, oracle_rate_ray, sell_amounts_at};
use stitch_bot::rpc::Wallet;
use stitch_bot::signer::{address_from_signing_key, parse_private_key};
use stitch_bot::submit::sign_submission;
use stitch_bot::tick::{is_stale, should_requote_now};
use stitch_bot::types::OrderParams;
use stitch_bot::update::{run_update, warn_if_outdated};

/// Build the blue-leg close target from a pool's config (assumes `closer_enabled`).
fn build_closer_pool(pool: &PoolConfig) -> anyhow::Result<CloserPool> {
    Ok(CloserPool {
        pool_address: pool
            .closer_pool
            .as_ref()
            .ok_or_else(|| anyhow!("closer_pool missing"))?
            .parse()
            .context("invalid closer_pool address")?,
        debt_token: pool.debt.parse().context("invalid debt address")?,
        params: PoolParams {
            floor_ray: pool
                .floor_ray
                .as_ref()
                .ok_or_else(|| anyhow!("floor_ray missing"))?
                .parse()
                .context("invalid floor_ray")?,
            buffer_ray: pool
                .buffer_ray
                .as_ref()
                .ok_or_else(|| anyhow!("buffer_ray missing"))?
                .parse()
                .context("invalid buffer_ray")?,
            window_secs: pool
                .window_secs
                .ok_or_else(|| anyhow!("window_secs missing"))?,
        },
        max_positions: pool.max_positions_per_fill.unwrap_or(10) as usize,
        discover_first: pool.discover_first.unwrap_or(200),
    })
}

/// Signs and posts one operator order to the indexer. Holds the static context
/// (key, reactor, permit2…) so the per-tick call sites stay small.
struct Poster<'a> {
    indexer: &'a Indexer,
    key: &'a SigningKey,
    permit2: Address,
    chain_id: u64,
    maker: Address,
    reactor: Address,
    dry_run: bool,
}

impl Poster<'_> {
    /// Build, sign, and POST one order. Returns true if it was posted (or would
    /// be, in dry-run) so the caller can record the quote.
    #[allow(clippy::too_many_arguments)]
    async fn post(
        &self,
        nonce: u64,
        deadline: u64,
        input_token: Address,
        input_amount: U256,
        output_token: Address,
        output_amount: U256,
        client_order_id: Option<String>,
        label: &str,
        price: f64,
    ) -> bool {
        if input_amount == U256::ZERO || output_amount == U256::ZERO {
            warn!(label, "zero-size order; skipping");
            return false;
        }
        let order = OrderParams {
            reactor: self.reactor,
            swapper: self.maker,
            nonce: U256::from(nonce),
            deadline: U256::from(deadline),
            input_token,
            input_amount,
            output_token,
            output_amount,
            recipient: self.maker,
        };
        let submission = match sign_submission(&order, self.permit2, self.chain_id, self.key) {
            Ok(mut s) => {
                s.client_order_id = client_order_id;
                s
            }
            Err(e) => {
                warn!(label, error = %e, "signing failed; skipping");
                return false;
            }
        };
        if self.dry_run {
            info!(label, price, input = %input_amount, output = %output_amount, "[dry-run] would post order");
            return true;
        }
        match self.indexer.submit(&submission).await {
            Ok(id) => {
                info!(label, price, id = %id, "posted order");
                true
            }
            Err(e) => {
                warn!(label, error = %e, "post failed");
                false
            }
        }
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PRIVATE_KEY_ENV: &str = "STITCH_PRIVATE_KEY";
const PRIVATE_KEY_FILE_ENV: &str = "STITCH_PRIVATE_KEY_FILE";

fn print_help() {
    println!(
        "stitch {VERSION}\n\
         The Textile filler network operator bot.\n\n\
         USAGE:\n    \
         STITCH_PRIVATE_KEY_FILE=/path/to/key stitch --config <path> [--dry-run]\n    \
         STITCH_PRIVATE_KEY_FILE=/path/to/key stitch approve --config <path> [--exact] [--dry-run]\n\n\
         COMMANDS:\n    \
         approve           Approve the config's input tokens to Permit2, then exit.\n                      \
         Required before going live; uses a max allowance unless --exact.\n\n\
         OPTIONS:\n    \
         --config <path>   Operator config (TOML). Read fresh on every start.\n    \
         --dry-run         Sign/plan without posting orders or sending tx.\n    \
         --exact           With `approve`: approve only the committed liquidity,\n                      \
         not an unlimited allowance (must re-approve as it's spent).\n    \
         --update          Update to the latest release, then exit.\n    \
         -V, --version     Print version and exit.\n    \
         -h, --help        Print this help and exit.\n\n\
         The wallet key is never in the config — pass STITCH_PRIVATE_KEY or \
         STITCH_PRIVATE_KEY_FILE in the env."
    );
}

/// A future that resolves on the first shutdown signal: Ctrl-C on any platform,
/// plus SIGTERM on Unix (what systemd and `docker stop` send).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "could not install SIGTERM handler; Ctrl-C only");
                ctrl_c.await;
                return;
            }
        };
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

fn load_key() -> anyhow::Result<SigningKey> {
    let raw = load_key_material()?;
    parse_private_key(&raw)
}

fn load_key_material() -> anyhow::Result<String> {
    load_key_material_from_vars(
        std::env::var(PRIVATE_KEY_FILE_ENV),
        std::env::var(PRIVATE_KEY_ENV),
    )
}

fn load_key_material_from_vars(
    key_file: Result<String, VarError>,
    key: Result<String, VarError>,
) -> anyhow::Result<String> {
    match key_file {
        Ok(path) => return read_private_key_file(&path),
        Err(VarError::NotPresent) => {}
        Err(VarError::NotUnicode(_)) => {
            return Err(anyhow!("{PRIVATE_KEY_FILE_ENV} must be valid unicode"));
        }
    }
    key.with_context(|| format!("set {PRIVATE_KEY_FILE_ENV} or {PRIVATE_KEY_ENV}"))
}

fn read_private_key_file(path: &str) -> anyhow::Result<String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("{PRIVATE_KEY_FILE_ENV} is empty"));
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {PRIVATE_KEY_FILE_ENV} {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn temp_key_file(contents: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "stitch-test-key-{}-{}.txt",
            std::process::id(),
            unix_now()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn key_material_uses_direct_env_when_no_file_is_set() {
        let raw = load_key_material_from_vars(Err(VarError::NotPresent), Ok(TEST_KEY.into()))
            .expect("key loads");
        assert_eq!(raw, TEST_KEY);
    }

    #[test]
    fn key_material_file_takes_precedence_over_direct_env() {
        let path = temp_key_file(&format!("0x{TEST_KEY}\n"));
        let raw = load_key_material_from_vars(
            Ok(path.to_string_lossy().into_owned()),
            Ok("0x0000000000000000000000000000000000000000000000000000000000000001".into()),
        )
        .expect("key file loads");
        std::fs::remove_file(path).unwrap();
        assert_eq!(raw.trim(), format!("0x{TEST_KEY}"));
    }

    #[test]
    fn key_material_requires_some_source() {
        let err = load_key_material_from_vars(Err(VarError::NotPresent), Err(VarError::NotPresent))
            .expect_err("missing key source should fail");
        assert!(err.to_string().contains(PRIVATE_KEY_FILE_ENV));
        assert!(err.to_string().contains(PRIVATE_KEY_ENV));
    }

    #[test]
    fn empty_key_file_path_is_an_error() {
        let err =
            load_key_material_from_vars(Ok(" ".into()), Ok(TEST_KEY.into())).expect_err("empty");
        assert!(err.to_string().contains(PRIVATE_KEY_FILE_ENV));
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_u128(value: &str, field: &str) -> anyhow::Result<u128> {
    value
        .parse::<u128>()
        .with_context(|| format!("invalid {field}"))
}

fn u256_to_u128(value: U256, field: &str) -> anyhow::Result<u128> {
    value
        .to_string()
        .parse::<u128>()
        .with_context(|| format!("{field} does not fit in u128"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match parse(std::env::args().skip(1))? {
        Command::Version => {
            println!("stitch {VERSION}");
            Ok(())
        }
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Update => run_update().await,
        Command::Approve {
            config,
            dry_run,
            exact,
        } => run_approve(config, dry_run, exact).await,
        Command::Run { config, dry_run } => run(config, dry_run).await,
    }
}

/// `stitch approve`: ensure the config's input tokens are approved to Permit2,
/// then exit. Max allowance by default; `--exact` approves only the committed
/// liquidity. `--dry-run` reports without sending.
async fn run_approve(config_path: String, dry_run: bool, exact: bool) -> anyhow::Result<()> {
    let cfg = Config::from_toml(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading config {config_path}"))?,
    )?;
    let key = load_key()?;
    let permit2: Address = cfg.permit2.parse().context("invalid permit2 address")?;
    let wallet = Wallet::new(cfg.rpc_url.clone(), key, cfg.chain_id);
    let mode = if exact {
        ApprovalMode::Exact
    } else {
        ApprovalMode::Max
    };
    info!(
        maker = %wallet.address(), chain_id = cfg.chain_id, mode = ?mode, dry_run,
        "stitch approve: ensuring Permit2 approvals"
    );
    let sent = run_approvals(&wallet, permit2, &cfg, mode, dry_run).await?;
    if dry_run {
        info!("dry-run complete; no transactions sent");
    } else {
        info!(approvals_sent = sent, "approvals complete");
    }
    Ok(())
}

async fn run(config_path: String, dry_run: bool) -> anyhow::Result<()> {
    let cfg = Config::from_toml(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading config {config_path}"))?,
    )?;
    let key = load_key()?;
    let maker = address_from_signing_key(&key);
    let permit2: Address = cfg.permit2.parse().context("invalid permit2 address")?;
    let reactor: Address = cfg.reactor.parse().context("invalid reactor address")?;

    print_startup_banner();

    info!(
        version = VERSION, maker = %maker, chain_id = cfg.chain_id,
        pools = cfg.pools.len(), dry_run, "Stitch starting"
    );
    warn_if_outdated().await;

    // One feed per distinct URL — pools can point at different price sources
    // (a shared feed can't price cNGN, COPM, and KES at once). Falls back to the
    // bot-level `[feed]` when a pool sets no `feed_url`.
    let mut feeds: HashMap<String, HttpFeed> = HashMap::new();
    for pool in &cfg.pools {
        let url = pool
            .feed_url
            .clone()
            .unwrap_or_else(|| cfg.feed.url.clone());
        feeds
            .entry(url.clone())
            .or_insert_with(|| HttpFeed::new(&url));
    }
    let indexer = Indexer::new(format!("{}/graphql", cfg.indexer_url.trim_end_matches('/')));

    // Blue-leg I/O: a signing wallet (pays gas, sends fill()) and the subgraph
    // discoverer. The discoverer is only built when a subgraph is configured.
    let wallet = Wallet::new(cfg.rpc_url.clone(), key.clone(), cfg.chain_id);

    // Preflight: a maker can't fill orders it hasn't approved Permit2 to pull,
    // so block a live start on a missing approval (orders would post but
    // silently revert on fill). In dry-run we only warn, so signing can still be
    // exercised offline. A flaky RPC shouldn't hard-block dry-run, but a live
    // start stays cautious and surfaces the error.
    match unapproved_tokens(&wallet, permit2, &cfg).await {
        Ok(missing) if !missing.is_empty() => {
            for m in &missing {
                warn!(token = %m.token, reasons = ?m.reasons, "input token not approved to Permit2");
            }
            if dry_run {
                warn!("missing Permit2 approvals: orders would post but fail to fill. Run `stitch approve` before going live.");
            } else {
                anyhow::bail!(
                    "missing Permit2 approvals for {} token(s); run `stitch approve` (or `stitch approve --exact`) first, or pass --dry-run to test without them",
                    missing.len()
                );
            }
        }
        Ok(_) => info!("Permit2 approvals present for all enabled sides"),
        Err(e) if dry_run => {
            warn!(error = %e, "could not verify Permit2 approvals; continuing dry-run")
        }
        Err(e) => return Err(e).context("verifying Permit2 approvals"),
    }

    let discoverer = cfg
        .subgraph_url
        .as_ref()
        .map(|u| Discoverer::new(u.clone()));
    let closer_pools = cfg.pools.iter().filter(|p| p.closer_enabled()).count();
    info!(
        blue_leg = discoverer.is_some(),
        closer_pools, "closer configured"
    );

    let poster = Poster {
        indexer: &indexer,
        key: &key,
        permit2,
        chain_id: cfg.chain_id,
        maker,
        reactor,
        dry_run,
    };

    // Last quoted price per side ("buy:<pair>" / "sell:<pair>"), to skip
    // re-signing until the price moves past the refresh threshold.
    // Per side ("buy:<pair>" / "sell:<pair>"): the last quoted price and when it
    // was posted, so we re-sign on a price move *or* before the order's TTL
    // lapses (a flat market must still keep a live order).
    let mut last_quote: HashMap<String, (f64, u64)> = HashMap::new();
    // Per closer pool: position id → unix time we last submitted a fill for it,
    // so a pending tx or lagging subgraph can't trigger a duplicate `fill()`.
    let mut closer_pending: HashMap<Address, HashMap<U256, u64>> = HashMap::new();
    let mut nonce: u64 = unix_now().saturating_mul(1000);
    let mut interval = tokio::time::interval(Duration::from_secs(cfg.tick_interval_secs.max(1)));

    // Exit cleanly on Ctrl-C / SIGTERM. We only check between ticks, so a signal
    // mid-tick lets the current tick finish (no half-sent fill or dangling sign)
    // before we stop.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received; stopping after current tick");
                return Ok(());
            }
            _ = interval.tick() => {}
        }
        let now = unix_now();

        for pool in &cfg.pools {
            // Each pool prices off its own feed (or the bot-level default).
            let feed_url = pool.feed_url.as_deref().unwrap_or(cfg.feed.url.as_str());
            let quote = match feeds
                .get(feed_url)
                .expect("feed built at startup")
                .fetch()
                .await
            {
                Ok(q) => q,
                Err(e) => {
                    warn!(feed = %feed_url, error = %e, "feed fetch failed; skipping pool");
                    continue;
                }
            };
            if is_stale(quote.timestamp, now, cfg.feed.staleness_secs) {
                warn!(feed = %feed_url, feed_ts = quote.timestamp, now, "stale feed; skipping pool");
                continue;
            }

            let (debt, collateral): (Address, Address) =
                match (pool.debt.parse(), pool.collateral.parse()) {
                    (Ok(d), Ok(c)) => (d, c),
                    _ => {
                        warn!(pool = %pool.collateral, "invalid token address; skipping");
                        continue;
                    }
                };
            let pair = format!(
                "{}/{}",
                pool.collateral.to_lowercase(),
                pool.debt.to_lowercase()
            );
            let deadline = now + pool.ttl_secs;

            // BID — buy collateral (cNGN) below mid with debt (USDT). "Buy low."
            if let Some(spread) = pool.buy_spread() {
                let bid = bid_price(quote.price, spread);
                let key_id = format!("buy:{pair}");
                if should_requote_now(
                    last_quote.get(&key_id).copied(),
                    bid,
                    pool.refresh_threshold_bps,
                    now,
                    pool.ttl_secs,
                ) {
                    let sizes = if pool.buy_ladder_enabled() {
                        match (
                            pool.buy_total_liquidity_debt.as_deref(),
                            pool.buy_min_slice_debt.as_deref(),
                        ) {
                            (Some(total), Some(min)) => match (
                                parse_u128(total, "buy_total_liquidity_debt"),
                                parse_u128(min, "buy_min_slice_debt"),
                            ) {
                                (Ok(total), Ok(min)) => balanced_ladder(
                                    total,
                                    min,
                                    pool.buy_max_orders.unwrap_or(150) as usize,
                                ),
                                (Err(e), _) | (_, Err(e)) => {
                                    warn!(pair = %pair, error = %e, "invalid buy ladder; skipping bid");
                                    Vec::new()
                                }
                            },
                            _ => Vec::new(),
                        }
                    } else if let Some(size_str) = &pool.buy_order_size_debt {
                        match parse_u128(size_str, "buy_order_size_debt") {
                            Ok(size) => vec![size],
                            Err(e) => {
                                warn!(pair = %pair, error = %e, "invalid buy_order_size_debt; skipping bid");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    };

                    let laddered = pool.buy_ladder_enabled();
                    let mut posted = 0usize;
                    for (i, size) in sizes.into_iter().enumerate() {
                        let (input, output) =
                            buy_amounts_at(bid, size, pool.debt_decimals, pool.collateral_decimals);
                        nonce += 1;
                        if poster
                            .post(
                                nonce,
                                deadline,
                                debt,
                                input,
                                collateral,
                                output,
                                laddered.then(|| format!("bid:{i}")),
                                "bid",
                                bid,
                            )
                            .await
                        {
                            posted += 1;
                        }
                    }
                    if posted > 0 {
                        info!(pair = %pair, orders = posted, "posted bid ladder");
                        last_quote.insert(key_id, (bid, now));
                    }
                }
            }

            // ASK — sell collateral (cNGN) above mid for debt (USDT). "Sell high."
            if let Some(spread) = pool.sell_spread() {
                let ask = ask_price(quote.price, spread);
                let key_id = format!("sell:{pair}");
                if should_requote_now(
                    last_quote.get(&key_id).copied(),
                    ask,
                    pool.refresh_threshold_bps,
                    now,
                    pool.ttl_secs,
                ) {
                    let sizes = if pool.sell_ladder_enabled() {
                        match (
                            pool.sell_total_liquidity_collateral.as_deref(),
                            pool.sell_min_slice_debt.as_deref(),
                        ) {
                            (Some(total_collateral), Some(min_debt)) => {
                                match (
                                    parse_u128(total_collateral, "sell_total_liquidity_collateral"),
                                    parse_u128(min_debt, "sell_min_slice_debt"),
                                ) {
                                    (Ok(total_collateral), Ok(min_debt)) => {
                                        let (_, total_debt) = sell_amounts_at(
                                            ask,
                                            total_collateral,
                                            pool.debt_decimals,
                                            pool.collateral_decimals,
                                        );
                                        match u256_to_u128(total_debt, "sell total debt equivalent")
                                        {
                                            Ok(total_debt) => balanced_ladder(
                                                total_debt,
                                                min_debt,
                                                pool.sell_max_orders.unwrap_or(150) as usize,
                                            ),
                                            Err(e) => {
                                                warn!(pair = %pair, error = %e, "invalid sell ladder; skipping ask");
                                                Vec::new()
                                            }
                                        }
                                    }
                                    (Err(e), _) | (_, Err(e)) => {
                                        warn!(pair = %pair, error = %e, "invalid sell ladder; skipping ask");
                                        Vec::new()
                                    }
                                }
                            }
                            _ => Vec::new(),
                        }
                    } else if let Some(size_str) = &pool.sell_order_size_collateral {
                        match parse_u128(size_str, "sell_order_size_collateral") {
                            Ok(size) => vec![size],
                            Err(e) => {
                                warn!(pair = %pair, error = %e, "invalid sell_order_size_collateral; skipping ask");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    };

                    let laddered = pool.sell_ladder_enabled();
                    let mut posted = 0usize;
                    for (i, size) in sizes.into_iter().enumerate() {
                        let (input, output) = if pool.sell_ladder_enabled() {
                            let (_, collateral_for_debt) = buy_amounts_at(
                                ask,
                                size,
                                pool.debt_decimals,
                                pool.collateral_decimals,
                            );
                            match u256_to_u128(collateral_for_debt, "sell ladder collateral slice")
                            {
                                Ok(collateral_size) => sell_amounts_at(
                                    ask,
                                    collateral_size,
                                    pool.debt_decimals,
                                    pool.collateral_decimals,
                                ),
                                Err(e) => {
                                    warn!(pair = %pair, error = %e, "invalid sell ladder slice; skipping ask order");
                                    continue;
                                }
                            }
                        } else {
                            let (input, output) = sell_amounts_at(
                                ask,
                                size,
                                pool.debt_decimals,
                                pool.collateral_decimals,
                            );
                            (input, output)
                        };
                        nonce += 1;
                        if poster
                            .post(
                                nonce,
                                deadline,
                                collateral,
                                input,
                                debt,
                                output,
                                laddered.then(|| format!("ask:{i}")),
                                "ask",
                                ask,
                            )
                            .await
                        {
                            posted += 1;
                        }
                    }
                    if posted > 0 {
                        info!(pair = %pair, orders = posted, "posted ask ladder");
                        last_quote.insert(key_id, (ask, now));
                    }
                }
            }

            // ----- Blue leg: close this pool's in-the-money auction positions. -----
            if let Some(discoverer) = discoverer.as_ref() {
                if pool.closer_enabled() {
                    let closer_pool = match build_closer_pool(pool) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "invalid closer config; skipping pool");
                            continue;
                        }
                    };
                    let strategy = StrategyConfig {
                        oracle_rate_ray: oracle_rate_ray(
                            quote.price,
                            pool.debt_decimals,
                            pool.collateral_decimals,
                        ),
                        min_margin_collateral: pool
                            .min_margin_collateral
                            .as_deref()
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(U256::ZERO),
                        skip_past_window: pool.skip_past_window.unwrap_or(true),
                        now,
                    };
                    let pending = closer_pending.entry(closer_pool.pool_address).or_default();
                    match close_pool_once(
                        &wallet,
                        discoverer,
                        &closer_pool,
                        &strategy,
                        dry_run,
                        pending,
                    )
                    .await
                    {
                        Ok(CloseOutcome::Nothing) => {}
                        Ok(CloseOutcome::Planned { positions, debt_in }) => info!(
                            pool = %closer_pool.pool_address, positions, debt_in = %debt_in,
                            "[dry-run] would close batch"
                        ),
                        Ok(CloseOutcome::Filled {
                            hash,
                            positions,
                            debt_in,
                        }) => info!(
                            pool = %closer_pool.pool_address, hash = %hash, positions,
                            debt_in = %debt_in, "closed batch"
                        ),
                        Err(e) => {
                            warn!(pool = %closer_pool.pool_address, error = %e, "close tick failed")
                        }
                    }
                }
            }
        }
    }
}
