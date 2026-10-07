// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Trade from an OperatorVault, or go back to the bot's own wallet.
//!
//! Writing `[vault]` is a small part of moving a bot onto a vault. The venue
//! registers one maker per funding wallet, so the bot needs a fresh enrollment
//! with the vault as funding wallet, and the API key that comes with it. The
//! public ladder has to be off (`Config` refuses `[vault]` with it on), and the
//! taker leg goes off too: it fills from the signer wallet's own funds, not the
//! vault's. `POST /vault` does all of that and writes nothing until every check
//! and the enrollment have succeeded. `POST /vault/check` runs the same checks
//! and writes nothing at all. `DELETE /vault` removes `[vault]` and re-enrolls
//! with the bot's own wallet.
//!
//! The checks are plain functions over facts fetched once up front: the chain's
//! answers about the address, and the venue's own vault check. Later checks
//! are skipped when an earlier one leaves nothing to ask (no contract means no
//! views to read). The venue's check is advice; enroll runs the same rules and
//! has the last word.
//!
//! Every signer backend enrolls the same way: `MakerEnroll` is EIP-712 typed
//! data signed through `sign_typed`, which local keys, Turnkey, MPCVault and
//! Fireblocks all support.

use std::collections::HashMap;
use std::path::Path;

use alloy_primitives::{Address, U256};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::chain_reads::{read_all, word, Ask};
use super::enroll::{
    bot_dir, enrollment_body, refuse_connect_on_unmigrated_flat_layout, signer_for_bot,
    store_maker_key,
};
use super::funding::{budgeted, decode_string_return, format_units, symbol_ask};
use super::settings::{config_path, network_name, read_toml, save_and_restart};
use super::{ApiError, AppState};
use crate::chain::rpc::Rpc;
use crate::config::{rfq_default_flag_in_dir, Config};
use crate::panel::inventory::Bot;
use crate::protocol::vault::{
    encode_close_only, encode_corridor_asset, encode_operator_admin, encode_paused,
    encode_quotable_corridor, encode_quotable_settlement, encode_settlement_asset,
    encode_strategy_signer, encode_yield_adapter,
};
use crate::setup;
use crate::signer::DynSigner;
use crate::venue::enroll::{
    apply_enrollment, enroll_url_from_config, register_maker, venue_origin_from_config,
    EnrollOutcome, EnrollResponse,
};
use crate::venue::vault_check::{check_vault, VaultCheckResponse};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultBody {
    pub address: String,
    /// Override the venue origin. Tests use this; the UI does not.
    #[serde(default)]
    pub venue_url: Option<String>,
}

/// The venue override for `DELETE`, which has no body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenueQuery {
    #[serde(default)]
    pub venue_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// Blocks connecting.
    Fail,
    /// Worth knowing, doesn't block: the vault can be connected and fixed after.
    Warn,
    /// Not run, because an earlier check left nothing to ask or the answer
    /// wasn't available.
    Skipped,
}

/// One row of the checklist the panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: String,
    pub label: String,
    pub status: Status,
    pub detail: String,
}

/// The fixed rows, in the order they run. The `textile` row stands for the
/// venue's answer, which can expand into several rows once it is in.
const ROWS: [&str; 10] = [
    "address",
    "not-signer",
    "contract",
    "operator-vault",
    "pair",
    "strategy-signer",
    "textile",
    "paused",
    "close-only",
    "inventory",
];

fn label(id: &str) -> &'static str {
    match id {
        "address" => "Valid address",
        "not-signer" => "Not the bot's own wallet",
        "contract" => "Contract on this network",
        "operator-vault" => "Answers as an OperatorVault",
        "pair" => "Same pair as this bot",
        "strategy-signer" => "Strategy signer is this bot",
        "textile-registered" => "Registered with Textile",
        "textile-risk-signer" => "Risk signer is Textile's",
        "textile-wiring" => "Settles through Textile's contracts",
        "paused" => "Not paused",
        "close-only" => "Not close-only",
        "inventory" => "Has something to quote",
        // `textile`, `textile-executor` and `textile-issue-N`.
        _ => "Textile would accept it",
    }
}

fn row(id: &str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        id: id.to_string(),
        label: label(id).to_string(),
        status,
        detail: detail.into(),
    }
}

/// `rows` as they stand, then every fixed row after `last` marked skipped.
fn skip_after(mut rows: Vec<Check>, last: &str) -> Vec<Check> {
    let from = ROWS.iter().position(|id| *id == last).map_or(0, |i| i + 1);
    rows.extend(
        ROWS[from..]
            .iter()
            .map(|id| row(id, Status::Skipped, "Needs the checks above to pass.")),
    );
    rows
}

fn failed(rows: &[Check]) -> bool {
    rows.iter().any(|c| c.status == Status::Fail)
}

/// One pool, as the pair check needs it. A token that doesn't parse is `None`
/// and matches nothing.
#[derive(Debug, Clone)]
struct PoolPair {
    label: String,
    collateral: Option<Address>,
    collateral_decimals: u8,
    debt: Option<Address>,
    debt_decimals: u8,
}

/// The bot side of the comparison.
#[derive(Debug, Clone)]
struct BotSide {
    /// The address the bot signs with. The vault's strategy signer must be it.
    signer: Address,
    network: String,
    pools: Vec<PoolPair>,
}

impl BotSide {
    fn of(cfg: &Config, signer: Address) -> Self {
        let pools = cfg
            .pools
            .iter()
            .map(|p| PoolPair {
                label: setup::pool_identity(cfg.chain_id, p)
                    .map(|c| c.display_name)
                    .unwrap_or_else(|| {
                        format!(
                            "{} / {}",
                            setup::short_addr(&p.collateral),
                            setup::short_addr(&p.debt)
                        )
                    }),
                collateral: p.collateral.trim().parse().ok(),
                collateral_decimals: p.collateral_decimals,
                debt: p.debt.trim().parse().ok(),
                debt_decimals: p.debt_decimals,
            })
            .collect();
        Self {
            signer,
            network: network_name(cfg.chain_id),
            pools,
        }
    }

    /// Decimals for a token one of the pools trades, so an amount can be
    /// shown in units rather than atoms.
    fn decimals_of(&self, token: Address) -> Option<u8> {
        self.pools.iter().find_map(|p| {
            if p.collateral == Some(token) {
                Some(p.collateral_decimals)
            } else if p.debt == Some(token) {
                Some(p.debt_decimals)
            } else {
                None
            }
        })
    }
}

/// What the chain says about the address. `None` means that view didn't
/// answer with a value of its type.
#[derive(Debug, Clone)]
struct ChainFacts {
    /// `eth_getCode` came back non-empty. `Err` is the node not answering.
    has_code: Result<bool, String>,
    settlement: Option<Address>,
    corridor: Option<Address>,
    strategy_signer: Option<Address>,
    operator_admin: Option<Address>,
    yield_adapter: Option<Address>,
    paused: Option<bool>,
    close_only: Option<bool>,
    quotable_settlement: Option<U256>,
    quotable_corridor: Option<U256>,
}

/// The venue's answer to `/v2/maker/vault-check`.
#[derive(Debug, Clone)]
enum Textile {
    /// Not asked: an earlier check already rules the address out.
    NotAsked,
    /// The venue has no such route (an API older than it). Enroll still runs
    /// the same checks.
    Unavailable,
    Failed(String),
    Answered(VaultCheckResponse),
}

#[derive(Debug, Clone)]
struct Facts {
    chain: ChainFacts,
    textile: Textile,
    /// Tickers for the vault's two assets, where known.
    symbols: HashMap<Address, String>,
}

impl Facts {
    fn symbol(&self, token: Address) -> String {
        self.symbols
            .get(&token)
            .cloned()
            .unwrap_or_else(|| setup::short_addr(&token.to_checksum(None)))
    }
}

/// `0x` and 40 hex characters. Mixed case has to be a valid EIP-55 checksum,
/// which catches a mistyped character in an address copied from an explorer.
fn parse_vault_address(raw: &str) -> Result<Address, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter the vault's address.".to_string());
    }
    let hex = raw
        .strip_prefix("0x")
        .filter(|h| h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()));
    let Some(hex) = hex else {
        return Err(format!(
            "{raw} isn't an address. It should be 0x followed by 40 hex characters."
        ));
    };
    let mixed_case =
        hex.chars().any(|c| c.is_ascii_lowercase()) && hex.chars().any(|c| c.is_ascii_uppercase());
    let parsed = if mixed_case {
        Address::parse_checksummed(raw, None).map_err(|_| {
            "The checksum doesn't match, so a character is probably wrong. Copy the address \
             again."
                .to_string()
        })?
    } else {
        raw.parse::<Address>()
            .map_err(|e| format!("{raw} isn't an address: {e}"))?
    };
    if parsed.is_zero() {
        return Err("That's the zero address.".to_string());
    }
    Ok(parsed)
}

