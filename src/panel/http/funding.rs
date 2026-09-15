// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! What the bot's wallet holds, in the bot's own terms: is there enough to start?
//!
//! The wizard's Fund screen polls this every few seconds while an operator
//! sends money in. One read answers every question that screen has — how much
//! of each corridor token is in the wallet, what that is worth in dollars,
//! whether there is gas, and whether Permit2 is approved — so the screen can
//! never show two snapshots that disagree with each other.
//!
//! The rule it applies, agreed with the product owner (2026-09-14): the bot
//! may be approved and started as soon as the wallet holds enough of the
//! chain's gas token to pay for the approvals still outstanding. Nothing else.
//! Approval is permission, not money, and needs no token balance; the trading
//! money arrives afterwards, on the Live screen, and the bot quotes the moment
//! one side holds [`FUND_MIN_TOKEN_USD`]. The token rows and `fundedTokens`
//! are still reported for that screen; they just no longer hold the gate. Gas
//! is priced by [`crate::panel::native_price`].
//!
//! Being a dollar is checked, not assumed. The pool's DEBT side is whatever
//! the corridor quotes against, and Textile lists corridors whose debt token
//! is not a dollar at all — `cNGN ↔ GD` on Celo prices in GoodDollar, worth
//! about $0.00006. So the debt side counts at a dollar only when it is on
//! [`DOLLAR_TOKENS`], and the soft side is valued off the pool's feed only
//! then too: that feed reads DEBT per collateral, which is dollars per token
//! exactly when the debt token is one. On any other pool both rows come back
//! unpriced, `funded` is unknown, and the gate refuses — rather than certify a
//! wallet holding a tenth of a cent and start a bot with nothing to quote.
//!
//! Approvals are NOT limited to the side that holds money, and the gas figure
//! is sized accordingly. The bot's own preflight (`src/main.rs`) refuses to
//! start a live bot while any ENABLED side is unapproved, so approving only
//! the funded token would produce a bot that can never start. Both sides are
//! approved, and [`min_gas_usd`] asks for one transaction's worth of gas per
//! approval still outstanding — otherwise the gate would certify a wallet with
//! a dollar of ETH on it and the second approve would die for gas.
//!
//! Everything after the config parse is a 200. A chain that won't answer, a
//! feed that is down, a price nobody can find — each of those degrades its own
//! row to "unknown" with a reason, never the whole request, because the screen
//! has to keep rendering and keep polling. Every outbound call is bounded on
//! its own (one slow `eth_call` costs that row, not the picture), and the reads
//! run concurrently, so the handler answers in about six seconds even with a
//! dead RPC and dead price sources.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{keccak256, Address, Bytes, U256};
use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use super::allowances::{read_allowance, short_token, token_symbols};
use super::settings::config_path;
use super::{ApiError, AppState};
use crate::chain::approve::{approval_action, required_approvals, ApprovalAction, ApprovalMode};
use crate::chain::rpc::Rpc;
use crate::closer::executor::encode_balance_of;
use crate::config::Config;
use crate::panel::inventory::Bot;
use crate::panel::native_price::{gas_symbol, gas_token, http_origin, GasToken, PriceSource};
use crate::pricing::feed::{HttpFeed, PriceFeed};
use crate::pricing::tick::is_price_usable;
use crate::protocol::vault::{
    address_from_word, encode_corridor_asset, encode_liquid_settlement, encode_quotable_corridor,
    encode_quotable_settlement, encode_settlement_asset, quotable_settlement_for_route,
};
use crate::setup;

/// A side counts as funded when its balance is worth at least this many dollars.
pub const FUND_MIN_TOKEN_USD: f64 = 20.0;

/// Dollars of gas to ask for per transaction on a chain the panel can't name.
/// Known chains carry their own figure in [`GasToken::tx_gas_usd`].
pub const FUND_MIN_GAS_USD: f64 = 1.0;

