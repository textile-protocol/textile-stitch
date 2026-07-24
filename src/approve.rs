// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Green-leg Permit2 approvals.
//!
//! The operator signs Permit2 `permitWitnessTransferFrom` orders whose spender
//! is the reactor (see [`crate::eip712`]). For a filler to execute one, Permit2
//! must be allowed to pull the order's *input* token from the maker — debt on
//! the buy side (we pay debt for collateral), collateral on the sell side. So
//! the maker needs a one-time `ERC20.approve(Permit2, …)` per token it quotes.
//!
//! Without it, orders still sign and post, then silently revert on fill. This
//! module figures out which tokens need approval and how much, decides
//! max-vs-exact per the operator's choice, and sends the approvals — mirroring
//! the closer's `ensure_allowance`. It backs the `approve` subcommand and the
//! live-start preflight in `main`.

use std::collections::BTreeMap;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::{bail, Context};
use tracing::info;

use crate::closer::executor::{encode_allowance, encode_approve};
use crate::config::{parse_liquidity_amount, Config, LiquidityAmount, PoolConfig};
use crate::rpc::Wallet;

/// How much of each token to approve to Permit2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Unlimited (`type(uint256).max`). Approve once and never think about it
    /// again — the recommended default for a market maker.
    Max,
    /// Exactly the liquidity the config commits for the token. Tighter blast
    /// radius, but the allowance is consumed as orders fill, so it must be
    /// re-approved to keep quoting.
    Exact,
}

/// A token the operator must approve to Permit2, with the exact liquidity the
/// config commits to it (summed across every enabled side that spends it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredApproval {
    pub token: Address,
    /// Committed input liquidity in this token's atomic units.
    pub required: U256,
    /// True when at least one side uses `"max"` and therefore cannot be
    /// represented by a fixed exact approval amount.
    pub uses_max_liquidity: bool,
    /// Human labels for logs, e.g. "debt (buy side)".
    pub reasons: Vec<String>,
}

/// What to do for one token given its current Permit2 allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAction {
    /// Allowance already covers the committed liquidity — nothing to do.
    AlreadyApproved,
    /// Send an `approve(Permit2, amount)`.
    Approve(U256),
}

/// The distinct tokens needing a Permit2 approval, with the exact committed
/// amount per token. Deduped by address (one approval covers every pool/side
/// that spends the token) and returned in a stable order.
pub fn required_approvals(cfg: &Config) -> anyhow::Result<Vec<RequiredApproval>> {
    let mut by_token: BTreeMap<Address, (U256, bool, Vec<String>)> = BTreeMap::new();
    for pool in &cfg.pools {
        if pool.buy_enabled() {
            let token = parse_addr(&pool.debt, "debt token")?;
            let entry = by_token
                .entry(token)
                .or_insert((U256::ZERO, false, Vec::new()));
            add_required_amount(entry, buy_input_amount(pool)?);
            entry.2.push("debt (buy side)".to_string());
        }
        if pool.sell_enabled() {
            let token = parse_addr(&pool.collateral, "collateral token")?;
            let entry = by_token
                .entry(token)
                .or_insert((U256::ZERO, false, Vec::new()));
            add_required_amount(entry, sell_input_amount(pool)?);
            entry.2.push("collateral (sell side)".to_string());
        }
    }
    Ok(by_token
        .into_iter()
        .map(
            |(token, (required, uses_max_liquidity, reasons))| RequiredApproval {
                token,
                required,
                uses_max_liquidity,
                reasons,
            },
        )
        .collect())
}

fn add_required_amount(entry: &mut (U256, bool, Vec<String>), amount: LiquidityAmount) {
    match amount {
        LiquidityAmount::Exact(amount) if !entry.1 => {
            entry.0 = entry.0.saturating_add(amount);
        }
        LiquidityAmount::Exact(_) => {}
        LiquidityAmount::Max => {
            entry.0 = U256::MAX;
            entry.1 = true;
        }
    }
}

/// Decide the action for one token. Skips when the current allowance already
/// covers the committed liquidity (idempotent — safe to re-run); otherwise
/// approves MAX or the exact committed amount per `mode`.
pub fn approval_action(
    current_allowance: U256,
    required: U256,
    uses_max_liquidity: bool,
    mode: ApprovalMode,
) -> ApprovalAction {
    // A side with no committed size needs nothing; and an allowance that already
    // covers the commitment is left alone whichever mode was asked for.
    if required == U256::ZERO
        || allowance_covers_requirement(current_allowance, required, uses_max_liquidity)
    {
        return ApprovalAction::AlreadyApproved;
    }
    match mode {
        ApprovalMode::Max => ApprovalAction::Approve(U256::MAX),
        ApprovalMode::Exact => ApprovalAction::Approve(required),
    }
}

