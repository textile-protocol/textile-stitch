// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! ERC-8004 agent identity for a bot: `stitch register`.
//!
//! One `register(agentURI)` on the Identity Registry of the bot's own chain
//! mints an ERC-721 to the sender (the operator wallet), and the registry
//! records that same address as the agent's wallet. That is the whole
//! identity. Nothing about quoting reads it.
//!
//! The agentURI is a base64 `data:` URI holding the full registration file, so
//! the profile is stored in the registry's own storage next to the token. No
//! server serves it, and only the owner can change it (`setAgentURI`). The
//! file's free text, name and description, is the operator's: a default is
//! generated from the config and the operator can edit both before the mint.
//!
//! The file carries no `registrations` back-reference. That field ties a file
//! hosted somewhere else to its onchain id; a file stored in the token's own
//! URI is already tied to it, and writing the id in would need a second
//! transaction after the mint.
//!
//! The registry is not enumerable and public RPCs refuse a log scan over its
//! history, so the token id of a wallet's existing identity cannot be looked
//! up. A wallet that already owns one has to name it (`--agent-id`), which is
//! checked against `ownerOf` before it is written to the config.

use std::time::Duration;

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::SolValue;
use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use crate::chain::rpc::{Rpc, Wallet};
use crate::config::Config;

/// Calldata for `signature` with ABI-encoded `params`.
fn calldata(signature: &str, params: Vec<u8>) -> Bytes {
    let mut out = keccak256(signature.as_bytes()).0[..4].to_vec();
    out.extend(params);
    Bytes::from(out)
}

/// `register(string agentURI)`: mints to the sender, sets the token URI, and
/// records the sender as the agent wallet.
pub fn encode_register(agent_uri: &str) -> Bytes {
    calldata(
        "register(string)",
        (agent_uri.to_string(),).abi_encode_params(),
    )
}

/// The URI back out of [`encode_register`] calldata. `None` for anything else.
pub fn decode_register(data: &[u8]) -> Option<String> {
    let rest = data.strip_prefix(&encode_register("")[..4])?;
    <(String,)>::abi_decode_params_validate(rest)
        .ok()
        .map(|(uri,)| uri)
}

/// Topic 0 of `Registered(uint256 indexed agentId, string agentURI, address indexed owner)`.
pub fn registered_topic() -> B256 {
    keccak256("Registered(uint256,string,address)")
}

/// The Identity Registry's address on every mainnet in [`REGISTRIES`]. The
/// ERC-8004 deployment uses one address per network class.
pub const MAINNET_REGISTRY: Address = address!("8004A169FB4a3325136EB29fA0ceB6D2e539a432");
/// The Identity Registry's address on every testnet in [`REGISTRIES`].
pub const TESTNET_REGISTRY: Address = address!("8004A818BFB912233c491871b3d84c89A494BD9e");

/// One chain's Identity Registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registry {
    pub chain_id: u64,
    pub address: Address,
    /// Network name for operator-facing text and the default description.
    pub network: &'static str,
    /// 8004scan's path segment for the chain: `/agents/{slug}/{id}`.
    pub scan_slug: &'static str,
    /// Ticker of the coin the mint's gas is paid in.
    pub gas_coin: &'static str,
}

/// Every chain with an Identity Registry that Stitch can register on. Each row
/// was checked onchain: the address has code and `name()` is "AgentIdentity".
/// A chain missing here gets no Register button rather than a mint aimed at an
/// address with nothing behind it.
pub const REGISTRIES: &[Registry] = &[
    reg(1, MAINNET_REGISTRY, "Ethereum", "ethereum", "ETH"),
    reg(10, MAINNET_REGISTRY, "Optimism", "optimism", "ETH"),
    reg(56, MAINNET_REGISTRY, "BNB Smart Chain", "bsc", "BNB"),
    reg(
        97,
        TESTNET_REGISTRY,
        "BNB Smart Chain testnet",
        "bsc-testnet",
        "BNB",
    ),
    reg(137, MAINNET_REGISTRY, "Polygon", "polygon", "POL"),
    reg(
        4663,
        MAINNET_REGISTRY,
        "Robinhood Chain",
        "robinhood-chain",
        "ETH",
    ),
    reg(8453, MAINNET_REGISTRY, "Base", "base", "ETH"),
    reg(42161, MAINNET_REGISTRY, "Arbitrum", "arbitrum", "ETH"),
    reg(42220, MAINNET_REGISTRY, "Celo", "celo", "CELO"),
    reg(
        84532,
        TESTNET_REGISTRY,
        "Base Sepolia",
        "base-sepolia",
        "ETH",
    ),
    reg(
        421614,
        TESTNET_REGISTRY,
        "Arbitrum Sepolia",
        "arbitrum-sepolia",
        "ETH",
    ),
    reg(
        11142220,
        TESTNET_REGISTRY,
        "Celo Sepolia",
        "celo-sepolia",
        "CELO",
    ),
    reg(
        11155111,
        TESTNET_REGISTRY,
        "Ethereum Sepolia",
        "sepolia",
        "ETH",
    ),
];