/// US dollar stablecoins, by `(chain id, lowercase address)`.
///
/// The only tokens the panel values at exactly one dollar. Nothing infers this
/// from a ticker or from a token's position in the pool: a corridor's debt
/// side is simply what it quotes against, and the live registry has corridors
/// that quote against tokens worth a fraction of a cent.
///
/// Add a row when Textile lists a corridor against another dollar stable. A
/// missing row is safe (that pool's rows read "unknown" and the gate waits); a
/// wrong row is not, so only addresses verified on the chain belong here.
const DOLLAR_TOKENS: &[(u64, &str)] = &[
    // Ethereum
    (1, "0xdac17f958d2ee523a2206206994597c13d831ec7"), // USDT
    (1, "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), // USDC
    // BNB Smart Chain, and its testnet faucet token
    (56, "0x55d398326f99059ff775485246999027b3197955"), // USDT
    (56, "0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d"), // USDC
    (97, "0x337610d27c682e347c9cd60bd4b3b107c9d34ddd"), // USDT (testnet)
    // Base
    (8453, "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"), // USDC
    (8453, "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2"), // USDT
    // Arbitrum One
    (42161, "0xaf88d065e77c8cc2239327c5edb3a432268e5831"), // USDC
    (42161, "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9"), // USDT
    // Celo
    (42220, "0x48065fbbe25f71c9282ddf5e1cd6d6a887483d5e"), // USDT
    (42220, "0xceba9300f2b948710d2653dd7b07f33a8b32118c"), // USDC
    // Robinhood Chain
    (4663, "0x5fc5360d0400a0fd4f2af552add042d716f1d168"), // USDG
];

/// Whether this token is a dollar the panel will value at 1.0.
///
/// `key` is the token's lowercase `0x…` form, the same shape [`TokenPlan::key`]
/// carries.
fn is_dollar_token(chain_id: u64, key: &str) -> bool {
    DOLLAR_TOKENS
        .iter()
        .any(|(chain, address)| *chain == chain_id && *address == key)
}

/// Why both rows of a pool that quotes against something else read "unknown".
const NOT_A_DOLLAR_PAIR: &str = "the panel can't price this token in dollars";

/// Each chain read gets this long, on its own. The shared RPC client allows
/// fifteen seconds per request, which is fine for a bot and far too long for
/// a screen polling every five.
const CHAIN_BUDGET: Duration = Duration::from_secs(6);

/// Each price lookup gets this long. They run beside the chain reads, so they
/// never extend the response past the chain budget.
const PRICE_BUDGET: Duration = Duration::from_secs(4);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingTokenBody {
    /// `"stable"` for a pool's debt token (USDT, USDC…), `"soft"` for its
    /// collateral (cNGN, wBRL…).
    pub role: &'static str,
    /// Ticker from the corridor's display name, else the token's own
    /// `symbol()`, else a shortened address.
    pub symbol: String,
    /// Lowercase `0x…` address.
    pub token: String,
    pub decimals: u8,
    /// Balance in atomic units, decimal. `null` when the read failed. On a
    /// vault maker this is what the vault can quote, not what its address
    /// holds — see [`read_inventory`].
    pub balance: Option<String>,
    /// The same balance in whole tokens, e.g. `"25"` or `"0.5"`.
    pub balance_text: Option<String>,
    /// Dollars per whole token. `1.0` for the stable side.
    pub price: Option<f64>,
    /// `"fixed"` for the stable side, `"feed"` when the pool's feed answered.
    pub price_source: Option<&'static str>,
    /// Why `price` is null, in operator words.
    pub price_error: Option<String>,
    /// This token can never be valued in dollars on this bot, because the pool
    /// it came from does not quote against a dollar stable.
    ///
    /// The per-row twin of [`FundingGateBody::unpriceable`], which can only
    /// speak for the whole bot. A bot with one dollar pool and one that is not
    /// leaves the gate's flag false while this corridor's own rows stay
    /// permanently unvalued, and a caller adding that second corridor needs to
    /// know the difference between "the feed is down" and "no feed can answer
    /// this". Rows are deduplicated by token, so a token shared with an earlier
    /// pool carries that pool's answer.
    pub unpriceable: bool,
    pub usd: Option<f64>,
    /// `usd >= minTokenUsd`. `null` when the value is unknown.
    pub funded: Option<bool>,
    /// The bot's own preflight requires a Permit2 approval on this token.
    pub approval_needed: bool,
    /// Current Permit2 allowance, decimal. `null` when the read failed.
    pub permit2_allowance: Option<String>,
    /// Whether that allowance satisfies the config — the same test the bot's
    /// live-start preflight applies. `null` when unknown.
    pub approved: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingGasBody {
    /// `CELO`, `BNB`, `ETH`, `POL`, or the plain word `gas` on a chain the
    /// panel doesn't know.
    pub symbol: &'static str,
    /// Wei, decimal. `null` when the read failed.
    pub balance: Option<String>,
    pub balance_text: Option<String>,
    pub price: Option<f64>,
    pub price_source: Option<PriceSource>,
    pub usd: Option<f64>,
    /// `usd >= minGasUsd`; on a chain with no price at all, any non-zero
    /// balance. `null` when the balance is unknown.
    pub ok: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingGateBody {
    /// The bot may be approved and started: gas covers the approvals still
    /// outstanding. Says nothing about the token sides (see `needs_side`).
    pub passes: bool,
    pub min_token_usd: f64,
    pub min_gas_usd: f64,
    /// Symbols of the tokens that clear `minTokenUsd`.
    pub funded_tokens: Vec<String>,
    /// Symbols the bot still needs a Permit2 approval for. An unknown allowance
    /// counts as missing: approving is idempotent, so erring this way costs at
    /// most a skipped transaction.
    pub approvals_missing: Vec<String>,
    /// No side is funded yet.
    pub needs_side: bool,
    /// Gas is short (or unknown).
    pub needs_gas: bool,
    /// Nothing this bot holds can ever be valued in dollars.
    ///
    /// True when no pool quotes against a token on [`DOLLAR_TOKENS`]: both rows
    /// of every pool come back unpriced, `funded` stays unknown and `needs_side`
    /// stays true whatever arrives in the wallet. It is a property of the
    /// corridor and of this build's allowlist, not of the wallet or of a feed,
    /// so it never clears by waiting.
    ///
    /// It has no say in `passes`, which is gas alone. Both wizard lanes wait on
    /// gas and leave the trading money to the bot page, so a pair nobody can
    /// value no longer stops a setup; the flag is reported for callers that
    /// want to explain a blank dollar figure.
    pub unpriceable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingBody {
    /// The wallet to fund. `null` when the panel has no key or `[signer]`
    /// address to derive it from; `readError` then says so.
    pub operator_address: Option<String>,
    /// The address the token rows were read at: the OperatorVault when
    /// `[vault]` is set, else the operator wallet.
    pub capital_address: Option<String>,
    /// Which of the two `capital_address` is: `"vault"` or `"wallet"`.
    pub capital_source: &'static str,
    /// Explorer page for `capital_address`, when the chain has one. Always
    /// real for a vault: it is a contract on this chain.
    pub capital_explorer_url: Option<String>,
    pub chain_id: u64,
    /// `Celo`, `BNB Smart Chain`… from the pool's corridor identity. `null`
    /// for a custom pool.
    pub network_label: Option<String>,
    /// The wallet on the chain's explorer, when the chain has one.
    pub explorer_url: Option<String>,
    pub permit2: String,
    /// What the bot quotes against — the vault's balances when there is one.
    pub tokens: Vec<FundingTokenBody>,
    /// What the signer wallet itself holds, when the capital is somewhere
    /// else. `null` without a vault, where `tokens` is already the wallet.
    ///
    /// Gas, dust, a mistaken transfer: the balances that die with the key, so
    /// this is what the remove gate weighs and what a withdraw can move.
    /// Never priced differently from `tokens` — same plans, same feeds, other
    /// address — and never carries approvals: a vault maker has none.
    pub wallet_tokens: Option<Vec<FundingTokenBody>>,
    pub gas: FundingGasBody,
    pub gate: FundingGateBody,
    /// The first chain error, if any. Set once for the whole request: every
    /// read hits the same node, so per-row copies would just repeat it.
    pub read_error: Option<String>,
    /// Why deleting this bot's key would lose money, in operator words, or
    /// `null` when the wallet is empty enough that removal is only cleanup.
    /// The page shows it next to Remove; the remove route enforces it.
    pub remove_blocked_by: Option<String>,
    pub checked_at_unix: u64,
}

/// Removing a bot below this much is cleanup; above it is losing money. Ten
/// dollars covers dust and leftover gas without covering anything an operator
/// would miss.
pub const REMOVE_MAX_USD: f64 = 10.0;

/// Dollars in the wallet, or `None` when nothing could be priced.
fn total_usd(tokens: &[FundingTokenBody], gas: &FundingGasBody) -> Option<f64> {
    let priced: Vec<f64> = tokens
        .iter()
        .map(|t| t.usd)
        .chain(std::iter::once(gas.usd))
        .flatten()
        .collect();
    (!priced.is_empty()).then(|| priced.iter().sum())
}

/// Why deleting the key behind this wallet would lose money, or `None` when it
/// would not. Refuses while the wallet holds more than [`REMOVE_MAX_USD`],
/// while any side with a balance is unpriced (it could be worth anything), and
/// while the wallet cannot be read at all (it could hold anything). Withdraw
/// first is the way through in every case. A bot with no readable address has
/// no key to lose.
///
/// The rows are the tokens the bot's corridors trade plus the gas coin: an RPC
/// cannot list every ERC-20 an address holds, so a token sent here by mistake
/// is outside the gate. The confirm dialog says so.
fn remove_refusal(
    operator_address: Option<&str>,
    tokens: &[FundingTokenBody],
    gas: &FundingGasBody,
    read_error: Option<&str>,
) -> Option<String> {
    operator_address?;
    if let Some(why) = read_error {
        return Some(format!(
            "The panel cannot read this wallet right now, so it cannot tell whether it is \
             empty: {why} Try again when it can."
        ));
    }
    let held_unpriced = |usd: Option<f64>, balance: Option<&str>| {
        usd.is_none() && balance.is_some_and(|b| b != "0")
    };
    let unpriced: Vec<&str> = tokens
        .iter()
        .filter(|t| held_unpriced(t.usd, t.balance.as_deref()))
        .map(|t| t.symbol.as_str())
        .chain(held_unpriced(gas.usd, gas.balance.as_deref()).then_some(&*gas.symbol))
        .collect();
    if !unpriced.is_empty() {
        return Some(format!(
            "The wallet holds {} the panel cannot price. Withdraw it first (Funds tab).",
            unpriced.join(" and ")
        ));
    }
    let total = total_usd(tokens, gas)?;
    (total > REMOVE_MAX_USD).then(|| {
        format!(
            "The wallet holds ${total:.2}. Removing the bot deletes its key, and the key is the \
             only way to that money. Withdraw first (Funds tab); removal unlocks under \
             ${REMOVE_MAX_USD:.0}."
        )
    })
}

/// One corridor token the wallet may hold.
struct TokenPlan {
    address: Address,
    key: String,
    role: &'static str,
    decimals: u8,
    /// The feed that prices it (soft tokens only).
    feed_url: Option<String>,
    /// The ticker the corridor gives it, when it has one.
    ticker: Option<String>,
    /// This pool quotes against a known dollar stable, so its rows can be
    /// valued in dollars: the debt side at 1.0, the collateral side off the
    /// feed. False leaves both rows unpriced.
    priced_in_usd: bool,
    approval_needed: bool,
    required: U256,
    uses_max_liquidity: bool,
}

/// What came back from the chain for one token.
struct TokenRead {
    balance: anyhow::Result<U256>,
    allowance: Option<anyhow::Result<U256>>,
    symbol: Option<String>,
}

/// `GET /api/bots/{name}/funding`
pub async fn funding(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    Ok(Json(read_funding(&state, &bot).await?).into_response())
}

/// One read of everything the bot's money sits in. Shared with the remove
/// route, which asks the same question before it deletes the key.
///
/// Two addresses when `[vault]` is set, not one. The token rows report the
/// OperatorVault — through its own inventory views, not `balanceOf`, see
/// [`read_inventory`] — because that is the balance the bot's quotes draw on;
/// the operator key only signs for it. Gas stays on the signer wallet, which
/// is what pays for the transactions. And the signer wallet's own token
/// balances are read as well, into `walletTokens`: removing a bot deletes that
/// key, and what the key can reach is what the remove gate has to weigh.
///
/// Which is why the read failures are kept apart by the address they came
/// from. A vault view that timed out must not refuse a delete: the vault
/// outlives the key, so the gate only cares whether the wallet's own balances
/// were read.
pub async fn read_funding(state: &AppState, bot: &Bot) -> Result<FundingBody, ApiError> {
    let path = config_path(bot)?;
    let toml = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::internal(&anyhow::anyhow!(e).context(format!("reading {}", path.display())))
    })?;
    let cfg = Config::from_toml(&toml).map_err(ApiError::bad_request)?;

    let operator_address = bot.config.as_ref().and_then(|c| c.operator_address.clone());
    let vault_address = bot.config.as_ref().and_then(|c| c.vault_address.clone());
    let plans = plan_tokens(&cfg, vault_address.is_some())?;

    let parse = |a: &Option<String>| a.as_deref().and_then(|a| a.parse::<Address>().ok());
    let wallet = parse(&operator_address);
    let vault = parse(&vault_address);
    // Where the money the bot quotes against sits.
    let capital = vault.or(wallet);
    // The second read, and only when there is a second address to read.
    let dust_owner = vault.and(wallet);
    let permit2 = cfg.permit2.parse::<Address>();

    // Kept apart by address: `capital_errors` are the vault's (or, with no
    // vault, the wallet's own, which is the same address), `wallet_errors` are
    // the signing key's. Only the second kind can block a removal.
    let mut capital_errors: Vec<String> = Vec::new();
    let mut wallet_errors: Vec<String> = Vec::new();
    if capital.is_none() {
        capital_errors.push(
            "this bot has no operator address the panel can read, so balances can't be checked."
                .to_string(),
        );
    } else if vault.is_none() {
        // Only where an allowance is actually read: a vault maker asks the
        // chain for none, so a bad permit2 address costs it nothing.
        if let Err(e) = &permit2 {
            capital_errors.push(format!("the config's permit2 address is not valid: {e}"));
        }
    }
    // A vault holds its own Permit2 approvals — granted in its constructor —
    // and the operator key only signs, so there is no allowance to read.
    let permit2_owner = match vault {
        Some(_) => None,
        None => permit2.as_ref().ok().copied(),
    };

    // Chain reads and price lookups side by side: neither waits for the other,
    // and each is bounded on its own, so a dead node and a dead feed together
    // still cost about six seconds, not their sum.
    let api_origin = http_origin(&cfg.indexer_url);
    let executor_routed = cfg
        .vault
        .as_ref()
        .and_then(|v| v.order_executor.as_deref())
        .is_some();
    let (reads, inventory, dust, native_balance, feed_prices, native) = tokio::join!(
        read_tokens(&cfg.rpc_url, capital, permit2_owner, &plans),
        read_inventory(&cfg.rpc_url, vault, executor_routed),
        read_balances(&cfg.rpc_url, dust_owner, &plans),
        read_native(&cfg.rpc_url, wallet),
        fetch_feed_prices(&plans),
        tokio::time::timeout(
            PRICE_BUDGET,
            state.native_prices.get(cfg.chain_id, api_origin.as_deref()),
        ),
    );
    let native = native.ok().flatten();
    if let Some(Err(e)) = &inventory {
        capital_errors.push(format!("{e:#}"));
    }
    let quotable = inventory.as_ref().and_then(|r| r.as_ref().ok());

    // One price per plan, shared by both sets of rows: the same token at two
    // addresses is worth the same, and a screen showing both must not imply
    // otherwise.
    let prices: Vec<PlanPrice> = plans.iter().map(|p| price_of(p, &feed_prices)).collect();

    let tokens: Vec<FundingTokenBody> = plans
        .iter()
        .enumerate()
        .map(|(i, plan)| {
            let read = reads.get(i);
            // The vault's own figure when it named this token. A token the
            // vault does not name is not one it can quote at all, so its raw
            // balance there is all there is to say about it; and when the
            // views themselves failed, nothing is.
            let balance = match (quotable.and_then(|q| q.get(&plan.key).copied()), &inventory) {
                (Some(q), _) => Some(q),
                (None, Some(Err(_))) => None,
                (None, _) => value_of(read.map(|r| &r.balance), &mut capital_errors),
            };
            row_of(
                plan,
                read.and_then(|r| r.symbol.clone()),
                balance,
                value_of(read.and_then(|r| r.allowance.as_ref()), &mut capital_errors),
                &prices[i],
            )
        })
        .collect();

    let wallet_tokens = vault.map(|_| {
        plans
            .iter()
            .enumerate()
            .map(|(i, plan)| {
                row_of(
                    plan,
                    tokens.get(i).map(|t| t.symbol.clone()),
                    value_of(dust.get(i), &mut wallet_errors),
                    None,
                    &prices[i],
                )
            })
            .collect::<Vec<_>>()
    });

    let gas_balance = value_of(native_balance.as_ref(), &mut wallet_errors);
    let gas_usd = match (gas_balance, native) {
        (Some(b), Some(p)) => Some(units(b, 18) * p.usd),
        _ => None,
    };
    // How much gas to ask for depends on how many approvals are still to send,
    // so this is read off the token rows, not off a constant.
    let min_gas = min_gas_usd(cfg.chain_id, approvals_missing(&tokens).len());
    let gas = FundingGasBody {
        symbol: gas_symbol(cfg.chain_id),
        balance: gas_balance.map(|b| b.to_string()),
        balance_text: gas_balance.map(|b| format_units(b, 18)),
        price: native.map(|p| p.usd),
        price_source: native.map(|p| p.source),
        usd: gas_usd,
        ok: gas_ok(
            gas_balance,
            gas_usd,
            native.is_some() || gas_token(cfg.chain_id).is_some(),
            min_gas,
        ),
    };

    // Read off the plans, not off the rows: a row can also be unpriced because
    // a feed is down, which does clear by waiting. This one does not.
    let unpriceable = !plans.is_empty() && plans.iter().all(|p| !p.priced_in_usd);
    let gate = gate_of(&tokens, &gas, min_gas, unpriceable);
    let network_label = cfg
        .pools
        .iter()
        .find_map(|pool| setup::pool_identity(cfg.chain_id, pool))
        .map(|c| c.network_label);
    let explorer = |address: &Option<String>| {
        address
            .as_deref()
            .and_then(|a| setup::address_explorer_url(cfg.chain_id, a))
    };
    let capital_address = vault_address.clone().or_else(|| operator_address.clone());

    // Every read is in; the first failure speaks for the request.
    let read_error = capital_errors
        .iter()
        .chain(wallet_errors.iter())
        .next()
        .cloned();
    // What the key can reach. The vault outlives the key — its money is not
    // lost with the bot — so with a vault only the signer wallet's own
    // balances, and only the failures reading them, stand in the way of
    // removing it. With no vault the capital is that wallet, so both count.
    let own_tokens = wallet_tokens.as_deref().unwrap_or(&tokens);
    let key_error = match vault {
        Some(_) => wallet_errors.first(),
        None => capital_errors.first().or_else(|| wallet_errors.first()),
    };
    let remove_blocked_by = remove_refusal(
        operator_address.as_deref(),
        own_tokens,
        &gas,
        key_error.map(String::as_str),
    );
    Ok(FundingBody {
        capital_explorer_url: explorer(&capital_address),
        capital_source: if vault.is_some() { "vault" } else { "wallet" },
        capital_address,
        explorer_url: explorer(&operator_address),
        operator_address,
        chain_id: cfg.chain_id,
        network_label,
        permit2: cfg.permit2.clone(),
        tokens,
        wallet_tokens,
        gas,
        gate,
        read_error,
        remove_blocked_by,
        checked_at_unix: now_unix(),
    })
}

/// A read's value, keeping its failure for `readError` instead of dropping it.
fn value_of<T: Copy>(read: Option<&anyhow::Result<T>>, errors: &mut Vec<String>) -> Option<T> {
    match read {
        Some(Ok(v)) => Some(*v),
        Some(Err(e)) => {
            errors.push(format!("{e:#}"));
            None
        }
        None => None,
    }
}

/// What a token is worth in dollars, and why it isn't when it isn't.
struct PlanPrice {
    price: Option<f64>,
    source: Option<&'static str>,
    error: Option<String>,
}

/// Price one plan off the feeds already fetched.
///
/// A pool that doesn't quote against a dollar can't be valued in dollars on
/// either side: the feed reads debt per collateral, so it is only a dollar
/// price when the debt token is a dollar.
fn price_of(plan: &TokenPlan, feed_prices: &HashMap<String, Result<f64, String>>) -> PlanPrice {
    let unpriced = |why: String| PlanPrice {
        price: None,
        source: None,
        error: Some(why),
    };
    if !plan.priced_in_usd {
        return unpriced(NOT_A_DOLLAR_PAIR.to_string());
    }
    if plan.role == "stable" {
        return PlanPrice {
            price: Some(1.0),
            source: Some("fixed"),
            error: None,
        };
    }
    match plan
        .feed_url
        .as_deref()
        .and_then(|url| feed_prices.get(url))
    {
        Some(Ok(p)) => PlanPrice {
            price: Some(*p),
            source: Some("feed"),
            error: None,
        },
        Some(Err(e)) => unpriced(e.clone()),
        None => unpriced("this pool has no price feed".to_string()),
    }
}

/// One token row: what this address holds of it, and what that is worth.
fn row_of(
    plan: &TokenPlan,
    symbol: Option<String>,
    balance: Option<U256>,
    allowance: Option<U256>,
    price: &PlanPrice,
) -> FundingTokenBody {
    let usd = match (balance, price.price) {
        (Some(b), Some(p)) => Some(units(b, plan.decimals) * p),
        _ => None,
    };
    FundingTokenBody {
        role: plan.role,
        symbol: plan
            .ticker
            .clone()
            .or(symbol)
            .unwrap_or_else(|| short_token(&plan.key)),
        token: plan.key.clone(),
        decimals: plan.decimals,
        balance: balance.map(|b| b.to_string()),
        balance_text: balance.map(|b| format_units(b, plan.decimals)),
        price: price.price,
        price_source: price.source,
        price_error: price.error.clone(),
        unpriceable: !plan.priced_in_usd,
        usd,
        funded: usd.map(|u| u >= FUND_MIN_TOKEN_USD),
        approval_needed: plan.approval_needed,
        permit2_allowance: allowance.map(|a| a.to_string()),
        approved: if plan.approval_needed {
            allowance.map(|a| {
                approval_action(a, plan.required, plan.uses_max_liquidity, ApprovalMode::Max)
                    == ApprovalAction::AlreadyApproved
            })
        } else {
            None
        },
    }
}

/// Every distinct token across the pools, stable side first per pool, with
/// what the approval preflight commits to it.
///
/// A vault maker commits nothing: the vault granted Permit2 on both legs in
/// its constructor and the operator key only signs, so every row comes back
/// with `approvalNeeded` false and the panel asks the chain for no allowances.
fn plan_tokens(cfg: &Config, vaulted: bool) -> Result<Vec<TokenPlan>, ApiError> {
    let required = match vaulted {
        true => Vec::new(),
        false => required_approvals(cfg).map_err(|e| ApiError::bad_request(format!("{e:#}")))?,
    };
    let tickers = token_symbols(cfg);
    let mut plans: Vec<TokenPlan> = Vec::new();
    for pool in &cfg.pools {
        let feed_url = pool
            .feed_url
            .clone()
            .unwrap_or_else(|| cfg.feed.url.clone());
        // Both rows of a pool are quoted in its debt token, so one question
        // settles both: is that token a dollar?
        let debt = parse_token(&pool.debt, "stable")?;
        let priced_in_usd = is_dollar_token(cfg.chain_id, &format!("{debt:#x}"));
        for (raw, role, decimals, feed) in [
            (&pool.debt, "stable", pool.debt_decimals, None),
            (
                &pool.collateral,
                "soft",
                pool.collateral_decimals,
                Some(feed_url.clone()),
            ),
        ] {
            let address = parse_token(raw, role)?;
            let key = format!("{address:#x}");
            if plans.iter().any(|p| p.key == key) {
                continue;
            }
            let req = required.iter().find(|r| r.token == address);
            plans.push(TokenPlan {
                address,
                ticker: tickers.get(&key).cloned(),
                key,
                role,
                decimals,
                feed_url: feed,
                priced_in_usd,
                approval_needed: req.is_some(),
                required: req.map(|r| r.required).unwrap_or(U256::ZERO),
                uses_max_liquidity: req.map(|r| r.uses_max_liquidity).unwrap_or(false),
            });
        }
    }
    Ok(plans)
}

/// One of a pool's token addresses, or a 400 naming the side that is wrong.
fn parse_token(raw: &str, role: &str) -> Result<Address, ApiError> {
    raw.trim().parse::<Address>().map_err(|e| {
        ApiError::bad_request(format!(
            "the pool's {role} token address {raw:?} is not valid: {e}"
        ))
    })
}

/// Every token read at once, each under its own budget.
///
/// An empty vec when there is no owner to read for. Every failure, the budget
/// included, stays inside its own read: one slow `eth_call` degrades that row
/// to "unknown" and the rows that answered still show their balances. The
/// public nodes these configs point at rate-limit one call at a time, so a
/// batch-wide timeout used to blank a fully funded wallet.
async fn read_tokens(
    rpc_url: &str,
    owner: Option<Address>,
    permit2: Option<Address>,
    plans: &[TokenPlan],
) -> Vec<TokenRead> {
    let Some(owner) = owner else {
        return Vec::new();
    };
    let rpc = Rpc::new(rpc_url.to_string());
    futures_util::future::join_all(plans.iter().map(|plan| {
        let rpc = rpc.clone();
        async move {
            let balance = budgeted(rpc_url, read_balance(&rpc, plan.address, owner));
            let allowance = async {
                match permit2 {
                    Some(permit2) => Some(
                        budgeted(rpc_url, read_allowance(&rpc, plan.address, owner, permit2)).await,
                    ),
                    None => None,
                }
            };
            let symbol = async {
                if plan.ticker.is_some() {
                    return None;
                }
                budgeted(rpc_url, read_symbol(&rpc, plan.address))
                    .await
                    .ok()
            };
            let (balance, allowance, symbol) = tokio::join!(balance, allowance, symbol);
            TokenRead {
                balance,
                allowance,
                symbol,
            }
        }
    }))
    .await
}

/// The same tokens at a second address: balances only. What the signer wallet
/// still holds of its own when the trading capital sits in a vault — there are
/// no allowances to ask about there, and the symbols are already known.
async fn read_balances(
    rpc_url: &str,
    owner: Option<Address>,
    plans: &[TokenPlan],
) -> Vec<anyhow::Result<U256>> {
    let Some(owner) = owner else {
        return Vec::new();
    };
    let rpc = Rpc::new(rpc_url.to_string());
    futures_util::future::join_all(
        plans
            .iter()
            .map(|plan| budgeted(rpc_url, read_balance(&rpc, plan.address, owner))),
    )
    .await
}

/// What the vault will actually quote, by lowercase token address.
///
/// Not `balanceOf`. `allocateIdle` parks settlement above the liquid floor in
/// the yield adapter, so a working vault's raw balance reads low — one holding
/// 2 USDT with 1.99 in Aave shows 0.01 — and it reads high the other way,
/// counting deposits queued for the next epoch and redemptions already
/// reserved, neither of which the vault may trade. These are the same views
/// the bot sizes its quotes from (`read_vault_inventory` in `src/rfq`), so the
/// panel's answer to "what does this corridor have to work with" is the bot's.
///
/// Whether the yield position counts depends on the route: with a
/// `[vault].order_executor` listed, a fill recalls from the adapter before the
/// Permit2 pull, so the whole position is quotable; without one only the idle
/// part is. Same rule as the bot's, from the same function.
///
/// `None` when there is no vault. An error when the views don't answer — a
/// quiet fall back to `balanceOf` would be the wrong number with nothing
/// saying so.
async fn read_inventory(
    rpc_url: &str,
    vault: Option<Address>,
    executor_routed: bool,
) -> Option<anyhow::Result<HashMap<String, U256>>> {
    let vault = vault?;
    let rpc = Rpc::new(rpc_url.to_string());
    let word = |data: Vec<u8>, what: &'static str| {
        let rpc = rpc.clone();
        async move { budgeted(rpc_url, read_word(&rpc, vault, data, what)).await }
    };
    let (settlement, corridor, quotable, liquid, corridor_qty) = tokio::join!(
        word(encode_settlement_asset(), "settlementAsset()"),
        word(encode_corridor_asset(), "corridorAsset()"),
        word(encode_quotable_settlement(), "quotableSettlement()"),
        word(encode_liquid_settlement(), "liquidSettlement()"),
        word(encode_quotable_corridor(), "quotableCorridor()"),
    );
    Some(vault_inventory(
        vault,
        [settlement, corridor, quotable, liquid, corridor_qty],
        executor_routed,
    ))
}

/// The map from the five reads, or the first of them that failed.
fn vault_inventory(
    vault: Address,
    reads: [anyhow::Result<U256>; 5],
    executor_routed: bool,
) -> anyhow::Result<HashMap<String, U256>> {
    let [settlement, corridor, quotable, liquid, corridor_qty] = reads;
    let settlement = address_from_word(settlement?);
    let corridor = address_from_word(corridor?);
    anyhow::ensure!(
        !settlement.is_zero() && !corridor.is_zero(),
        "the vault at {vault} names no settlement or corridor asset — is that address an \
         OperatorVault on this chain?"
    );
    Ok(HashMap::from([
        (
            format!("{settlement:#x}"),
            quotable_settlement_for_route(quotable?, liquid?, executor_routed),
        ),
        (format!("{corridor:#x}"), corridor_qty?),
    ]))
}

/// One no-argument view returning a single word. `what` names the call, so a
/// failure says which view rather than just which address.
async fn read_word(rpc: &Rpc, to: Address, data: Vec<u8>, what: &str) -> anyhow::Result<U256> {
    let out = rpc.eth_call(to, &Bytes::from(data)).await?;
    anyhow::ensure!(
        out.len() >= 32,
        "{what} on {to} returned {} bytes, not a word",
        out.len()
    );
    Ok(U256::from_be_slice(&out[out.len() - 32..]))
}

/// The gas coin, on the wallet that pays for the transactions. `None` when
/// there is no signer address to read.
async fn read_native(rpc_url: &str, owner: Option<Address>) -> Option<anyhow::Result<U256>> {
    let owner = owner?;
    let rpc = Rpc::new(rpc_url.to_string());
    Some(budgeted(rpc_url, rpc.get_balance(owner)).await)
}

/// One chain read under the screen's budget. A read that runs out of time
/// fails like any other read, with a message that says which node it was.
async fn budgeted<T>(
    rpc_url: &str,
    read: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(CHAIN_BUDGET, read).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "the RPC at {rpc_url} didn't answer within {} seconds",
            CHAIN_BUDGET.as_secs()
        )),
    }
}

