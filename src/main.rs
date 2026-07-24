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
use std::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::{anyhow, Context};
use tracing::{info, warn};

use stitch_bot::approve::{run_approvals, unapproved_tokens, ApprovalMode};
use stitch_bot::banner::print_startup_banner;
use stitch_bot::cli::{parse, Command};
use stitch_bot::closer::discover::Discoverer;
use stitch_bot::closer::executor::encode_balance_of;
use stitch_bot::closer::runner::{close_pool_once, CloseOutcome, CloserPool};
use stitch_bot::closer::strategy::{PoolParams, StrategyConfig};
use stitch_bot::config::{Config, PoolConfig};
use stitch_bot::feed::{HttpFeed, PriceFeed};
use stitch_bot::funding::{count_max_sides, u256_to_u128, TickBudgets};
use stitch_bot::indexer::Indexer;
use stitch_bot::lean::{LeanDecision, LeanMode, LeanParams, LeanState};
use stitch_bot::maker::{quote_side, QuoteState, Side, SideOutcome, TickCtx};
use stitch_bot::poster::Poster;
use stitch_bot::quote::oracle_rate_ray;
use stitch_bot::quote::{ask_price, bid_price, SpotDeviationGuard};
use stitch_bot::rpc::Wallet;
use stitch_bot::setup;
use stitch_bot::signer::{address_from_signing_key, build_signer};
use stitch_bot::slots::{load_slot_nonce_state, slot_nonce_state_path};
use stitch_bot::taker::{resolve_fee_bps, take_pool_once, TakeOutcome, TakerCtx};
use stitch_bot::tick::{is_price_usable, is_stale, unix_now};
use stitch_bot::twap::Twap;
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

/// The maker's on-chain inventory for a pair: `(collateral, debt)` wallet
/// balances, atomic. Signed orders don't move tokens until they fill (Permit2),
/// so the wallet balance IS the inventory.
async fn read_inventory(
    wallet: &stitch_bot::rpc::Wallet,
    maker: Address,
    collateral: Address,
    debt: Address,
) -> anyhow::Result<(u128, u128)> {
    let calldata = Bytes::from(encode_balance_of(maker));
    let coll = wallet
        .read_uint(collateral, &calldata)
        .await
        .context("reading collateral balance")?;
    let debt_bal = wallet
        .read_uint(debt, &calldata)
        .await
        .context("reading debt balance")?;
    Ok((
        u256_to_u128(coll, "collateral balance")?,
        u256_to_u128(debt_bal, "debt balance")?,
    ))
}

/// One log line per tick for a lean pool. Shadow mode prints the lean quotes
/// next to the live (configured-spread) ones — the rollout's verification
/// dataset; live mode prints what the book is actually quoting. Either way,
/// warn loudly if a lean quote ever lands on the wrong side of the fair floor
/// (impossible by construction — seeing this means a bug or bad params).
fn log_lean(
    mode: LeanMode,
    pair: &str,
    pool: &PoolConfig,
    fair: f64,
    params: &LeanParams,
    d: &LeanDecision,
) {
    let px = |p: Option<f64>| p.map_or_else(|| "pulled".to_string(), |v| format!("{v:.6}"));
    let bid_bound = fair * (1.0 - params.floor_bps / 10_000.0);
    let ask_bound = fair * (1.0 + params.floor_bps / 10_000.0);
    let bid_ok = d.bid.is_none_or(|b| b <= bid_bound * (1.0 + 1e-12));
    let ask_ok = d.ask.is_none_or(|a| a >= ask_bound * (1.0 - 1e-12));
    if !bid_ok || !ask_ok {
        warn!(
            pair = %pair, fair, bid = %px(d.bid), ask = %px(d.ask),
            floor_bps = params.floor_bps,
            "lean quote crossed the fair floor — this must never happen; check lean params"
        );
    }
    let x = d.x.map_or_else(|| "-".to_string(), |x| format!("{x:.3}"));
    match mode {
        LeanMode::Shadow => {
            let fmt = |p: Option<f64>| p.map_or_else(|| "-".to_string(), |v| format!("{v:.6}"));
            let live_bid = pool.buy_spread().map(|s| bid_price(fair, s));
            let live_ask = pool.sell_spread().map(|s| ask_price(fair, s));
            info!(
                pair = %pair, fair, x = %x,
                lean_bid = %px(d.bid), lean_ask = %px(d.ask),
                live_bid = %fmt(live_bid), live_ask = %fmt(live_ask),
                pulled = d.pulled.unwrap_or(""),
                "lean shadow"
            );
        }
        LeanMode::Live => info!(
            pair = %pair, fair, x = %x,
            bid = %px(d.bid), ask = %px(d.ask),
            pulled = d.pulled.unwrap_or(""),
            "lean quote"
        ),
        LeanMode::Off => {}
    }
}