fn allowance_covers_requirement(
    current_allowance: U256,
    required: U256,
    uses_max_liquidity: bool,
) -> bool {
    if uses_max_liquidity {
        // Permit2 spends down ERC20 allowance on some tokens, so a max-liquidity
        // side should tolerate a once-max approval that has already filled.
        return current_allowance >= max_liquidity_allowance_floor();
    }
    current_allowance >= required
}

fn max_liquidity_allowance_floor() -> U256 {
    U256::MAX >> 1
}

/// The committed buy-side input is debt. Use the same active sizing field as
/// the quote path: ladder total only when the ladder is fully configured,
/// otherwise the single order size.
pub(crate) fn buy_input_amount(pool: &PoolConfig) -> anyhow::Result<LiquidityAmount> {
    let raw = if pool.buy_ladder_enabled() {
        pool.buy_total_liquidity_debt.as_ref()
    } else {
        pool.buy_order_size_debt.as_ref()
    }
    .context("buy side enabled but no active debt size configured")?;
    parse_liquidity_amount(raw, "buy debt size")
}

/// The committed sell-side input is collateral inventory, selected from the
/// same active sizing field used by the quote path.
pub(crate) fn sell_input_amount(pool: &PoolConfig) -> anyhow::Result<LiquidityAmount> {
    let raw = if pool.sell_ladder_enabled() {
        pool.sell_total_liquidity_collateral.as_ref()
    } else {
        pool.sell_order_size_collateral.as_ref()
    }
    .context("sell side enabled but no active collateral size configured")?;
    parse_liquidity_amount(raw, "sell collateral size")
}

fn parse_addr(s: &str, what: &str) -> anyhow::Result<Address> {
    s.parse()
        .with_context(|| format!("invalid {what} address: {s}"))
}

/// Read a token's current Permit2 allowance for the operator wallet.
async fn permit2_allowance(
    wallet: &Wallet,
    token: Address,
    permit2: Address,
) -> anyhow::Result<U256> {
    wallet
        .read_uint(
            token,
            &Bytes::from(encode_allowance(wallet.address(), permit2)),
        )
        .await
        .with_context(|| format!("reading {token} allowance to Permit2"))
}

/// Tokens whose Permit2 allowance doesn't cover the committed liquidity. Used by
/// the live-start preflight: a non-empty result means orders would post but fail
/// to fill.
pub async fn unapproved_tokens(
    wallet: &Wallet,
    permit2: Address,
    cfg: &Config,
) -> anyhow::Result<Vec<RequiredApproval>> {
    let mut short = Vec::new();
    for req in required_approvals(cfg)? {
        let allowance = permit2_allowance(wallet, req.token, permit2).await?;
        if matches!(
            approval_action(
                allowance,
                req.required,
                req.uses_max_liquidity,
                ApprovalMode::Max
            ),
            ApprovalAction::Approve(_)
        ) {
            short.push(req);
        }
    }
    Ok(short)
}

