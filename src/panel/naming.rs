// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Bot ids, container names, and the labels that make a container recognisable as
//! a Stitch bot.
//!
//! A bot id is the one operator-chosen string that ties everything together: the
//! container name, the config directory under the bots root, and the service name
//! in an exported compose file. It has to be safe in all three, so validation is
//! stricter than any single one of them requires.

use std::collections::HashMap;

use anyhow::{bail, Result};

/// Label carrying the bot id on containers the panel created. Presence of this
/// label is what makes a container panel-native rather than adopted.
pub const LABEL_BOT: &str = "com.textile.stitch.bot";

/// Label marking a throwaway `approve` / `dry-run` container, valued
/// `<bot>:<action>`. It gets its own key rather than reusing [`LABEL_BOT`] so
/// discovery can skip these outright: a one-shot shares the bot's image and
/// mounts, and while it runs it would otherwise show up in the fleet as a second
/// bot with lifecycle buttons on it.
pub const LABEL_ONE_SHOT: &str = "com.textile.stitch.one-shot";

/// Label recording which mount layout the panel created the container with, so a
/// later panel version can tell a deliberate layout from a legacy one.
pub const LABEL_LAYOUT: &str = "com.textile.stitch.layout";

/// Label recording the corridor id the bot was created for. The config is still
/// the source of truth; this is a hint for listing without parsing every TOML.
pub const LABEL_CORRIDOR: &str = "com.textile.stitch.corridor";

/// Image label declaring how the bot binary accounts for RFQ reservations.
///
/// Set on the *image* by `packages/stitch-bot/Dockerfile`, not on containers.
/// A bot that quotes more than one pool is only safe on a binary that reserves
/// against the wallet token rather than the corridor slug, and the panel has no
/// other way to ask: `STITCH_PANEL_BOT_IMAGE` is pinned to a `sha-*` tag in
/// production, so comparing a container against it proves the bot is running
/// what was configured, not that what was configured is new enough.
///
/// Presence is coextensive with the feature, because the label ships in the
/// same commit that made reservations token-aware — an image without it predates
/// that change.
pub const LABEL_RFQ_RESERVATIONS: &str = "com.textile.stitch.rfq-reservations";

/// The value [`LABEL_RFQ_RESERVATIONS`] carries on an image that reserves per
/// wallet token.
pub const RFQ_RESERVATIONS_TOKEN: &str = "token";

/// Image label listing the one-shot verbs the bot binary's CLI accepts,
/// comma-separated (`approve,dry-run,withdraw`).
///
/// Set on the *image* by `packages/stitch-bot/Dockerfile`. A one-shot runs the
/// bot's own image, which is only as new as the last time that bot was updated,
/// so the panel can offer a verb the binary has never heard of — the container
/// then exits 1 with `unknown argument: <verb>` and the operator reads it as a
/// failed operation rather than one that never started.
///
/// An image without the label predates it, so it can only be assumed to have
/// the verbs that predate it too (`approve`, `dry-run`). Gate a verb on this
/// only when the verb is newer than the label; see
/// [`crate::panel::http::logs`].
///
/// The label landed a few days after `withdraw` itself, so an image built in
/// that window has the verb and doesn't say so. It gets refused and told to
/// update, which costs an update it didn't strictly need — the other way round
/// is a run that dies on the command line with the wallet already claimed.
pub const LABEL_COMMANDS: &str = "com.textile.stitch.commands";

/// Whether an image's labels say its binary accepts `verb` as a command.
pub fn image_declares_command(labels: &HashMap<String, String>, verb: &str) -> bool {
    labels
        .get(LABEL_COMMANDS)
        .is_some_and(|list| list.split(',').any(|declared| declared.trim() == verb))
}

/// Compose's own service label, present on containers `docker compose` created.
/// The panel reads it to adopt an existing hand-written fleet.
pub const LABEL_COMPOSE_SERVICE: &str = "com.docker.compose.service";

/// Compose's project label, used to tell the operator which project an adopted
/// bot belongs to (and to warn that compose may fight the panel over it).
pub const LABEL_COMPOSE_PROJECT: &str = "com.docker.compose.project";

/// Longest bot id we accept. Container names allow far more, but the id also
/// becomes a directory name and a compose service key, and long ids make the
/// fleet list unreadable.
const MAX_ID_LEN: usize = 40;

/// Ids that would collide with the panel's own container or read as a path
/// traversal once joined onto the bots root.
const RESERVED_IDS: &[&str] = &["panel", "stitch-panel", "tailscale"];

/// Validate an operator-supplied bot id. Accepts lowercase letters, digits and
/// single interior hyphens — the intersection of what a Docker container name, a
/// POSIX directory name, and a compose service key all tolerate.
///
/// Rejecting here is the only defence against the id being used to escape the
/// bots root (`..`, `a/b`) or to shadow the panel's own container, so this runs
/// before any path is joined or any container is created.
pub fn validate_bot_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("bot name can't be empty");
    }
    if id.len() > MAX_ID_LEN {
        bail!("bot name can't be longer than {MAX_ID_LEN} characters");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("bot name can only use lowercase letters, digits and hyphens");
    }
    // A leading/trailing hyphen is a valid directory name but an invalid Docker
    // container name, and a doubled hyphen reads as a typo.
    if id.starts_with('-') || id.ends_with('-') {
        bail!("bot name must start and end with a letter or digit");
    }
    if id.contains("--") {
        bail!("bot name can't contain two hyphens in a row");
    }
    if RESERVED_IDS.contains(&id) {
        bail!("\"{id}\" is reserved; pick another name");
    }
    Ok(())
}