async fn read_balance(rpc: &Rpc, token: Address, owner: Address) -> anyhow::Result<U256> {
    let data = Bytes::from(encode_balance_of(owner));
    let out = rpc.eth_call(token, &data).await?;
    anyhow::ensure!(
        out.len() >= 32,
        "balanceOf() on {token} returned {} bytes, not a uint256 — is that address an ERC-20 on \
         this chain?",
        out.len()
    );
    Ok(U256::from_be_slice(&out[out.len() - 32..]))
}

/// ERC-20 `symbol()`, for a token the corridor catalog can't name.
async fn read_symbol(rpc: &Rpc, token: Address) -> anyhow::Result<String> {
    let data = Bytes::from(keccak256(b"symbol()")[..4].to_vec());
    let out = rpc.eth_call(token, &data).await?;
    decode_string_return(&out)
        .ok_or_else(|| anyhow::anyhow!("symbol() on {token} returned no string"))
}

/// Decode an ABI `string` return, accepting the `bytes32` form some older
/// tokens use. `None` for anything else, including an empty string.
fn decode_string_return(out: &[u8]) -> Option<String> {
    let text = if out.len() >= 64 {
        let offset = U256::from_be_slice(&out[..32]).try_into().ok()?;
        let len: usize = U256::from_be_slice(out.get(offset..offset + 32)?)
            .try_into()
            .ok()?;
        std::str::from_utf8(out.get(offset + 32..offset + 32 + len)?).ok()?
    } else if out.len() == 32 {
        let end = out.iter().position(|b| *b == 0).unwrap_or(32);
        std::str::from_utf8(&out[..end]).ok()?
    } else {
        return None;
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// One fetch per distinct feed URL, each under its own budget.
async fn fetch_feed_prices(plans: &[TokenPlan]) -> HashMap<String, Result<f64, String>> {
    let mut urls: Vec<&str> = Vec::new();
    for plan in plans {
        // A pool that doesn't quote against a dollar is unpriced whatever its
        // feed says, so there is nothing to fetch for it.
        if !plan.priced_in_usd {
            continue;
        }
        if let Some(url) = plan.feed_url.as_deref() {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    let fetched = futures_util::future::join_all(urls.iter().map(|url| async move {
        let result = match tokio::time::timeout(PRICE_BUDGET, HttpFeed::new(*url).fetch()).await {
            Ok(Ok(quote)) if is_price_usable(quote.price) => Ok(quote.price),
            Ok(Ok(quote)) => Err(format!(
                "the feed returned an unusable price ({})",
                quote.price
            )),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err(format!(
                "the feed at {url} didn't answer within {} seconds",
                PRICE_BUDGET.as_secs()
            )),
        };
        (url.to_string(), result)
    }))
    .await;
    fetched.into_iter().collect()
}

/// Whether the gas balance clears the gate.
///
/// With a price: the dollar test, against the figure this chain and this many
/// pending approvals call for. Without one (a chain nobody prices): any
/// non-zero balance, because there is no threshold to hold it to.
fn gas_ok(balance: Option<U256>, usd: Option<f64>, priced: bool, min_usd: f64) -> Option<bool> {
    let balance = balance?;
    if priced {
        return usd.map(|u| u >= min_usd);
    }
    Some(balance > U256::ZERO)
}

/// The dollars of gas the wallet needs before the wizard starts spending it.
///
/// One transaction's worth per approval still to send, and never less than one
/// transaction's worth: after the approvals the bot still has to start. A flat
/// figure across chains does not work. Two ERC-20 approves on Ethereum cost
/// several dollars, so a flat $1 would pass the gate and then die on the second
/// approve, with no way left in the wizard to end at either "live" or
/// "waiting"; on Celo the same $1 asks for a hundred times what is needed.
fn min_gas_usd(chain_id: u64, approvals_pending: usize) -> f64 {
    let per_tx = gas_token(chain_id)
        .map(|g: GasToken| g.tx_gas_usd)
        .unwrap_or(FUND_MIN_GAS_USD);
    per_tx * approvals_pending.max(1) as f64
}

/// Symbols the bot still needs a Permit2 approval for.
///
/// Deliberately not filtered by balance. The bot's live-start preflight bails
/// while any enabled side is unapproved, so a bot approved only on the funded
/// side could never start. See the module docs.
fn approvals_missing(tokens: &[FundingTokenBody]) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| t.approval_needed && t.approved != Some(true))
        .map(|t| t.symbol.clone())
        .collect()
}

/// The start decision, from the rows.
///
/// `unpriceable` says the rows could not be valued for a reason no wallet can
/// fix: see [`FundingGateBody::unpriceable`]. It does not change the decision
/// (an unvalued row is not funded either way), only what the screen may say
/// about it.
fn gate_of(
    tokens: &[FundingTokenBody],
    gas: &FundingGasBody,
    min_gas_usd: f64,
    unpriceable: bool,
) -> FundingGateBody {
    let funded_tokens: Vec<String> = tokens
        .iter()
        .filter(|t| t.funded == Some(true))
        .map(|t| t.symbol.clone())
        .collect();
    let needs_side = funded_tokens.is_empty();
    let needs_gas = gas.ok != Some(true);
    FundingGateBody {
        // Gas alone. `needs_side` is reported for Live, which waits for the
        // money, but an empty wallet with gas in it is ready to be approved.
        passes: !needs_gas,
        min_token_usd: FUND_MIN_TOKEN_USD,
        min_gas_usd,
        funded_tokens,
        approvals_missing: approvals_missing(tokens),
        needs_side,
        needs_gas,
        unpriceable,
    }
}

/// Whole tokens as a float, for dollar math. Precise enough for a threshold.
pub(super) fn units(atomic: U256, decimals: u8) -> f64 {
    let raw: f64 = atomic.to_string().parse().unwrap_or(f64::INFINITY);
    raw / 10f64.powi(i32::from(decimals))
}

/// Whole tokens as text, exactly, with trailing zeros trimmed: `"25"`, `"0.5"`.
pub(super) fn format_units(atomic: U256, decimals: u8) -> String {
    if decimals == 0 {
        return atomic.to_string();
    }
    let scale = U256::from(10u8).pow(U256::from(decimals));
    let whole = atomic / scale;
    let frac = atomic % scale;
    if frac.is_zero() {
        return whole.to_string();
    }
    let frac = format!(
        "{:0>width$}",
        frac.to_string(),
        width = usize::from(decimals)
    );
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::ContainerState;
    use crate::panel::http::mock_chain::{mock_rpc, MockChain};
    use crate::panel::http::testkit::{harness, Harness, TEST_KEY};
    use crate::setup;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    const USDT: &str = "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e";
    const CNGN: &str = "0xF6829D7393dAe24509eb1E52eE8e572e2E271a4f";

    /// Textile's API as the funding check sees it: `/native-price` (counted)
    /// and the `/price` feed, each with a fixed status.
    struct MockApi {
        base: String,
        native_hits: Arc<AtomicUsize>,
        _server: tokio::task::JoinHandle<()>,
    }

    async fn mock_api(native_status: u16, feed_status: u16, feed_price: f64) -> MockApi {
        let native_hits = Arc::new(AtomicUsize::new(0));
        let counter = native_hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/native-price",
                get(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::from_u16(native_status).unwrap(),
                            Json(json!({ "chainId": 42220, "symbol": "CELOUSDT", "priceUsd": 0.08, "timestamp": 1 })),
                        )
                    }
                }),
            )
            .route(
                "/price",
                get(move || async move {
                    (
                        StatusCode::from_u16(feed_status).unwrap(),
                        Json(json!({ "price": feed_price, "timestamp": 1 })),
                    )
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        MockApi {
            base: format!("http://{addr}"),
            native_hits,
            _server: server,
        }
    }

    /// A cNGN/USDT Celo bot whose chain, API and feed are all local mocks.
    fn seed(h: &Harness, name: &str, rpc_url: &str, api_base: &str, feed_url: &str) {
        let corridor = setup::find_corridor("cngn-usdt-celo").unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace("https://forno.celo.org", rpc_url)
            .replace(
                "https://api.textilecredit.com/price?chainId=42220&pair=cngn-usdt",
                feed_url,
            )
            .replace("https://api.textilecredit.com", api_base);
        std::fs::write(&path, toml).unwrap();
        add_container(h, name);
    }

    /// A vault address the seeded bot's key is not: the OperatorVault holds
    /// the trading capital, the key only signs for it.
    const VAULT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    const WALLET: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    /// The chain's VaultOrderExecutor. Listed on a bot's `[vault]`, it is what
    /// makes the yield-adapter position quotable.
    const EXECUTOR: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

    /// The same bot, with `[vault]` set. A top-level table header appended to
    /// the config can't land inside another table, so this is a safe suffix.
    fn attach_vault(h: &Harness, name: &str, vault: &str, executor: Option<&str>) {
        let path = h.root.join(name).join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap();
        let executor = match executor {
            Some(e) => format!("order_executor = \"{e}\"\n"),
            None => String::new(),
        };
        std::fs::write(
            &path,
            format!("{toml}\n[vault]\naddress = \"{vault}\"\n{executor}"),
        )
        .unwrap();
    }

    /// A vault that names this corridor's pair and answers its inventory
    /// views. `quotable` is the whole settlement position, `liquid` only the
    /// idle part; the gap is what `allocateIdle` put in the yield adapter.
    fn vault_views(chain: MockChain, quotable: u128, liquid: u128, corridor: u128) -> MockChain {
        chain
            .view_address(VAULT, "settlementAsset()", USDT)
            .view_address(VAULT, "corridorAsset()", CNGN)
            .view(VAULT, "quotableSettlement()", U256::from(quotable))
            .view(VAULT, "liquidSettlement()", U256::from(liquid))
            .view(VAULT, "quotableCorridor()", U256::from(corridor))
    }

    fn add_container(h: &Harness, name: &str) {
        let mut c = crate::panel::docker::fake::container(
            &format!("stitch-{name}"),
            ContainerState::Exited,
        );
        c.image = h.state.cfg.bot_image.clone();
        c.labels.insert(
            crate::panel::naming::LABEL_BOT.to_string(),
            name.to_string(),
        );
        c.mounts =
            crate::panel::docker::fake::dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    fn feed_of(api: &MockApi) -> String {
        format!("{}/price?chainId=42220&pair=cngn-usdt", api.base)
    }

    fn token<'a>(v: &'a serde_json::Value, symbol: &str) -> &'a serde_json::Value {
        v["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["symbol"] == symbol)
            .unwrap_or_else(|| panic!("no {symbol} row: {v}"))
    }

    fn row(symbol: &str, balance: Option<&str>, usd: Option<f64>) -> FundingTokenBody {
        FundingTokenBody {
            role: "stable",
            symbol: symbol.into(),
            token: "0x00".into(),
            decimals: 6,
            balance: balance.map(str::to_string),
            balance_text: None,
            price: None,
            price_source: None,
            price_error: None,
            unpriceable: false,
            usd,
            funded: None,
            approval_needed: false,
            permit2_allowance: None,
            approved: None,
        }
    }

    fn gas(usd: Option<f64>) -> FundingGasBody {
        FundingGasBody {
            symbol: "BNB".into(),
            balance: Some("1".into()),
            balance_text: None,
            price: None,
            price_source: None,
            usd,
            ok: None,
        }
    }

    #[test]
    fn remove_is_refused_for_money_unpriced_holdings_and_unreadable_wallets() {
        let owner = Some("0xabc");
        // Dust and leftover gas are cleanup.
        assert_eq!(
            remove_refusal(
                owner,
                &[row("USDT", Some("1"), Some(0.5))],
                &gas(Some(2.0)),
                None
            ),
            None
        );
        // Over the floor is losing money.
        let why = remove_refusal(
            owner,
            &[row("USDT", Some("1"), Some(9.0))],
            &gas(Some(2.0)),
            None,
        )
        .unwrap();
        assert!(
            why.contains("$11.00") && why.contains("Withdraw first"),
            "{why}"
        );
        // A balance nobody could price could be worth anything.
        let why = remove_refusal(
            owner,
            &[row("cNGN", Some("5"), None)],
            &gas(Some(0.0)),
            None,
        )
        .unwrap();
        assert!(
            why.contains("cNGN") && why.contains("cannot price"),
            "{why}"
        );
        // A balance that could not be read is the read error's business, not
        // an unpriced holding.
        let why = remove_refusal(
            owner,
            &[row("cNGN", None, None)],
            &gas(None),
            Some("node down."),
        )
        .unwrap();
        assert!(
            why.contains("cannot read this wallet") && why.contains("node down."),
            "{why}"
        );
        // Gas the panel can't price is a holding too: a custom chain's coin
        // with no dollar price could be worth anything.
        let why = remove_refusal(owner, &[row("cNGN", Some("0"), None)], &gas(None), None).unwrap();
        assert!(why.contains("BNB") && why.contains("cannot price"), "{why}");
        // Nothing held, nothing priced, no read error: nothing to weigh.
        let empty = FundingGasBody {
            balance: Some("0".into()),
            ..gas(None)
        };
        assert_eq!(
            remove_refusal(owner, &[row("cNGN", Some("0"), None)], &empty, None),
            None
        );
        // No key, nothing to lose, whatever else is true.
        assert_eq!(
            remove_refusal(
                None,
                &[row("USDT", Some("1"), Some(900.0))],
                &gas(None),
                Some("x")
            ),
            None
        );
    }

    #[tokio::test]
    async fn funding_reports_balances_gate_and_approvals() {
        let h = harness("funding-ok");
        let node = mock_rpc(
            MockChain::default()
                .balance(USDT, 25_000_000)
                .balance(CNGN, 0)
                .native(20_000_000_000_000_000_000),
        )
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["readError"].is_null(), "{body}");
        // The inventory reports the wallet in lowercase hex; the wizard shows
        // it as-is and the explorer accepts either case.
        let operator = v["operatorAddress"].as_str().unwrap().to_string();
        assert_eq!(
            operator.to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
        assert_eq!(v["chainId"], 42220);
        assert_eq!(v["networkLabel"], "Celo");
        assert_eq!(
            v["explorerUrl"],
            format!("https://celoscan.io/address/{operator}")
        );

        let tokens = v["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 2, "{body}");
        assert_eq!(tokens[0]["role"], "stable");
        assert_eq!(tokens[0]["symbol"], "USDT");
        assert_eq!(tokens[0]["balance"], "25000000");
        assert_eq!(tokens[0]["balanceText"], "25");
        assert_eq!(tokens[0]["price"], 1.0);
        assert_eq!(tokens[0]["priceSource"], "fixed");
        assert_eq!(tokens[0]["usd"], 25.0);
        assert_eq!(tokens[0]["funded"], true);
        assert_eq!(tokens[0]["approvalNeeded"], true);
        assert_eq!(tokens[0]["permit2Allowance"], "0");
        assert_eq!(tokens[0]["approved"], false);
        assert_eq!(tokens[1]["role"], "soft");
        assert_eq!(tokens[1]["symbol"], "cNGN");
        assert_eq!(tokens[1]["priceSource"], "feed");
        assert_eq!(tokens[1]["price"], 0.00073);
        assert_eq!(tokens[1]["usd"], 0.0);
        assert_eq!(tokens[1]["funded"], false);

        assert_eq!(v["gas"]["symbol"], "CELO");
        assert_eq!(v["gas"]["balanceText"], "20");
        assert_eq!(v["gas"]["priceSource"], "textile");
        let gas_usd = v["gas"]["usd"].as_f64().unwrap();
        assert!((gas_usd - 1.6).abs() < 1e-9, "{body}");
        assert_eq!(v["gas"]["ok"], true);

        assert_eq!(v["gate"]["passes"], true, "{body}");
        assert_eq!(v["gate"]["minTokenUsd"], 20.0);
        assert_eq!(v["gate"]["minGasUsd"], 1.0);
        assert_eq!(v["gate"]["fundedTokens"], json!(["USDT"]));
        assert_eq!(v["gate"]["approvalsMissing"], json!(["USDT", "cNGN"]));
        assert_eq!(v["gate"]["needsSide"], false);
        assert_eq!(v["gate"]["needsGas"], false);
        assert!(v["checkedAtUnix"].as_u64().unwrap() > 1_700_000_000);
    }

    #[tokio::test]
    async fn a_vault_maker_reports_the_vault_as_its_assets() {
        // The money the bot quotes against is the vault's; the signer wallet
        // holds a dollar of dust and the gas.
        let h = harness("funding-vault");
        let node = mock_rpc(vault_views(
            MockChain::default()
                .balance_of(USDT, VAULT, 25_000_000)
                .balance_of(USDT, WALLET, 1_000_000)
                .balance(CNGN, 0)
                .native(20_000_000_000_000_000_000),
            25_000_000,
            25_000_000,
            0,
        ))
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));
        attach_vault(&h, "bot-a", VAULT, None);

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["readError"].is_null(), "{body}");

        // The rows are the vault's, and they say so.
        assert_eq!(v["capitalSource"], "vault");
        // Normalised by the inventory, whatever casing the operator typed.
        let vault = v["capitalAddress"].as_str().unwrap().to_string();
        assert_eq!(vault, VAULT.to_lowercase());
        assert_eq!(
            v["capitalExplorerUrl"],
            format!("https://celoscan.io/address/{vault}")
        );
        assert_eq!(token(&v, "USDT")["balanceText"], "25", "{body}");
        assert_eq!(token(&v, "USDT")["usd"], 25.0);
        assert_eq!(v["gate"]["fundedTokens"], json!(["USDT"]));

        // The signer wallet's own money is reported apart from it, priced the
        // same way, with no approvals attached.
        let wallet = v["walletTokens"].as_array().unwrap();
        let dust = wallet.iter().find(|t| t["symbol"] == "USDT").unwrap();
        assert_eq!(dust["balanceText"], "1", "{body}");
        assert_eq!(dust["usd"], 1.0);
        assert!(dust["permit2Allowance"].is_null(), "{body}");

        // A vault approved Permit2 in its constructor and the key signs only:
        // nothing here is waiting on an approval.
        assert_eq!(token(&v, "USDT")["approvalNeeded"], false);
        assert_eq!(v["gate"]["approvalsMissing"], json!([]));
        assert_eq!(v["gate"]["passes"], true, "{body}");

        // Gas is still the signer wallet's — it pays for the transactions.
        assert_eq!(v["gas"]["balanceText"], "20");
        assert_eq!(v["gas"]["ok"], true);

        // And the vault's $25 is not the key's to lose: the vault outlives it.
        assert!(v["removeBlockedBy"].is_null(), "{body}");
    }

    #[tokio::test]
    async fn a_vault_makers_own_dust_still_blocks_removal() {
        let h = harness("funding-vault-dust");
        let node = mock_rpc(vault_views(
            MockChain::default()
                .balance_of(USDT, VAULT, 25_000_000)
                .balance_of(USDT, WALLET, 40_000_000)
                .balance(CNGN, 0)
                .native(20_000_000_000_000_000_000),
            25_000_000,
            25_000_000,
            0,
        ))
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));
        attach_vault(&h, "bot-a", VAULT, None);

        let (_, body) = h.get("/api/bots/bot-a/funding").await;
        let v = Harness::parse(&body);
        // $40 of USDT sat down on the signing key by mistake. Deleting the key
        // loses it, vault or no vault.
        let why = v["removeBlockedBy"].as_str().unwrap_or_default();
        assert!(why.contains("$41.60"), "{body}");
    }

    #[tokio::test]
    async fn a_vault_maker_reports_what_it_can_quote_not_what_it_holds() {
        // A working vault has most of its settlement in the yield adapter, so
        // `balanceOf` reads almost empty. With an executor listed the whole
        // position is fillable, and that is the number the bot quotes.
        let h = harness("funding-vault-quotable");
        let node = mock_rpc(vault_views(
            MockChain::default()
                .balance_of(USDT, VAULT, 1_000_000)
                .balance(CNGN, 0)
                .native(20_000_000_000_000_000_000),
            30_000_000,
            1_000_000,
            0,
        ))
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));
        attach_vault(&h, "bot-a", VAULT, Some(EXECUTOR));

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["readError"].is_null(), "{body}");
        assert_eq!(token(&v, "USDT")["balanceText"], "30", "{body}");
        assert_eq!(token(&v, "USDT")["usd"], 30.0);
        assert_eq!(v["gate"]["fundedTokens"], json!(["USDT"]));
    }

    #[tokio::test]
    async fn a_vault_read_that_fails_does_not_block_removing_the_key() {
        // No views on this address, so the inventory read fails. The signer
        // wallet answered, and it is the only thing removal can lose.
        let h = harness("funding-vault-unreadable");
        let node = mock_rpc(
            MockChain::default()
                .balance_of(USDT, WALLET, 1_000_000)
                .balance(CNGN, 0)
                .native(20_000_000_000_000_000_000),
        )
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));
        attach_vault(&h, "bot-a", VAULT, None);

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        // The vault's rows are unknown, and the page says why.
        assert!(token(&v, "USDT")["balance"].is_null(), "{body}");
        let why = v["readError"].as_str().unwrap_or_default();
        assert!(why.contains("settlementAsset()"), "{body}");
        // But the key's own balances were read, so Remove is not held up by a
        // vault nobody could reach.
        assert!(v["removeBlockedBy"].is_null(), "{body}");
    }

    #[tokio::test]
    async fn funding_gate_needs_gas() {
        let h = harness("funding-gas-short");
        let node = mock_rpc(
            MockChain::default()
                .balance(USDT, 25_000_000)
                .native(500_000_000_000_000_000),
        )
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["gas"]["ok"], false, "{body}");
        assert_eq!(v["gate"]["passes"], false);
        assert_eq!(v["gate"]["needsGas"], true);
        assert_eq!(v["gate"]["needsSide"], false, "the USDT side is fine");
    }

    #[tokio::test]
    async fn funding_values_soft_token_off_the_feed() {
        let h = harness("funding-soft-side");
        let node = mock_rpc(
            MockChain::default()
                .balance(USDT, 0)
                .balance(CNGN, 30_000_000_000)
                .native(20_000_000_000_000_000_000),
        )
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        let cngn = token(&v, "cNGN");
        assert_eq!(cngn["balanceText"], "30000");
        let usd = cngn["usd"].as_f64().unwrap();
        assert!((usd - 21.9).abs() < 1e-6, "{body}");
        assert_eq!(cngn["funded"], true);
        assert_eq!(v["gate"]["passes"], true, "{body}");
        assert_eq!(v["gate"]["fundedTokens"], json!(["cNGN"]));
    }

    #[tokio::test]
    async fn funding_soft_side_unpriced_when_feed_is_down() {
        let h = harness("funding-feed-down");
        let node = mock_rpc(
            MockChain::default()
                .balance(CNGN, 30_000_000_000)
                .native(20_000_000_000_000_000_000),
        )
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(
            &h,
            "bot-a",
            &node.url,
            &api.base,
            "http://127.0.0.1:1/price",
        );

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        let cngn = token(&v, "cNGN");
        assert!(cngn["price"].is_null(), "{body}");
        assert!(cngn["priceSource"].is_null(), "{body}");
        assert!(
            cngn["priceError"].as_str().is_some_and(|e| !e.is_empty()),
            "{body}"
        );
        assert!(cngn["funded"].is_null(), "unknown, not false: {body}");
        // The balance itself still reads fine.
        assert_eq!(cngn["balanceText"], "30000");
        // A dead feed cannot value the side, but the gate is gas-only now, so
        // the bot can still be approved and started; Live waits for the value.
        assert_eq!(v["gate"]["passes"], true, "{body}");
        assert_eq!(v["gate"]["needsSide"], true);
        assert!(v["readError"].is_null(), "a dead feed is not a chain error");
    }

    #[tokio::test]
    async fn funding_chain_unreachable_is_a_read_error_not_a_5xx() {
        let h = harness("funding-no-chain");
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", "http://127.0.0.1:1", &api.base, &feed_of(&api));

        let started = Instant::now();
        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["readError"].as_str().is_some(), "{body}");
        for t in v["tokens"].as_array().unwrap() {
            assert!(t["balance"].is_null(), "{body}");
            assert!(t["permit2Allowance"].is_null(), "{body}");
            assert!(t["approved"].is_null(), "{body}");
            assert!(t["funded"].is_null(), "{body}");
        }
        assert!(v["gas"]["balance"].is_null());
        assert!(v["gas"]["ok"].is_null());
        assert_eq!(v["gate"]["passes"], false);
        assert_eq!(v["gate"]["needsSide"], true);
        assert_eq!(v["gate"]["needsGas"], true);
        assert_eq!(v["gate"]["approvalsMissing"], json!(["USDT", "cNGN"]));
        // Prices are independent of the chain and still come through.
        assert_eq!(token(&v, "cNGN")["price"], 0.00073);
        assert_eq!(v["gas"]["priceSource"], "textile");
    }

    #[tokio::test]
    async fn funding_answers_within_budget_when_rpc_hangs() {
        let h = harness("funding-rpc-hang");
        let node = mock_rpc(
            MockChain::default()
                .balance(USDT, 25_000_000)
                .hung_for(Duration::from_secs(20)),
        )
        .await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        let started = Instant::now();
        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(
            v["readError"]
                .as_str()
                .is_some_and(|e| e.contains("didn't answer within 6 seconds")),
            "{body}"
        );
        assert!(token(&v, "USDT")["balance"].is_null());
    }

    #[tokio::test]
    async fn funding_gas_price_falls_back_when_both_lookups_fail() {
        let h = harness("funding-gas-fallback");
        let node = mock_rpc(
            MockChain::default()
                .balance(USDT, 25_000_000)
                .native(20_000_000_000_000_000_000),
        )
        .await;
        // Textile 500s and the harness has no CoinGecko origin.
        let api = mock_api(500, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["gas"]["priceSource"], "fallback", "{body}");
        assert_eq!(v["gas"]["price"], 0.05);
        let usd = v["gas"]["usd"].as_f64().unwrap();
        assert!((usd - 1.0).abs() < 1e-9, "{body}");
        assert_eq!(
            v["gas"]["ok"], true,
            "20 CELO at the low figure is exactly $1"
        );
    }

    #[tokio::test]
    async fn funding_gas_price_is_cached_for_60s() {
        let h = harness("funding-gas-cache");
        let node = mock_rpc(MockChain::default().native(1)).await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        for _ in 0..2 {
            let (status, body) = h.get("/api/bots/bot-a/funding").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v = Harness::parse(&body);
            assert_eq!(v["gas"]["priceSource"], "textile", "{body}");
        }
        assert_eq!(api.native_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn funding_no_operator_key_is_a_read_error() {
        let h = harness("funding-no-key");
        let node = mock_rpc(MockChain::default().native(1)).await;
        let api = mock_api(200, 200, 0.00073).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));
        std::fs::remove_file(h.root.join("bot-a").join("stitch.key")).unwrap();

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["operatorAddress"].is_null(), "{body}");
        assert_eq!(
            v["readError"],
            "this bot has no operator address the panel can read, so balances can't be checked."
        );
        assert!(v["explorerUrl"].is_null());
        assert_eq!(v["gate"]["passes"], false);
        assert_eq!(node.hits.load(Ordering::SeqCst), 0, "nothing to read for");
    }

    #[tokio::test]
    async fn funding_unknown_chain_counts_nonzero_gas() {
        let h = harness("funding-unknown-chain");
        let node = mock_rpc(
            MockChain::default()
                .native(1)
                .symbol(USDT, "USD₮")
                .symbol(CNGN, "cNGN"),
        )
        .await;
        // No API at all: nothing prices this chain's gas.
        seed(
            &h,
            "bot-a",
            &node.url,
            "http://127.0.0.1:1",
            "http://127.0.0.1:1/price",
        );
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace("chain_id        = 42220", "chain_id = 999");
        std::fs::write(&path, toml).unwrap();

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["gas"]["symbol"], "gas", "{body}");
        assert!(v["gas"]["price"].is_null());
        assert!(v["gas"]["priceSource"].is_null());
        assert_eq!(
            v["gas"]["ok"], true,
            "one wei counts when nothing can price it"
        );
        assert!(v["networkLabel"].is_null());
        assert!(v["explorerUrl"].is_null());
        // No corridor on chain 999, so the tickers come from the chain itself.
        assert_eq!(v["tokens"][0]["symbol"], "USD₮");
        assert_eq!(v["tokens"][1]["symbol"], "cNGN");

        let empty = mock_rpc(MockChain::default().native(0)).await;
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace(&node.url, &empty.url);
        std::fs::write(&path, toml).unwrap();
        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["gas"]["ok"], false, "{body}");
        assert_eq!(v["gate"]["needsGas"], true);
        // With no symbol() either, the row falls back to a shortened address.
        assert_eq!(v["tokens"][0]["symbol"], "0x4806…3d5e");
    }

    #[tokio::test]
    async fn funding_requires_editable_config() {
        let h = harness("funding-foreign");
        let mut c =
            crate::panel::docker::fake::container("stitch-adopted", ContainerState::Running);
        c.mounts = crate::panel::docker::fake::dir_layout_mounts("/srv/elsewhere/adopted");
        h.docker.add_container(c);

        let (status, body) = h.get("/api/bots/adopted/funding").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn funding_prices_usdc_debt_at_one_dollar() {
        let h = harness("funding-usdc-base");
        let usdc = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
        let node = mock_rpc(MockChain::default().balance(usdc, 40_000_000).native(1)).await;
        let api = mock_api(200, 200, 0.00073).await;
        let corridor = setup::find_corridor("cngn-usdc-base").unwrap();
        setup::write_config(h.root.join("bot-a"), corridor, TEST_KEY).unwrap();
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace("https://mainnet.base.org", &node.url)
            .replace("https://api.textilecredit.com", &api.base);
        std::fs::write(&path, toml).unwrap();
        add_container(&h, "bot-a");

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        let row = token(&v, "USDC");
        assert_eq!(row["role"], "stable");
        assert_eq!(row["price"], 1.0);
        assert_eq!(row["priceSource"], "fixed");
        assert_eq!(row["usd"], 40.0);
        assert_eq!(v["gas"]["symbol"], "ETH");
        assert_eq!(v["networkLabel"], "Base");
    }

    /// Textile lists `cNGN ↔ GD` on Celo, whose debt token is GoodDollar at
    /// about $0.00006. Valuing that side at a dollar (and the soft side off a
    /// feed denominated in it) certified a wallet holding a tenth of a cent as
    /// funded and started a bot with nothing to quote. Both rows must read
    /// unknown, and the gate must refuse.
    #[tokio::test]
    async fn funding_refuses_a_pool_that_does_not_quote_against_a_dollar() {
        let h = harness("funding-not-a-dollar");
        // GD on Celo, the debt token of the live cNGN/GD corridor.
        let gd = "0x62B8B11039FcfE5aB0C56E502b1C372A3d2a9c7A";
        let node = mock_rpc(
            MockChain::default()
                .balance(gd, 20_000_000_000_000_000_000)
                .balance(CNGN, 2_000_000)
                .native(20_000_000_000_000_000_000)
                .symbol(gd, "G$")
                .symbol(CNGN, "cNGN"),
        )
        .await;
        let api = mock_api(200, 200, 11.5).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path)
            .unwrap()
            .replace(USDT, gd)
            .replace("debt_decimals = 6", "debt_decimals = 18");
        std::fs::write(&path, toml).unwrap();

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        for row in v["tokens"].as_array().unwrap() {
            assert!(row["price"].is_null(), "{body}");
            assert!(row["priceSource"].is_null(), "{body}");
            assert!(row["usd"].is_null(), "{body}");
            assert!(row["funded"].is_null(), "unknown, not funded: {body}");
            assert_eq!(row["priceError"], NOT_A_DOLLAR_PAIR, "{body}");
            // Per row, so a caller adding this corridor to a bot that also
            // quotes a dollar pair can still tell that THIS one can never be
            // checked. The bot-wide flag below cannot answer that.
            assert_eq!(row["unpriceable"], true, "{body}");
        }
        // The balances themselves still read fine; only the dollar value is
        // missing, so the screen can still show what arrived.
        assert!(v["tokens"][0]["balance"].as_str().is_some(), "{body}");
        // Gas-only gate: an unvaluable pair can still be approved and started.
        assert_eq!(v["gate"]["passes"], true, "{body}");
        assert_eq!(v["gate"]["needsSide"], true);
        // And the gate says WHY the sides will never value, so Live can say so
        // with an explanation instead of polling a wallet that cannot clear it
        // and asking for money that would not help.
        assert_eq!(v["gate"]["unpriceable"], true, "{body}");
    }

    /// The flag is about the pair, not about the wallet or the feed: a normal
    /// corridor never sets it, however empty the wallet is.
    #[tokio::test]
    async fn a_dollar_pool_is_never_flagged_unpriceable() {
        let h = harness("funding-priceable");
        let node = mock_rpc(MockChain::default().native(1)).await;
        let api = mock_api(200, 200, 11.5).await;
        seed(&h, "bot-a", &node.url, &api.base, &feed_of(&api));

        let (status, body) = h.get("/api/bots/bot-a/funding").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["gate"]["unpriceable"], false, "{body}");
        assert_eq!(v["gate"]["needsSide"], true, "empty wallet: {body}");
        for row in v["tokens"].as_array().unwrap() {
            assert_eq!(row["unpriceable"], false, "{body}");
        }
    }

    #[test]
    fn only_listed_addresses_count_as_dollars() {
        // Celo USDT, in both cases the panel may see it in.
        assert!(is_dollar_token(
            42220,
            "0x48065fbbe25f71c9282ddf5e1cd6d6a887483d5e"
        ));
        // The same token on another chain is a different token.
        assert!(!is_dollar_token(
            56,
            "0x48065fbbe25f71c9282ddf5e1cd6d6a887483d5e"
        ));
        // GoodDollar on Celo: a debt token, and nowhere near a dollar.
        assert!(!is_dollar_token(
            42220,
            "0x62b8b11039fcfe5ab0c56e502b1c372a3d2a9c7a"
        ));
        // Every listed address is stored the way `TokenPlan::key` writes one:
        // lowercase `0x…`, so the comparison never misses on case.
        for (_, address) in DOLLAR_TOKENS {
            let parsed = address.parse::<Address>().expect(address);
            assert_eq!(&format!("{parsed:#x}"), address, "not lowercase: {address}");
        }
    }

    #[tokio::test]
    async fn funding_unknown_bot_is_a_404() {
        let h = harness("funding-missing");
        let (status, body) = h.get("/api/bots/nope/funding").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[test]
    fn units_are_exact_text_and_close_enough_floats() {
        assert_eq!(format_units(U256::from(25_000_000u64), 6), "25");
        assert_eq!(format_units(U256::from(500_000u64), 6), "0.5");
        assert_eq!(format_units(U256::from(1_368_726_683u64), 6), "1368.726683");
        assert_eq!(format_units(U256::from(100u64), 6), "0.0001");
        assert_eq!(format_units(U256::ZERO, 18), "0");
        assert_eq!(format_units(U256::from(7u64), 0), "7");
        assert_eq!(
            format_units(U256::from(20_000_000_000_000_000_000u128), 18),
            "20"
        );
        assert!((units(U256::from(30_000_000_000u64), 6) - 30_000.0).abs() < 1e-9);
    }

    #[test]
    fn the_gate_needs_gas_and_reports_the_sides() {
        fn row(symbol: &str, funded: Option<bool>, approved: Option<bool>) -> FundingTokenBody {
            FundingTokenBody {
                role: "stable",
                symbol: symbol.into(),
                token: String::new(),
                decimals: 6,
                balance: None,
                balance_text: None,
                price: None,
                price_source: None,
                price_error: None,
                unpriceable: false,
                usd: None,
                funded,
                approval_needed: true,
                permit2_allowance: None,
                approved,
            }
        }
        fn gas(ok: Option<bool>) -> FundingGasBody {
            FundingGasBody {
                symbol: "CELO",
                balance: None,
                balance_text: None,
                price: None,
                price_source: None,
                usd: None,
                ok,
            }
        }
        let g = gate_of(
            &[
                row("USDT", Some(true), Some(true)),
                row("cNGN", Some(false), None),
            ],
            &gas(Some(true)),
            0.5,
            false,
        );
        assert!(g.passes);
        assert_eq!(g.funded_tokens, vec!["USDT"]);
        assert_eq!(
            g.approvals_missing,
            vec!["cNGN"],
            "unknown counts as missing"
        );
        assert_eq!(g.min_gas_usd, 0.5, "the figure the caller sized");
        assert!(!g.unpriceable);

        // No side funded, gas fine: passes. Approval needs no money, and Live
        // is the screen that waits for it. `needs_side` still says so.
        let g = gate_of(&[row("USDT", None, None)], &gas(Some(true)), 1.0, false);
        assert!(g.passes && g.needs_side && !g.needs_gas);

        // A funded side without gas: refused. Nothing can be sent.
        let g = gate_of(&[row("USDT", Some(true), None)], &gas(None), 1.0, false);
        assert!(!g.passes && !g.needs_side && g.needs_gas);

        // Unpriceable is carried through untouched and no longer holds the
        // gate: the bot can be approved and started, and Live says why the
        // sides will never show a dollar value.
        let g = gate_of(&[row("GD", None, None)], &gas(Some(true)), 1.0, true);
        assert!(g.passes && g.needs_side && g.unpriceable);
    }

    /// The wizard sends one approve per unapproved side and then a start, so
    /// the gate has to ask for gas per transaction, per chain.
    #[test]
    fn the_gas_floor_scales_with_the_chain_and_the_approvals() {
        // Celo: cheap, two approvals to send.
        assert_eq!(min_gas_usd(42220, 2), 1.0);
        assert_eq!(min_gas_usd(42220, 0), 0.5, "the start still needs gas");
        // Ethereum: two approves there are dollars, not cents. A flat $1 floor
        // used to certify a wallet that could not pay for one of them.
        assert_eq!(min_gas_usd(1, 2), 10.0);
        assert!(min_gas_usd(1, 1) > 1.0);
        // Base pays in ETH too, but at L2 prices.
        assert_eq!(min_gas_usd(8453, 2), 1.0);
        // A chain the panel can't name falls back to the flat figure.
        assert_eq!(min_gas_usd(999, 2), 2.0);
    }

    #[test]
    fn gas_without_a_price_counts_any_balance() {
        assert_eq!(gas_ok(Some(U256::from(1u8)), None, false, 1.0), Some(true));
        assert_eq!(gas_ok(Some(U256::ZERO), None, false, 1.0), Some(false));
        assert_eq!(
            gas_ok(Some(U256::from(1u8)), Some(0.5), true, 1.0),
            Some(false)
        );
        assert_eq!(
            gas_ok(Some(U256::from(1u8)), Some(1.0), true, 1.0),
            Some(true)
        );
        // The same balance against a bigger floor: an Ethereum wallet with a
        // dollar of ETH on it does not clear two approvals.
        assert_eq!(
            gas_ok(Some(U256::from(1u8)), Some(1.0), true, 10.0),
            Some(false)
        );
        assert_eq!(gas_ok(None, None, true, 1.0), None);
    }

    #[test]
    fn symbol_returns_decode_both_abi_shapes() {
        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&U256::from(32u8).to_be_bytes::<32>());
        dynamic.extend_from_slice(&U256::from(4u8).to_be_bytes::<32>());
        dynamic.extend_from_slice(b"cNGN");
        dynamic.extend_from_slice(&[0u8; 28]);
        assert_eq!(decode_string_return(&dynamic).as_deref(), Some("cNGN"));

        let mut fixed = [0u8; 32];
        fixed[..3].copy_from_slice(b"MKR");
        assert_eq!(decode_string_return(&fixed).as_deref(), Some("MKR"));

        assert_eq!(decode_string_return(&[]), None);
        assert_eq!(decode_string_return(&[0u8; 32]), None);
    }
}