/// The checks that need no network: the address itself, and that it isn't the
/// bot's signer. `Err` carries the finished checklist.
fn precheck(raw: &str, signer: Address) -> Result<(Address, Vec<Check>), Vec<Check>> {
    let vault = match parse_vault_address(raw) {
        Ok(vault) => vault,
        Err(why) => {
            return Err(skip_after(
                vec![row("address", Status::Fail, why)],
                "address",
            ))
        }
    };
    let mut rows = vec![row("address", Status::Ok, vault.to_checksum(None))];
    rows.push(check_not_signer(vault, signer));
    if failed(&rows) {
        return Err(skip_after(rows, "not-signer"));
    }
    Ok((vault, rows))
}

fn check_not_signer(vault: Address, signer: Address) -> Check {
    if vault == signer {
        return row(
            "not-signer",
            Status::Fail,
            format!(
                "That's this bot's own signing wallet ({}). Enter the OperatorVault's address.",
                signer.to_checksum(None)
            ),
        );
    }
    row(
        "not-signer",
        Status::Ok,
        format!("The bot signs as {}.", signer.to_checksum(None)),
    )
}

fn check_contract(network: &str, chain: &ChainFacts) -> Check {
    match &chain.has_code {
        Ok(true) => row("contract", Status::Ok, format!("Code found on {network}.")),
        Ok(false) => row(
            "contract",
            Status::Fail,
            format!(
                "No contract at this address on {network}. Check the address, and that the \
                 vault is on the same network as the bot."
            ),
        ),
        Err(e) => row(
            "contract",
            Status::Fail,
            format!("Couldn't read {network} to check: {e}"),
        ),
    }
}

/// What an OperatorVault has to answer for the bot to trade it.
#[derive(Debug, Clone, Copy)]
struct Identity {
    settlement: Address,
    corridor: Address,
    strategy_signer: Address,
}

/// The identity, when the views answered as an OperatorVault does: both
/// assets and the strategy signer set, and the assets distinct.
fn identity(chain: &ChainFacts) -> Option<Identity> {
    let found = Identity {
        settlement: chain.settlement?,
        corridor: chain.corridor?,
        strategy_signer: chain.strategy_signer?,
    };
    (!found.settlement.is_zero()
        && !found.corridor.is_zero()
        && found.settlement != found.corridor
        && !found.strategy_signer.is_zero())
    .then_some(found)
}

fn check_views(facts: &Facts) -> Check {
    match identity(&facts.chain) {
        Some(id) => row(
            "operator-vault",
            Status::Ok,
            format!(
                "Settlement {}, corridor {}.",
                facts.symbol(id.settlement),
                facts.symbol(id.corridor)
            ),
        ),
        None => row(
            "operator-vault",
            Status::Fail,
            "This contract doesn't answer settlementAsset(), corridorAsset() and \
             strategySigner(), so it isn't an OperatorVault.",
        ),
    }
}

/// Whether the address is worth asking the venue about. The same two checks
/// that end the checklist early, so the two can't disagree.
fn identity_ok(bot: &BotSide, facts: &Facts) -> bool {
    check_contract(&bot.network, &facts.chain).status == Status::Ok
        && check_views(facts).status == Status::Ok
}

/// A vault trades exactly its settlement↔corridor pair (`VaultPolicy`), in
/// either direction, so every pool on the bot has to be that pair.
fn check_pair(bot: &BotSide, facts: &Facts, settlement: Address, corridor: Address) -> Check {
    let pair = format!("{} ↔ {}", facts.symbol(settlement), facts.symbol(corridor));
    let is_vault_pair = |p: &PoolPair| {
        matches!(
            (p.collateral, p.debt),
            (Some(c), Some(d))
                if (c == settlement && d == corridor) || (c == corridor && d == settlement)
        )
    };
    let others: Vec<&str> = bot
        .pools
        .iter()
        .filter(|p| !is_vault_pair(p))
        .map(|p| p.label.as_str())
        .collect();
    match (others.as_slice(), bot.pools.len()) {
        ([], _) => row("pair", Status::Ok, format!("{pair}, same as this bot.")),
        ([only], 1) => row(
            "pair",
            Status::Fail,
            format!(
                "This bot quotes {only}, but the vault trades {pair}. A vault trades one pair, \
                 so the bot has to be that pair."
            ),
        ),
        (others, _) => row(
            "pair",
            Status::Fail,
            format!(
                "The vault trades {pair}, and every corridor on this bot has to be that pair. \
                 Not this vault's pair: {}. Remove {} first, or connect the vault to a bot of \
                 its own.",
                others.join(", "),
                if others.len() == 1 { "it" } else { "them" }
            ),
        ),
    }
}

fn check_strategy_signer(bot: &BotSide, chain: &ChainFacts, strategy: Address) -> Check {
    let signer = bot.signer.to_checksum(None);
    if strategy == bot.signer {
        return row(
            "strategy-signer",
            Status::Ok,
            format!("{signer} signs for the vault."),
        );
    }
    let admin = chain
        .operator_admin
        .filter(|a| !a.is_zero())
        .map(|a| format!(" ({})", a.to_checksum(None)))
        .unwrap_or_default();
    row(
        "strategy-signer",
        Status::Fail,
        format!(
            "The vault's strategy signer is {}, but this bot signs as {signer}. The vault's \
             operator admin{admin} can rotate the strategy signer to {signer} with \
             setStrategySigner, or deploy a new vault with this bot as its strategy signer.",
            strategy.to_checksum(None)
        ),
    )
}

/// The venue's verdict as rows. Its `issues` are the reasons enroll would
/// refuse, already in operator language, so each one is a failing row as-is.
/// The three confirmations show only when they hold: a false one always comes
/// with an issue saying why.
fn textile_checks(network: &str, textile: &Textile, yield_adapter: Option<Address>) -> Vec<Check> {
    let answer = match textile {
        Textile::NotAsked => {
            return vec![row(
                "textile",
                Status::Skipped,
                "Needs the checks above to pass.",
            )]
        }
        Textile::Unavailable => return vec![unavailable_check(yield_adapter)],
        Textile::Failed(e) => {
            return vec![row(
                "textile",
                Status::Fail,
                format!("Couldn't ask Textile about this vault: {e}"),
            )]
        }
        Textile::Answered(answer) => answer,
    };
    let confirmed = [
        (
            "textile-registered",
            answer.registered,
            format!("From an OperatorVault factory Textile trusts on {network}."),
        ),
        (
            "textile-risk-signer",
            answer.risk_signer_ok == Some(true),
            "Textile can co-sign its quotes.".to_string(),
        ),
        (
            "textile-wiring",
            answer.wiring_ok == Some(true),
            "Reactor, Permit2 and filler validation match Textile's.".to_string(),
        ),
    ];
    let mut rows: Vec<Check> = confirmed
        .iter()
        .filter(|(_, ok, _)| *ok)
        .map(|(id, _, detail)| row(id, Status::Ok, detail.clone()))
        .collect();
    rows.extend(
        answer
            .issues
            .iter()
            .enumerate()
            .map(|(i, issue)| row(&format!("textile-issue-{i}"), Status::Fail, issue.clone())),
    );
    // An answer with no issues that still doesn't confirm all three is one
    // enroll's rules don't produce. Refuse rather than read silence as yes.
    if answer.issues.is_empty() {
        rows.extend(confirmed.iter().filter(|(_, ok, _)| !ok).map(|(id, _, _)| {
            row(
                id,
                Status::Fail,
                "Textile didn't confirm this, and gave no reason. Try again.",
            )
        }));
    }
    if let Some(raw) = &answer.order_executor {
        if raw.trim().parse::<Address>().is_err() {
            rows.push(row(
                "textile-executor",
                Status::Fail,
                format!("Textile named an order executor that isn't an address: {raw}"),
            ));
        }
    }
    rows
}

/// No vault check on this Textile API (an older one). Enroll still checks
/// registration, risk signer and wiring when connecting, so for most vaults
/// this is fine. Not for one that stakes idle settlement: whether the bot has
/// to name an order executor is only in the vault check, and when Textile
/// routes fills through one, a binding without it gets the vault's
/// settlement-side quotes refused. So a yield vault, or one whose adapter
/// couldn't be read, fails closed.
fn unavailable_check(yield_adapter: Option<Address>) -> Check {
    match yield_adapter {
        Some(adapter) if adapter.is_zero() => row(
            "textile",
            Status::Skipped,
            "This Textile API has no vault check yet, so registration, risk signer and wiring \
             are checked when you connect. This vault doesn't stake, so it needs no order \
             executor.",
        ),
        _ => row(
            "textile",
            Status::Fail,
            "This Textile API has no vault check yet, so the panel can't tell which order \
             executor this vault needs for its idle-yield funds, and without the right one its \
             quotes that sell settlement could be refused. Try again once Textile's API is \
             updated.",
        ),
    }
}

