// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The bot's ERC-8004 agent identity, under Tools. See [`crate::chain::erc8004`].
//!
//! `GET` reads everything the card needs and decides whether Register is on.
//! The mint itself runs one of two ways, the same split approvals have:
//!
//! - `POST …/identity/register` runs `stitch register` as a one-shot with the
//!   bot's own key, streamed like approve. The config is mounted read-only, so
//!   the run prints the id and the browser hands it to `…/identity/link`.
//! - `POST …/identity/register/custody` is for a Fireblocks signer without raw
//!   signing. Fireblocks sends the mint as a CONTRACT_CALL; the calldata is
//!   built here and the destination is the chain's registry, nothing else.
//!   The call can wait on a human approver for longer than any request lasts,
//!   so a [`PendingMint`] is written before it is sent, carrying the
//!   `externalTxId` it is sent with. Records live in one panel-wide store,
//!   [`PENDING_DIR`] under the bots root, one file per chain and wallet. While
//!   one exists for a wallet, whichever bot started it and whatever that bot
//!   is configured as now, the route checks on that call instead of sending
//!   another and nothing else mints for the wallet. The external id is what
//!   finds the call again when the response that named it was lost.
//!
//! `POST …/identity/link` writes `[erc8004] agent_id`, and only after `ownerOf`
//! says the bot's wallet owns that token, so the id can come from the browser,
//! a one-shot's output or the operator's own paste without being trusted.