fn print_help() {
    println!(
        "stitch {VERSION}\n\
         The Textile filler network operator bot.\n\n\
         USAGE:\n    \
         STITCH_PRIVATE_KEY_FILE=/path/to/key stitch --config <path> [--dry-run]\n    \
         STITCH_PRIVATE_KEY_FILE=/path/to/key stitch approve --config <path> [--exact] [--dry-run]\n\n\
         COMMANDS:\n    \
         approve           Approve the config's input tokens to Permit2, then exit.\n                      \
         Required before going live; uses a max allowance unless --exact.\n    \
         init              Interactively create stitch.toml/.env/.key, then exit.\n\n\
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Only emit ANSI color codes when stdout is a real terminal. When the output
    // is piped (the desktop app captures it) or redirected to a file/journald,
    // colors would otherwise show up as literal escape sequences.
    let use_ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_ansi(use_ansi)
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
        Command::Init { dir } => run_init(dir),
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
    let signer = build_signer(&cfg).await?;
    let permit2: Address = cfg.permit2.parse().context("invalid permit2 address")?;
    let wallet = Wallet::new(cfg.rpc_url.clone(), signer, cfg.chain_id);
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

/// `stitch init`: pick a corridor, take the key without echo, write the config.
fn run_init(dir: Option<String>) -> anyhow::Result<()> {
    use std::io::Write;
    use zeroize::Zeroize;

    // Default to the current working directory, not the executable's location:
    // the install guides tell operators to `cd ~/Stitch && stitch init`, and the
    // installer puts the binary elsewhere (e.g. ~/.cargo/bin).
    let target = match dir {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_dir().context("could not determine the current directory")?,
    };

    if setup::has_operator_files(&target) {
        println!(
            "{} already has Stitch operator files (overwriting replaces stitch.toml \
             and the stitch.key private key).",
            target.display()
        );
        print!("Overwrite it? [y/N]: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Left the existing config untouched.");
            return Ok(());
        }
    }

    let corridors = setup::catalog();
    println!("\nChoose a corridor:");
    for (i, c) in corridors.iter().enumerate() {
        println!("  {}) {} — {}", i + 1, c.display_name, c.network_label);
    }
    print!("Enter a number [1]: ");
    std::io::stdout().flush().ok();
    let mut choice = String::new();
    std::io::stdin().read_line(&mut choice)?;
    let idx = match choice.trim() {
        "" => 0,
        s => s
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1 && *n <= corridors.len())
            .map(|n| n - 1)
            .ok_or_else(|| anyhow!("not a valid choice: {s}"))?,
    };
    let corridor = &corridors[idx];

    // When stdin is not a TTY (e.g. piped input in smoke-tests), rpassword's
    // default console path fails. Fall back to reading from stdin directly.
    // `IsTerminal` is cross-platform, so this builds on Windows too.
    let mut key = {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            rpassword::prompt_password("\nPaste the operator wallet private key (hidden): ")?
        } else {
            print!("\nPaste the operator wallet private key (hidden): ");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let trimmed = line.trim().to_string();
            line.zeroize();
            trimmed
        }
    };
    let parsed =
        stitch_bot::signer::parse_private_key(&key).context("that private key is not valid hex")?;
    let operator = address_from_signing_key(&parsed);
    println!("Operator wallet: {operator:?}");

    let paths = setup::write_config(&target, corridor, &key)?;
    key.zeroize();

    println!("\nConfig written to {}", paths.dir.display());
    println!("  {}", paths.toml.display());
    println!("  {}", paths.env.display());
    println!("  {} (key, owner-only)", paths.key.display());
    println!("\nNext steps:");
    print_next_steps(&paths);
    Ok(())
}

/// Print copy-pasteable approve + dry-run commands for the host shell, with the
/// key/config paths quoted so directories with spaces don't break.
#[cfg(unix)]
fn print_next_steps(paths: &setup::ConfigPaths) {
    // POSIX single-quote. Source stitch.env (it sets STITCH_PRIVATE_KEY_FILE).
    let q = |s: String| format!("'{}'", s.replace('\'', "'\\''"));
    let env = q(paths.env.display().to_string());
    let toml = q(paths.toml.display().to_string());
    println!("  set -a; . {env}; set +a");
    println!("  stitch approve --config {toml}");
    println!("  stitch --config {toml} --dry-run");
}