fn check_paused(chain: &ChainFacts) -> Check {
    match chain.paused {
        Some(false) => row("paused", Status::Ok, "Trading is open."),
        Some(true) => row(
            "paused",
            Status::Warn,
            "The vault is paused, so it quotes nothing until it's unpaused.",
        ),
        None => row(
            "paused",
            Status::Skipped,
            "The vault didn't answer paused().",
        ),
    }
}

fn check_close_only(facts: &Facts, settlement: Address, corridor: Address) -> Check {
    match facts.chain.close_only {
        Some(false) => row("close-only", Status::Ok, "Both sides can trade."),
        Some(true) => row(
            "close-only",
            Status::Warn,
            format!(
                "The vault is close-only: it won't spend its {}, so only its {} quotes.",
                facts.symbol(settlement),
                facts.symbol(corridor)
            ),
        ),
        None => row(
            "close-only",
            Status::Skipped,
            "The vault didn't answer closeOnly().",
        ),
    }
}

/// `quotableSettlement()` and `quotableCorridor()`, the figures the bot sizes
/// its quotes from. Settlement here includes any staked part, so a vault the
/// bot can only reach the idle part of may quote less than this says.
fn check_inventory(bot: &BotSide, facts: &Facts, settlement: Address, corridor: Address) -> Check {
    let (Some(s), Some(c)) = (
        facts.chain.quotable_settlement,
        facts.chain.quotable_corridor,
    ) else {
        return row(
            "inventory",
            Status::Skipped,
            "The vault didn't answer quotableSettlement() and quotableCorridor().",
        );
    };
    if s.is_zero() && c.is_zero() {
        return row(
            "inventory",
            Status::Warn,
            "Nothing quotable on either side yet. Fund the vault before it can quote.",
        );
    }
    let amount = |qty: U256, token: Address| match bot.decimals_of(token) {
        Some(decimals) => format!("{} {}", format_units(qty, decimals), facts.symbol(token)),
        None => format!("{qty} atomic {}", facts.symbol(token)),
    };
    row(
        "inventory",
        Status::Ok,
        format!(
            "{} and {} quotable.",
            amount(s, settlement),
            amount(c, corridor)
        ),
    )
}

/// Every check after the precheck, over facts already fetched.
fn assess_facts(bot: &BotSide, facts: &Facts, mut rows: Vec<Check>) -> Vec<Check> {
    rows.push(check_contract(&bot.network, &facts.chain));
    if failed(&rows) {
        return skip_after(rows, "contract");
    }
    rows.push(check_views(facts));
    let Some(id) = identity(&facts.chain) else {
        return skip_after(rows, "operator-vault");
    };
    rows.push(check_pair(bot, facts, id.settlement, id.corridor));
    rows.push(check_strategy_signer(bot, &facts.chain, id.strategy_signer));
    rows.extend(textile_checks(
        &bot.network,
        &facts.textile,
        facts.chain.yield_adapter,
    ));
    rows.push(check_paused(&facts.chain));
    rows.push(check_close_only(facts, id.settlement, id.corridor));
    rows.push(check_inventory(bot, facts, id.settlement, id.corridor));
    rows
}

/// What the panel shows about the vault next to the checklist.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub vault: String,
    pub explorer_url: Option<String>,
    pub settlement_asset: Option<String>,
    pub settlement_symbol: Option<String>,
    pub corridor_asset: Option<String>,
    pub corridor_symbol: Option<String>,
    pub strategy_signer: Option<String>,
    /// Whether idle settlement is staked in a yield adapter. `None` when the
    /// view didn't answer.
    pub yield_enabled: Option<bool>,
    pub paused: Option<bool>,
    pub close_only: Option<bool>,
    /// What `[vault].order_executor` becomes on connect. `None` leaves it unset.
    pub order_executor: Option<String>,
    /// False when the venue couldn't run its own check. Connecting still does.
    pub textile_checked: bool,
}

fn summarize(vault: Address, chain_id: u64, facts: &Facts) -> Summary {
    let chain = &facts.chain;
    let checksum = |a: Option<Address>| a.map(|a| a.to_checksum(None));
    let vault_text = vault.to_checksum(None);
    Summary {
        explorer_url: setup::address_explorer_url(chain_id, &vault_text),
        vault: vault_text,
        settlement_asset: checksum(chain.settlement),
        settlement_symbol: chain.settlement.map(|a| facts.symbol(a)),
        corridor_asset: checksum(chain.corridor),
        corridor_symbol: chain.corridor.map(|a| facts.symbol(a)),
        strategy_signer: checksum(chain.strategy_signer),
        yield_enabled: chain.yield_adapter.map(|a| !a.is_zero()),
        paused: chain.paused,
        close_only: chain.close_only,
        order_executor: checksum(order_executor(&facts.textile)),
        textile_checked: matches!(facts.textile, Textile::Answered(_)),
    }
}

/// The executor the venue named for this vault, if any.
fn order_executor(textile: &Textile) -> Option<Address> {
    match textile {
        Textile::Answered(answer) => answer
            .order_executor
            .as_deref()
            .and_then(|raw| raw.trim().parse().ok()),
        _ => None,
    }
}

/// What connecting writes, when every check passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    vault: Address,
    order_executor: Option<Address>,
}

#[derive(Debug, Clone)]
struct Verdict {
    checks: Vec<Check>,
    summary: Option<Summary>,
    /// `Some` exactly when nothing failed.
    target: Option<Target>,
}

impl Verdict {
    fn body(&self) -> Value {
        json!({
            "ok": self.target.is_some(),
            "checks": self.checks,
            "summary": self.summary,
        })
    }
}

/// A view returning `address`: the high 12 bytes must be zero, or this is some
/// other contract's word and not an address at all.
fn as_address(word: U256) -> Option<Address> {
    (word >> 160u32)
        .is_zero()
        .then(|| Address::from_slice(&word.to_be_bytes::<32>()[12..]))
}

/// A view returning `bool`: exactly 0 or 1.
fn as_bool(word: U256) -> Option<bool> {
    if word.is_zero() {
        Some(false)
    } else if word == U256::from(1u8) {
        Some(true)
    } else {
        None
    }
}

/// Every chain read at once: the code check beside one batch of views.
async fn read_chain(rpc_url: &str, vault: Address) -> ChainFacts {
    let rpc = Rpc::new(rpc_url.to_string());
    let asks: Vec<Ask> = [
        (encode_settlement_asset(), "settlementAsset()"),
        (encode_corridor_asset(), "corridorAsset()"),
        (encode_strategy_signer(), "strategySigner()"),
        (encode_operator_admin(), "operatorAdmin()"),
        (encode_yield_adapter(), "yieldAdapter()"),
        (encode_paused(), "paused()"),
        (encode_close_only(), "closeOnly()"),
        (encode_quotable_settlement(), "quotableSettlement()"),
        (encode_quotable_corridor(), "quotableCorridor()"),
    ]
    .into_iter()
    .map(|(data, view)| Ask::call(vault, data, format!("{view} on {vault}")))
    .collect();
    let (code, answers) = tokio::join!(
        budgeted(rpc_url, rpc.get_code(vault)),
        read_all(rpc_url, &asks),
    );
    let views: Vec<Option<U256>> = asks
        .iter()
        .zip(&answers)
        .map(|(ask, answer)| word(answer, &ask.what).ok())
        .collect();
    let address = |i: usize| views[i].and_then(as_address);
    let flag = |i: usize| views[i].and_then(as_bool);
    ChainFacts {
        has_code: code.map(|c| !c.is_empty()).map_err(|e| format!("{e:#}")),
        settlement: address(0),
        corridor: address(1),
        strategy_signer: address(2),
        operator_admin: address(3),
        yield_adapter: address(4),
        paused: flag(5),
        close_only: flag(6),
        quotable_settlement: views[7],
        quotable_corridor: views[8],
    }
}

/// Tickers for the vault's two assets: the bot's corridor names first, then
/// the token's own `symbol()` for anything they don't cover.
async fn read_symbols(cfg: &Config, chain: &ChainFacts) -> HashMap<Address, String> {
    let known = super::allowances::token_symbols(cfg);
    let tokens: Vec<Address> = [chain.settlement, chain.corridor]
        .into_iter()
        .flatten()
        .filter(|a| !a.is_zero())
        .collect();
    let unknown: Vec<Address> = tokens
        .iter()
        .copied()
        .filter(|token| !known.contains_key(&format!("{token:#x}")))
        .collect();
    let asks: Vec<Ask> = unknown.iter().map(|token| symbol_ask(*token)).collect();
    let answers = read_all(&cfg.rpc_url, &asks).await;
    let read = unknown.iter().zip(&answers).filter_map(|(token, answer)| {
        let symbol = decode_string_return(answer.as_ref().ok()?)?;
        Some((*token, symbol))
    });
    tokens
        .iter()
        .filter_map(|token| {
            known
                .get(&format!("{token:#x}"))
                .map(|symbol| (*token, symbol.clone()))
        })
        .chain(read)
        .collect()
}