const fn reg(
    chain_id: u64,
    address: Address,
    network: &'static str,
    scan_slug: &'static str,
    gas_coin: &'static str,
) -> Registry {
    Registry {
        chain_id,
        address,
        network,
        scan_slug,
        gas_coin,
    }
}

/// The registry on `chain_id`, or `None` where ERC-8004 isn't deployed.
pub fn registry_for_chain(chain_id: u64) -> Option<&'static Registry> {
    REGISTRIES.iter().find(|r| r.chain_id == chain_id)
}

/// The agent's page on 8004scan.
pub fn scan_url(registry: &Registry, agent_id: u64) -> String {
    format!(
        "https://www.8004scan.io/agents/{}/{agent_id}",
        registry.scan_slug
    )
}

/// Longest name the panel and the CLI accept, in characters.
pub const NAME_MAX_CHARS: usize = 64;
/// Longest description accepted, in characters. Every 32 bytes of URI is one
/// more storage slot the mint pays for, so the limit is also a gas bound.
pub const DESCRIPTION_MAX_CHARS: usize = 280;

/// The registration file's free text: what the agent is called and says it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub name: String,
    pub description: String,
}

/// Trim and collapse every run of whitespace, newlines included, to one space.
/// The file is one JSON line on a public page, and a stray newline or double
/// space pasted into a form shouldn't be published as-is.
pub fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize and check operator-supplied text. Errors name the field and say
/// what to change.
pub fn profile_from_input(name: &str, description: &str) -> anyhow::Result<Profile> {
    Ok(Profile {
        name: checked_field("name", name, NAME_MAX_CHARS)?,
        description: checked_field("description", description, DESCRIPTION_MAX_CHARS)?,
    })
}

fn checked_field(field: &str, raw: &str, max_chars: usize) -> anyhow::Result<String> {
    let text = normalize_text(raw);
    if text.is_empty() {
        bail!("the {field} is empty");
    }
    let chars = text.chars().count();
    if chars > max_chars {
        bail!("the {field} is {chars} characters; keep it to {max_chars}");
    }
    if text.chars().any(char::is_control) {
        bail!("the {field} contains a control character");
    }
    Ok(text)
}

/// The text the form starts with: `label · pair` and a one-line description
/// of what the bot quotes and where. Always passes [`profile_from_input`].
///
/// `pairs` are display names as the panel shows them ("cNGN / USDT").
pub fn default_profile(label: &str, pairs: &[String], network: &str) -> Profile {
    let compact: Vec<String> = pairs.iter().map(|p| compact_pair(p)).collect();
    let headline = match compact.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [first, rest @ ..] => format!("{first} +{}", rest.len()),
    };
    let label = normalize_text(label);
    let name = if headline.is_empty() {
        label
    } else {
        format!("{label} · {headline}")
    };
    let quoting = if compact.is_empty() {
        String::new()
    } else {
        format!(" quoting {}", join_and(&compact))
    };
    let description =
        format!("Market-making bot{quoting} on Textile FX ({network}), running Stitch.");
    let description = if description.chars().count() > DESCRIPTION_MAX_CHARS {
        format!(
            "Market-making bot quoting {} pairs on Textile FX ({network}), running Stitch.",
            compact.len()
        )
    } else {
        description
    };
    Profile {
        name: truncate_chars(&normalize_text(&name), NAME_MAX_CHARS),
        description: truncate_chars(&normalize_text(&description), DESCRIPTION_MAX_CHARS),
    }
}

