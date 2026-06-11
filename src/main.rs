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
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, Bytes, U256};
use anyhow::{anyhow, Context};
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use stitch_bot::approve::{run_approvals, unapproved_tokens, ApprovalMode};
use stitch_bot::banner::print_startup_banner;
use stitch_bot::cli::{parse, Command};
use stitch_bot::closer::discover::Discoverer;
use stitch_bot::closer::executor::{encode_allowance, encode_balance_of};
use stitch_bot::closer::runner::{close_pool_once, CloseOutcome, CloserPool};
use stitch_bot::closer::strategy::{PoolParams, StrategyConfig};
use stitch_bot::config::{
    parse_liquidity_amount, Config, LiquidityAmount, PoolConfig, DEFAULT_MAX_LADDER_ORDERS,
};
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

struct OrderDraft {
    nonce: u64,
    slot_key: String,
    input_amount: U256,
    output_amount: U256,
    client_order_id: Option<String>,
}

#[derive(Default)]
struct PostResult {
    posted: usize,
    spent_nonce: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SlotNonceState {
    chain_id: u64,
    maker: String,
    next_nonce: u64,
    slot_nonces: HashMap<String, u64>,
    #[serde(default)]
    slot_inputs: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FundedInputBudget {
    funded: U256,
    committed: U256,
    reserved: U256,
}

impl Poster<'_> {
    /// Build, sign, and POST a side's order batch. Returns the number of orders
    /// posted (or that would be posted in dry-run). The indexer writes the batch
    /// atomically, so a ladder refresh cannot partially replace live slots.
    async fn post_many(
        &self,
        ttl_secs: u64,
        input_token: Address,
        output_token: Address,
        drafts: &[OrderDraft],
        label: &str,
        price: f64,
    ) -> PostResult {
        let deadline = unix_now().saturating_add(ttl_secs);
        let mut submissions = Vec::new();

        for draft in drafts {
            if draft.input_amount == U256::ZERO || draft.output_amount == U256::ZERO {
                warn!(label, "zero-size order; skipping");
                continue;
            }

            let order = OrderParams {
                reactor: self.reactor,
                swapper: self.maker,
                nonce: U256::from(draft.nonce),
                deadline: U256::from(deadline),
                input_token,
                input_amount: draft.input_amount,
                output_token,
                output_amount: draft.output_amount,
                recipient: self.maker,
            };
            match sign_submission(&order, self.permit2, self.chain_id, self.key) {
                Ok(mut s) => {
                    s.client_order_id = draft.client_order_id.clone();
                    submissions.push(s);
                }
                Err(e) => {
                    warn!(label, error = %e, "signing failed; skipping batch");
                    return PostResult::default();
                }
            }
        }

        if submissions.is_empty() {
            return PostResult::default();
        }

        if self.dry_run {
            for submission in &submissions {
                info!(
                    label,
                    price,
                    input = %submission.input_amount,
                    output = %submission.output_amount,
                    "[dry-run] would post order"
                );
            }
            return PostResult {
                posted: submissions.len(),
                spent_nonce: None,
            };
        }

        match self.indexer.submit_many(&submissions).await {
            Ok(ids) => {
                info!(label, price, orders = ids.len(), "posted order batch");
                PostResult {
                    posted: ids.len(),
                    spent_nonce: None,
                }
            }
            Err(e) => {
                warn!(label, error = %e, "batch post failed");
                PostResult {
                    posted: 0,
                    spent_nonce: spent_nonce_from_error(&e.to_string()),
                }
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

    fn temp_state_file(label: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "stitch-test-state-{label}-{}-{}.json",
            std::process::id(),
            unix_now()
        ));
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

    #[test]
    fn input_liquidity_keeps_configured_size_when_fully_funded() {
        assert_eq!(
            cap_input_liquidity(InputLiquidity::Exact(1_000_000), U256::from(2_000_000u64))
                .unwrap(),
            1_000_000
        );
    }

    #[test]
    fn input_liquidity_caps_to_available_funded_input() {
        assert_eq!(
            cap_input_liquidity(InputLiquidity::Exact(1_000_000), U256::from(500_000u64)).unwrap(),
            500_000
        );
    }

    #[test]
    fn max_input_liquidity_uses_all_available_funded_input() {
        assert_eq!(
            parse_input_liquidity("max", "buy_total_liquidity_debt").unwrap(),
            InputLiquidity::Max
        );
        assert_eq!(
            parse_input_liquidity("MAX", "sell_total_liquidity_collateral").unwrap(),
            InputLiquidity::Max
        );
        assert_eq!(
            cap_input_liquidity(InputLiquidity::Max, U256::from(750_000u64)).unwrap(),
            750_000
        );
    }

    #[test]
    fn input_budget_subtracts_other_corridor_commitments_before_max() {
        let budget = FundedInputBudget {
            funded: U256::from(4_000u64),
            committed: U256::from(8_000u64),
            reserved: U256::ZERO,
        };

        assert_eq!(
            available_funded_input(&budget, U256::from(4_000u64)),
            U256::ZERO
        );
    }

    #[test]
    fn input_budget_reuses_current_corridor_commitment_for_replacement() {
        let budget = FundedInputBudget {
            funded: U256::from(4_000u64),
            committed: U256::from(4_000u64),
            reserved: U256::ZERO,
        };

        assert_eq!(
            available_funded_input(&budget, U256::from(4_000u64)),
            U256::from(4_000u64)
        );
    }

    #[test]
    fn reserving_funded_input_decrements_a_shared_token_budget() {
        let token: Address = "0x00000000000000000000000000000000000000bb"
            .parse()
            .unwrap();
        let mut funded_inputs = HashMap::from([(
            token,
            FundedInputBudget {
                funded: U256::from(75u64),
                committed: U256::ZERO,
                reserved: U256::ZERO,
            },
        )]);

        reserve_funded_input(&mut funded_inputs, token, U256::from(50u64));
        assert_eq!(
            available_funded_input(&funded_inputs[&token], U256::ZERO),
            U256::from(25u64)
        );

        reserve_funded_input(&mut funded_inputs, token, U256::from(50u64));
        assert_eq!(
            available_funded_input(&funded_inputs[&token], U256::ZERO),
            U256::ZERO
        );
    }

    #[test]
    fn replacement_reservation_only_charges_the_incremental_delta() {
        assert_eq!(
            replacement_reservation(U256::from(100u64), U256::from(75u64)),
            U256::from(25u64)
        );
        assert_eq!(
            replacement_reservation(U256::from(75u64), U256::from(100u64)),
            U256::ZERO
        );
    }

    #[test]
    fn slot_nonce_is_stable_per_replacement_slot() {
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 1_000u64;

        let bid_0 = slot_nonce(&mut slot_nonces, &mut next_nonce, "buy:pair:bid:0");
        let bid_1 = slot_nonce(&mut slot_nonces, &mut next_nonce, "buy:pair:bid:1");
        let bid_0_again = slot_nonce(&mut slot_nonces, &mut next_nonce, "buy:pair:bid:0");

        assert_eq!(bid_0, 1_001);
        assert_eq!(bid_1, 1_002);
        assert_eq!(bid_0_again, bid_0);
    }

    #[test]
    fn spent_nonce_errors_are_parsed_from_indexer_failures() {
        let error =
            r#"indexer rejected order batch: [{"message":"Permit2 nonce already spent: 1002"}]"#;

        assert_eq!(spent_nonce_from_error(error), Some(1002));
    }

    #[test]
    fn forgetting_a_spent_nonce_only_rotates_that_slot() {
        let mut slot_nonces = HashMap::new();
        let mut slot_inputs = HashMap::new();
        slot_nonces.insert("buy:pair:bid:0".to_string(), 1001);
        slot_nonces.insert("buy:pair:bid:1".to_string(), 1002);
        slot_inputs.insert("buy:pair:bid:0".to_string(), "1".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "1".to_string());
        let drafts = vec![
            OrderDraft {
                nonce: 1001,
                slot_key: "buy:pair:bid:0".to_string(),
                input_amount: U256::from(1u64),
                output_amount: U256::from(1u64),
                client_order_id: Some("bid:0".to_string()),
            },
            OrderDraft {
                nonce: 1002,
                slot_key: "buy:pair:bid:1".to_string(),
                input_amount: U256::from(1u64),
                output_amount: U256::from(1u64),
                client_order_id: Some("bid:1".to_string()),
            },
        ];

        forget_spent_slot_nonce(&mut slot_nonces, &mut slot_inputs, &drafts, 1002);

        assert_eq!(slot_nonces.get("buy:pair:bid:0"), Some(&1001));
        assert!(!slot_nonces.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_inputs.get("buy:pair:bid:0"), Some(&"1".to_string()));
        assert!(!slot_inputs.contains_key("buy:pair:bid:1"));
    }

    #[test]
    fn reusable_slot_input_sums_only_the_current_side() {
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "50".to_string());

        assert_eq!(
            reusable_slot_input(&slot_inputs, "buy:pair"),
            U256::from(125u64)
        );
    }

    #[test]
    fn remembering_slot_inputs_replaces_only_the_current_side() {
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "50".to_string());
        let drafts = vec![OrderDraft {
            nonce: 1001,
            slot_key: "buy:pair:bid:0".to_string(),
            input_amount: U256::from(80u64),
            output_amount: U256::from(1u64),
            client_order_id: Some("bid:0".to_string()),
        }];

        remember_slot_inputs(&mut slot_inputs, "buy:pair", &drafts);

        assert_eq!(slot_inputs.get("buy:pair:bid:0"), Some(&"80".to_string()));
        assert!(!slot_inputs.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_inputs.get("sell:pair:ask:0"), Some(&"50".to_string()));
    }

    #[test]
    fn slot_nonce_state_path_is_scoped_by_chain_and_maker() {
        let maker: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();

        let path = slot_nonce_state_path("/tmp/stitch.toml", 8453, maker);

        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/stitch.8453.0x00000000000000000000000000000000000000aa.slot-nonces.json"
            )
        );
    }

    #[test]
    fn persisted_slot_nonce_state_round_trips() {
        let maker: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let path = temp_state_file("round-trip");
        let mut slot_nonces = HashMap::new();
        let mut slot_inputs = HashMap::new();
        slot_nonces.insert("buy:pair:bid:0".to_string(), 1001);
        slot_nonces.insert("sell:pair:ask:0".to_string(), 1002);
        slot_inputs.insert("buy:pair:bid:0".to_string(), "10".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "20".to_string());

        save_slot_nonce_state(&path, 8453, maker, 1002, &slot_nonces, &slot_inputs)
            .expect("state saves");
        let (next_nonce, loaded_nonces, loaded_inputs) =
            load_slot_nonce_state(&path, 8453, maker, 1).expect("state loads");

        std::fs::remove_file(path).unwrap();
        assert_eq!(next_nonce, 1002);
        assert_eq!(loaded_nonces, slot_nonces);
        assert_eq!(loaded_inputs, slot_inputs);
    }

    #[test]
    fn persisted_slot_nonce_state_rejects_wrong_maker() {
        let maker: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let other: Address = "0x00000000000000000000000000000000000000bb"
            .parse()
            .unwrap();
        let path = temp_state_file("wrong-maker");
        save_slot_nonce_state(&path, 8453, maker, 1002, &HashMap::new(), &HashMap::new())
            .expect("state saves");

        let err = load_slot_nonce_state(&path, 8453, other, 1).expect_err("maker mismatch");

        std::fs::remove_file(path).unwrap();
        assert!(err.to_string().contains("maker"));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputLiquidity {
    Exact(u128),
    Max,
}

fn parse_input_liquidity(value: &str, field: &str) -> anyhow::Result<InputLiquidity> {
    match parse_liquidity_amount(value, field)? {
        LiquidityAmount::Exact(amount) => u256_to_u128(amount, field).map(InputLiquidity::Exact),
        LiquidityAmount::Max => Ok(InputLiquidity::Max),
    }
}

fn u256_to_u128(value: U256, field: &str) -> anyhow::Result<u128> {
    value
        .to_string()
        .parse::<u128>()
        .with_context(|| format!("{field} does not fit in u128"))
}

fn fallback_liquidity(configured: InputLiquidity) -> U256 {
    match configured {
        InputLiquidity::Exact(configured) => U256::from(configured),
        InputLiquidity::Max => U256::from(u128::MAX),
    }
}

fn cap_input_liquidity(configured: InputLiquidity, available: U256) -> anyhow::Result<u128> {
    let capped = match configured {
        InputLiquidity::Exact(configured) => available.min(U256::from(configured)),
        InputLiquidity::Max => available,
    };
    u256_to_u128(capped, "funded input liquidity")
}

async fn funded_input_cap(
    indexer: &Indexer,
    wallet: &Wallet,
    chain_id: u64,
    maker: Address,
    token: Address,
    permit2: Address,
    configured: InputLiquidity,
    reusable_input: U256,
    dry_run: bool,
    funded_inputs: &mut HashMap<Address, FundedInputBudget>,
    pair: &str,
    label: &str,
) -> Option<u128> {
    if !funded_inputs.contains_key(&token) {
        let balance = wallet
            .read_uint(token, &Bytes::from(encode_balance_of(wallet.address())))
            .await;
        let allowance = wallet
            .read_uint(
                token,
                &Bytes::from(encode_allowance(wallet.address(), permit2)),
            )
            .await;

        match (balance, allowance) {
            (Ok(balance), Ok(allowance)) => {
                let funded = if balance < allowance {
                    balance
                } else {
                    allowance
                };
                match indexer
                    .committed_input(chain_id, &maker.to_string(), &token.to_string())
                    .await
                {
                    Ok(committed) => match committed.parse::<U256>() {
                        Ok(committed) => {
                            funded_inputs.insert(
                                token,
                                FundedInputBudget {
                                    funded,
                                    committed,
                                    reserved: U256::ZERO,
                                },
                            );
                        }
                        Err(e) if dry_run => {
                            warn!(pair = %pair, label, committed = %committed, error = %e, "could not parse committed input; using configured size for dry-run");
                            funded_inputs.insert(
                                token,
                                FundedInputBudget {
                                    funded: fallback_liquidity(configured),
                                    committed: U256::ZERO,
                                    reserved: U256::ZERO,
                                },
                            );
                        }
                        Err(e) => {
                            warn!(pair = %pair, label, committed = %committed, error = %e, "could not parse committed input; skipping side");
                            return None;
                        }
                    },
                    Err(e) if dry_run => {
                        warn!(pair = %pair, label, error = %e, "could not read committed input; using configured size for dry-run");
                        funded_inputs.insert(
                            token,
                            FundedInputBudget {
                                funded: fallback_liquidity(configured),
                                committed: U256::ZERO,
                                reserved: U256::ZERO,
                            },
                        );
                    }
                    Err(e) => {
                        warn!(pair = %pair, label, error = %e, "could not read committed input; skipping side");
                        return None;
                    }
                }
            }
            (Err(e), _) | (_, Err(e)) if dry_run => {
                warn!(pair = %pair, label, error = %e, "could not read funded input; using configured size for dry-run");
                funded_inputs.insert(
                    token,
                    FundedInputBudget {
                        funded: fallback_liquidity(configured),
                        committed: U256::ZERO,
                        reserved: U256::ZERO,
                    },
                );
            }
            (Err(e), _) | (_, Err(e)) => {
                warn!(pair = %pair, label, error = %e, "could not read funded input; skipping side");
                return None;
            }
        }
    }

    let budget = funded_inputs
        .get(&token)
        .copied()
        .unwrap_or(FundedInputBudget {
            funded: U256::ZERO,
            committed: U256::ZERO,
            reserved: U256::ZERO,
        });
    let available = available_funded_input(&budget, reusable_input);
    let capped = match cap_input_liquidity(configured, available) {
        Ok(capped) => capped,
        Err(e) => {
            warn!(pair = %pair, label, error = %e, "funded input liquidity is too large; skipping side");
            return None;
        }
    };
    if let InputLiquidity::Exact(configured) = configured {
        if capped < configured {
            warn!(
                pair = %pair,
                label,
                configured,
                capped,
                funded = %budget.funded,
                committed = %budget.committed,
                reserved = %budget.reserved,
                reusable = %reusable_input,
                "capping order liquidity to remaining funded input"
            );
        }
    }
    Some(capped)
}

fn available_funded_input(budget: &FundedInputBudget, reusable_input: U256) -> U256 {
    budget
        .funded
        .saturating_add(reusable_input)
        .saturating_sub(budget.committed)
        .saturating_sub(budget.reserved)
}

fn reserve_funded_input(
    funded_inputs: &mut HashMap<Address, FundedInputBudget>,
    token: Address,
    amount: U256,
) {
    if amount == U256::ZERO {
        return;
    }
    let budget = funded_inputs.entry(token).or_insert(FundedInputBudget {
        funded: U256::ZERO,
        committed: U256::ZERO,
        reserved: U256::ZERO,
    });
    budget.reserved = budget.reserved.saturating_add(amount);
}

fn drafted_input(drafts: &[OrderDraft]) -> U256 {
    drafts.iter().fold(U256::ZERO, |sum, draft| {
        sum.saturating_add(draft.input_amount)
    })
}

fn replacement_reservation(drafted_input: U256, reusable_input: U256) -> U256 {
    drafted_input.saturating_sub(reusable_input)
}

fn slot_nonce(
    slot_nonces: &mut HashMap<String, u64>,
    next_nonce: &mut u64,
    slot_key: impl Into<String>,
) -> u64 {
    *slot_nonces.entry(slot_key.into()).or_insert_with(|| {
        *next_nonce = next_nonce.saturating_add(1);
        *next_nonce
    })
}

fn spent_nonce_from_error(error: &str) -> Option<u64> {
    const MARKER: &str = "Permit2 nonce already spent:";
    let tail = error.split(MARKER).nth(1)?;
    let digits: String = tail
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn forget_spent_slot_nonce(
    slot_nonces: &mut HashMap<String, u64>,
    slot_inputs: &mut HashMap<String, String>,
    drafts: &[OrderDraft],
    spent_nonce: u64,
) {
    for draft in drafts {
        if draft.nonce == spent_nonce {
            slot_nonces.remove(&draft.slot_key);
            slot_inputs.remove(&draft.slot_key);
        }
    }
}

fn reusable_slot_input(slot_inputs: &HashMap<String, String>, key_id: &str) -> U256 {
    let prefix = format!("{key_id}:");
    slot_inputs
        .iter()
        .filter(|(slot_key, _)| slot_key.starts_with(&prefix))
        .fold(U256::ZERO, |sum, (_, input)| {
            sum.saturating_add(input.parse::<U256>().unwrap_or(U256::ZERO))
        })
}

fn remember_slot_inputs(
    slot_inputs: &mut HashMap<String, String>,
    key_id: &str,
    drafts: &[OrderDraft],
) {
    let prefix = format!("{key_id}:");
    slot_inputs.retain(|slot_key, _| !slot_key.starts_with(&prefix));
    for draft in drafts {
        slot_inputs.insert(draft.slot_key.clone(), draft.input_amount.to_string());
    }
}

fn slot_nonce_state_path(config_path: &str, chain_id: u64, maker: Address) -> PathBuf {
    let config_path = Path::new(config_path);
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("stitch");
    dir.join(format!(
        "{stem}.{chain_id}.{}.slot-nonces.json",
        maker.to_string().to_lowercase()
    ))
}

fn load_slot_nonce_state(
    path: &Path,
    chain_id: u64,
    maker: Address,
    initial_next_nonce: u64,
) -> anyhow::Result<(u64, HashMap<String, u64>, HashMap<String, String>)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok((initial_next_nonce, HashMap::new(), HashMap::new()));
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", path.display()));
        }
    };
    let state: SlotNonceState =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let maker = maker.to_string().to_lowercase();
    anyhow::ensure!(
        state.chain_id == chain_id,
        "slot nonce state chain_id {} does not match {chain_id}",
        state.chain_id
    );
    anyhow::ensure!(
        state.maker.eq_ignore_ascii_case(&maker),
        "slot nonce state maker {} does not match {maker}",
        state.maker
    );
    for (slot_key, input) in &state.slot_inputs {
        input.parse::<U256>().with_context(|| {
            format!(
                "invalid slot input amount for {slot_key} in {}",
                path.display()
            )
        })?;
    }
    let max_slot_nonce = state.slot_nonces.values().copied().max().unwrap_or(0);
    Ok((
        initial_next_nonce.max(state.next_nonce).max(max_slot_nonce),
        state.slot_nonces,
        state.slot_inputs,
    ))
}

