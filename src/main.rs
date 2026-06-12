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
use std::time::Duration;

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
use stitch_bot::funding::{count_max_sides, TickBudgets};
use stitch_bot::indexer::Indexer;
use stitch_bot::maker::{quote_side, QuoteState, Side, SideOutcome, TickCtx};
use stitch_bot::poster::Poster;
use stitch_bot::quote::oracle_rate_ray;
use stitch_bot::rpc::Wallet;
use stitch_bot::signer::{address_from_signing_key, parse_private_key};
use stitch_bot::slots::{load_slot_nonce_state, slot_nonce_state_path};
use stitch_bot::tick::{is_stale, unix_now};
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
                    "missing Permit2 approvals for {} token(s); run `stitch approve` first, or pass --dry-run to test without them",
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

    // Per closer pool: position id → unix time we last submitted a fill for it,
    // so a pending tx or lagging subgraph can't trigger a duplicate `fill()`.
    let mut closer_pending: HashMap<Address, HashMap<U256, u64>> = HashMap::new();
    let slot_nonce_state_path = slot_nonce_state_path(&config_path, cfg.chain_id, maker);
    let initial_next_nonce = unix_now().saturating_mul(1000);
    let (next_nonce, slot_nonces, slot_inputs) = if dry_run {
        (initial_next_nonce, HashMap::new(), HashMap::new())
    } else {
        load_slot_nonce_state(
            &slot_nonce_state_path,
            cfg.chain_id,
            maker,
            initial_next_nonce,
        )?
    };
    if !dry_run {
        info!(
            path = %slot_nonce_state_path.display(),
            slots = slot_nonces.len(),
            slot_inputs = slot_inputs.len(),
            next_nonce,
            "loaded slot nonce state"
        );
    }
    // `last_quote` (the last posted price per side, "buy:<pair>"/"sell:<pair>")
    // gates re-signing until the price moves past the refresh threshold or the
    // order nears its TTL (a flat market must still keep a live order).
    let mut state = QuoteState {
        last_quote: HashMap::new(),
        next_nonce,
        slot_nonces,
        slot_inputs,
    };
    let ctx = TickCtx {
        poster: &poster,
        wallet: &wallet,
        state_path: &slot_nonce_state_path,
    };
    // Sides quoting "max" liquidity target an even share of each token's funded
    // balance instead of letting the first corridor keep draining it.
    let max_sides_by_token = count_max_sides(&cfg);
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
        let mut budgets = TickBudgets::new(max_sides_by_token.clone());

        'pools: for pool in &cfg.pools {
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

            for side in [Side::Bid, Side::Ask] {
                let outcome = quote_side(
                    &ctx,
                    &mut state,
                    &mut budgets,
                    pool,
                    &pair,
                    debt,
                    collateral,
                    side,
                    quote.price,
                    now,
                )
                .await;
                if let SideOutcome::AbortPool = outcome {
                    // The nonce ledger could not be persisted; never post on a
                    // stale ledger. Skip the rest of this pool's tick.
                    continue 'pools;
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