/// "cNGN / USDT" → "cNGN/USDT".
fn compact_pair(pair: &str) -> String {
    pair.split('/').map(str::trim).collect::<Vec<_>>().join("/")
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The bot's name as its config path gives it, for the default when nothing
/// better is known (the CLI without `--name`). The flat layout names configs
/// `stitch.<bot>.toml` side by side in one folder, so the file says which bot
/// it is; the per-bot layout keeps a plain `stitch.toml` in a folder named
/// after the bot. Any other file name is the operator's own and is used as is.
pub fn label_from_config_path(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let from_file = match stem.strip_prefix("stitch.") {
        Some(bot) => bot,
        None if stem == "stitch" => "",
        None => stem,
    };
    let label = if from_file.trim().is_empty() {
        path.parent()?.file_name()?.to_str()?
    } else {
        from_file
    };
    Some(label.trim().to_string()).filter(|l| !l.is_empty())
}

/// What the bot's pools quote, as the panel labels them: the corridor's display
/// name when the config or the catalog knows it, else the shortened addresses.
pub fn pair_labels(cfg: &Config) -> Vec<String> {
    cfg.pools
        .iter()
        .map(|p| {
            crate::setup::catalog::pool_identity(cfg.chain_id, p)
                .map(|c| c.display_name)
                .unwrap_or_else(|| {
                    format!(
                        "{} / {}",
                        crate::setup::short_addr(&p.collateral),
                        crate::setup::short_addr(&p.debt)
                    )
                })
        })
        .collect()
}

/// The registration-v1 type URI every ERC-8004 file starts with.
pub const REGISTRATION_TYPE: &str = "https://eips.ethereum.org/EIPS/eip-8004#registration-v1";

/// Where counterparties trade with the bot. Its quotes are served on Textile FX.
pub const WEB_ENDPOINT: &str = "https://app.textilecredit.com";

#[derive(Serialize)]
struct RegistrationFile<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    services: Vec<Service>,
    active: bool,
    #[serde(rename = "supportedTrust")]
    supported_trust: [&'static str; 1],
}

#[derive(Serialize)]
struct Service {
    name: &'static str,
    endpoint: String,
}

/// The registration file, as compact JSON. Everything except the profile is
/// fixed: the web service is Textile FX, trust is reputation, and a vault
/// maker lists its OperatorVault as a `wallet` service, because the vault is
/// the address its trades settle from while the identity's wallet is the key
/// that signs for it.
pub fn registration_json(profile: &Profile, chain_id: u64, vault: Option<Address>) -> String {
    let mut services = vec![Service {
        name: "web",
        endpoint: WEB_ENDPOINT.to_string(),
    }];
    if let Some(vault) = vault {
        services.push(Service {
            name: "wallet",
            endpoint: format!("eip155:{chain_id}:{vault}"),
        });
    }
    serde_json::to_string(&RegistrationFile {
        kind: REGISTRATION_TYPE,
        name: &profile.name,
        description: &profile.description,
        services,
        active: true,
        supported_trust: ["reputation"],
    })
    .expect("a struct of strings and bools always serializes")
}

const DATA_URI_PREFIX: &str = "data:application/json;base64,";

/// The agentURI: the file as a base64 data URI, the form ERC-8004 gives for a
/// file stored fully onchain.
pub fn data_uri(json: &str) -> String {
    format!(
        "{DATA_URI_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(json)
    )
}

/// Read the profile back out of an agentURI this module wrote, or any base64
/// JSON data URI with a string `name` and `description`. `None` for anything
/// else (an `https://` or `ipfs://` URI, malformed base64, other JSON).
pub fn profile_from_uri(uri: &str) -> Option<Profile> {
    let encoded = uri.strip_prefix(DATA_URI_PREFIX)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let file: Value = serde_json::from_slice(&bytes).ok()?;
    Some(Profile {
        name: file.get("name")?.as_str()?.to_string(),
        description: file.get("description")?.as_str()?.to_string(),
    })
}

/// Wei of an 18-decimal gas coin as a short decimal: up to four digits after
/// the first non-zero one, which is enough for "about". Display only.
pub fn format_coin(wei: U256) -> String {
    let text = crate::rfq::math::format_units(wei, 18);
    let Some((whole, frac)) = text.split_once('.') else {
        return text;
    };
    let keep = frac
        .find(|c| c != '0')
        .map_or(frac.len(), |first| (first + 4).min(frac.len()));
    format!("{whole}.{}", &frac[..keep])
}

/// Everything a mint needs, decided before the wallet is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterPlan {
    pub registry: Registry,
    pub owner: Address,
    pub profile: Profile,
    pub json: String,
    pub agent_uri: String,
    pub calldata: Bytes,
}