fn save_slot_nonce_state(
    path: &Path,
    chain_id: u64,
    maker: Address,
    next_nonce: u64,
    slot_nonces: &HashMap<String, u64>,
    slot_inputs: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let state = SlotNonceState {
        chain_id,
        maker: maker.to_string().to_lowercase(),
        next_nonce,
        slot_nonces: slot_nonces.clone(),
        slot_inputs: slot_inputs.clone(),
    };
    let mut json = serde_json::to_string_pretty(&state)?;
    json.push('\n');
    let mut tmp = path.to_path_buf();
    tmp.set_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))
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

    // Last quoted price per side ("buy:<pair>" / "sell:<pair>"), to skip
    // re-signing until the price moves past the refresh threshold.
    // Per side ("buy:<pair>" / "sell:<pair>"): the last quoted price and when it
    // was posted, so we re-sign on a price move *or* before the order's TTL
    // lapses (a flat market must still keep a live order).
    let mut last_quote: HashMap<String, (f64, u64)> = HashMap::new();
    // Per closer pool: position id → unix time we last submitted a fill for it,
    // so a pending tx or lagging subgraph can't trigger a duplicate `fill()`.
    let mut closer_pending: HashMap<Address, HashMap<U256, u64>> = HashMap::new();
    let slot_nonce_state_path = slot_nonce_state_path(&config_path, cfg.chain_id, maker);
    let initial_next_nonce = unix_now().saturating_mul(1000);
    let (mut next_nonce, mut slot_nonces, mut slot_inputs) = if dry_run {
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
        let mut funded_inputs: HashMap<Address, FundedInputBudget> = HashMap::new();

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
                    pool.repost_lead_secs(),
                ) {
                    let reusable_input = reusable_slot_input(&slot_inputs, &key_id);
                    let sizes = if pool.buy_ladder_enabled() {
                        match (
                            pool.buy_total_liquidity_debt.as_deref(),
                            pool.buy_min_slice_debt.as_deref(),
                        ) {
                            (Some(total), Some(min)) => match (
                                parse_input_liquidity(total, "buy_total_liquidity_debt"),
                                parse_u128(min, "buy_min_slice_debt"),
                            ) {
                                (Ok(total), Ok(min)) => {
                                    match funded_input_cap(
                                        &indexer,
                                        &wallet,
                                        cfg.chain_id,
                                        maker,
                                        debt,
                                        permit2,
                                        total,
                                        reusable_input,
                                        dry_run,
                                        &mut funded_inputs,
                                        &pair,
                                        "bid",
                                    )
                                    .await
                                    {
                                        Some(total) => balanced_ladder(
                                            total,
                                            min,
                                            pool.buy_max_orders.unwrap_or(DEFAULT_MAX_LADDER_ORDERS)
                                                as usize,
                                        ),
                                        None => Vec::new(),
                                    }
                                }
                                (Err(e), _) | (_, Err(e)) => {
                                    warn!(pair = %pair, error = %e, "invalid buy ladder; skipping bid");
                                    Vec::new()
                                }
                            },
                            _ => Vec::new(),
                        }
                    } else if let Some(size_str) = &pool.buy_order_size_debt {
                        match parse_input_liquidity(size_str, "buy_order_size_debt") {
                            Ok(size) => funded_input_cap(
                                &indexer,
                                &wallet,
                                cfg.chain_id,
                                maker,
                                debt,
                                permit2,
                                size,
                                reusable_input,
                                dry_run,
                                &mut funded_inputs,
                                &pair,
                                "bid",
                            )
                            .await
                            .filter(|size| *size > 0)
                            .map(|size| vec![size])
                            .unwrap_or_default(),
                            Err(e) => {
                                warn!(pair = %pair, error = %e, "invalid buy_order_size_debt; skipping bid");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    };

                    let laddered = pool.buy_ladder_enabled();
                    let mut drafts = Vec::new();
                    for (i, size) in sizes.into_iter().enumerate() {
                        let (input, output) =
                            buy_amounts_at(bid, size, pool.debt_decimals, pool.collateral_decimals);
                        let slot_id = if laddered {
                            format!("bid:{i}")
                        } else {
                            "default".to_string()
                        };
                        let slot_key = format!("{key_id}:{slot_id}");
                        let nonce = slot_nonce(&mut slot_nonces, &mut next_nonce, slot_key.clone());
                        drafts.push(OrderDraft {
                            nonce,
                            slot_key,
                            input_amount: input,
                            output_amount: output,
                            client_order_id: laddered.then_some(slot_id),
                        });
                    }
                    let input_reserved = drafted_input(&drafts);
                    if !dry_run && !drafts.is_empty() {
                        if let Err(e) = save_slot_nonce_state(
                            &slot_nonce_state_path,
                            cfg.chain_id,
                            maker,
                            next_nonce,
                            &slot_nonces,
                            &slot_inputs,
                        ) {
                            warn!(pair = %pair, label = "bid", error = %e, "could not persist slot nonce state; skipping post");
                            continue;
                        }
                    }
                    let result = poster
                        .post_many(pool.ttl_secs, debt, collateral, &drafts, "bid", bid)
                        .await;
                    if let Some(spent_nonce) = result.spent_nonce {
                        forget_spent_slot_nonce(
                            &mut slot_nonces,
                            &mut slot_inputs,
                            &drafts,
                            spent_nonce,
                        );
                        if !dry_run {
                            if let Err(e) = save_slot_nonce_state(
                                &slot_nonce_state_path,
                                cfg.chain_id,
                                maker,
                                next_nonce,
                                &slot_nonces,
                                &slot_inputs,
                            ) {
                                warn!(pair = %pair, label = "bid", error = %e, "could not persist spent nonce rotation");
                            }
                        }
                    }
                    if result.posted > 0 {
                        remember_slot_inputs(&mut slot_inputs, &key_id, &drafts);
                        if !dry_run {
                            if let Err(e) = save_slot_nonce_state(
                                &slot_nonce_state_path,
                                cfg.chain_id,
                                maker,
                                next_nonce,
                                &slot_nonces,
                                &slot_inputs,
                            ) {
                                warn!(pair = %pair, label = "bid", error = %e, "could not persist posted slot inputs");
                            }
                        }
                        reserve_funded_input(
                            &mut funded_inputs,
                            debt,
                            replacement_reservation(input_reserved, reusable_input),
                        );
                        info!(pair = %pair, orders = result.posted, "posted bid ladder");
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
                    pool.repost_lead_secs(),
                ) {
                    let reusable_input = reusable_slot_input(&slot_inputs, &key_id);
                    let sizes = if pool.sell_ladder_enabled() {
                        match (
                            pool.sell_total_liquidity_collateral.as_deref(),
                            pool.sell_min_slice_debt.as_deref(),
                        ) {
                            (Some(total_collateral), Some(min_debt)) => {
                                match (
                                    parse_input_liquidity(
                                        total_collateral,
                                        "sell_total_liquidity_collateral",
                                    ),
                                    parse_u128(min_debt, "sell_min_slice_debt"),
                                ) {
                                    (Ok(total_collateral), Ok(min_debt)) => match funded_input_cap(
                                        &indexer,
                                        &wallet,
                                        cfg.chain_id,
                                        maker,
                                        collateral,
                                        permit2,
                                        total_collateral,
                                        reusable_input,
                                        dry_run,
                                        &mut funded_inputs,
                                        &pair,
                                        "ask",
                                    )
                                    .await
                                    {
                                        Some(total_collateral) => {
                                            let (_, total_debt) = sell_amounts_at(
                                                ask,
                                                total_collateral,
                                                pool.debt_decimals,
                                                pool.collateral_decimals,
                                            );
                                            match u256_to_u128(
                                                total_debt,
                                                "sell total debt equivalent",
                                            ) {
                                                Ok(total_debt) => balanced_ladder(
                                                    total_debt,
                                                    min_debt,
                                                    pool.sell_max_orders
                                                        .unwrap_or(DEFAULT_MAX_LADDER_ORDERS)
                                                        as usize,
                                                ),
                                                Err(e) => {
                                                    warn!(pair = %pair, error = %e, "invalid sell ladder; skipping ask");
                                                    Vec::new()
                                                }
                                            }
                                        }
                                        None => Vec::new(),
                                    },
                                    (Err(e), _) | (_, Err(e)) => {
                                        warn!(pair = %pair, error = %e, "invalid sell ladder; skipping ask");
                                        Vec::new()
                                    }
                                }
                            }
                            _ => Vec::new(),
                        }
                    } else if let Some(size_str) = &pool.sell_order_size_collateral {
                        match parse_input_liquidity(size_str, "sell_order_size_collateral") {
                            Ok(size) => funded_input_cap(
                                &indexer,
                                &wallet,
                                cfg.chain_id,
                                maker,
                                collateral,
                                permit2,
                                size,
                                reusable_input,
                                dry_run,
                                &mut funded_inputs,
                                &pair,
                                "ask",
                            )
                            .await
                            .filter(|size| *size > 0)
                            .map(|size| vec![size])
                            .unwrap_or_default(),
                            Err(e) => {
                                warn!(pair = %pair, error = %e, "invalid sell_order_size_collateral; skipping ask");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    };

                    let laddered = pool.sell_ladder_enabled();
                    let mut drafts = Vec::new();
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
                        let slot_id = if laddered {
                            format!("ask:{i}")
                        } else {
                            "default".to_string()
                        };
                        let slot_key = format!("{key_id}:{slot_id}");
                        let nonce = slot_nonce(&mut slot_nonces, &mut next_nonce, slot_key.clone());
                        drafts.push(OrderDraft {
                            nonce,
                            slot_key,
                            input_amount: input,
                            output_amount: output,
                            client_order_id: laddered.then_some(slot_id),
                        });
                    }
                    let input_reserved = drafted_input(&drafts);
                    if !dry_run && !drafts.is_empty() {
                        if let Err(e) = save_slot_nonce_state(
                            &slot_nonce_state_path,
                            cfg.chain_id,
                            maker,
                            next_nonce,
                            &slot_nonces,
                            &slot_inputs,
                        ) {
                            warn!(pair = %pair, label = "ask", error = %e, "could not persist slot nonce state; skipping post");
                            continue;
                        }
                    }
                    let result = poster
                        .post_many(pool.ttl_secs, collateral, debt, &drafts, "ask", ask)
                        .await;
                    if let Some(spent_nonce) = result.spent_nonce {
                        forget_spent_slot_nonce(
                            &mut slot_nonces,
                            &mut slot_inputs,
                            &drafts,
                            spent_nonce,
                        );
                        if !dry_run {
                            if let Err(e) = save_slot_nonce_state(
                                &slot_nonce_state_path,
                                cfg.chain_id,
                                maker,
                                next_nonce,
                                &slot_nonces,
                                &slot_inputs,
                            ) {
                                warn!(pair = %pair, label = "ask", error = %e, "could not persist spent nonce rotation");
                            }
                        }
                    }
                    if result.posted > 0 {
                        remember_slot_inputs(&mut slot_inputs, &key_id, &drafts);
                        if !dry_run {
                            if let Err(e) = save_slot_nonce_state(
                                &slot_nonce_state_path,
                                cfg.chain_id,
                                maker,
                                next_nonce,
                                &slot_nonces,
                                &slot_inputs,
                            ) {
                                warn!(pair = %pair, label = "ask", error = %e, "could not persist posted slot inputs");
                            }
                        }
                        reserve_funded_input(
                            &mut funded_inputs,
                            collateral,
                            replacement_reservation(input_reserved, reusable_input),
                        );
                        info!(pair = %pair, orders = result.posted, "posted ask ladder");
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