async fn ask_textile(origin: &str, chain_id: u64, vault: Address, signer: Address) -> Textile {
    match check_vault(origin, chain_id, vault, signer).await {
        Ok(Some(answer)) => Textile::Answered(answer),
        Ok(None) => Textile::Unavailable,
        Err(e) => Textile::Failed(format!("{e:#}")),
    }
}

/// Run every check for `raw` against this bot. Reads only.
async fn assess(cfg: &Config, signer: Address, raw: &str, venue_url: Option<&str>) -> Verdict {
    let bot = BotSide::of(cfg, signer);
    let (vault, rows) = match precheck(raw, signer) {
        Ok(found) => found,
        Err(checks) => {
            return Verdict {
                checks,
                summary: None,
                target: None,
            }
        }
    };
    let chain = read_chain(&cfg.rpc_url, vault).await;
    let symbols = read_symbols(cfg, &chain).await;
    let mut facts = Facts {
        chain,
        textile: Textile::NotAsked,
        symbols,
    };
    if identity_ok(&bot, &facts) {
        let origin = venue_origin_from_config(cfg, venue_url);
        facts.textile = ask_textile(&origin, cfg.chain_id, vault, signer).await;
    }
    let checks = assess_facts(&bot, &facts, rows);
    let target = (!failed(&checks)).then(|| Target {
        vault,
        order_executor: order_executor(&facts.textile),
    });
    Verdict {
        summary: Some(summarize(vault, cfg.chain_id, &facts)),
        checks,
        target,
    }
}

