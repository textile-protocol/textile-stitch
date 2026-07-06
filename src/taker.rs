// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Taker leg: fill users' resting limit orders when their price reaches the
//! operator's own quote.
//!
//! Users park signed limit orders in the same book the bot quotes into
//! (kind=LIMIT on the indexer). Takers' instant swaps consume them like any
//! other depth; this leg is the second fill path — the operator acting as the
//! on-chain taker via `reactor.executeBatch`, paying the order's output (plus
//! the reactor's native fee) and receiving its input.
//!
//! Pricing reuses the maker legs' own rule: a user selling collateral is
//! filled when their price is at or below the operator's *bid* for that pool
//! (buy low), a user selling debt when at or above the *ask* (sell high). No
//! separate strategy knobs beyond an enable flag, a per-order minimum profit,
//! and a batch cap.
//!
//! Trust model: the indexer is NOT trusted with funds. Every candidate is
//! re-verified locally — the EIP-712 digest is recomputed from the served
//! fields and the signature must recover to the claimed maker, the reactor
//! must be the configured one, and the executed order bytes are re-encoded
//! locally from those same fields. A lying indexer can therefore at worst
//! waste the gas of a reverting transaction, never redirect a fill.

use std::collections::HashMap;

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde_json::Value;
use tracing::{info, warn};

use crate::closer::executor::{encode_approve, encode_balance_of};
use crate::eip712::permit2_digest;
use crate::indexer::Indexer;
use crate::quote::sell_amounts_at;
use crate::rpc::Wallet;
use crate::signer::address_from_verifying_key;
use crate::types::OrderParams;

/// Cooldown before an order we already submitted a fill for is eligible again
/// — covers the pending-tx + indexer-reconcile window, mirroring the closer.
const RESUBMIT_COOLDOWN_SECS: u64 = 180;
/// Don't fill an order about to expire: the tx must land before the Permit2
/// deadline or it reverts with SignatureExpired.
const DEADLINE_MARGIN_SECS: u64 = 30;
const BPS: u64 = 10_000;

fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes()).0;
    [h[0], h[1], h[2], h[3]]
}

// ---------------------------------------------------------------------------
// Indexer payload
// ---------------------------------------------------------------------------

/// A resting user limit order as served by `restingLimitOrders`.
#[derive(Debug, Clone)]
pub struct RestingOrder {
    pub id: String,
    pub reactor: Address,
    pub maker: Address,
    pub input_token: Address,
    pub input_amount: U256,
    pub output_token: Address,
    pub output_amount: U256,
    pub nonce: U256,
    pub deadline_sec: u64,
    /// 65-byte EIP-712 signature over the Permit2 witness digest.
    pub signature: Vec<u8>,
}

impl RestingOrder {
    /// The order struct the reactor executes — recipient is always the maker
    /// for user limit orders (enforced at submit time by the indexer, and by
    /// the signature itself: different fields don't recover).
    pub fn params(&self) -> OrderParams {
        OrderParams {
            reactor: self.reactor,
            swapper: self.maker,
            nonce: self.nonce,
            deadline: U256::from(self.deadline_sec),
            input_token: self.input_token,
            input_amount: self.input_amount,
            output_token: self.output_token,
            output_amount: self.output_amount,
            recipient: self.maker,
        }
    }
}

/// GraphQL BigInt serializes as a number when it fits, a string when it
/// doesn't — accept both (same tolerance as the committed-input read).
fn parse_u256_field(v: &Value) -> Option<U256> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(U256::from),
        _ => None,
    }
}

fn parse_address_field(v: &Value) -> Option<Address> {
    v.as_str().and_then(|s| s.parse().ok())
}

fn parse_signature_field(v: &Value) -> Option<Vec<u8>> {
    let s = v.as_str()?;
    let bytes = alloy_primitives::hex::decode(s).ok()?;
    (bytes.len() == 65).then_some(bytes)
}