/// How many hex characters of the wallet a bot id carries. Eight matches the
/// venue's own maker ids (`stitch-42220-5cffb39d`) and how operators talk about
/// their wallets, and stays readable in the fleet list where the full address
/// would not.
const WALLET_ID_HEX: usize = 8;

/// The id for a new bot, from the wallet it will transact with: `0x` plus the
/// first [`WALLET_ID_HEX`] hex characters, lowercase. The same wallet on a
/// second chain (the common setup: one key, several networks) would collide,
/// so a taken id gets `-2`, `-3`, … rather than a refusal; the fleet list shows
/// the chain next to each bot, which is all that tells them apart anyway.
///
/// A bot is one wallet on one chain and may quote several corridors, which is
/// why it is not named after a corridor: that name would go stale the day a
/// second corridor is added. The address never does.
pub fn bot_id_for_wallet(
    address: &alloy_primitives::Address,
    taken: impl Fn(&str) -> bool,
) -> String {
    let hex = format!("{address:x}");
    let base = format!("0x{}", &hex[..WALLET_ID_HEX.min(hex.len())]);
    if !taken(&base) {
        return base;
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded suffix search terminates")
}

/// The container name for a bot id. Matches the `stitch-bot-a` convention the
/// hand-written compose files already use, so an operator's muscle memory for
/// `docker logs stitch-<id>` keeps working.
pub fn container_name(id: &str) -> String {
    format!("stitch-{id}")
}

/// Recover a bot id from a container name, for containers that carry no bot label
/// (an adopted fleet). Returns `None` when the name isn't in our shape, so the
/// caller falls back to the compose service label.
pub fn id_from_container_name(name: &str) -> Option<&str> {
    // The Docker API returns names with a leading slash.
    let name = name.strip_prefix('/').unwrap_or(name);
    let id = name.strip_prefix("stitch-")?;
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bot_is_named_after_its_wallet() {
        let addr: alloy_primitives::Address = "0x5CFfB39D2e641a0e461E8956b383fC60dB457bD5"
            .parse()
            .unwrap();
        let id = bot_id_for_wallet(&addr, |_| false);
        assert_eq!(id, "0x5cffb39d");
        validate_bot_id(&id).unwrap();
    }

    #[test]
    fn the_same_wallet_on_another_chain_gets_a_suffix_not_a_refusal() {
        let addr: alloy_primitives::Address = "0x5CFfB39D2e641a0e461E8956b383fC60dB457bD5"
            .parse()
            .unwrap();
        let existing = ["0x5cffb39d", "0x5cffb39d-2"];
        let id = bot_id_for_wallet(&addr, |c| existing.contains(&c));
        assert_eq!(id, "0x5cffb39d-3");
        validate_bot_id(&id).unwrap();
    }

    #[test]
    fn accepts_the_names_operators_actually_use() {
        for id in ["bot-a", "bot1", "cngn-usdt-bsc", "a", "x9"] {
            validate_bot_id(id).unwrap_or_else(|e| panic!("{id} should be valid: {e}"));
        }
    }

    #[test]
    fn rejects_path_traversal_and_separators() {
        // These are the cases that would let an id escape the bots root once
        // joined, so they must fail before any path is built.
        for id in ["..", ".", "a/b", "a\\b", "../../etc", "a b", "a.b"] {
            assert!(
                validate_bot_id(id).is_err(),
                "{id} must be rejected as a bot name"
            );
        }
    }

    #[test]
    fn rejects_uppercase_and_overlong_names() {
        assert!(validate_bot_id("Bot-A").is_err());
        assert!(validate_bot_id(&"a".repeat(MAX_ID_LEN + 1)).is_err());
        assert!(validate_bot_id(&"a".repeat(MAX_ID_LEN)).is_ok());
    }

    #[test]
    fn rejects_hyphen_edges_and_doubles() {
        assert!(validate_bot_id("-bot").is_err());
        assert!(validate_bot_id("bot-").is_err());
        assert!(validate_bot_id("bot--a").is_err());
    }

    #[test]
    fn rejects_the_panels_own_names() {
        // Creating a bot called "panel" would let it collide with the panel
        // container itself on a plain `docker rm`.
        assert!(validate_bot_id("panel").is_err());
        assert!(validate_bot_id("stitch-panel").is_err());
    }

    #[test]
    fn container_name_round_trips_through_the_id() {
        let name = container_name("bot-a");
        assert_eq!(name, "stitch-bot-a");
        assert_eq!(id_from_container_name(&name), Some("bot-a"));
        // The Docker API's leading slash is tolerated.
        assert_eq!(id_from_container_name("/stitch-bot-a"), Some("bot-a"));
    }

    #[test]
    fn id_from_container_name_ignores_foreign_containers() {
        assert_eq!(id_from_container_name("postgres"), None);
        assert_eq!(id_from_container_name("stitch-"), None);
    }
}