use alloy_primitives::{Address, U256};
use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::settings::{config_path, read_toml};
use super::{ApiError, AppState};
use crate::chain::erc8004::{self, Profile, Registry};
use crate::chain::rpc::Rpc;
use crate::config::Config;
use crate::panel::auth::Identity;
use crate::panel::inventory::{Bot, Fleet, Layout};
use crate::panel::provision::OneShot;
use crate::signer::fireblocks::{CallState, FireblocksSigner, CONTRACT_CALL_TIMEOUT};
use crate::signer::SignerConfig;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IdentityBody {
    pub chain_id: u64,
    pub network_label: Option<String>,
    /// The Identity Registry on this chain. `null` where ERC-8004 isn't deployed.
    pub registry: Option<String>,
    /// The operator wallet: who mints, owns, and is the agent's wallet.
    pub owner: Option<String>,
    /// Decimal. `null` until `[erc8004]` names one.
    pub agent_id: Option<String>,
    /// What the registry stores for the agent now, read back from its URI.
    /// `null` before registering, or for a URI that isn't a data URI.
    pub stored: Option<Profile>,
    /// Who owns the agent when it isn't the operator wallet any more (the
    /// token was transferred). The id in the config then names someone else's.
    pub owned_by: Option<String>,
    /// The generated text the form starts from. `null` once registered.
    pub defaults: Option<Profile>,
    pub limits: Limits,
    /// The wallet owns an identity the config doesn't name. Registering would
    /// mint a second one, so the card offers to link instead.
    pub owned_elsewhere: bool,
    /// A bot on the same chain and wallet that has recorded an agent id.
    pub sibling: Option<Sibling>,
    pub links: Links,
    pub gas: Option<GasBody>,
    /// Fireblocks without raw signing: register through the custody route.
    pub via_custody: bool,
    /// A Fireblocks registration was submitted and hasn't been seen to land.
    /// The card checks on it instead of offering a second one.
    pub pending: Option<PendingBody>,
    pub can_register: bool,
    pub blocked_reason: Option<String>,
    /// `transacting`, `gas` or `signer`.
    pub blocked_kind: Option<&'static str>,
    /// The live bot to stop, for `transacting`.
    pub blocked_by: Option<String>,
    /// First chain read that failed. The card still renders; Register stays
    /// off while the wallet's identities couldn't be counted.
    pub read_error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub name: usize,
    pub description: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sibling {
    pub bot: String,
    pub agent_id: String,
}

#[derive(Debug, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Links {
    pub agent: Option<String>,
    pub registry: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingBody {
    pub message: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GasBody {
    pub estimate: String,
    pub balance: String,
    pub symbol: &'static str,
    pub enough: bool,
}

/// What the chain said, gathered before any decision is made.
#[derive(Debug, Default)]
struct Reads {
    /// Identities the operator wallet holds.
    owned: Option<U256>,
    /// For a registered bot: the token's owner and URI.
    agent_owner: Option<Address>,
    agent_uri: Option<String>,
    /// For an unregistered one: the mint's gas, the gas price, the balance.
    gas_units: Option<U256>,
    gas_price: Option<U256>,
    balance: Option<U256>,
    error: Option<String>,
}

/// Facts about the bot that come from the panel, not the chain.
struct Facts<'a> {
    chain_id: u64,
    network_label: Option<String>,
    registry: Option<&'a Registry>,
    owner: Option<Address>,
    agent_id: Option<u64>,
    defaults: Profile,
    sibling: Option<Sibling>,
    via_custody: bool,
    /// What to say about a Fireblocks registration that may still land.
    pending: Option<String>,
    /// Why this signer can't send a transaction at all, when it can't and
    /// there's no custody route either.
    signer_blocked: Option<String>,
    /// The nonce check, as approvals run it.
    wallet_blocked: Option<(String, Option<String>)>,
}

/// Leave a fee bump of headroom: a mint that sits at the first price gets
/// re-sent higher by `send_and_wait`, and a wallet with exactly the estimate
/// can't pay for that.
fn gas_needed(units: U256, price: U256) -> U256 {
    units * price * U256::from(3u8) / U256::from(2u8)
}

/// The card's state from the facts and the reads. No I/O.
fn assess(f: Facts<'_>, r: Reads) -> IdentityBody {
    let registered = f.agent_id.is_some();
    // Recording the id clears the file, so a registered bot has nothing pending.
    let pending = f.pending.filter(|_| !registered);
    let owned_elsewhere = !registered && r.owned.is_some_and(|n| !n.is_zero());
    let gas = match (f.registry, r.gas_units, r.gas_price, r.balance) {
        (Some(reg), Some(units), Some(price), Some(balance)) if !registered => Some(GasBody {
            estimate: erc8004::format_coin(units * price),
            balance: erc8004::format_coin(balance),
            symbol: reg.gas_coin,
            enough: balance >= gas_needed(units, price),
        }),
        _ => None,
    };
    let blocked = if f.registry.is_none() || registered || owned_elsewhere || pending.is_some() {
        None
    } else if let Some(why) = f.signer_blocked {
        Some(("signer", why, None))
    } else if let Some((why, by)) = f.wallet_blocked {
        Some(("transacting", why, by))
    } else if let (Some(g), Some(reg), Some(owner)) = (&gas, f.registry, f.owner) {
        (!g.enough).then(|| {
            (
                "gas",
                format!(
                    "The operator wallet has {} {coin}. Registering costs about {} {coin}, and \
                     the wallet needs a bit more than that in case the fee has to be raised. \
                     Send a little {coin} to {owner} first.",
                    g.balance,
                    g.estimate,
                    coin = reg.gas_coin
                ),
                None,
            )
        })
    } else {
        None
    };
    // Counting the wallet's identities is what keeps a second mint from going
    // out, so an unanswered count keeps Register off even with nothing blocking.
    let counted = r.owned.is_some();
    let can_register = f.registry.is_some()
        && f.owner.is_some()
        && !registered
        && !owned_elsewhere
        && counted
        && pending.is_none()
        && blocked.is_none();
    let (blocked_kind, blocked_reason, blocked_by) = match blocked {
        Some((kind, why, by)) => (Some(kind), Some(why), by),
        None => (None, None, None),
    };
    let owner = f.owner;
    IdentityBody {
        chain_id: f.chain_id,
        network_label: f
            .registry
            .map(|r| r.network.to_string())
            .or(f.network_label),
        registry: f.registry.map(|r| r.address.to_string()),
        owner: owner.map(|a| a.to_string()),
        agent_id: f.agent_id.map(|id| id.to_string()),
        stored: r.agent_uri.as_deref().and_then(erc8004::profile_from_uri),
        owned_by: r
            .agent_owner
            .filter(|actual| Some(*actual) != owner)
            .map(|a| a.to_string()),
        defaults: (!registered).then_some(f.defaults),
        limits: Limits {
            name: erc8004::NAME_MAX_CHARS,
            description: erc8004::DESCRIPTION_MAX_CHARS,
        },
        owned_elsewhere,
        sibling: f.sibling,
        links: Links {
            agent: f
                .registry
                .zip(f.agent_id)
                .map(|(reg, id)| erc8004::scan_url(reg, id)),
            registry: f.registry.and_then(|reg| {
                crate::setup::address_explorer_url(reg.chain_id, &reg.address.to_string())
            }),
        },
        gas,
        via_custody: f.via_custody,
        pending: pending.map(|message| PendingBody { message }),
        can_register,
        blocked_reason,
        blocked_kind,
        blocked_by,
        read_error: r.error,
    }
}

/// Run the reads one card needs. Failures land in `error`, first one wins.
async fn read_chain(
    cfg: &Config,
    registry: &Registry,
    owner: Address,
    agent_id: Option<u64>,
    defaults: &Profile,
) -> Reads {
    let rpc = Rpc::new(cfg.rpc_url.clone());
    let mut reads = Reads::default();
    let mut note = |e: anyhow::Error, what: &str| {
        reads_error(&mut reads.error, format!("couldn't read {what}: {e:#}"));
    };
    match agent_id {
        Some(id) => {
            let (agent_owner, uri) = tokio::join!(
                erc8004::owner_of(&rpc, registry.address, id),
                erc8004::token_uri(&rpc, registry.address, id),
            );
            match agent_owner {
                Ok(a) => reads.agent_owner = Some(a),
                Err(e) => note(e, "the agent's owner"),
            }
            match uri {
                Ok(u) => reads.agent_uri = Some(u),
                Err(e) => note(e, "the agent's stored file"),
            }
        }
        None => {
            // The estimate is for the default text; an edit changes the size by
            // a few slots at most, which is why the card says "about".
            let calldata = erc8004::plan_register(cfg, owner, defaults.clone())
                .map(|p| p.calldata)
                .unwrap_or_default();
            let (owned, units, price, balance) = tokio::join!(
                erc8004::owned_count(&rpc, registry.address, owner),
                rpc.estimate_gas(owner, registry.address, &calldata, U256::ZERO),
                rpc.gas_price(),
                rpc.get_balance(owner),
            );
            match owned {
                Ok(n) => reads.owned = Some(n),
                Err(e) => note(e, "the wallet's identities"),
            }
            match units {
                Ok(n) => reads.gas_units = Some(n),
                Err(e) => note(e, "the mint's gas"),
            }
            match price {
                Ok(n) => reads.gas_price = Some(n),
                Err(e) => note(e, "the gas price"),
            }
            match balance {
                Ok(n) => reads.balance = Some(n),
                Err(e) => note(e, "the wallet's balance"),
            }
        }
    }
    reads
}

fn reads_error(slot: &mut Option<String>, message: String) {
    slot.get_or_insert(message);
}

/// The bot's own name for itself: its panel name, else its id.
fn bot_label(bot: &Bot) -> String {
    (bot.layout != Layout::FlatFiles)
        .then(|| bot.config_dir())
        .flatten()
        .and_then(|dir| crate::panel::label::read_display_name(&dir))
        .unwrap_or_else(|| bot.name.clone())
}

/// The first other bot on the same chain and wallet that records an agent id.
fn sibling_with_identity(bot: &Bot, fleet: &Fleet) -> Option<Sibling> {
    let wallet = bot.wallet()?;
    fleet
        .bots()
        .iter()
        .filter(|other| other.name != bot.name && other.wallet().as_ref() == Some(&wallet))
        .find_map(|other| {
            Some(Sibling {
                bot: other.name.clone(),
                agent_id: other.config.as_ref()?.erc8004_agent_id?.to_string(),
            })
        })
}

fn read_config(path: &std::path::Path) -> Result<Config, ApiError> {
    Config::from_toml(&read_toml(path)?).map_err(ApiError::bad_request)
}

fn owner_of_bot(bot: &Bot) -> Option<Address> {
    bot.config.as_ref()?.operator_address.as_ref()?.parse().ok()
}

/// Everything the card shows for `name`, read fresh.
async fn identity_for(state: &AppState, name: &str) -> Result<IdentityBody, ApiError> {
    let (bot, fleet) = state.bot_and_fleet(name).await?;
    let path = config_path(&bot)?;
    let cfg = read_config(&path)?;
    let summary = bot.config.as_ref();
    let registry = erc8004::registry_for_chain(cfg.chain_id);
    let owner = owner_of_bot(&bot);
    let agent_id = erc8004::configured_agent_id(&cfg);
    let network = registry
        .map(|r| r.network.to_string())
        .or_else(|| summary.and_then(|s| s.network_label.clone()))
        .unwrap_or_else(|| format!("chain {}", cfg.chain_id));
    let defaults =
        erc8004::default_profile(&bot_label(&bot), &erc8004::pair_labels(&cfg), &network);
    let via_custody = summary.is_some_and(|s| s.custody_approvals);
    let pending = match mint_key(&bot) {
        Some(key) => read_pending(&pending_store(state), key)?.map(|p| p.describe(&bot.name)),
        None => None,
    };
    let signer_blocked = super::logs::register_signer_check(&bot)
        .err()
        .filter(|_| !via_custody)
        .map(|e| format!("{e:#}"));
    let wallet_blocked = super::logs::approve_wallet_check(&bot, &fleet)
        .err()
        .map(|e| {
            (
                format!("{e:#}"),
                super::logs::approve_blocked_by(&bot, &fleet),
            )
        });
    let reads = match (registry, owner) {
        (Some(reg), Some(owner)) => read_chain(&cfg, reg, owner, agent_id, &defaults).await,
        (Some(_), None) => Reads {
            error: Some(
                "this bot has no operator address the panel can read, so it can't be registered"
                    .to_string(),
            ),
            ..Reads::default()
        },
        (None, _) => Reads::default(),
    };
    Ok(assess(
        Facts {
            chain_id: cfg.chain_id,
            network_label: summary.and_then(|s| s.network_label.clone()),
            registry,
            owner,
            agent_id,
            defaults,
            sibling: (agent_id.is_none())
                .then(|| sibling_with_identity(&bot, &fleet))
                .flatten(),
            via_custody,
            pending,
            signer_blocked,
            wallet_blocked,
        },
        reads,
    ))
}

/// `GET /api/bots/{name}/identity`
pub async fn show(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    Ok(Json(identity_for(&state, &name).await?).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterBody {
    pub name: String,
    pub description: String,
}

/// Refuse what the run would refuse without a container: a chain with no
/// registry, a config that already names an id, text that doesn't pass.
fn precheck(cfg: &Config, body: &RegisterBody) -> Result<Profile, ApiError> {
    let registry = erc8004::registry_for_chain(cfg.chain_id).ok_or_else(|| {
        ApiError::conflict(format!(
            "ERC-8004 has no Identity Registry on chain {}",
            cfg.chain_id
        ))
    })?;
    if let Some(id) = erc8004::configured_agent_id(cfg) {
        return Err(ApiError::conflict(format!(
            "this bot is already registered as agent #{id} on {}",
            registry.network
        )));
    }
    erc8004::profile_from_input(&body.name, &body.description)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))
}

/// `POST /api/bots/{name}/identity/register` — streams `stitch register`.
pub async fn register(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<RegisterBody>,
) -> Result<Response, ApiError> {
    let bot = state.bot(&name).await?;
    if bot.config.as_ref().is_some_and(|c| c.custody_approvals) {
        return Err(ApiError::conflict(format!(
            "{name} signs through Fireblocks without raw signing, so its registration goes out \
             as a Fireblocks Contract Call. Use the custody route."
        )));
    }
    let path = config_path(&bot)?;
    if let Some(key) = mint_key(&bot) {
        if let Some(pending) = read_pending(&pending_store(&state), key)? {
            return Err(ApiError::conflict(pending.describe(&bot.name)));
        }
    }
    let profile = precheck(&read_config(&path)?, &body)?;
    super::logs::one_shot(
        state,
        &name,
        OneShot::Register {
            name: profile.name,
            description: profile.description,
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkBody {
    /// Decimal token id. A string so a large id survives JSON numbers.
    pub agent_id: String,
}

/// `POST /api/bots/{name}/identity/link` — record an id the wallet owns.
pub async fn link(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Extension(identity): Extension<Identity>,
    Json(body): Json<LinkBody>,
) -> Result<Response, ApiError> {
    let id: u64 =
        body.agent_id.trim().parse().map_err(|_| {
            ApiError::bad_request(format!("{:?} is not an agent id", body.agent_id))
        })?;
    record(&state, &name, &identity, id).await?;
    Ok(Json(identity_for(&state, &name).await?).into_response())
}

/// Check `ownerOf` and write `[erc8004] agent_id`, under the config lock.
/// Writes without a restart: nothing the running bot does reads the id.
async fn record(
    state: &AppState,
    name: &str,
    identity: &Identity,
    id: u64,
) -> Result<(), ApiError> {
    let (_saving, bot) = super::bots::lock_config(name, state).await?;
    let path = config_path(&bot)?;
    let toml = read_toml(&path)?;
    let cfg = Config::from_toml(&toml).map_err(ApiError::bad_request)?;
    let registry = erc8004::registry_for_chain(cfg.chain_id).ok_or_else(|| {
        ApiError::conflict(format!(
            "ERC-8004 has no Identity Registry on chain {}",
            cfg.chain_id
        ))
    })?;
    let owner = owner_of_bot(&bot).ok_or_else(|| {
        ApiError::conflict(format!("{name} has no operator address the panel can read"))
    })?;
    confirm_ownership(&Rpc::new(cfg.rpc_url.clone()), registry, owner, id).await?;
    let next = crate::setup::set_erc8004_agent_id(&toml, id).map_err(ApiError::bad_request)?;
    let origin = super::changes::origin(identity, crate::changelog::Source::PanelIdentity, name);
    super::changes::write_config(state, &path, &next, &origin)?;
    // The wallet has its identity now, so a pending Fireblocks call on it has
    // nothing left to guard, whichever bot started it. Left in place it would
    // block every bot on the wallet.
    clear_pending(
        &pending_store(state),
        MintKey {
            chain_id: cfg.chain_id,
            wallet: owner,
        },
    )?;
    tracing::info!(bot = %name, agent_id = id, "recorded ERC-8004 agent id");
    Ok(())
}

/// `ensure_owns`, tolerating a node that hasn't seen the mint yet. A link
/// straight after a mint can reach a load-balanced RPC a block behind the one
/// that returned the receipt, where the token doesn't exist yet. A token that
/// exists and belongs to someone else is refused at once.
async fn confirm_ownership(
    rpc: &Rpc,
    registry: &Registry,
    owner: Address,
    id: u64,
) -> Result<(), ApiError> {
    // Up to three probes two seconds apart, then the check that decides.
    for _ in 0..3 {
        if erc8004::owner_of(rpc, registry.address, id).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    erc8004::ensure_owns(rpc, registry, owner, id)
        .await
        .map_err(|e| ApiError::conflict(format!("{e:#}")))
}

/// The panel-wide store of pending Fireblocks registrations, under the bots
/// root. Hidden, and holds no `stitch.toml`, so the fleet scan never reads it
/// as a bot.
pub const PENDING_DIR: &str = ".erc8004-pending";

/// What a pending registration is keyed on: the chain and wallet it mints for.
/// Not the bot. Siblings on the wallet would mint the same identity, and a bot
/// can be moved to another wallet while its call is still out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MintKey {
    chain_id: u64,
    wallet: Address,
}

/// The key `bot` would mint under now, from its config.
fn mint_key(bot: &Bot) -> Option<MintKey> {
    let config = bot.config.as_ref()?;
    Some(MintKey {
        chain_id: config.chain_id,
        wallet: config.operator_address.as_ref()?.parse().ok()?,
    })
}

fn pending_store(state: &AppState) -> std::path::PathBuf {
    state.cfg.bots_dir.join(PENDING_DIR)
}

/// One file per key. Built from the parsed address, so the name is always
/// `<chain>-0x<40 hex>.json` whatever spelling the config used.
fn pending_file(store: &std::path::Path, key: MintKey) -> std::path::PathBuf {
    store.join(format!("{}-{:#x}.json", key.chain_id, key.wallet))
}

/// A Fireblocks registration that may have been sent and hasn't been seen to
/// land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMint {
    /// Sent as Fireblocks' `externalTxId`. Fireblocks refuses a second
    /// transaction with it, and looks the first one up by it.
    pub external_tx_id: String,
    /// Fireblocks' own id, once its response named one.
    #[serde(default)]
    pub fireblocks_tx_id: Option<String>,
    /// The bot that started it, for the message other bots on the wallet show.
    pub started_by: String,
    pub submitted_at_unix: u64,
}

impl PendingMint {
    fn new(key: MintKey, started_by: &str) -> Self {
        Self {
            external_tx_id: format!(
                "stitch-erc8004-{}-{:#x}-{:016x}",
                key.chain_id,
                key.wallet,
                rand::random::<u64>()
            ),
            fireblocks_tx_id: None,
            started_by: started_by.to_string(),
            submitted_at_unix: crate::time::unix_now_ms() / 1000,
        }
    }

    /// The id to quote to the operator: Fireblocks' when known, else ours.
    fn reference(&self) -> &str {
        self.fireblocks_tx_id
            .as_deref()
            .unwrap_or(&self.external_tx_id)
    }

    /// What the card and the routes say about it, from `viewer`'s page.
    fn describe(&self, viewer: &str) -> String {
        let from = if self.started_by == viewer {
            String::new()
        } else {
            format!(", started from {}", self.started_by)
        };
        format!(
            "A registration for this wallet is waiting on Fireblocks (transaction {}{from}). If \
             a policy needs a co-signer, approve the Contract Call there, then check again. \
             Nothing else is sent for this wallet until that one lands or fails, so it can't be \
             registered twice.",
            self.reference()
        )
    }
}

/// The pending registration for `key`, if any. A file that exists but
/// doesn't parse is an error, not "nothing pending": guessing wrong there is
/// how a second mint goes out.
fn read_pending(store: &std::path::Path, key: MintKey) -> Result<Option<PendingMint>, ApiError> {
    let path = pending_file(store, key);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
            ApiError::conflict(format!(
                "{} doesn't parse ({e}). It records a Fireblocks registration that may still be \
                 in flight: check the Fireblocks console, then delete the file.",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ApiError::internal(
            &anyhow::anyhow!(e).context(format!("reading {}", path.display())),
        )),
    }
}

fn write_pending(
    store: &std::path::Path,
    key: MintKey,
    pending: &PendingMint,
) -> Result<(), ApiError> {
    let json = serde_json::to_string_pretty(pending).expect("strings and numbers serialize");
    std::fs::create_dir_all(store)
        .map_err(anyhow::Error::from)
        .and_then(|()| crate::setup::write_file_atomic(&pending_file(store, key), json))
        .map_err(|e| {
            ApiError::internal(&e.context(format!(
                "recording Fireblocks registration {}. Check the Fireblocks console before \
                 registering again",
                pending.reference()
            )))
        })
}

fn clear_pending(store: &std::path::Path, key: MintKey) -> Result<(), ApiError> {
    match std::fs::remove_file(pending_file(store, key)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(ApiError::internal(
            &anyhow::anyhow!(e).context(format!("removing a {PENDING_DIR} record")),
        )),
        _ => Ok(()),
    }
}

/// `POST /api/bots/{name}/identity/register/custody` — Fireblocks sends the
/// mint. Same nonce rule as the custody approval: the wallet is claimed for
/// the call, and the signer half of the check is what this route routes around.
///
/// With a registration already pending on the wallet this sends nothing and
/// checks on that one instead, so pressing the button again is always safe.
pub async fn register_via_custody(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Extension(identity): Extension<Identity>,
    Json(body): Json<RegisterBody>,
) -> Result<Response, ApiError> {
    let store = pending_store(&state);
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    let pending_now = match mint_key(&bot) {
        Some(key) => read_pending(&store, key)?,
        None => None,
    };
    if pending_now.is_none() {
        precheck(&read_config(&path)?, &body)?;
    }
    let (bot, claim) =
        super::logs::reserve_approval(&state, &name, bot, super::logs::approve_wallet_check)
            .await?;

    // Re-read under the claim and act on that read.
    let path = config_path(&bot)?;
    let cfg = read_config(&path)?;
    let fireblocks = match cfg.signer.as_ref() {
        Some(SignerConfig::Fireblocks(c)) if !c.raw_signing => c.clone(),
        _ => {
            return Err(ApiError::conflict(format!(
                "{name} isn't on a Fireblocks signer without raw signing, so it registers with \
                 its own key. Use the normal Register."
            )))
        }
    };
    let owner: Address = fireblocks.operator_address.parse().map_err(|e| {
        ApiError::bad_request(format!("the config's operator address is invalid: {e}"))
    })?;
    let key = MintKey {
        chain_id: cfg.chain_id,
        wallet: owner,
    };
    let registry = *erc8004::registry_for_chain(cfg.chain_id).ok_or_else(|| {
        ApiError::conflict(format!(
            "ERC-8004 has no Identity Registry on chain {}",
            cfg.chain_id
        ))
    })?;
    let signer =
        FireblocksSigner::from_config_with(&fireblocks, &super::enroll::secrets_beside(&path))
            .map_err(|e| {
                ApiError::conflict(format!(
                    "couldn't build a Fireblocks client from {name}'s config: {e:#}"
                ))
            })?;
    let rpc = Rpc::new(cfg.rpc_url.clone());

    let (pending, fireblocks_tx_id) = match read_pending(&store, key)? {
        Some(pending) => {
            tracing::info!(bot = %name, id = %pending.reference(), "checking a pending ERC-8004 registration");
            let id = match pending.fireblocks_tx_id.clone() {
                Some(id) => id,
                None => recover_lost_submission(&signer, &store, key, &pending, None).await?,
            };
            (pending, id)
        }
        None => submit_custody_mint(&name, &cfg, &body, key, &signer, &rpc, &store).await?,
    };
    let outcome = signer
        .wait_for_contract_call(&fireblocks_tx_id, CONTRACT_CALL_TIMEOUT)
        .await
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    drop(claim);

    let tx_hash = match outcome {
        CallState::InFlight => return Err(ApiError::conflict(pending.describe(&name))),
        CallState::Dead(why) => {
            // Never mined and never will, so nothing it could mint is left to
            // guard. The next Register is a fresh one with a fresh external id.
            clear_pending(&store, key)?;
            return Err(ApiError::conflict(format!(
                "{why}. The registration was not sent; Register again to send a new one."
            )));
        }
        CallState::Mined(hash) => hash,
    };
    let id = minted_agent_id(&rpc, &registry, owner, &tx_hash).await?;
    record(&state, &name, &identity, id).await?;
    Ok(Json(identity_for(&state, &name).await?).into_response())
}

/// Check the wallet has no identity yet, record the registration as pending,
/// then submit it. Recorded first, so a crash or a lost response between the
/// two still leaves the external id to find it by.
async fn submit_custody_mint(
    name: &str,
    cfg: &Config,
    body: &RegisterBody,
    key: MintKey,
    signer: &FireblocksSigner,
    rpc: &Rpc,
    store: &std::path::Path,
) -> Result<(PendingMint, String), ApiError> {
    let profile = precheck(cfg, body)?;
    let plan = erc8004::plan_register(cfg, key.wallet, profile)
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    erc8004::ensure_no_identity_yet(rpc, &plan.registry, key.wallet)
        .await
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    let asset_id = signer
        .evm_asset_id(cfg.chain_id)
        .await
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;

    let mut pending = PendingMint::new(key, name);
    write_pending(store, key, &pending)?;
    tracing::info!(bot = %name, %asset_id, external = %pending.external_tx_id, "sending an ERC-8004 registration through Fireblocks");
    let submitted = signer
        .submit_contract_call(
            &asset_id,
            plan.registry.address,
            &plan.calldata,
            &format!("Textile Stitch: ERC-8004 registration for {name}"),
            Some(&pending.external_tx_id),
        )
        .await;
    let id = match submitted {
        Ok(id) => id,
        Err(e) => recover_lost_submission(signer, store, key, &pending, Some(e)).await?,
    };
    pending.fireblocks_tx_id = Some(id.clone());
    write_pending(store, key, &pending)?;
    Ok((pending, id))
}

/// Fireblocks' id for a registration whose submission never told us one: a
/// failed POST, or a lost response. Looked up by the external id. Only a
/// definite "not found" clears the record, because only then is it certain
/// nothing was sent.
async fn recover_lost_submission(
    signer: &FireblocksSigner,
    store: &std::path::Path,
    key: MintKey,
    pending: &PendingMint,
    submit_error: Option<anyhow::Error>,
) -> Result<String, ApiError> {
    match signer.find_by_external_id(&pending.external_tx_id).await {
        Ok(Some(id)) => {
            let mut known = pending.clone();
            known.fireblocks_tx_id = Some(id.clone());
            write_pending(store, key, &known)?;
            Ok(id)
        }
        Ok(None) => {
            clear_pending(store, key)?;
            Err(ApiError::conflict(match submit_error {
                Some(e) => format!(
                    "Fireblocks didn't take the registration ({e:#}), and has no record of it, \
                     so nothing was sent. Register again once that is fixed."
                ),
                None => "Fireblocks has no record of the registration this panel started, so \
                         nothing was sent. Register again."
                    .to_string(),
            }))
        }
        Err(lookup) => Err(ApiError::conflict(format!(
            "{}couldn't ask Fireblocks whether it received the registration ({lookup:#}). Check \
             again: nothing new is sent until Fireblocks answers.",
            submit_error
                .map(|e| format!("The registration request failed ({e:#}) and the panel "))
                .unwrap_or_else(|| "The panel ".to_string())
        ))),
    }
}

/// The agent id a mined registration minted for `owner`.
async fn minted_agent_id(
    rpc: &Rpc,
    registry: &Registry,
    owner: Address,
    tx_hash: &str,
) -> Result<u64, ApiError> {
    let hash = tx_hash
        .parse()
        .map_err(|_| ApiError::conflict(format!("Fireblocks returned {tx_hash:?} as the hash")))?;
    let receipt = rpc
        .receipt(hash)
        .await
        .map_err(|e| {
            ApiError::conflict(format!(
                "the mint landed but its receipt couldn't be read: {e:#}. Check again in a moment."
            ))
        })?
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "no receipt yet for {tx_hash}. Check again in a moment."
            ))
        })?;
    erc8004::agent_id_from_receipt(&receipt, registry.address, owner).ok_or_else(|| {
        ApiError::conflict(format!(
            "tx {tx_hash} landed without a Registered event for {owner}. If the wallet owns an \
             identity now, link it by id."
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const OWNER: Address = address!("3d17bee70baf9ec50f6f35ea73cff0fea290d409");

    fn celo() -> &'static Registry {
        erc8004::registry_for_chain(42220).unwrap()
    }

    fn facts() -> Facts<'static> {
        Facts {
            chain_id: 42220,
            network_label: Some("Celo".into()),
            registry: Some(celo()),
            owner: Some(OWNER),
            agent_id: None,
            defaults: erc8004::default_profile("Acme", &["cNGN / USDT".into()], "Celo"),
            sibling: None,
            via_custody: false,
            pending: None,
            signer_blocked: None,
            wallet_blocked: None,
        }
    }

    fn gwei(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000u64)
    }

    fn unregistered_reads(balance_wei: U256) -> Reads {
        Reads {
            owned: Some(U256::ZERO),
            gas_units: Some(U256::from(463_772u64)),
            gas_price: Some(gwei(200)),
            balance: Some(balance_wei),
            ..Reads::default()
        }
    }

    const FUNDED: u128 = 12_410_000_000_000_000_000;

    #[test]
    fn a_funded_unregistered_bot_can_register() {
        let body = assess(facts(), unregistered_reads(U256::from(FUNDED)));
        assert!(body.can_register, "{body:?}");
        assert_eq!(body.blocked_kind, None);
        let gas = body.gas.unwrap();
        assert_eq!(
            (gas.estimate.as_str(), gas.symbol, gas.enough),
            ("0.09275", "CELO", true)
        );
        assert_eq!(body.defaults.unwrap().name, "Acme · cNGN/USDT");
        assert_eq!(
            body.registry.as_deref(),
            Some("0x8004A169FB4a3325136EB29fA0ceB6D2e539a432")
        );
    }

    #[test]
    fn gas_needs_room_for_a_fee_bump() {
        // Exactly the estimate isn't enough: a re-bid would strand the mint.
        let exact = U256::from(463_772u64) * gwei(200);
        let body = assess(facts(), unregistered_reads(exact));
        assert!(!body.can_register);
        assert_eq!(body.blocked_kind, Some("gas"));
        assert!(body
            .blocked_reason
            .unwrap()
            .contains("in case the fee has to be raised"));
    }

    #[test]
    fn signer_then_nonce_then_gas_is_the_order_reasons_are_given() {
        let mut f = facts();
        f.signer_blocked = Some("no tx".into());
        f.wallet_blocked = Some(("taker on".into(), Some("bot-a".into())));
        let body = assess(f, unregistered_reads(U256::ZERO));
        assert_eq!(body.blocked_kind, Some("signer"));

        let mut f = facts();
        f.wallet_blocked = Some(("taker on".into(), Some("bot-a".into())));
        let body = assess(f, unregistered_reads(U256::ZERO));
        assert_eq!(body.blocked_kind, Some("transacting"));
        assert_eq!(body.blocked_by.as_deref(), Some("bot-a"));
    }

    #[test]
    fn a_wallet_that_already_owns_one_is_offered_a_link_not_a_mint() {
        let mut reads = unregistered_reads(U256::from(FUNDED));
        reads.owned = Some(U256::from(1u8));
        let body = assess(facts(), reads);
        assert!(body.owned_elsewhere);
        assert!(!body.can_register);
        assert_eq!(body.blocked_kind, None, "linking isn't blocked by anything");
    }

    #[test]
    fn an_uncounted_wallet_keeps_register_off() {
        let mut reads = unregistered_reads(U256::from(FUNDED));
        reads.owned = None;
        reads.error = Some("couldn't read the wallet's identities".into());
        let body = assess(facts(), reads);
        assert!(!body.can_register);
        assert!(body.read_error.is_some());
    }

    #[test]
    fn a_registered_bot_shows_what_is_stored_and_who_owns_it() {
        let stored = erc8004::default_profile("Acme", &["cNGN / USDT".into()], "Celo");
        let uri = erc8004::data_uri(&erc8004::registration_json(&stored, 42220, None));
        let mut f = facts();
        f.agent_id = Some(1847);
        let body = assess(
            f,
            Reads {
                agent_owner: Some(OWNER),
                agent_uri: Some(uri.clone()),
                ..Reads::default()
            },
        );
        assert_eq!(body.agent_id.as_deref(), Some("1847"));
        assert_eq!(body.stored, Some(stored));
        assert_eq!(body.owned_by, None);
        assert_eq!(body.defaults, None);
        assert!(!body.can_register);
        assert_eq!(
            body.links.agent.as_deref(),
            Some("https://www.8004scan.io/agents/celo/1847")
        );

        let mut f = facts();
        f.agent_id = Some(1847);
        let someone = address!("00000000000000000000000000000000000000cc");
        let body = assess(
            f,
            Reads {
                agent_owner: Some(someone),
                agent_uri: Some(uri),
                ..Reads::default()
            },
        );
        assert_eq!(body.owned_by, Some(someone.to_string()));
    }

    #[test]
    fn a_pending_custody_mint_keeps_register_off_until_it_resolves() {
        let mut f = facts();
        f.via_custody = true;
        f.pending = Some("waiting on fb-tx-1".into());
        let body = assess(f, unregistered_reads(U256::from(FUNDED)));
        assert!(
            !body.can_register,
            "a second Contract Call must not be offered"
        );
        assert_eq!(body.blocked_kind, None);
        assert_eq!(body.pending.unwrap().message, "waiting on fb-tx-1");

        // Once the id is recorded the file is gone, and a stale one is ignored.
        let mut f = facts();
        f.agent_id = Some(7);
        f.pending = Some("waiting".into());
        assert_eq!(assess(f, Reads::default()).pending, None);
    }

    const OTHER: Address = address!("00000000000000000000000000000000000000aa");

    fn key(chain_id: u64, wallet: Address) -> MintKey {
        MintKey { chain_id, wallet }
    }

    fn temp_store() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "stitch-identity-{}-{}/{PENDING_DIR}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn one_record_per_chain_and_wallet_whatever_bot_or_folder_started_it() {
        let store = temp_store();
        let celo = key(42220, OWNER);
        assert_eq!(
            read_pending(&store, celo).unwrap(),
            None,
            "no store yet is nothing pending"
        );

        // Two flat-layout bots share a folder, so a file beside the config
        // would be one file for both. Here each wallet gets its own.
        let a = PendingMint::new(celo, "bot-a");
        let b = PendingMint::new(key(42220, OTHER), "bot-b");
        write_pending(&store, celo, &a).unwrap();
        write_pending(&store, key(42220, OTHER), &b).unwrap();
        assert_eq!(read_pending(&store, celo).unwrap(), Some(a.clone()));
        assert_eq!(read_pending(&store, key(42220, OTHER)).unwrap(), Some(b));

        // Same key, other chain: a different identity, so no record.
        assert_eq!(read_pending(&store, key(8453, OWNER)).unwrap(), None);

        // A bot moved off the wallet keeps nothing hidden: any bot now on it
        // finds the record by the wallet alone.
        assert_eq!(
            read_pending(&store, celo).unwrap().unwrap().started_by,
            "bot-a"
        );

        clear_pending(&store, celo).unwrap();
        assert_eq!(read_pending(&store, celo).unwrap(), None);
        clear_pending(&store, celo).unwrap();
        std::fs::remove_dir_all(store.parent().unwrap()).ok();
    }

    #[test]
    fn the_file_name_comes_from_the_parsed_address() {
        let path = pending_file(
            std::path::Path::new("/bots/.erc8004-pending"),
            key(42220, OWNER),
        );
        assert_eq!(
            path,
            std::path::Path::new(
                "/bots/.erc8004-pending/42220-0x3d17bee70baf9ec50f6f35ea73cff0fea290d409.json"
            )
        );
    }

    #[test]
    fn each_registration_gets_a_fresh_external_id_naming_its_wallet() {
        let celo = key(42220, OWNER);
        let pending = PendingMint::new(celo, "bot-a");
        // Fireblocks refuses a reused external id forever, so a dead one must
        // never be sent again.
        assert!(pending
            .external_tx_id
            .starts_with("stitch-erc8004-42220-0x3d17bee70baf9ec50f6f35ea73cff0fea290d409-"));
        assert!(pending.external_tx_id.len() <= 255);
        assert_ne!(
            PendingMint::new(celo, "bot-a").external_tx_id,
            pending.external_tx_id
        );
        assert_eq!(pending.reference(), pending.external_tx_id);
    }

    #[test]
    fn another_bot_on_the_wallet_is_told_which_bot_started_it() {
        let mut pending = PendingMint::new(key(42220, OWNER), "usdc-celo");
        pending.fireblocks_tx_id = Some("fb-tx-1".into());
        assert!(pending
            .describe("usdc-celo")
            .contains("(transaction fb-tx-1)"));
        assert!(pending
            .describe("cngn-celo")
            .contains("(transaction fb-tx-1, started from usdc-celo)"));
    }

    #[test]
    fn an_unreadable_pending_record_is_not_taken_as_nothing_pending() {
        let store = temp_store();
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(pending_file(&store, key(42220, OWNER)), "not json").unwrap();
        let err = read_pending(&store, key(42220, OWNER)).unwrap_err();
        assert!(
            err.message.contains("may still be in flight"),
            "{}",
            err.message
        );
        std::fs::remove_dir_all(store.parent().unwrap()).ok();
    }

    #[test]
    fn a_chain_without_a_registry_offers_nothing() {
        let mut f = facts();
        f.registry = None;
        f.chain_id = 146;
        f.network_label = None;
        let body = assess(f, Reads::default());
        assert_eq!(body.registry, None);
        assert!(!body.can_register);
        assert_eq!(body.blocked_kind, None);
    }
}