/// Plan a mint for `owner` from this config. Refuses a chain with no registry
/// and a config that already names an agent id.
pub fn plan_register(
    cfg: &Config,
    owner: Address,
    profile: Profile,
) -> anyhow::Result<RegisterPlan> {
    let registry = *registry_for_chain(cfg.chain_id).ok_or_else(|| {
        anyhow!(
            "ERC-8004 has no Identity Registry on chain {} that Stitch knows of",
            cfg.chain_id
        )
    })?;
    if let Some(id) = configured_agent_id(cfg) {
        bail!("this config is already registered as agent #{id}; nothing to send");
    }
    let vault = cfg
        .vault
        .as_ref()
        .map(|v| v.address.parse::<Address>())
        .transpose()
        .context("the [vault] address is not an address")?;
    let json = registration_json(&profile, cfg.chain_id, vault);
    let agent_uri = data_uri(&json);
    let calldata = encode_register(&agent_uri);
    Ok(RegisterPlan {
        registry,
        owner,
        profile,
        json,
        agent_uri,
        calldata,
    })
}

/// The agent id `[erc8004]` names, if any.
pub fn configured_agent_id(cfg: &Config) -> Option<u64> {
    cfg.erc8004.and_then(|e| e.agent_id)
}

/// The `Registered` log for `owner` in a mint's receipt, as the agent id.
/// Matched on the registry address, the event signature and the indexed owner,
/// so an unrelated log in the same receipt can't be mistaken for it.
pub fn agent_id_from_receipt(receipt: &Value, registry: Address, owner: Address) -> Option<u64> {
    let owner_topic = B256::left_padding_from(owner.as_slice());
    receipt.get("logs")?.as_array()?.iter().find_map(|log| {
        let from: Address = log.get("address")?.as_str()?.parse().ok()?;
        let topics: Vec<B256> = log
            .get("topics")?
            .as_array()?
            .iter()
            .filter_map(|t| t.as_str()?.parse().ok())
            .collect();
        let is_ours = from == registry
            && topics.len() == 3
            && topics[0] == registered_topic()
            && topics[2] == owner_topic;
        is_ours.then(|| U256::from_be_bytes(topics[1].0).try_into().ok())?
    })
}

/// How many identities `owner` holds on the registry.
pub async fn owned_count(rpc: &Rpc, registry: Address, owner: Address) -> anyhow::Result<U256> {
    let out = rpc
        .eth_call(
            registry,
            &calldata("balanceOf(address)", owner.abi_encode()),
        )
        .await?;
    U256::abi_decode_validate(&out).context("decoding balanceOf")
}

/// Who owns agent `id`. Errors when the token doesn't exist, which the
/// registry answers with a revert.
pub async fn owner_of(rpc: &Rpc, registry: Address, id: u64) -> anyhow::Result<Address> {
    let out = rpc
        .eth_call(
            registry,
            &calldata("ownerOf(uint256)", U256::from(id).abi_encode()),
        )
        .await?;
    Address::abi_decode_validate(&out).context("decoding ownerOf")
}

/// Agent `id`'s current agentURI.
pub async fn token_uri(rpc: &Rpc, registry: Address, id: u64) -> anyhow::Result<String> {
    let out = rpc
        .eth_call(
            registry,
            &calldata("tokenURI(uint256)", U256::from(id).abi_encode()),
        )
        .await?;
    String::abi_decode_validate(&out).context("decoding tokenURI")
}

/// Refuse a second mint for a wallet that already owns an identity. The id of
/// the one it has can't be looked up (see the module docs), so the way forward
/// is to name it.
pub async fn ensure_no_identity_yet(
    rpc: &Rpc,
    registry: &Registry,
    owner: Address,
) -> anyhow::Result<()> {
    let held = owned_count(rpc, registry.address, owner).await?;
    if held.is_zero() {
        return Ok(());
    }
    bail!(
        "{owner} already owns {held} ERC-8004 identit{} on {}. Registering again would mint a \
         second one. Link the existing one instead: `stitch register --agent-id <id>` (its id is \
         on 8004scan, or in the Registered event of the transaction that minted it).",
        if held == U256::from(1u8) { "y" } else { "ies" },
        registry.network
    )
}