/// A 400 that still carries the checklist, so the panel can show which rows
/// stopped the connect.
fn refused(verdict: &Verdict) -> Response {
    let first = verdict
        .checks
        .iter()
        .find(|c| c.status == Status::Fail)
        .map(|c| c.detail.as_str())
        .unwrap_or("A check failed.");
    let mut body = verdict.body();
    body["error"] = json!(format!("Nothing was changed. {first}"));
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

fn parse_config(toml: &str) -> Result<Config, ApiError> {
    Config::from_toml(toml)
        .map_err(|e| ApiError::bad_request(format!("this config isn't valid: {e:#}")))
}

/// The address a dry run checks against: the one the bot's config resolves
/// to (the key file for a hot wallet, `[signer].operator_address` otherwise).
/// Building the signer itself would read secrets and, for MPCVault, bind its
/// callback port, which a check run on every keystroke has no business doing.
fn configured_signer(bot: &Bot) -> Result<Address, ApiError> {
    bot.config
        .as_ref()
        .and_then(|c| c.operator_address.as_deref())
        .and_then(|a| a.parse().ok())
        .ok_or_else(|| {
            ApiError::bad_request(
                "this bot has no signing address the panel can read, so a vault can't be \
                 checked against it",
            )
        })
}

/// The built signer is what signs `MakerEnroll`, so it is the address the
/// vault actually has to accept. It should always be the configured one; if a
/// key file or `[signer].operator_address` says otherwise, the checks passed
/// for the wrong address.
fn refuse_signer_drift(checked: Address, signing: Address) -> Result<(), ApiError> {
    if checked == signing {
        return Ok(());
    }
    Err(ApiError::conflict(format!(
        "Nothing was changed. The checks ran for {}, but this bot's signer signs as {}. Fix \
         the signer's operator address, then try again.",
        checked.to_checksum(None),
        signing.to_checksum(None)
    )))
}

/// `POST /api/bots/:name/vault/check`: run every check, write nothing.
pub async fn check(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<VaultBody>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    refuse_connect_on_unmigrated_flat_layout(&bot, state.cfg.runtime)?;
    let path = config_path(&bot)?;
    let cfg = parse_config(&read_toml(&path)?)?;
    let signer = configured_signer(&bot)?;
    let verdict = assess(&cfg, signer, &body.address, body.venue_url.as_deref()).await;
    Ok(Json(verdict.body()).into_response())
}

/// `POST /api/bots/:name/vault`: check, then move the bot onto the vault.
pub async fn link(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<VaultBody>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    refuse_connect_on_unmigrated_flat_layout(&bot, state.cfg.runtime)?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = parse_config(&current_toml)?;

    // Checked against the configured address, the same one the dry run used,
    // so Save can't disagree with the checklist the operator just read. The
    // signer is only built once the checks pass: building an MPCVault signer
    // binds its callback port, which a refused connect has no reason to do.
    let checked_as = configured_signer(&bot)?;
    let verdict = assess(&cfg, checked_as, &body.address, body.venue_url.as_deref()).await;
    let Some(target) = verdict.target else {
        return Ok(refused(&verdict));
    };
    let signer = signer_for_bot(&cfg, &path).await?;
    refuse_signer_drift(checked_as, signer.address())?;

    let candidate = setup::link_vault(&current_toml, target.vault, target.order_executor)
        .map_err(|e| ApiError::bad_request(format!("Nothing was changed: {e:#}")))?;
    let candidate_cfg = parse_config(&candidate)?;
    let enrolled = enroll(&candidate_cfg, &signer, body.venue_url.as_deref()).await?;
    let (edited, outcome) = apply_enrollment(
        &candidate,
        &candidate_cfg,
        &enrolled,
        rfq_default(&cfg, &state),
    )
    .map_err(|e| ApiError::bad_request(format!("Nothing was changed: {e:#}")))?;

    let taker_was_on = cfg.pools.iter().any(|p| p.limit_taker_enabled());
    let message = linked_message(outcome, &enrolled, target.vault, taker_was_on);
    switch_maker(
        &state,
        &bot,
        &path,
        &edited,
        &enrolled,
        current_maker(&cfg),
        json!({
            "message": message,
            "enrollment": enrollment_body(&enrolled),
            "checks": verdict.checks,
            "summary": verdict.summary,
        }),
    )
    .await
}

/// `DELETE /api/bots/:name/vault`: trade from the bot's own wallet again.
pub async fn unlink(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Query(query): Query<VenueQuery>,
) -> Result<Response, ApiError> {
    let (_saving, bot) = super::bots::lock_config(&name, &state).await?;
    refuse_connect_on_unmigrated_flat_layout(&bot, state.cfg.runtime)?;
    let path = config_path(&bot)?;
    let current_toml = read_toml(&path)?;
    let cfg = parse_config(&current_toml)?;
    if cfg.vault.is_none() {
        return Err(ApiError::bad_request(format!(
            "{} already trades from its own wallet.",
            bot.name
        )));
    }

    let signer = signer_for_bot(&cfg, &path).await?;
    let candidate = setup::unlink_vault(&current_toml)
        .map_err(|e| ApiError::bad_request(format!("Nothing was changed: {e:#}")))?;
    let candidate_cfg = parse_config(&candidate)?;
    let enrolled = enroll(&candidate_cfg, &signer, query.venue_url.as_deref()).await?;
    let (edited, outcome) = apply_enrollment(
        &candidate,
        &candidate_cfg,
        &enrolled,
        rfq_default(&cfg, &state),
    )
    .map_err(|e| ApiError::bad_request(format!("Nothing was changed: {e:#}")))?;

    let message = unlinked_message(outcome, &enrolled, &bot.name);
    switch_maker(
        &state,
        &bot,
        &path,
        &edited,
        &enrolled,
        current_maker(&cfg),
        json!({
            "message": message,
            "enrollment": enrollment_body(&enrolled),
        }),
    )
    .await
}

fn rfq_default(cfg: &Config, state: &AppState) -> bool {
    cfg.rfq_default_unlocked() || rfq_default_flag_in_dir(&state.cfg.bots_dir)
}

/// Register against the candidate config, so the funding wallet the venue
/// records is the one the bot is about to trade from.
async fn enroll(
    cfg: &Config,
    signer: &DynSigner,
    venue_url: Option<&str>,
) -> Result<EnrollResponse, ApiError> {
    let venue = enroll_url_from_config(cfg, venue_url);
    register_maker(cfg, signer, &venue)
        .await
        .map_err(|e| ApiError::bad_request(format!("Nothing was changed: {e:#}")))
}

/// The maker the config on disk names, if it has one.
fn current_maker(cfg: &Config) -> Option<String> {
    cfg.rfq
        .as_ref()
        .map(|rfq| rfq.maker_id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// What the key file should hold when the new config didn't land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAfterFailedSave {
    /// The bot moved to another maker (onto or off a vault). That maker's
    /// enrollment never touched the old maker's key, and the config on disk
    /// still names the old maker, so its key goes back.
    RestorePrevious,
    /// The bot stayed on the same maker (reconnecting the vault it already
    /// has). Enroll rotated that maker's key and revoked the old one on the
    /// spot, so the old file would hold a dead key; the new one is the only
    /// key that works with the unchanged config.
    KeepNew,
}

fn key_after_failed_save(previous_maker: Option<&str>, enrolled_maker: &str) -> KeyAfterFailedSave {
    if previous_maker == Some(enrolled_maker.trim()) {
        KeyAfterFailedSave::KeepNew
    } else {
        KeyAfterFailedSave::RestorePrevious
    }
}

/// Write the new maker key and the config, then restart.
///
/// The key and `[rfq].maker_id` name one maker together. Enroll has already
/// happened by now, so if the config doesn't end up on disk the key file has
/// to match the config that did stay: see [`KeyAfterFailedSave`]. That covers
/// a key store that fails partway (the key file and its `stitch.env` pointer
/// are written before the hand-over to the bot user, which can still fail), a
/// save refused before writing, and a failed restart that rolled it back.
async fn switch_maker(
    state: &AppState,
    bot: &Bot,
    path: &Path,
    edited: &str,
    enrolled: &EnrollResponse,
    previous_maker: Option<String>,
    extra: Value,
) -> Result<Response, ApiError> {
    let dir = bot_dir(bot, path)?;
    let previous = setup::read_rfq_api_key(dir).ok();
    let undo = || {
        undo_key_switch(
            state,
            dir,
            previous_maker.as_deref(),
            enrolled,
            previous.as_deref(),
        )
    };
    if let Err(e) = store_maker_key(state, dir, &enrolled.api_key) {
        undo();
        return Err(e);
    }
    let saved = save_and_restart(state, bot, path, edited, 0, Some(extra)).await;
    if read_toml(path).ok().as_deref() != Some(edited) {
        undo();
    }
    saved
}

fn undo_key_switch(
    state: &AppState,
    dir: &Path,
    previous_maker: Option<&str>,
    enrolled: &EnrollResponse,
    previous_key: Option<&str>,
) {
    match key_after_failed_save(previous_maker, &enrolled.maker_id) {
        KeyAfterFailedSave::RestorePrevious => restore_maker_key(state, dir, previous_key),
        KeyAfterFailedSave::KeepNew => tracing::warn!(
            dir = %dir.display(),
            maker = %enrolled.maker_id,
            "the config wasn't saved; keeping the rotated maker key, since enroll revoked the old one"
        ),
    }
}

fn restore_maker_key(state: &AppState, dir: &Path, previous: Option<&str>) {
    let restored = match previous {
        Some(key) => store_maker_key(state, dir, key).map_err(|e| e.message),
        None => std::fs::remove_file(dir.join(setup::RFQ_API_KEY_FILE)).map_err(|e| e.to_string()),
    };
    if let Err(e) = restored {
        tracing::error!(
            dir = %dir.display(),
            "the config wasn't saved, and putting the previous maker key back failed: {e}"
        );
    }
}

const TAKER_OFF_NOTE: &str =
    " The taker leg is off: it fills from the bot's own wallet, not the vault.";

fn linked_message(
    outcome: EnrollOutcome,
    enrolled: &EnrollResponse,
    vault: Address,
    taker_was_on: bool,
) -> String {
    let vault = setup::short_addr(&vault.to_checksum(None));
    let note = if taker_was_on { TAKER_OFF_NOTE } else { "" };
    let (slug, env) = (&enrolled.maker_slug, &enrolled.environment);
    match outcome {
        EnrollOutcome::Live => format!(
            "Connected vault {vault}. The bot now quotes Swap from the vault's balances, as \
             {slug} ({env}).{note}"
        ),
        EnrollOutcome::Waiting => format!(
            "Registered {slug} ({env}) with vault {vault} as its funding wallet. Confirm your \
             email address to go live — that is the only step left.{note}"
        ),
        EnrollOutcome::Flagged => format!(
            "Registered {slug} ({env}) with vault {vault}, but Textile has blocked this maker — \
             you will not receive Swap quotes."
        ),
    }
}

fn unlinked_message(outcome: EnrollOutcome, enrolled: &EnrollResponse, bot: &str) -> String {
    let (slug, env) = (&enrolled.maker_slug, &enrolled.environment);
    let fund = "Fund that wallet and approve Permit2 on the Funds tab before it can quote; \
                until then a live start stops at the approval check.";
    match outcome {
        EnrollOutcome::Live => format!(
            "Disconnected the vault. {bot} trades from its own wallet again, as {slug} ({env}). \
             {fund}"
        ),
        EnrollOutcome::Waiting => format!(
            "Disconnected the vault and registered {slug} ({env}) for {bot}'s own wallet. \
             Confirm your email address to go live. {fund}"
        ),
        EnrollOutcome::Flagged => format!(
            "Disconnected the vault. Textile has blocked {slug} — you will not receive Swap \
             quotes."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    const SIGNER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const ADMIN: &str = "0x90F79bf6EB2c4f870365E785982E1f101E93b906";
    const USDT: &str = "0x55d398326f99059fF775485246999027B3197955";
    const CNGN: &str = "0xa8AEA66B361a8d53e8865c62D142167Af28Af058";
    const WBRL: &str = "0x00000000000000000000000000000000000000b1";

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn pool(label: &str, collateral: &str, debt: &str) -> PoolPair {
        PoolPair {
            label: label.to_string(),
            collateral: collateral.parse().ok(),
            collateral_decimals: 6,
            debt: debt.parse().ok(),
            debt_decimals: 18,
        }
    }

    fn bot() -> BotSide {
        BotSide {
            signer: addr(SIGNER),
            network: "BNB Smart Chain".to_string(),
            pools: vec![pool("cNGN / USDT", CNGN, USDT)],
        }
    }

    /// A healthy vault for [`bot`]: its pair, its signer, open, and funded.
    fn facts() -> Facts {
        Facts {
            chain: ChainFacts {
                has_code: Ok(true),
                settlement: Some(addr(USDT)),
                corridor: Some(addr(CNGN)),
                strategy_signer: Some(addr(SIGNER)),
                operator_admin: Some(addr(ADMIN)),
                yield_adapter: Some(Address::ZERO),
                paused: Some(false),
                close_only: Some(false),
                quotable_settlement: Some(U256::from(25_000_000_000_000_000_000u128)),
                quotable_corridor: Some(U256::ZERO),
            },
            textile: Textile::Answered(VaultCheckResponse {
                registered: true,
                risk_signer_ok: Some(true),
                wiring_ok: Some(true),
                strategy_signer_matches: Some(true),
                order_executor: None,
                issues: vec![],
            }),
            symbols: HashMap::from([
                (addr(USDT), "USDT".to_string()),
                (addr(CNGN), "cNGN".to_string()),
            ]),
        }
    }

    fn rows_for(facts: &Facts) -> Vec<Check> {
        let (_, rows) = precheck(VAULT, addr(SIGNER)).unwrap();
        assess_facts(&bot(), facts, rows)
    }

    fn status_of<'a>(rows: &'a [Check], id: &str) -> &'a Check {
        rows.iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no {id} row in {rows:#?}"))
    }

    #[test]
    fn a_healthy_vault_passes_every_row() {
        let rows = rows_for(&facts());
        assert!(!failed(&rows), "{rows:#?}");
        let ids: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "address",
                "not-signer",
                "contract",
                "operator-vault",
                "pair",
                "strategy-signer",
                "textile-registered",
                "textile-risk-signer",
                "textile-wiring",
                "paused",
                "close-only",
                "inventory",
            ]
        );
        assert_eq!(
            status_of(&rows, "inventory").detail,
            "25 USDT and 0 cNGN quotable."
        );
    }

    #[test]
    fn the_address_has_to_be_one() {
        for (raw, says) in [
            ("", "Enter the vault's address"),
            ("0x1234", "isn't an address"),
            (
                "70997970C51812dc3A010C7d01b50e0d17dc79C8",
                "isn't an address",
            ),
            ("0x0000000000000000000000000000000000000000", "zero address"),
            // One character of a checksummed address flipped in case.
            ("0x70997970c51812dc3A010C7d01b50e0d17dc79C8", "checksum"),
        ] {
            let rows = precheck(raw, addr(SIGNER)).unwrap_err();
            assert_eq!(rows[0].status, Status::Fail, "{raw}");
            assert!(rows[0].detail.contains(says), "{raw}: {}", rows[0].detail);
            assert!(
                rows[1..].iter().all(|c| c.status == Status::Skipped),
                "nothing else can run: {rows:#?}"
            );
            assert_eq!(rows.len(), ROWS.len());
        }
        // All-lowercase carries no checksum to check.
        assert!(precheck(&VAULT.to_lowercase(), addr(SIGNER)).is_ok());
    }

    #[test]
    fn the_bots_own_wallet_is_not_a_vault() {
        let rows = precheck(SIGNER, addr(SIGNER)).unwrap_err();
        assert_eq!(status_of(&rows, "not-signer").status, Status::Fail);
        assert!(status_of(&rows, "contract").status == Status::Skipped);
    }

    #[test]
    fn no_contract_stops_the_checklist() {
        let mut f = facts();
        f.chain.has_code = Ok(false);
        let rows = rows_for(&f);
        let contract = status_of(&rows, "contract");
        assert_eq!(contract.status, Status::Fail);
        assert!(contract.detail.contains("BNB Smart Chain"), "{contract:?}");
        assert!(rows
            .iter()
            .skip_while(|c| c.id != "operator-vault")
            .all(|c| c.status == Status::Skipped));
    }

    #[test]
    fn an_unreadable_chain_is_a_failure_not_a_pass() {
        let mut f = facts();
        f.chain.has_code = Err("connection refused".to_string());
        let rows = rows_for(&f);
        assert!(status_of(&rows, "contract")
            .detail
            .contains("connection refused"));
        assert!(failed(&rows));
    }

    #[test]
    fn a_contract_without_the_vault_views_is_not_a_vault() {
        let mut f = facts();
        f.chain.settlement = None;
        let rows = rows_for(&f);
        let views = status_of(&rows, "operator-vault");
        assert_eq!(views.status, Status::Fail);
        assert!(views.detail.contains("isn't an OperatorVault"));
        assert!(!identity_ok(&bot(), &f), "the venue is not asked");
        assert_eq!(status_of(&rows, "textile").status, Status::Skipped);
    }

    #[test]
    fn a_view_answering_a_non_address_word_is_no_address() {
        assert_eq!(as_address(U256::MAX), None);
        assert_eq!(
            as_address(U256::from_be_slice(addr(USDT).as_slice())),
            Some(addr(USDT))
        );
        assert_eq!(as_bool(U256::from(1u8)), Some(true));
        assert_eq!(as_bool(U256::ZERO), Some(false));
        assert_eq!(as_bool(U256::from(2u8)), None);
    }

    #[test]
    fn the_pair_matches_either_way_round() {
        let mut b = bot();
        b.pools = vec![pool("USDT / cNGN", USDT, CNGN)];
        let rows = assess_facts(&b, &facts(), precheck(VAULT, addr(SIGNER)).unwrap().1);
        assert_eq!(status_of(&rows, "pair").status, Status::Ok);
    }

    #[test]
    fn a_bot_on_another_pair_cannot_use_the_vault() {
        let mut b = bot();
        b.pools = vec![pool("wBRL / USDT", WBRL, USDT)];
        let rows = assess_facts(&b, &facts(), precheck(VAULT, addr(SIGNER)).unwrap().1);
        let pair = status_of(&rows, "pair");
        assert_eq!(pair.status, Status::Fail);
        assert!(
            pair.detail.contains("This bot quotes wBRL / USDT"),
            "{pair:?}"
        );
        assert!(pair.detail.contains("USDT ↔ cNGN"), "{pair:?}");
    }

    #[test]
    fn a_multi_pool_bot_names_the_pool_that_doesnt_fit() {
        let mut b = bot();
        b.pools.push(pool("wBRL / USDT", WBRL, USDT));
        let rows = assess_facts(&b, &facts(), precheck(VAULT, addr(SIGNER)).unwrap().1);
        let pair = status_of(&rows, "pair");
        assert_eq!(pair.status, Status::Fail);
        assert!(
            pair.detail.contains("Not this vault's pair: wBRL / USDT"),
            "{pair:?}"
        );
        assert!(!pair.detail.contains("cNGN / USDT"), "{pair:?}");
    }

    #[test]
    fn a_foreign_strategy_signer_names_who_can_fix_it() {
        let mut f = facts();
        f.chain.strategy_signer = Some(addr(WBRL));
        let rows = rows_for(&f);
        let signer = status_of(&rows, "strategy-signer");
        assert_eq!(signer.status, Status::Fail);
        assert!(signer.detail.contains(SIGNER), "{signer:?}");
        assert!(signer.detail.contains(ADMIN), "{signer:?}");
        assert!(signer.detail.contains("setStrategySigner"), "{signer:?}");
        // The rest still runs: the operator sees every problem at once.
        assert_eq!(status_of(&rows, "paused").status, Status::Ok);
    }

    #[test]
    fn textiles_issues_are_failing_rows_in_its_words() {
        let mut f = facts();
        f.textile = Textile::Answered(VaultCheckResponse {
            registered: false,
            risk_signer_ok: Some(true),
            wiring_ok: Some(true),
            issues: vec!["0x… isn't a vault from Textile's OperatorVaultFactory.".to_string()],
            ..Default::default()
        });
        let rows = rows_for(&f);
        assert!(failed(&rows));
        let issue = status_of(&rows, "textile-issue-0");
        assert_eq!(issue.status, Status::Fail);
        assert!(issue.detail.contains("OperatorVaultFactory"));
        assert!(
            !rows.iter().any(|c| c.id == "textile-registered"),
            "the issue already says it; no second row for the same thing"
        );
        assert_eq!(status_of(&rows, "textile-wiring").status, Status::Ok);
    }

    #[test]
    fn silence_from_textile_is_not_a_yes() {
        let mut f = facts();
        f.textile = Textile::Answered(VaultCheckResponse {
            registered: true,
            risk_signer_ok: None,
            wiring_ok: Some(true),
            ..Default::default()
        });
        let rows = rows_for(&f);
        assert_eq!(status_of(&rows, "textile-risk-signer").status, Status::Fail);
    }

    #[test]
    fn an_older_api_skips_the_registry_without_blocking() {
        let mut f = facts();
        f.textile = Textile::Unavailable;
        let rows = rows_for(&f);
        let textile = status_of(&rows, "textile");
        assert_eq!(textile.status, Status::Skipped);
        assert!(textile.detail.contains("checked when you connect"));
        assert!(!failed(&rows));
        assert_eq!(order_executor(&f.textile), None);
    }

    #[test]
    fn an_older_api_blocks_a_vault_that_stakes() {
        // Without the vault check there's no way to learn the executor a
        // yield vault's settlement-side fills need, so connecting would leave
        // those quotes refused. Same when the adapter couldn't be read.
        for adapter in [
            Some(
                "0x00000000000000000000000000000000000000ad"
                    .parse()
                    .unwrap(),
            ),
            None,
        ] {
            let mut f = facts();
            f.chain.yield_adapter = adapter;
            f.textile = Textile::Unavailable;
            let rows = rows_for(&f);
            let textile = status_of(&rows, "textile");
            assert_eq!(textile.status, Status::Fail, "{adapter:?}");
            assert!(
                textile.detail.contains("order executor"),
                "{}",
                textile.detail
            );
        }
    }

    #[test]
    fn a_failed_save_keeps_the_rotated_key_only_on_the_same_maker() {
        // Reconnecting the vault the bot already has: enroll revoked the old
        // key, so restoring it would leave the bot unable to authenticate.
        assert_eq!(
            key_after_failed_save(Some("mk_vault"), "mk_vault"),
            KeyAfterFailedSave::KeepNew
        );
        // Moving onto (or off) a vault: the old maker's key is still live and
        // the unchanged config still names that maker.
        assert_eq!(
            key_after_failed_save(Some("mk_wallet"), "mk_vault"),
            KeyAfterFailedSave::RestorePrevious
        );
        assert_eq!(
            key_after_failed_save(None, "mk_vault"),
            KeyAfterFailedSave::RestorePrevious
        );
    }

    #[test]
    fn a_textile_that_errors_blocks() {
        let mut f = facts();
        f.textile = Textile::Failed("Too many vault checks".to_string());
        let rows = rows_for(&f);
        assert_eq!(status_of(&rows, "textile").status, Status::Fail);
    }

    #[test]
    fn the_executor_comes_from_textile() {
        let mut f = facts();
        let executor = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
        f.textile = Textile::Answered(VaultCheckResponse {
            registered: true,
            risk_signer_ok: Some(true),
            wiring_ok: Some(true),
            order_executor: Some(executor.to_string()),
            ..Default::default()
        });
        assert_eq!(order_executor(&f.textile), Some(addr(executor)));
        let summary = summarize(addr(VAULT), 56, &f);
        assert_eq!(summary.order_executor.as_deref(), Some(executor));
        assert!(summary.textile_checked);

        f.textile = Textile::Answered(VaultCheckResponse {
            registered: true,
            risk_signer_ok: Some(true),
            wiring_ok: Some(true),
            order_executor: Some("nope".to_string()),
            ..Default::default()
        });
        assert_eq!(
            status_of(&rows_for(&f), "textile-executor").status,
            Status::Fail
        );
    }

    #[test]
    fn paused_close_only_and_empty_are_warnings() {
        let mut f = facts();
        f.chain.paused = Some(true);
        f.chain.close_only = Some(true);
        f.chain.quotable_settlement = Some(U256::ZERO);
        let rows = rows_for(&f);
        assert_eq!(status_of(&rows, "paused").status, Status::Warn);
        let close = status_of(&rows, "close-only");
        assert_eq!(close.status, Status::Warn);
        assert!(close.detail.contains("won't spend its USDT"), "{close:?}");
        let inventory = status_of(&rows, "inventory");
        assert_eq!(inventory.status, Status::Warn);
        assert!(inventory.detail.contains("Fund the vault"));
        assert!(!failed(&rows), "warnings never block");
    }

    #[test]
    fn unread_flags_are_skipped_not_assumed() {
        let mut f = facts();
        f.chain.paused = None;
        f.chain.quotable_corridor = None;
        let rows = rows_for(&f);
        assert_eq!(status_of(&rows, "paused").status, Status::Skipped);
        assert_eq!(status_of(&rows, "inventory").status, Status::Skipped);
    }

    #[test]
    fn the_summary_reports_yield_and_assets() {
        let mut f = facts();
        f.chain.yield_adapter = Some(addr(WBRL));
        let summary = summarize(addr(VAULT), 56, &f);
        assert_eq!(summary.vault, VAULT);
        assert_eq!(summary.settlement_symbol.as_deref(), Some("USDT"));
        assert_eq!(summary.corridor_symbol.as_deref(), Some("cNGN"));
        assert_eq!(summary.yield_enabled, Some(true));
        assert_eq!(
            summary.explorer_url.as_deref(),
            Some(format!("https://bscscan.com/address/{VAULT}").as_str())
        );
    }

    /// The routes end to end: fake Docker, a mock chain, a mock venue.
    mod handlers {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        use axum::extract::Query;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use serde_json::{json, Value};

        use super::{ADMIN, CNGN, SIGNER, USDT, VAULT, WBRL};
        use crate::chain::mock_node::{mock_rpc, MockChain, MockNode};
        use crate::panel::docker::fake::{container, dir_layout_mounts, flat_layout_mounts, Call};
        use crate::panel::docker::ContainerState;
        use crate::panel::http::testkit::{harness, Harness, TEST_KEY};
        use crate::panel::naming::LABEL_BOT;
        use crate::setup;
        use alloy_primitives::U256;

        const EXECUTOR: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
        const VAULT_KEY: &str = "tx_live_vault_maker_key";
        const OWN_KEY: &str = "tx_live_own_wallet_key";

        /// Textile as the vault flow sees it: enroll, and the vault check when
        /// `check` is given (without it the route 404s, like an older API).
        struct Venue {
            origin: String,
            enrolls: Arc<Mutex<Vec<Value>>>,
            checks: Arc<AtomicUsize>,
            _server: tokio::task::JoinHandle<()>,
        }

        async fn venue(check: Option<Value>, enroll_status: u16, api_key: &'static str) -> Venue {
            let enrolls = Arc::new(Mutex::new(Vec::new()));
            let checks = Arc::new(AtomicUsize::new(0));
            let seen = enrolls.clone();
            let mut app = Router::new().route(
                "/v2/maker/enroll",
                post(move |Json(body): Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(body);
                        if enroll_status != 200 {
                            return (
                                StatusCode::from_u16(enroll_status).unwrap(),
                                Json(json!({ "error": {
                                    "code": "forbidden",
                                    "message": "fundingWallet is not a registered OperatorVault",
                                }})),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(json!({
                                "makerId": "clvaultmaker1",
                                "makerSlug": "stitch-56-vault",
                                "environment": "LIVE",
                                "apiKey": api_key,
                                "streamUrl": "wss://api.textilecredit.com/v2/maker/stream",
                                "validationContract": "0xBCA5E344077AaC751A1C548a45F28215bB7ec165",
                                "corridors": ["cngn-usdt-bsc"],
                                "corridorPairs": [],
                                "flagged": false,
                            })),
                        )
                    }
                }),
            );
            if let Some(answer) = check {
                let counter = checks.clone();
                app = app.route(
                    "/v2/maker/vault-check",
                    get(
                        move |Query(q): Query<std::collections::HashMap<String, String>>| {
                            let counter = counter.clone();
                            let answer = answer.clone();
                            async move {
                                counter.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(q.get("chainId").map(String::as_str), Some("56"));
                                assert_eq!(q.get("vault").map(String::as_str), Some(VAULT));
                                assert_eq!(
                                    q.get("signingAddress").map(String::as_str),
                                    Some(SIGNER)
                                );
                                Json(answer)
                            }
                        },
                    ),
                );
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            Venue {
                origin: format!("http://{addr}"),
                enrolls,
                checks,
                _server: server,
            }
        }

        fn accepted(executor: Option<&str>) -> Value {
            json!({
                "chainId": 56,
                "vault": VAULT,
                "registered": true,
                "settlementAsset": USDT,
                "corridorAsset": CNGN,
                "strategySigner": SIGNER,
                "strategySignerMatches": true,
                "riskSignerOk": true,
                "wiringOk": true,
                "paused": false,
                "closeOnly": false,
                "yieldAdapter": executor.map(|_| WBRL),
                "orderExecutor": executor,
                "issues": [],
            })
        }

        /// A vault on the BSC cNGN/USDT pair, signed for by `strategy`.
        fn vault_chain(strategy: &str, settlement: &str, corridor: &str) -> MockChain {
            MockChain::default()
                .with_code(VAULT, vec![0x60, 0x80, 0x60, 0x40])
                .view_address(VAULT, "settlementAsset()", settlement)
                .view_address(VAULT, "corridorAsset()", corridor)
                .view_address(VAULT, "strategySigner()", strategy)
                .view_address(VAULT, "operatorAdmin()", ADMIN)
                .view_address(
                    VAULT,
                    "yieldAdapter()",
                    "0x0000000000000000000000000000000000000000",
                )
                .view(VAULT, "paused()", U256::ZERO)
                .view(VAULT, "closeOnly()", U256::ZERO)
                .view(
                    VAULT,
                    "quotableSettlement()",
                    U256::from(25_000_000_000_000_000_000u128),
                )
                .view(VAULT, "quotableCorridor()", U256::ZERO)
        }

        async fn healthy_chain() -> MockNode {
            mock_rpc(vault_chain(SIGNER, USDT, CNGN)).await
        }

        /// A panel bot on BSC cNGN/USDT: ladder off, taker on, pointed at `rpc_url`.
        fn seed(h: &Harness, name: &str, rpc_url: &str, state: ContainerState) {
            let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
            let dir = h.root.join(name);
            setup::write_config(&dir, corridor, TEST_KEY).unwrap();
            let path = dir.join("stitch.toml");
            let toml = std::fs::read_to_string(&path)
                .unwrap()
                .replace("https://bsc-dataseed.binance.org", rpc_url);
            let toml = setup::apply_rfq_default_preset(&toml).unwrap();
            assert!(toml.contains("limit_taker_enabled = true"), "{toml}");
            std::fs::write(&path, toml).unwrap();
            let mut c = container(&format!("stitch-{name}"), state);
            c.labels.insert(LABEL_BOT.to_string(), name.to_string());
            c.mounts = dir_layout_mounts(&dir.display().to_string());
            h.docker.add_container(c);
        }

        fn config(h: &Harness, name: &str) -> String {
            std::fs::read_to_string(h.root.join(name).join("stitch.toml")).unwrap()
        }

        fn key(h: &Harness, name: &str) -> Option<String> {
            setup::read_rfq_api_key(h.root.join(name)).ok()
        }

        fn restarted(h: &Harness, name: &str) -> bool {
            let container = format!("stitch-{name}");
            h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Restart { name, .. } if *name == container))
        }

        fn check_row<'a>(body: &'a Value, id: &str) -> &'a Value {
            body["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == id)
                .unwrap_or_else(|| panic!("no {id} row: {body}"))
        }

        #[tokio::test]
        async fn a_dry_run_reports_and_writes_nothing() {
            let h = harness("vault-check-dry");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(Some(EXECUTOR))), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault/check",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v = Harness::parse(&body);
            assert_eq!(v["ok"], true, "{body}");
            assert_eq!(v["summary"]["vault"], VAULT);
            assert_eq!(v["summary"]["orderExecutor"], EXECUTOR);
            assert_eq!(v["summary"]["settlementSymbol"], "USDT");
            assert_eq!(v["summary"]["textileChecked"], true);
            assert_eq!(check_row(&v, "strategy-signer")["status"], "ok");

            assert_eq!(config(&h, "bot-a"), before, "a dry run writes nothing");
            assert_eq!(key(&h, "bot-a"), None);
            assert!(
                venue.enrolls.lock().unwrap().is_empty(),
                "and enrolls nothing"
            );
            assert!(!restarted(&h, "bot-a"));
        }

        #[tokio::test]
        async fn connecting_writes_the_whole_vault_setup_and_restarts() {
            let h = harness("vault-link");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let venue = venue(Some(accepted(Some(EXECUTOR))), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT.to_lowercase(), "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(!body.contains(VAULT_KEY), "the key never goes on the wire");
            let v = Harness::parse(&body);
            let message = v["message"].as_str().unwrap();
            assert!(message.contains("Connected vault"), "{body}");
            assert!(message.contains("taker leg is off"), "{body}");
            assert_eq!(v["enrollment"]["makerSlug"], "stitch-56-vault");
            assert_eq!(v["settings"]["vaultAddress"], VAULT);
            assert_eq!(v["settings"]["takerEnabled"], false);
            assert_eq!(v["settings"]["rfqEnabled"], true);
            assert!(v["checks"].as_array().is_some_and(|c| !c.is_empty()));

            // Enroll named the vault as the funding wallet, signed by the bot.
            let enrolls = venue.enrolls.lock().unwrap().clone();
            assert_eq!(enrolls.len(), 1);
            assert_eq!(enrolls[0]["fundingWallet"], VAULT);
            assert_eq!(
                enrolls[0]["signingAddress"]
                    .as_str()
                    .unwrap()
                    .to_lowercase(),
                SIGNER.to_lowercase()
            );

            let cfg = crate::config::Config::from_toml(&config(&h, "bot-a")).unwrap();
            let vault = cfg.vault.as_ref().unwrap();
            assert_eq!(vault.address, VAULT, "checksummed");
            assert_eq!(vault.order_executor.as_deref(), Some(EXECUTOR));
            assert!(!cfg.book_enabled);
            assert!(cfg.pools.iter().all(|p| !p.limit_taker_enabled()));
            assert_eq!(cfg.rfq.as_ref().unwrap().maker_id, "clvaultmaker1");
            assert_eq!(key(&h, "bot-a").as_deref(), Some(VAULT_KEY));
            assert!(restarted(&h, "bot-a"));
        }

        #[tokio::test]
        async fn an_address_with_no_contract_writes_nothing() {
            let h = harness("vault-link-eoa");
            let node = mock_rpc(MockChain::default()).await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(None)), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let v = Harness::parse(&body);
            assert!(
                v["error"].as_str().unwrap().contains("No contract"),
                "{body}"
            );
            assert_eq!(check_row(&v, "contract")["status"], "fail");
            assert_eq!(check_row(&v, "textile")["status"], "skipped");
            assert_eq!(
                venue.checks.load(Ordering::SeqCst),
                0,
                "Textile isn't asked"
            );
            assert!(venue.enrolls.lock().unwrap().is_empty());
            assert_eq!(config(&h, "bot-a"), before);
            assert_eq!(key(&h, "bot-a"), None);
            assert!(!restarted(&h, "bot-a"));
        }

        #[tokio::test]
        async fn a_vault_another_key_signs_for_writes_nothing() {
            let h = harness("vault-link-signer");
            let node = mock_rpc(vault_chain(ADMIN, USDT, CNGN)).await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(None)), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let v = Harness::parse(&body);
            let row = check_row(&v, "strategy-signer");
            assert_eq!(row["status"], "fail");
            assert!(row["detail"]
                .as_str()
                .unwrap()
                .contains("setStrategySigner"));
            assert!(venue.enrolls.lock().unwrap().is_empty());
            assert_eq!(config(&h, "bot-a"), before);
            assert_eq!(key(&h, "bot-a"), None);
        }

        #[tokio::test]
        async fn a_vault_on_another_pair_writes_nothing() {
            let h = harness("vault-link-pair");
            let node = mock_rpc(vault_chain(SIGNER, USDT, WBRL)).await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(None)), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let v = Harness::parse(&body);
            assert_eq!(check_row(&v, "pair")["status"], "fail");
            assert!(venue.enrolls.lock().unwrap().is_empty());
            assert_eq!(config(&h, "bot-a"), before);
        }

        #[tokio::test]
        async fn a_refused_enroll_writes_nothing() {
            let h = harness("vault-link-enroll-refused");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(None)), 403, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("not a registered OperatorVault"), "{body}");
            assert!(body.contains("Nothing was changed"), "{body}");
            assert_eq!(config(&h, "bot-a"), before);
            assert_eq!(key(&h, "bot-a"), None);
            assert!(!restarted(&h, "bot-a"));
        }

        #[tokio::test]
        async fn an_api_without_the_check_still_connects_without_an_executor() {
            let h = harness("vault-link-old-api");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let venue = venue(None, 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v = Harness::parse(&body);
            assert_eq!(check_row(&v, "textile")["status"], "skipped");
            assert_eq!(v["summary"]["textileChecked"], false);
            let cfg = crate::config::Config::from_toml(&config(&h, "bot-a")).unwrap();
            assert!(cfg.vault.unwrap().order_executor.is_none());
            assert_eq!(key(&h, "bot-a").as_deref(), Some(VAULT_KEY));
        }

        #[tokio::test]
        async fn a_save_that_cant_land_puts_the_old_key_back() {
            // Paused, with the taker on: turning the taker off is a change the
            // panel won't persist without a clean restart, so the save is refused
            // after enroll has already handed over a new key.
            let h = harness("vault-link-paused");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Paused);
            setup::write_rfq_api_key(h.root.join("bot-a"), OWN_KEY).unwrap();
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(None)), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(config(&h, "bot-a"), before);
            assert_eq!(
                key(&h, "bot-a").as_deref(),
                Some(OWN_KEY),
                "the key goes with the config it was written for"
            );
        }

        #[tokio::test]
        async fn a_key_store_that_fails_partway_puts_the_old_key_back() {
            // Storing a key writes the key file first, then points
            // `stitch.env` at it, then hands both to the bot user. An env file
            // that can't be read fails the second step with the new key
            // already written.
            let h = harness("vault-link-key-store");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let dir = h.root.join("bot-a");
            setup::write_rfq_api_key(&dir, OWN_KEY).unwrap();
            std::fs::remove_file(dir.join("stitch.env")).unwrap();
            std::fs::create_dir(dir.join("stitch.env")).unwrap();
            let before = config(&h, "bot-a");
            let venue = venue(Some(accepted(None)), 200, VAULT_KEY).await;

            let (status, body) = h
                .post_json(
                    "/api/bots/bot-a/vault",
                    json!({ "address": VAULT, "venueUrl": venue.origin }),
                )
                .await;
            assert!(!status.is_success(), "{status} {body}");
            assert_eq!(venue.enrolls.lock().unwrap().len(), 1, "{body}");
            assert_eq!(config(&h, "bot-a"), before);
            assert_eq!(
                key(&h, "bot-a").as_deref(),
                Some(OWN_KEY),
                "the key goes with the config it was written for"
            );
            assert!(!restarted(&h, "bot-a"));
        }

        #[tokio::test]
        async fn disconnecting_goes_back_to_the_bots_own_wallet() {
            let h = harness("vault-unlink");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let path = h.root.join("bot-a").join("stitch.toml");
            let linked =
                setup::link_vault(&config(&h, "bot-a"), VAULT.parse().unwrap(), None).unwrap();
            std::fs::write(&path, linked).unwrap();
            setup::write_rfq_api_key(h.root.join("bot-a"), VAULT_KEY).unwrap();
            let venue = venue(None, 200, OWN_KEY).await;

            let (status, body) = h
                .delete(&format!("/api/bots/bot-a/vault?venueUrl={}", venue.origin))
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let v = Harness::parse(&body);
            let message = v["message"].as_str().unwrap();
            assert!(message.contains("own wallet"), "{body}");
            assert!(message.contains("Funds tab"), "{body}");
            assert_eq!(v["settings"]["vaultAddress"], "");

            let enrolls = venue.enrolls.lock().unwrap().clone();
            assert_eq!(enrolls.len(), 1);
            assert!(
                enrolls[0].get("fundingWallet").is_none(),
                "the signer is its own funding wallet: {}",
                enrolls[0]
            );
            assert!(!config(&h, "bot-a").contains("[vault]"));
            assert_eq!(key(&h, "bot-a").as_deref(), Some(OWN_KEY));
            assert!(restarted(&h, "bot-a"));
        }

        #[tokio::test]
        async fn disconnecting_a_bot_with_no_vault_is_refused() {
            let h = harness("vault-unlink-none");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Running);
            let (status, body) = h.delete("/api/bots/bot-a/vault").await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(
                body.contains("already trades from its own wallet"),
                "{body}"
            );
        }

        #[tokio::test]
        async fn a_settings_save_cannot_move_the_vault() {
            let h = harness("vault-patch");
            let node = healthy_chain().await;
            seed(&h, "bot-a", &node.url, ContainerState::Exited);
            let before = config(&h, "bot-a");

            let (status, body) = h
                .patch_json("/api/bots/bot-a/settings", json!({ "vaultAddress": VAULT }))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("can't change the vault"), "{body}");
            assert_eq!(config(&h, "bot-a"), before);

            // Sending back what is already there is an ordinary save.
            let (status, body) = h
                .patch_json("/api/bots/bot-a/settings", json!({ "vaultAddress": "" }))
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }

        #[tokio::test]
        async fn a_flat_layout_docker_bot_is_told_to_migrate() {
            let h = harness("vault-flat");
            let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
            std::fs::write(h.root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
            std::fs::write(h.root.join("stitch.bot1.key"), TEST_KEY).unwrap();
            let mut c = container("stitch-bot1", ContainerState::Running);
            c.labels.insert(LABEL_BOT.to_string(), "bot1".to_string());
            c.mounts = flat_layout_mounts(&h.root.display().to_string(), "bot1");
            h.docker.add_container(c);

            let (status, body) = h
                .post_json("/api/bots/bot1/vault", json!({ "address": VAULT }))
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(body.contains("Migrate"), "{body}");
        }
    }
}