/// PowerShell variant: `VAR=value cmd` is POSIX-only and doesn't set an env var
/// for the child on Windows, so set `$env:` then run. Single-quote for literals.
#[cfg(windows)]
fn print_next_steps(paths: &setup::ConfigPaths) {
    let q = |s: String| format!("'{}'", s.replace('\'', "''"));
    let key = q(paths.key.display().to_string());
    let toml = q(paths.toml.display().to_string());
    println!("  $env:STITCH_PRIVATE_KEY_FILE = {key}");
    println!("  stitch approve --config {toml}");
    println!("  stitch --config {toml} --dry-run");
}

async fn run(config_path: String, dry_run: bool) -> anyhow::Result<()> {
    let cfg = Config::from_toml(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading config {config_path}"))?,
    )?;
    let signer = build_signer(&cfg).await?;
    let maker = signer.address();
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
    let wallet = Wallet::new(cfg.rpc_url.clone(), signer.clone(), cfg.chain_id);

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

    // Taker leg: the reactor's native fee is part of every fill's cost, so
    // read it on-chain once (the controller hard-caps it at 5 bps). If the
    // read fails the leg stays off for this run — never guess a fee.
    let taker_pools = cfg.pools.iter().filter(|p| p.limit_taker_enabled()).count();
    let taker_fee_bps = if taker_pools > 0 {
        match resolve_fee_bps(&wallet, reactor).await {
            Ok(fee) => {
                info!(
                    taker_pools,
                    fee_bps = fee,
                    "limit-order taker leg configured"
                );
                Some(fee)
            }
            Err(e) => {
                warn!(error = %e, "could not read the reactor fee; taker leg disabled for this run");
                None
            }
        }
    } else {
        None
    };

    let poster = Poster {
        indexer: &indexer,
        signer: signer.clone(),
        permit2,
        chain_id: cfg.chain_id,
        maker,
        reactor,
        dry_run,
    };

    // Per closer pool: position id → unix time we last submitted a fill for it,
    // so a pending tx or lagging subgraph can't trigger a duplicate `fill()`.
    let mut closer_pending: HashMap<Address, HashMap<U256, u64>> = HashMap::new();
    // Per pair: resting-order id → unix time we last submitted an executeBatch
    // for it, so a pending fill or indexer lag can't double-take an order.
    let mut taker_pending: HashMap<String, HashMap<String, u64>> = HashMap::new();
    // Per pool: unix time of the last heartbeat line. A working bot at steady
    // state posts nothing most ticks (fully committed, or a side has no
    // inventory), so without this the log looks dead. We emit a line whenever a
    // side posts, and at least every HEARTBEAT_SECS otherwise, so "it's alive"
    // and "why a side is quiet" are always visible.
    let mut last_heartbeat: HashMap<String, u64> = HashMap::new();
    const HEARTBEAT_SECS: u64 = 60;
    let slot_nonce_state_path = slot_nonce_state_path(&config_path, cfg.chain_id, maker);
    let initial_next_nonce = unix_now().saturating_mul(1000);
    let (next_nonce, slot_nonces, slot_inputs, slot_deadlines) = if dry_run {
        (
            initial_next_nonce,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        )
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
        slot_deadlines,
    };
    let ctx = TickCtx {
        poster: &poster,
        wallet: &wallet,
        state_path: &slot_nonce_state_path,
    };
    // Sides quoting "max" liquidity target an even share of each token's funded
    // balance instead of letting the first corridor keep draining it.
    let max_sides_by_token = count_max_sides(&cfg);
    // Per pair: the inventory lean's smoothed offsets, share, and jump guard.
    let mut lean_states: HashMap<String, LeanState> = HashMap::new();
    // Per pool index: the rolling TWAP the maker/taker center on when
    // configured. Keyed by index (not token pair) so two [[pools]] blocks that
    // share a pair — or override feed_url differently — each keep their own
    // history and window instead of silently sharing one Twap.
    let mut twaps: HashMap<usize, Twap> = HashMap::new();
    for pool in &cfg.pools {
        if let Some(window) = pool.twap_window() {
            info!(
                collateral = %pool.collateral,
                window_secs = window,
                max_deviation_bps = pool.twap_deviation_bps(),
                "TWAP quoting on: spreads center on the rolling feed average"
            );
            if pool.refresh_threshold_bps > 0 {
                warn!(
                    collateral = %pool.collateral,
                    refresh_threshold_bps = pool.refresh_threshold_bps,
                    "a re-quote deadband on a slow-moving TWAP center lets quotes sit stale \
                     between threshold crossings; set refresh_threshold_bps = 0 (or remove it) \
                     to keep the book pinned to the smoothed center"
                );
            }
        }
    }
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
        let mut budgets = TickBudgets::new(max_sides_by_token.clone());

        'pools: for (pool_idx, pool) in cfg.pools.iter().enumerate() {
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
            // Read the clock per pool, AFTER the awaited fetch — not once at
            // tick start. Earlier pools' signing/posting awaits (or the fetch
            // itself) can stall past the TWAP window; a tick-start `now` would
            // understate that gap, defeat the TWAP's bot-observation-gap
            // reset, and place the averaging window in the pre-stall past.
            let now = unix_now();
            if is_stale(quote.timestamp, now, cfg.feed.staleness_secs) {
                warn!(feed = %feed_url, feed_ts = quote.timestamp, now, "stale feed; skipping pool");
                continue;
            }
            // A fresh timestamp with a garbage price (zero, negative, NaN) is
            // as untradeable as a stale one: go dark for the pool instead of
            // letting the maker, taker, closer — or a TWAP carrying an old
            // valid sample — keep quoting through the malfunction.
            if !is_price_usable(quote.price) {
                warn!(feed = %feed_url, price = quote.price, "unusable feed price; skipping pool");
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

            // ----- Price surface. `spot` is this tick's feed value; `mid` is
            // the center the maker, taker, and lean quote off — the rolling
            // TWAP when configured, so the book holds near the settled mean
            // instead of chasing every tick. The guard bounds how far a
            // lagging center may post through spot on a persistent move. -----
            let spot = quote.price;
            let (mid, guard) = match pool.twap_window() {
                Some(window) => {
                    let twap = twaps
                        .entry(pool_idx)
                        .or_insert_with(|| Twap::new(window, cfg.feed.staleness_secs));
                    if let Some(reason) = twap.observe(quote.timestamp, spot, now) {
                        // A reset drops the TWAP back to spot: the smoothing is
                        // gone until the window refills. Never silent — an
                        // operator watching a flapping feed or a slept laptop
                        // needs to know the fade behavior isn't live.
                        warn!(pair = %pair, reason, "TWAP history reset — quoting off spot until the window refills");
                    }
                    (
                        twap.value(now).unwrap_or(spot),
                        Some(SpotDeviationGuard::new(spot, pool.twap_deviation_bps())),
                    )
                }
                None => (spot, None),
            };

            // ----- Inventory lean. Shadow computes and logs next to the live
            // quotes (no behavior change); Live replaces the maker prices. The
            // share is re-read on fills and at most once per 60s otherwise. -----
            let lean = match (pool.lean_mode(), pool.lean_params()) {
                (LeanMode::Off, _) | (_, None) => None,
                (mode, Some(params)) => {
                    let lean_state = lean_states
                        .entry(pair.clone())
                        .or_insert_with(|| LeanState::new(&params));
                    if lean_state.needs_inventory(now) {
                        match read_inventory(&wallet, maker, collateral, debt).await {
                            Ok((coll, debt_bal)) => lean_state.set_inventory(
                                coll,
                                pool.collateral_decimals,
                                debt_bal,
                                pool.debt_decimals,
                                mid,
                                now,
                                &params,
                            ),
                            Err(e) => warn!(
                                pair = %pair, error = %format!("{e:#}"),
                                "could not read inventory; lean keeps its last offsets"
                            ),
                        }
                    }
                    let decision = lean_state.decide(mid, &params);
                    log_lean(mode, &pair, pool, mid, &params, &decision);
                    Some((mode, decision))
                }
            };

            let mut posted_bid = 0usize;
            let mut posted_ask = 0usize;
            for side in [Side::Bid, Side::Ask] {
                let price_override = match lean {
                    Some((LeanMode::Live, decision)) => {
                        let lean_price = match side {
                            Side::Bid => decision.bid,
                            Side::Ask => decision.ask,
                        };
                        match lean_price {
                            Some(price) => Some(price),
                            // The lean pulled this side (critical inventory or
                            // a fair jump): stop reposting and let the live
                            // orders age out through their TTL.
                            None => continue,
                        }
                    }
                    _ => None,
                };
                let outcome = quote_side(
                    &ctx,
                    &mut state,
                    &mut budgets,
                    pool,
                    &pair,
                    debt,
                    collateral,
                    side,
                    mid,
                    price_override,
                    guard,
                    now,
                )
                .await;
                match outcome {
                    // The nonce ledger could not be persisted; never post on a
                    // stale ledger. Skip the rest of this pool's tick.
                    SideOutcome::AbortPool => continue 'pools,
                    SideOutcome::Done { posted, fills } => {
                        if fills > 0 {
                            // Orders of ours filled since the last post: the
                            // inventory moved, so re-read the share next tick.
                            if let Some(lean_state) = lean_states.get_mut(&pair) {
                                lean_state.note_fill(now);
                            }
                        }
                        match side {
                            Side::Bid => posted_bid = posted,
                            Side::Ask => posted_ask = posted,
                        }
                    }
                }
            }
            // Heartbeat: always on a post, otherwise throttled to HEARTBEAT_SECS
            // so a healthy-but-idle bot still shows a pulse (and its live price).
            let last = last_heartbeat.entry(pair.clone()).or_insert(0);
            if posted_bid + posted_ask > 0 || now.saturating_sub(*last) >= HEARTBEAT_SECS {
                *last = now;
                info!(
                    pair = %pair,
                    price = spot,
                    mid,
                    bids_posted = posted_bid,
                    asks_posted = posted_ask,
                    "tick"
                );
            }
            // ----- Taker leg: fill users' resting limit orders that crossed our
            // own quote (a user ask at/below our bid, a user bid at/above our
            // ask). Same feed tick, same spreads — no separate strategy. -----
            if let Some(fee_bps) = taker_fee_bps {
                if pool.limit_taker_enabled() {
                    // With the lean live, the taker prices off the same lean
                    // quotes as the maker — one price surface, and a pulled
                    // side takes nothing. A side without a configured spread
                    // stays off either way (the lean never enables a side).
                    // The same TWAP center and spot-deviation guard apply, so
                    // the maker's book and the taker's fills never disagree.
                    let (taker_bid, taker_ask) = match lean {
                        Some((LeanMode::Live, decision)) => (
                            pool.buy_spread().and(decision.bid),
                            pool.sell_spread().and(decision.ask),
                        ),
                        _ => (
                            pool.buy_spread().map(|s| bid_price(mid, s)),
                            pool.sell_spread().map(|s| ask_price(mid, s)),
                        ),
                    };
                    let taker_bid = taker_bid.map(|p| Side::Bid.guarded_price(p, guard));
                    let taker_ask = taker_ask.map(|p| Side::Ask.guarded_price(p, guard));
                    let taker_ctx = TakerCtx {
                        collateral,
                        debt,
                        collateral_decimals: pool.collateral_decimals,
                        debt_decimals: pool.debt_decimals,
                        bid: taker_bid,
                        ask: taker_ask,
                        fee_bps,
                        min_profit_debt: pool
                            .limit_taker_min_profit_debt
                            .as_deref()
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(U256::ZERO),
                        max_orders: pool.limit_taker_max_orders.unwrap_or(10) as usize,
                    };
                    let pending = taker_pending.entry(pair.clone()).or_default();
                    for (side, outcome) in take_pool_once(
                        &wallet,
                        &indexer,
                        &taker_ctx,
                        cfg.chain_id,
                        permit2,
                        reactor,
                        dry_run,
                        pending,
                        now,
                    )
                    .await
                    {
                        match outcome {
                            Ok(TakeOutcome::Nothing) => {}
                            Ok(TakeOutcome::Planned { orders, spend }) => info!(
                                pair = %pair, side, orders, spend = %spend,
                                "[dry-run] would fill resting limit orders"
                            ),
                            Ok(TakeOutcome::Filled {
                                hash,
                                orders,
                                spend,
                            }) => {
                                info!(
                                    pair = %pair, side, hash = %hash, orders, spend = %spend,
                                    "filled resting limit orders"
                                );
                                // A taker fill spends one wallet asset and
                                // receives the other — that's an inventory
                                // move like any maker fill, so the lean
                                // re-reads the share next tick.
                                if let Some(lean_state) = lean_states.get_mut(&pair) {
                                    lean_state.note_fill(now);
                                }
                            }
                            Err(e) => {
                                warn!(pair = %pair, side, error = %e, "taker tick failed")
                            }
                        }
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
                        // Deliberately the instantaneous feed, not the TWAP
                        // mid: the closer values a one-shot on-chain fill at
                        // execution time, where the current price is the
                        // right mark. The TWAP smooths standing quotes that
                        // rest in the book waiting to be picked off.
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
                        }) => {
                            info!(
                                pool = %closer_pool.pool_address, hash = %hash, positions,
                                debt_in = %debt_in, "closed batch"
                            );
                            // Closing spends debt and receives collateral from
                            // the same wallet the lean measures.
                            if let Some(lean_state) = lean_states.get_mut(&pair) {
                                lean_state.note_fill(now);
                            }
                        }
                        Err(e) => {
                            warn!(pool = %closer_pool.pool_address, error = %e, "close tick failed")
                        }
                    }
                }
            }
        }
    }
}