/// Ensure every required token is approved to Permit2. With `dry_run`, reports
/// what it would do without sending. Returns the number of approvals sent.
pub async fn run_approvals(
    wallet: &Wallet,
    permit2: Address,
    cfg: &Config,
    mode: ApprovalMode,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let required = required_approvals(cfg)?;
    if required.is_empty() {
        info!("no enabled order sides; nothing to approve");
        return Ok(0);
    }
    if mode == ApprovalMode::Exact {
        for req in &required {
            if req.uses_max_liquidity {
                bail!(
                    "approve --exact cannot be used with \"max\" liquidity for token {}; use the default max approval or set a fixed liquidity amount",
                    req.token
                );
            }
        }
    }
    let mut sent = 0usize;
    for req in &required {
        let allowance = permit2_allowance(wallet, req.token, permit2).await?;
        match approval_action(allowance, req.required, req.uses_max_liquidity, mode) {
            ApprovalAction::AlreadyApproved => {
                info!(token = %req.token, reasons = ?req.reasons, "already approved; skipping");
            }
            ApprovalAction::Approve(amount) => {
                let shown = if amount == U256::MAX {
                    "max".to_string()
                } else {
                    amount.to_string()
                };
                if dry_run {
                    info!(token = %req.token, amount = %shown, reasons = ?req.reasons, "would approve to Permit2 (dry-run)");
                    continue;
                }
                info!(token = %req.token, amount = %shown, reasons = ?req.reasons, "approving to Permit2");
                wallet
                    .send_and_wait(
                        req.token,
                        Bytes::from(encode_approve(permit2, amount)),
                        U256::ZERO,
                        Duration::from_secs(120),
                    )
                    .await
                    .with_context(|| format!("approving {} to Permit2", req.token))?;
                sent += 1;
            }
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBT: &str = "0x0000000000000000000000000000000000000002";
    const COLLATERAL: &str = "0x0000000000000000000000000000000000000001";

    fn cfg_from_pool(pool_body: &str) -> Config {
        let toml = format!(
            r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "{COLLATERAL}"
            collateral_decimals = 6
            debt = "{DEBT}"
            debt_decimals = 6
            ttl_secs = 60
            refresh_threshold_bps = 10
            {pool_body}
        "#
        );
        Config::from_toml(&toml).expect("config parses")
    }

    #[test]
    fn both_sides_require_their_input_token() {
        let cfg = cfg_from_pool(
            r#"
            buy_offset_bps = 150
            buy_total_liquidity_debt = "50000000000"
            buy_min_slice_debt = "10000000"
            sell_offset_bps = 150
            sell_total_liquidity_collateral = "30000000000000000000000"
            sell_min_slice_debt = "10000000"
        "#,
        );
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 2, "one approval per input token");
        let debt: Address = DEBT.parse().unwrap();
        let coll: Address = COLLATERAL.parse().unwrap();
        let debt_req = reqs.iter().find(|r| r.token == debt).unwrap();
        let coll_req = reqs.iter().find(|r| r.token == coll).unwrap();
        assert!(!debt_req.uses_max_liquidity);
        assert!(!coll_req.uses_max_liquidity);
        assert_eq!(debt_req.required, U256::from(50_000_000_000u64));
        assert_eq!(
            coll_req.required,
            "30000000000000000000000".parse::<U256>().unwrap()
        );
    }

    #[test]
    fn buy_only_pool_requires_only_the_debt_token() {
        let cfg = cfg_from_pool(
            r#"
            buy_offset_bps = 150
            buy_order_size_debt = "1000000000"
        "#,
        );
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].token, DEBT.parse::<Address>().unwrap());
        assert_eq!(reqs[0].required, U256::from(1_000_000_000u64));
    }

    #[test]
    fn a_token_used_by_two_pools_sums_and_dedupes() {
        // Two pools, both buying with the same debt token: one approval, summed.
        let toml = format!(
            r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x00000000000000000000000000000000000000a1"
            collateral_decimals = 6
            debt = "{DEBT}"
            debt_decimals = 6
            ttl_secs = 60
            refresh_threshold_bps = 10
            buy_offset_bps = 150
            buy_order_size_debt = "1000000000"
            [[pools]]
            collateral = "0x00000000000000000000000000000000000000a2"
            collateral_decimals = 6
            debt = "{DEBT}"
            debt_decimals = 6
            ttl_secs = 60
            refresh_threshold_bps = 10
            buy_offset_bps = 150
            buy_order_size_debt = "2000000000"
        "#
        );
        let cfg = Config::from_toml(&toml).unwrap();
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 1, "same token deduped to one approval");
        assert_eq!(
            reqs[0].required,
            U256::from(3_000_000_000u64),
            "amounts sum"
        );
        assert_eq!(reqs[0].reasons.len(), 2);
    }

    #[test]
    fn disabled_sides_need_no_approval() {
        // A spread with no size is not an enabled side.
        let cfg = cfg_from_pool(r#"buy_offset_bps = 150"#);
        assert!(required_approvals(&cfg).unwrap().is_empty());
    }

    #[test]
    fn max_liquidity_requires_max_approval() {
        let cfg = cfg_from_pool(
            r#"
            buy_offset_bps = 150
            buy_total_liquidity_debt = "max"
            buy_min_slice_debt = "10000000"
        "#,
        );
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].required, U256::MAX);
        assert!(reqs[0].uses_max_liquidity);
    }

    #[test]
    fn inactive_buy_ladder_total_does_not_drive_approval() {
        let cfg = cfg_from_pool(
            r#"
            buy_offset_bps = 150
            buy_total_liquidity_debt = "max"
            buy_order_size_debt = "1000000000"
        "#,
        );
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].required, U256::from(1_000_000_000u64));
        assert!(!reqs[0].uses_max_liquidity);
    }

    #[test]
    fn inactive_sell_ladder_total_does_not_drive_approval() {
        let cfg = cfg_from_pool(
            r#"
            sell_offset_bps = 150
            sell_total_liquidity_collateral = "max"
            sell_order_size_collateral = "2000000000"
        "#,
        );
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].token, COLLATERAL.parse::<Address>().unwrap());
        assert_eq!(reqs[0].required, U256::from(2_000_000_000u64));
        assert!(!reqs[0].uses_max_liquidity);
    }

    #[test]
    fn max_liquidity_dominates_fixed_requirements_for_the_same_token() {
        let toml = format!(
            r#"
            chain_id = 8453
            rpc_url = "http://x"
            indexer_url = "http://x"
            permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
            reactor = "0x0000000000000000000000000000000000000000"
            tick_interval_secs = 5
            [feed]
            url = "http://x"
            staleness_secs = 30
            [[pools]]
            collateral = "0x00000000000000000000000000000000000000a1"
            collateral_decimals = 6
            debt = "{DEBT}"
            debt_decimals = 6
            ttl_secs = 60
            refresh_threshold_bps = 10
            buy_offset_bps = 150
            buy_total_liquidity_debt = "max"
            buy_min_slice_debt = "10000000"
            [[pools]]
            collateral = "0x00000000000000000000000000000000000000a2"
            collateral_decimals = 6
            debt = "{DEBT}"
            debt_decimals = 6
            ttl_secs = 60
            refresh_threshold_bps = 10
            buy_offset_bps = 150
            buy_order_size_debt = "2000000000"
        "#
        );
        let cfg = Config::from_toml(&toml).unwrap();
        let reqs = required_approvals(&cfg).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].required, U256::MAX);
        assert!(reqs[0].uses_max_liquidity);
        assert_eq!(reqs[0].reasons.len(), 2);
    }

    #[test]
    fn already_approved_when_allowance_covers_the_commitment() {
        let required = U256::from(50_000_000_000u64);
        assert_eq!(
            approval_action(required, required, false, ApprovalMode::Max),
            ApprovalAction::AlreadyApproved
        );
        assert_eq!(
            approval_action(
                required + U256::from(1u8),
                required,
                false,
                ApprovalMode::Exact
            ),
            ApprovalAction::AlreadyApproved
        );
    }

    #[test]
    fn max_mode_approves_uint_max_when_short() {
        let required = U256::from(50_000_000_000u64);
        assert_eq!(
            approval_action(U256::ZERO, required, false, ApprovalMode::Max),
            ApprovalAction::Approve(U256::MAX)
        );
    }

    #[test]
    fn max_liquidity_allowance_stays_covered_as_fills_consume_it() {
        let consumed_allowance = U256::MAX - U256::from(50_000u64);
        assert_eq!(
            approval_action(consumed_allowance, U256::MAX, true, ApprovalMode::Max),
            ApprovalAction::AlreadyApproved
        );
        assert_eq!(
            approval_action(
                max_liquidity_allowance_floor() - U256::from(1u8),
                U256::MAX,
                true,
                ApprovalMode::Max
            ),
            ApprovalAction::Approve(U256::MAX)
        );
    }

    #[test]
    fn exact_max_amount_still_requires_literal_coverage_without_max_liquidity() {
        assert_eq!(
            approval_action(
                U256::MAX - U256::from(1u8),
                U256::MAX,
                false,
                ApprovalMode::Max
            ),
            ApprovalAction::Approve(U256::MAX)
        );
    }

    #[test]
    fn exact_mode_approves_the_committed_amount_when_short() {
        let required = U256::from(50_000_000_000u64);
        // Even a partial existing allowance is topped up to exactly `required`.
        assert_eq!(
            approval_action(U256::from(10u8), required, false, ApprovalMode::Exact),
            ApprovalAction::Approve(required)
        );
    }

    #[test]
    fn zero_requirement_is_a_noop() {
        assert_eq!(
            approval_action(U256::ZERO, U256::ZERO, false, ApprovalMode::Max),
            ApprovalAction::AlreadyApproved
        );
    }
}