/// Parse the `restingLimitOrders` response rows; malformed rows are dropped
/// (they can only have come from a broken or hostile indexer).
pub fn parse_resting_orders(rows: &Value) -> Vec<RestingOrder> {
    rows.as_array()
        .map(|list| {
            list.iter()
                .filter_map(|row| {
                    Some(RestingOrder {
                        id: row.get("id")?.as_str()?.to_string(),
                        reactor: parse_address_field(row.get("reactor")?)?,
                        maker: parse_address_field(row.get("maker")?)?,
                        input_token: parse_address_field(row.get("inputToken")?)?,
                        input_amount: parse_u256_field(row.get("inputAmount")?)?,
                        output_token: parse_address_field(row.get("outputToken")?)?,
                        output_amount: parse_u256_field(row.get("outputAmount")?)?,
                        nonce: parse_u256_field(row.get("nonce")?)?,
                        deadline_sec: parse_u256_field(row.get("deadlineSec")?)?.try_into().ok()?,
                        signature: parse_signature_field(row.get("signature")?)?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Verification — never trust the indexer with fund-moving inputs
// ---------------------------------------------------------------------------

fn recover_signer(digest: B256, sig: &[u8]) -> Option<Address> {
    if sig.len() != 65 {
        return None;
    }
    let v = sig[64];
    let rid = RecoveryId::try_from(if v >= 27 { v - 27 } else { v }).ok()?;
    let signature = Signature::try_from(&sig[..64]).ok()?;
    let vk = VerifyingKey::recover_from_prehash(digest.as_slice(), &signature, rid).ok()?;
    Some(address_from_verifying_key(&vk))
}

/// Reject anything we would not want to execute: a foreign reactor, our own
/// order, a near/past deadline, or fields that don't match the signature.
pub fn verify_order(
    order: &RestingOrder,
    expected_reactor: Address,
    self_address: Address,
    permit2: Address,
    chain_id: u64,
    now: u64,
) -> Result<(), &'static str> {
    if order.reactor != expected_reactor {
        return Err("foreign reactor");
    }
    if order.maker == self_address {
        return Err("own order");
    }
    if order.deadline_sec <= now + DEADLINE_MARGIN_SECS {
        return Err("expiring");
    }
    let digest = permit2_digest(&order.params(), permit2, chain_id);
    match recover_signer(digest, &order.signature) {
        Some(signer) if signer == order.maker => Ok(()),
        Some(_) => Err("signature does not match maker"),
        None => Err("unrecoverable signature"),
    }
}

// ---------------------------------------------------------------------------
// Economics
// ---------------------------------------------------------------------------

/// One pool's taker-leg pricing context for a tick.
#[derive(Debug, Clone)]
pub struct TakerCtx {
    pub collateral: Address,
    pub debt: Address,
    pub collateral_decimals: u8,
    pub debt_decimals: u8,
    /// The operator's own bid (USDT-per-cNGN, spread applied) — fills users
    /// selling collateral. None when the buy side isn't configured.
    pub bid: Option<f64>,
    /// The operator's own ask — fills users selling debt. None when the sell
    /// side isn't configured.
    pub ask: Option<f64>,
    /// The reactor's native fee, read on-chain at startup.
    pub fee_bps: u32,
    /// Per-order minimum profit, valued in debt atomic units (gas/dust guard).
    pub min_profit_debt: U256,
    pub max_orders: usize,
}

/// A profitable resting order: what filling it spends and clears.
#[derive(Debug, Clone)]
pub struct FillCandidate {
    pub order: RestingOrder,
    /// Output-token amount executeBatch pulls from the taker (output + fee).
    pub spend: U256,
    /// Estimated profit valued in debt atomic units; the ranking key.
    pub profit_debt: U256,
}

/// Evaluate one verified order against the operator's own quote. Returns the
/// candidate when filling it beats quoting the same side of the book.
pub fn evaluate(order: &RestingOrder, ctx: &TakerCtx) -> Option<FillCandidate> {
    let fee = order.output_amount * U256::from(ctx.fee_bps) / U256::from(BPS);
    // The reactor reverts a zero fee (FeeRoundsToZero); the indexer filters
    // these but a hostile one might not.
    if fee.is_zero() {
        return None;
    }
    let spend = order.output_amount + fee;

    let profit_debt = if order.input_token == ctx.collateral && order.output_token == ctx.debt {
        // User sells collateral for debt — we buy collateral. Pay `spend`
        // debt, receive `input_amount` collateral, valued at our own bid.
        let bid = ctx.bid?;
        let received: u128 = order.input_amount.try_into().ok()?;
        let (_, value_debt) =
            sell_amounts_at(bid, received, ctx.debt_decimals, ctx.collateral_decimals);
        value_debt.checked_sub(spend)?
    } else if order.input_token == ctx.debt && order.output_token == ctx.collateral {
        // User sells debt for collateral — we sell collateral. Pay `spend`
        // collateral (valued at our own ask), receive `input_amount` debt.
        let ask = ctx.ask?;
        let paid: u128 = spend.try_into().ok()?;
        let (_, cost_debt) = sell_amounts_at(ask, paid, ctx.debt_decimals, ctx.collateral_decimals);
        order.input_amount.checked_sub(cost_debt)?
    } else {
        return None; // not this pool's pair
    };

    if profit_debt.is_zero() || profit_debt < ctx.min_profit_debt {
        return None;
    }
    Some(FillCandidate {
        order: order.clone(),
        spend,
        profit_debt,
    })
}

/// Rank candidates most-profitable-first and keep what the wallet's live
/// output-token balance covers, up to `max_orders`. One batch spends a single
/// output token, so the running total is against one balance.
pub fn plan_batch(
    mut candidates: Vec<FillCandidate>,
    balance: U256,
    max_orders: usize,
) -> Vec<FillCandidate> {
    candidates.sort_by(|a, b| b.profit_debt.cmp(&a.profit_debt));
    let mut spent = U256::ZERO;
    candidates
        .into_iter()
        .filter(|c| {
            if spent + c.spend > balance {
                return false;
            }
            spent += c.spend;
            true
        })
        .take(max_orders)
        .collect()
}

// ---------------------------------------------------------------------------
// Calldata — hand-rolled ABI, golden-locked to the TS reference
// ---------------------------------------------------------------------------

fn word_u256(v: U256) -> [u8; 32] {
    v.to_be_bytes::<32>()
}

fn word_usize(v: usize) -> [u8; 32] {
    word_u256(U256::from(v))
}

fn word_address(a: Address) -> [u8; 32] {
    a.into_word().0
}

/// `abi.encode(LimitOrder)` — the `order` bytes `executeBatch` decodes. Must
/// match the indexer's `encodeLimitOrder` byte for byte (goldens below);
/// encoding locally from verified fields is what keeps a hostile indexer's
/// `encodedOrder` out of the transaction entirely.
pub fn encode_order_bytes(o: &OrderParams) -> Vec<u8> {
    let mut out = Vec::with_capacity(17 * 32);
    out.extend_from_slice(&word_usize(0x20)); // offset to the tuple

    // Tuple head: [info offset, input.token, input.amount, input.maxAmount,
    // outputs offset]. `info` carries dynamic bytes so it's referenced by
    // offset; `input` is static and inlines.
    out.extend_from_slice(&word_usize(5 * 32)); // info starts after the head
    out.extend_from_slice(&word_address(o.input_token));
    out.extend_from_slice(&word_u256(o.input_amount));
    out.extend_from_slice(&word_u256(o.input_amount)); // maxAmount == amount
    out.extend_from_slice(&word_usize(5 * 32 + 7 * 32)); // outputs after info

    // OrderInfo: 5 static words + offset to empty `additionalValidationData`.
    out.extend_from_slice(&word_address(o.reactor));
    out.extend_from_slice(&word_address(o.swapper));
    out.extend_from_slice(&word_u256(o.nonce));
    out.extend_from_slice(&word_u256(o.deadline));
    out.extend_from_slice(&word_address(Address::ZERO));
    out.extend_from_slice(&word_usize(6 * 32)); // bytes offset within info
    out.extend_from_slice(&word_usize(0)); // len(additionalValidationData)

    // OutputToken[1]
    out.extend_from_slice(&word_usize(1));
    out.extend_from_slice(&word_address(o.output_token));
    out.extend_from_slice(&word_u256(o.output_amount));
    out.extend_from_slice(&word_address(o.recipient));
    out
}

fn padded_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + data.len().div_ceil(32) * 32);
    out.extend_from_slice(&word_usize(data.len()));
    out.extend_from_slice(data);
    out.resize(32 + data.len().div_ceil(32) * 32, 0);
    out
}

/// Calldata for `executeBatch(SignedOrder[])`, SignedOrder = (bytes, bytes).
pub fn encode_execute_batch(orders: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&selector("executeBatch((bytes,bytes)[])"));
    data.extend_from_slice(&word_usize(0x20)); // offset to the array
    data.extend_from_slice(&word_usize(orders.len()));

    // Per-element offsets are relative to the word right after the length.
    let mut tuples: Vec<Vec<u8>> = Vec::with_capacity(orders.len());
    for (order_bytes, sig) in orders {
        let order_area = padded_bytes(order_bytes);
        let sig_area = padded_bytes(sig);
        let mut tuple = Vec::with_capacity(64 + order_area.len() + sig_area.len());
        tuple.extend_from_slice(&word_usize(0x40)); // order bytes offset
        tuple.extend_from_slice(&word_usize(0x40 + order_area.len())); // sig offset
        tuple.extend_from_slice(&order_area);
        tuple.extend_from_slice(&sig_area);
        tuples.push(tuple);
    }
    let mut offset = orders.len() * 32;
    for tuple in &tuples {
        data.extend_from_slice(&word_usize(offset));
        offset += tuple.len();
    }
    for tuple in &tuples {
        data.extend_from_slice(tuple);
    }
    data
}

// ---------------------------------------------------------------------------
// On-chain fee discovery
// ---------------------------------------------------------------------------

/// Read the reactor's native fee: `reactor.feeController()` then
/// `controller.FEE_BPS()`. On-chain truth, so the planning math can never
/// drift from what executeBatch will actually pull (the controller hard-caps
/// at 5 bps regardless).
pub async fn resolve_fee_bps(wallet: &Wallet, reactor: Address) -> anyhow::Result<u32> {
    let controller_word = wallet
        .read_uint(reactor, &Bytes::from(selector("feeController()").to_vec()))
        .await?;
    let controller = Address::from_word(controller_word.into());
    if controller == Address::ZERO {
        anyhow::bail!("reactor has no fee controller configured");
    }
    let fee = wallet
        .read_uint(controller, &Bytes::from(selector("FEE_BPS()").to_vec()))
        .await?;
    let fee: u32 = fee
        .try_into()
        .map_err(|_| anyhow::anyhow!("fee controller returned an oversized FEE_BPS"))?;
    anyhow::ensure!(fee > 0 && fee <= 100, "implausible native fee: {fee} bps");
    Ok(fee)
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// What one taker tick did for one pool direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeOutcome {
    Nothing,
    /// Dry run: the batch that would have been filled.
    Planned {
        orders: usize,
        spend: U256,
    },
    /// Submitted an executeBatch; carries the tx hash.
    Filled {
        hash: B256,
        orders: usize,
        spend: U256,
    },
}

/// Approve `spender` to pull `token` if the allowance is short (one-time max
/// approval — same pattern as the closer; the reactor only ever pulls what a
/// signed order + its hard-capped fee dictate).
async fn ensure_reactor_allowance(
    wallet: &Wallet,
    token: Address,
    reactor: Address,
    needed: U256,
) -> anyhow::Result<()> {
    let allowance = wallet
        .read_uint(
            token,
            &Bytes::from(crate::closer::executor::encode_allowance(
                wallet.address(),
                reactor,
            )),
        )
        .await?;
    if allowance >= needed {
        return Ok(());
    }
    // USDT-safe: tokens like USDT reject a nonzero→nonzero allowance change,
    // so clear any short leftover (a finite or manual approval) before the max.
    if allowance > U256::ZERO {
        info!(token = %token, reactor = %reactor, "resetting short reactor allowance to 0");
        wallet
            .send_and_wait(
                token,
                Bytes::from(encode_approve(reactor, U256::ZERO)),
                U256::ZERO,
                std::time::Duration::from_secs(120),
            )
            .await?;
    }
    info!(token = %token, reactor = %reactor, "approving output token to reactor");
    wallet
        .send_and_wait(
            token,
            Bytes::from(encode_approve(reactor, U256::MAX)),
            U256::ZERO,
            std::time::Duration::from_secs(120),
        )
        .await?;
    Ok(())
}

/// Run the taker leg for one pool direction: discover resting LIMIT orders
/// with `output_token` as the side we'd spend, verify + evaluate them against
/// our own quote, and fill the profitable batch.
#[allow(clippy::too_many_arguments)]
async fn take_direction_once(
    wallet: &Wallet,
    indexer: &Indexer,
    ctx: &TakerCtx,
    chain_id: u64,
    permit2: Address,
    reactor: Address,
    input_token: Address,
    output_token: Address,
    dry_run: bool,
    recently_submitted: &mut HashMap<String, u64>,
    now: u64,
) -> anyhow::Result<TakeOutcome> {
    let rows = indexer
        .resting_limit_orders(
            chain_id,
            &input_token.to_string(),
            &output_token.to_string(),
        )
        .await?;
    let orders = parse_resting_orders(&rows);
    if orders.is_empty() {
        return Ok(TakeOutcome::Nothing);
    }

    let candidates: Vec<FillCandidate> = orders
        .iter()
        .filter(|o| match recently_submitted.get(&o.id) {
            Some(&t) => now.saturating_sub(t) >= RESUBMIT_COOLDOWN_SECS,
            None => true,
        })
        .filter(|o| {
            match verify_order(o, reactor, wallet.address(), permit2, chain_id, now) {
                Ok(()) => true,
                Err(reason) => {
                    // Own orders and expiring ones are normal; anything else
                    // points at a broken (or hostile) indexer.
                    if reason != "own order" && reason != "expiring" {
                        warn!(order = %o.id, reason, "dropping unverifiable resting order");
                    }
                    false
                }
            }
        })
        .filter_map(|o| evaluate(o, ctx))
        .collect();
    if candidates.is_empty() {
        return Ok(TakeOutcome::Nothing);
    }

    let balance = wallet
        .read_uint(
            output_token,
            &Bytes::from(encode_balance_of(wallet.address())),
        )
        .await?;
    let batch = plan_batch(candidates, balance, ctx.max_orders);
    if batch.is_empty() {
        return Ok(TakeOutcome::Nothing);
    }
    let spend = batch.iter().fold(U256::ZERO, |acc, c| acc + c.spend);

    if dry_run {
        return Ok(TakeOutcome::Planned {
            orders: batch.len(),
            spend,
        });
    }

    ensure_reactor_allowance(wallet, output_token, reactor, spend).await?;
    let payload: Vec<(Vec<u8>, Vec<u8>)> = batch
        .iter()
        .map(|c| {
            (
                encode_order_bytes(&c.order.params()),
                c.order.signature.clone(),
            )
        })
        .collect();
    let hash = wallet
        .send(
            reactor,
            Bytes::from(encode_execute_batch(&payload)),
            U256::ZERO,
        )
        .await?;
    for c in &batch {
        recently_submitted.insert(c.order.id.clone(), now);
    }
    Ok(TakeOutcome::Filled {
        hash,
        orders: batch.len(),
        spend,
    })
}

/// Run the taker leg for one pool tick: both directions, each priced by the
/// side of the operator's own book that would fill it.
#[allow(clippy::too_many_arguments)]
pub async fn take_pool_once(
    wallet: &Wallet,
    indexer: &Indexer,
    ctx: &TakerCtx,
    chain_id: u64,
    permit2: Address,
    reactor: Address,
    dry_run: bool,
    recently_submitted: &mut HashMap<String, u64>,
    now: u64,
) -> Vec<(&'static str, anyhow::Result<TakeOutcome>)> {
    recently_submitted.retain(|_, t| now.saturating_sub(*t) < RESUBMIT_COOLDOWN_SECS);

    let mut outcomes = Vec::with_capacity(2);
    // Users selling collateral (we pay debt) — priced by our bid.
    if ctx.bid.is_some() {
        outcomes.push((
            "buy",
            take_direction_once(
                wallet,
                indexer,
                ctx,
                chain_id,
                permit2,
                reactor,
                ctx.collateral,
                ctx.debt,
                dry_run,
                recently_submitted,
                now,
            )
            .await,
        ));
    }
    // Users selling debt (we pay collateral) — priced by our ask.
    if ctx.ask.is_some() {
        outcomes.push((
            "sell",
            take_direction_once(
                wallet,
                indexer,
                ctx,
                chain_id,
                permit2,
                reactor,
                ctx.debt,
                ctx.collateral,
                dry_run,
                recently_submitted,
                now,
            )
            .await,
        ));
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex};
    use k256::ecdsa::SigningKey;
    use serde_json::json;

    use crate::signer::{address_from_signing_key, parse_private_key, sign_digest};

    const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
    const REACTOR: Address = address!("1111111111111111111111111111111111111111");
    const CNGN: Address = address!("4444444444444444444444444444444444444444");
    const USDT: Address = address!("3333333333333333333333333333333333333333");
    const KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const CHAIN: u64 = 8453;

    fn signing_key() -> SigningKey {
        parse_private_key(KEY).unwrap()
    }

    /// A signed resting order: the user sells 1550 cNGN (6dp) for 1 USDT (6dp).
    fn signed_order(deadline_sec: u64) -> RestingOrder {
        let key = signing_key();
        let maker = address_from_signing_key(&key);
        let mut order = RestingOrder {
            id: "r1".into(),
            reactor: REACTOR,
            maker,
            input_token: CNGN,
            input_amount: U256::from(1_550_000_000u64),
            output_token: USDT,
            output_amount: U256::from(1_000_000u64),
            nonce: U256::from(42u64),
            deadline_sec,
            signature: vec![],
        };
        let digest = permit2_digest(&order.params(), PERMIT2, CHAIN);
        order.signature = sign_digest(&key, digest).unwrap().to_vec();
        order
    }

    fn ctx() -> TakerCtx {
        TakerCtx {
            collateral: CNGN,
            debt: USDT,
            collateral_decimals: 6,
            debt_decimals: 6,
            // Feed mid 1/1400 USDT-per-cNGN with some spread on both sides.
            bid: Some(1.0 / 1420.0),
            ask: Some(1.0 / 1380.0),
            fee_bps: 5,
            min_profit_debt: U256::ZERO,
            max_orders: 10,
        }
    }

    // --- verification ------------------------------------------------------

    #[test]
    fn verifies_a_genuine_order_and_rejects_tampering() {
        let order = signed_order(4_000_000_000);
        let me = address!("00000000000000000000000000000000000000aa");
        assert!(verify_order(&order, REACTOR, me, PERMIT2, CHAIN, 1_000).is_ok());

        // Any field change breaks recovery — a lying indexer can't redirect.
        let mut tampered = order.clone();
        tampered.output_amount = U256::from(999_999u64);
        assert_eq!(
            verify_order(&tampered, REACTOR, me, PERMIT2, CHAIN, 1_000),
            Err("signature does not match maker")
        );

        let mut foreign = order.clone();
        foreign.reactor = address!("00000000000000000000000000000000000000ff");
        assert_eq!(
            verify_order(&foreign, REACTOR, me, PERMIT2, CHAIN, 1_000),
            Err("foreign reactor")
        );

        assert_eq!(
            verify_order(&order, REACTOR, order.maker, PERMIT2, CHAIN, 1_000),
            Err("own order")
        );

        assert_eq!(
            verify_order(&order, REACTOR, me, PERMIT2, CHAIN, 4_000_000_000),
            Err("expiring")
        );
    }

    // --- economics ---------------------------------------------------------

    #[test]
    fn fills_a_collateral_seller_below_the_bid() {
        // Order: 1550 cNGN for 1 USDT → 1550 cNGN/USDT, cheaper than the
        // bid's 1420. Spend = 1 USDT + 5bps fee = 1.0005; value at bid =
        // 1550/1420 ≈ 1.0915 USDT. The expected value comes from the same
        // scaled-integer pricing the maker legs use (quote.rs owns its own
        // correctness tests), so this pins the wiring: value − spend.
        let c = evaluate(&signed_order(4_000_000_000), &ctx()).expect("profitable");
        assert_eq!(c.spend, U256::from(1_000_500u64));
        let (_, expected_value) = sell_amounts_at(1.0 / 1420.0, 1_550_000_000, 6, 6);
        assert!(expected_value > c.spend);
        assert_eq!(c.profit_debt, expected_value - c.spend);
    }

    #[test]
    fn skips_a_collateral_seller_above_the_bid() {
        // 1380 cNGN per USDT is more expensive than our 1420 bid.
        let mut order = signed_order(4_000_000_000);
        order.input_amount = U256::from(1_380_000_000u64);
        assert!(evaluate(&order, &ctx()).is_none());
    }

    #[test]
    fn fills_a_debt_seller_above_the_ask() {
        // User sells 1 USDT for 1300 cNGN — we hand out collateral at 1300,
        // dearer than our 1380 ask ⇒ profitable.
        let mut order = signed_order(4_000_000_000);
        order.input_token = USDT;
        order.input_amount = U256::from(1_000_000u64);
        order.output_token = CNGN;
        order.output_amount = U256::from(1_300_000_000u64);
        let c = evaluate(&order, &ctx()).expect("profitable");
        // Spend = 1300 cNGN + 5 bps.
        assert_eq!(c.spend, U256::from(1_300_650_000u64));
        let cost_debt = U256::from((1_300_650_000f64 / 1380.0) as u64);
        assert_eq!(c.profit_debt, U256::from(1_000_000u64) - cost_debt);
    }

    #[test]
    fn respects_min_profit_and_disabled_sides() {
        let order = signed_order(4_000_000_000);
        let mut c = ctx();
        c.min_profit_debt = U256::from(10_000_000u64); // 10 USDT — too high
        assert!(evaluate(&order, &c).is_none());

        let mut no_bid = ctx();
        no_bid.bid = None; // buy side not configured → don't take that side
        assert!(evaluate(&order, &no_bid).is_none());
    }

    #[test]
    fn plans_best_profit_first_within_balance_and_cap() {
        let order = signed_order(4_000_000_000);
        let candidate = |id: &str, spend: u64, profit: u64| FillCandidate {
            order: RestingOrder {
                id: id.into(),
                ..order.clone()
            },
            spend: U256::from(spend),
            profit_debt: U256::from(profit),
        };
        let batch = plan_batch(
            vec![
                candidate("small", 100, 1),
                candidate("best", 500, 50),
                candidate("second", 450, 20),
            ],
            U256::from(1_000u64),
            10,
        );
        // Best first; "second" would overflow the balance after "best", the
        // smaller one still fits.
        let ids: Vec<&str> = batch.iter().map(|c| c.order.id.as_str()).collect();
        assert_eq!(ids, vec!["best", "second"]);

        let capped = plan_batch(
            vec![
                candidate("a", 1, 3),
                candidate("b", 1, 2),
                candidate("c", 1, 1),
            ],
            U256::from(1_000u64),
            2,
        );
        assert_eq!(capped.len(), 2);
    }

    // --- parsing -----------------------------------------------------------

    #[test]
    fn parses_indexer_rows_and_drops_malformed_ones() {
        let genuine = signed_order(4_000_000_000);
        let sig_hex = alloy_primitives::hex::encode_prefixed(&genuine.signature);
        let rows = json!([
            {
                "id": "r1",
                "reactor": REACTOR.to_string(),
                "maker": genuine.maker.to_string(),
                "inputToken": CNGN.to_string(),
                "inputAmount": "1550000000",
                "outputToken": USDT.to_string(),
                "outputAmount": 1000000,
                "nonce": "42",
                "deadlineSec": "4000000000",
                "signature": sig_hex,
            },
            { "id": "broken", "maker": "not-an-address" }
        ]);
        let parsed = parse_resting_orders(&rows);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].input_amount, U256::from(1_550_000_000u64));
        assert_eq!(parsed[0].deadline_sec, 4_000_000_000);
        assert_eq!(parsed[0].signature, genuine.signature);
    }

    // --- calldata goldens (from the TS reference implementation) -----------

    fn golden_order() -> OrderParams {
        OrderParams {
            reactor: REACTOR,
            swapper: address!("2222222222222222222222222222222222222222"),
            nonce: U256::from(7u64),
            deadline: U256::from(1_900_000_000u64),
            input_token: USDT,
            input_amount: U256::from(1_000_000u64),
            output_token: CNGN,
            output_amount: U256::from(1_550_000_000u64),
            recipient: address!("2222222222222222222222222222222222222222"),
        }
    }

    #[test]
    fn order_bytes_match_the_indexer_encoding() {
        // Golden from `encodeLimitOrder` in
        // api/src/services/fillerOrders/permit2Order.ts for the same order —
        // the reactor must decode our bytes to the exact struct the maker
        // signed.
        let expected = hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000000000000000020",
            "00000000000000000000000000000000000000000000000000000000000000a0",
            "0000000000000000000000003333333333333333333333333333333333333333",
            "00000000000000000000000000000000000000000000000000000000000f4240",
            "00000000000000000000000000000000000000000000000000000000000f4240",
            "0000000000000000000000000000000000000000000000000000000000000180",
            "0000000000000000000000001111111111111111111111111111111111111111",
            "0000000000000000000000002222222222222222222222222222222222222222",
            "0000000000000000000000000000000000000000000000000000000000000007",
            "00000000000000000000000000000000000000000000000000000000713fb300",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "00000000000000000000000000000000000000000000000000000000000000c0",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000004444444444444444444444444444444444444444",
            "000000000000000000000000000000000000000000000000000000005c631f80",
            "0000000000000000000000002222222222222222222222222222222222222222",
        ))
        .unwrap();
        assert_eq!(encode_order_bytes(&golden_order()), expected);
    }

    #[test]
    fn execute_batch_matches_the_viem_encoding() {
        // Golden from viem's encodeFunctionData for
        // executeBatch([{order: 0x11223344, sig: 0xaabb}]).
        let data = encode_execute_batch(&[(vec![0x11, 0x22, 0x33, 0x44], vec![0xaa, 0xbb])]);
        let expected = hex::decode(concat!(
            "0d7a16c3",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000080",
            "0000000000000000000000000000000000000000000000000000000000000004",
            "1122334400000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000002",
            "aabb000000000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap();
        assert_eq!(data, expected);
    }

    #[test]
    fn execute_batch_encodes_two_orders_with_correct_offsets() {
        // Two orders of different lengths: the second element's offset must
        // account for the first tuple's full padded size.
        let data = encode_execute_batch(&[
            (vec![0x11; 33], vec![0xaa; 65]),
            (vec![0x22; 4], vec![0xbb; 65]),
        ]);
        assert_eq!(&data[..4], &hex::decode("0d7a16c3").unwrap()[..]);
        // Words after the selector: [array offset, len, offset0, offset1, …].
        assert_eq!(U256::from_be_slice(&data[36..68]), U256::from(2u8)); // len
        assert_eq!(U256::from_be_slice(&data[68..100]), U256::from(0x40u8));
        // First tuple: heads (2) + order (1 len + 2 data words) + sig
        // (1 len + 3 data words) = 9 words; second offset = 0x40 + 9*32.
        assert_eq!(
            U256::from_be_slice(&data[100..132]),
            U256::from(0x40 + 9 * 32)
        );
    }
}