/// Refuse to mint while the wallet has a transaction in the pool. `balanceOf`
/// can't see a mint that hasn't landed, so a rerun after a timed-out one would
/// queue a second behind it and both would mint. The pending nonce running
/// ahead of the latest one is the only sign of it the chain gives.
pub async fn ensure_nothing_in_flight(rpc: &Rpc, owner: Address) -> anyhow::Result<()> {
    let (pending, latest) = tokio::try_join!(
        rpc.transaction_count(owner),
        rpc.latest_transaction_count(owner)
    )?;
    in_flight_check(owner, pending, latest)
}

fn in_flight_check(owner: Address, pending: u64, latest: u64) -> anyhow::Result<()> {
    if pending > latest {
        bail!(
            "{owner} has {} transaction{} waiting in the pool (nonce {latest} onward). If one is an \
             earlier register, it may still land; wait for it, then run register again and it \
             will see the identity instead of minting a second.",
            pending - latest,
            if pending - latest == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Check that `owner` owns agent `id` before a config is pointed at it.
pub async fn ensure_owns(
    rpc: &Rpc,
    registry: &Registry,
    owner: Address,
    id: u64,
) -> anyhow::Result<()> {
    let actual = owner_of(rpc, registry.address, id)
        .await
        .with_context(|| format!("agent #{id} doesn't exist on {}", registry.network))?;
    if actual != owner {
        bail!(
            "agent #{id} on {} is owned by {actual}, not this bot's wallet {owner}",
            registry.network
        );
    }
    Ok(())
}

/// How long a mint may sit unmined before the run gives up. Long enough for a
/// fee bump or two; a run that times out leaves the wallet owning the identity
/// if the transaction lands later, and the next run says so.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(180);

/// Send the mint and return the agent id from its receipt.
pub async fn send_register(wallet: &Wallet, plan: &RegisterPlan) -> anyhow::Result<(u64, String)> {
    let receipt = wallet
        .send_and_wait(
            plan.registry.address,
            plan.calldata.clone(),
            U256::ZERO,
            REGISTER_TIMEOUT,
        )
        .await
        .context("sending register(agentURI)")?;
    let hash = receipt
        .get("transactionHash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = agent_id_from_receipt(&receipt, plan.registry.address, plan.owner).ok_or_else(|| {
        anyhow!(
            "tx {hash} landed but its receipt has no Registered event for {}. The wallet may own \
             the identity anyway: find its id on 8004scan and run `stitch register --agent-id <id>`",
            plan.owner
        )
    })?;
    Ok((id, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: Address = address!("3d17bee70baf9ec50f6f35ea73cff0fea290d409");

    fn pairs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_registry_row_is_unique_and_on_one_of_the_two_addresses() {
        for (i, r) in REGISTRIES.iter().enumerate() {
            assert!(
                r.address == MAINNET_REGISTRY || r.address == TESTNET_REGISTRY,
                "{}",
                r.network
            );
            assert!(
                REGISTRIES[i + 1..].iter().all(|o| o.chain_id != r.chain_id),
                "chain {} listed twice",
                r.chain_id
            );
        }
        assert_eq!(registry_for_chain(42220).unwrap().address, MAINNET_REGISTRY);
        assert_eq!(registry_for_chain(84532).unwrap().address, TESTNET_REGISTRY);
        assert!(registry_for_chain(146).is_none());
    }

    #[test]
    fn every_shipped_corridor_chain_has_a_registry() {
        for c in crate::setup::catalog() {
            assert!(
                registry_for_chain(c.chain_id).is_some(),
                "{} ({}) has no ERC-8004 registry row",
                c.id,
                c.chain_id
            );
        }
    }

    #[test]
    fn default_profile_names_the_bot_and_what_it_quotes() {
        let p = default_profile("Acme MM", &pairs(&["cNGN / USDT"]), "Celo");
        assert_eq!(p.name, "Acme MM · cNGN/USDT");
        assert_eq!(
            p.description,
            "Market-making bot quoting cNGN/USDT on Textile FX (Celo), running Stitch."
        );

        let p = default_profile(
            "0x3d17d4",
            &pairs(&["wARS / USDT", "wBRL / USDT", "cNGN / USDT"]),
            "Celo",
        );
        assert_eq!(p.name, "0x3d17d4 · wARS/USDT +2");
        assert!(p
            .description
            .contains("quoting wARS/USDT, wBRL/USDT and cNGN/USDT on Textile FX (Celo)"));

        let p = default_profile("solo", &[], "Base");
        assert_eq!(p.name, "solo");
        assert_eq!(
            p.description,
            "Market-making bot on Textile FX (Base), running Stitch."
        );
    }

    #[test]
    fn defaults_always_pass_the_input_check() {
        let long_label = "x".repeat(200);
        let many: Vec<String> = (0..40).map(|i| format!("TOKEN{i} / USDT")).collect();
        let p = default_profile(&long_label, &many, "BNB Smart Chain testnet");
        assert_eq!(profile_from_input(&p.name, &p.description).unwrap(), p);
        assert!(p.description.contains("40 pairs"), "{}", p.description);
    }

    #[test]
    fn the_default_label_follows_the_config_layout() {
        let label = |p: &str| label_from_config_path(std::path::Path::new(p));
        // Per-bot layout: the folder is the bot.
        assert_eq!(
            label("/bots/0x3d17d4/stitch.toml").as_deref(),
            Some("0x3d17d4")
        );
        // Flat layout: every bot shares the folder, the file names it.
        assert_eq!(
            label("/srv/stitch/stitch.bot1.toml").as_deref(),
            Some("bot1")
        );
        assert_eq!(
            label("/srv/stitch/stitch.bot2.toml").as_deref(),
            Some("bot2")
        );
        // An operator's own file name is theirs.
        assert_eq!(label("/home/me/acme.toml").as_deref(), Some("acme"));
        // Nothing to go on.
        assert_eq!(label("/stitch.toml"), None);
    }

    #[test]
    fn input_is_normalized_and_bounded() {
        let p = profile_from_input("  Acme\n MM ", "Quotes\tcNGN\n\n").unwrap();
        assert_eq!(p.name, "Acme MM");
        assert_eq!(p.description, "Quotes cNGN");

        assert!(profile_from_input("   ", "ok")
            .unwrap_err()
            .to_string()
            .contains("name is empty"));
        assert!(profile_from_input(&"n".repeat(65), "ok")
            .unwrap_err()
            .to_string()
            .contains("65 characters; keep it to 64"));
        assert!(profile_from_input("ok", &"d".repeat(281)).is_err());
        assert!(profile_from_input("a\u{0}b", "ok")
            .unwrap_err()
            .to_string()
            .contains("control character"));
        // Characters, not bytes: 64 multi-byte characters fit.
        assert!(profile_from_input(&"é".repeat(64), "ok").is_ok());
    }

    #[test]
    fn coin_amounts_keep_four_significant_decimals() {
        let wei = |s: &str| U256::from_str_radix(s, 10).unwrap();
        assert_eq!(format_coin(wei("93900000000000000")), "0.0939");
        assert_eq!(format_coin(wei("93915678000000000")), "0.09391");
        assert_eq!(format_coin(wei("1000000000000")), "0.000001");
        assert_eq!(format_coin(wei("12410000000000000000")), "12.41");
        assert_eq!(format_coin(wei("2000000000000000000")), "2");
    }

    #[test]
    fn the_file_round_trips_through_its_data_uri() {
        let profile = default_profile("Acme MM", &pairs(&["cNGN / USDT"]), "Celo");
        let json = registration_json(&profile, 42220, None);
        let file: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            file,
            json!({
                "type": REGISTRATION_TYPE,
                "name": "Acme MM · cNGN/USDT",
                "description": "Market-making bot quoting cNGN/USDT on Textile FX (Celo), running Stitch.",
                "services": [{ "name": "web", "endpoint": "https://app.textilecredit.com" }],
                "active": true,
                "supportedTrust": ["reputation"],
            })
        );
        let uri = data_uri(&json);
        assert!(uri.starts_with("data:application/json;base64,"));
        assert_eq!(profile_from_uri(&uri), Some(profile));
        assert_eq!(profile_from_uri("https://example.com/agent.json"), None);
        assert_eq!(profile_from_uri("data:application/json;base64,!!!"), None);
    }

    #[test]
    fn the_stored_json_is_compact_and_in_key_order() {
        // The panel's preview (web/src/agentIdentity.ts) asserts this same literal.
        let profile = Profile {
            name: "Acme MM · cNGN/USDT".into(),
            description:
                "Market-making bot quoting cNGN/USDT on Textile FX (Celo), running Stitch.".into(),
        };
        assert_eq!(
            registration_json(&profile, 42220, None),
            concat!(
                r#"{"type":"https://eips.ethereum.org/EIPS/eip-8004#registration-v1","#,
                r#""name":"Acme MM · cNGN/USDT","#,
                r#""description":"Market-making bot quoting cNGN/USDT on Textile FX (Celo), running Stitch.","#,
                r#""services":[{"name":"web","endpoint":"https://app.textilecredit.com"}],"#,
                r#""active":true,"supportedTrust":["reputation"]}"#,
            )
        );
    }

    #[test]
    fn a_vault_maker_lists_its_vault_as_a_wallet_service() {
        let vault = address!("00000000000000000000000000000000000000aa");
        let json = registration_json(&default_profile("v", &[], "Celo"), 42220, Some(vault));
        let file: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            file["services"][1],
            json!({ "name": "wallet", "endpoint": format!("eip155:42220:{vault}") })
        );
    }

    fn registered_log(registry: Address, id: u64, owner: Address) -> Value {
        json!({
            "address": format!("{registry:#x}"),
            "topics": [
                registered_topic().to_string(),
                B256::from(U256::from(id)).to_string(),
                B256::left_padding_from(owner.as_slice()).to_string(),
            ],
            "data": "0x",
        })
    }

    #[test]
    fn the_agent_id_comes_from_our_registered_log_only() {
        let other = address!("00000000000000000000000000000000000000bb");
        let receipt = json!({ "logs": [
            // Right event, wrong contract.
            registered_log(other, 9, OWNER),
            // Right contract, someone else's mint.
            registered_log(MAINNET_REGISTRY, 8, other),
            registered_log(MAINNET_REGISTRY, 1847, OWNER),
        ]});
        assert_eq!(
            agent_id_from_receipt(&receipt, MAINNET_REGISTRY, OWNER),
            Some(1847)
        );
        assert_eq!(
            agent_id_from_receipt(&json!({ "logs": [] }), MAINNET_REGISTRY, OWNER),
            None
        );
        // Agent ids start at 0, so 0 is an id, not a miss.
        let zero = json!({ "logs": [registered_log(MAINNET_REGISTRY, 0, OWNER)] });
        assert_eq!(
            agent_id_from_receipt(&zero, MAINNET_REGISTRY, OWNER),
            Some(0)
        );
    }

    #[test]
    fn the_selector_and_topic_match_the_registry() {
        // keccak256("Registered(uint256,string,address)"), checked against the registry's logs.
        assert_eq!(
            registered_topic().to_string(),
            "0xca52e62c367d81bb2e328eb795f7c7ba24afb478408a26c0e201d155c449bc4a"
        );
        assert_eq!(encode_register("")[..4], [0xf2, 0xc2, 0x98, 0xbe]);
    }

    #[test]
    fn a_wallet_with_a_transaction_in_the_pool_is_refused() {
        assert!(in_flight_check(OWNER, 5, 5).is_ok());
        let err = in_flight_check(OWNER, 6, 5).unwrap_err().to_string();
        assert!(
            err.contains("1 transaction waiting in the pool (nonce 5 onward)"),
            "{err}"
        );
        assert!(in_flight_check(OWNER, 8, 5)
            .unwrap_err()
            .to_string()
            .contains("3 transactions"));
    }

    fn celo_config(extra: &str) -> Config {
        let toml = format!(
            "{}\n{extra}",
            crate::setup::find_corridor("cngn-usdt-celo")
                .unwrap()
                .toml_template
        );
        Config::from_toml(&toml).unwrap()
    }

    #[test]
    fn a_plan_carries_the_file_in_its_calldata() {
        let profile = default_profile("Acme", &pairs(&["cNGN / USDT"]), "Celo");
        let plan = plan_register(&celo_config(""), OWNER, profile.clone()).unwrap();
        assert_eq!(plan.registry.chain_id, 42220);
        let uri = decode_register(&plan.calldata).unwrap();
        assert_eq!(uri, plan.agent_uri);
        assert_eq!(profile_from_uri(&uri), Some(profile));
    }

    #[test]
    fn a_registered_config_or_an_unknown_chain_plans_nothing() {
        let profile = default_profile("Acme", &[], "Celo");
        let err = plan_register(
            &celo_config("[erc8004]\nagent_id = 0\n"),
            OWNER,
            profile.clone(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("already registered as agent #0"),
            "{err}"
        );

        let mut cfg = celo_config("");
        cfg.chain_id = 146;
        assert!(plan_register(&cfg, OWNER, profile).is_err());
    }
}
