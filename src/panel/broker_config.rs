// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Bot config access for the hosted profile, done by the runtime broker.
//!
//! A hosted panel has no bot-directory mount at all. What it shows comes from
//! [`read_bot_files`], and the one change it may make, a quoting edit, goes
//! through [`save_quoting`]. Both run in the broker, which mounts the host's
//! bots directory and the envelope file.
//!
//! Exposure: only [`ALLOWED_FILES`] ever leave the broker: `stitch.toml` and the
//! change-log files beside it. Nothing takes a file name or a path from the
//! caller. A bot's directory is derived from its enrolled container's run-dir
//! mount, must sit under the host bots directory, and every component of it
//! must be a real directory, not a symlink. Files are opened without following
//! links and must be regular, singly linked files. `rfq-api.key`, `stitch.key`,
//! `fireblocks-api.key` and everything else in the directory stay where they are.
//!
//! Writes: a [`QuotingEdit`] names only the fields the envelope can bound, so a
//! request carrying anything else fails to parse. The broker applies it to the
//! file on disk with the panel's own settings code, checks the result against
//! the envelope with [`crate::panel::hosted::envelope::check`] (the same check
//! the panel runs, so a compromised panel can't skip it), then writes and
//! records it with [`crate::changelog::journal::write_config_recorded`], which
//! is what the panel's own saves call.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::changelog::{files, Origin, Source};
use crate::panel::docker::ContainerInfo;
use crate::panel::hosted::envelope::{self, Envelope};
use crate::panel::http::ApiError;
use crate::panel::inventory::RUN_DIR;
use crate::setup::{self, SettingsView, SideSizing, SpreadEdit, SpreadKind};

/// The only files the broker reads out of a bot directory.
pub const ALLOWED_FILES: [&str; 5] = [
    "stitch.toml",
    "change-log.jsonl",
    "change-log.snapshot",
    "change-log.shipped",
    "change-log.refused",
];

/// Cap on one file, in bytes. A config is a few KB and a journal grows by a
/// record per save, so anything near this isn't what the panel expects.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Cap on all the files one fleet read carries together, in bytes. The broker
/// holds them in memory (256 MiB in the compose file) and serializes them, so
/// the per-file cap alone isn't a bound once a host has many bots. Configs are
/// read first; a bot whose change log no longer fits is served without it,
/// and one whose config doesn't fit (only possible with outsized configs) is
/// served without files, `error` saying why either way.
pub const MAX_REPLY_BYTES: u64 = 24 * 1024 * 1024;

/// The config, the one file a bot needs served.
const CONFIG_FILE: &str = "stitch.toml";

/// Where the broker finds bot directories. Built once at startup.
#[derive(Debug, Clone)]
pub struct ConfigPolicy {
    /// The bots directory as the host sees it: container mount sources start here.
    pub host_root: PathBuf,
    /// The same directory as mounted into the broker.
    pub root: PathBuf,
    /// The agreed quoting bounds. Empty when the file is missing: no edits.
    pub envelope: Envelope,
    /// Files the change log creates are handed to this uid.
    pub bot_uid: u32,
}

/// What the broker returns about one enrolled bot's config.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct WireBotFiles {
    /// The enrolled container.
    pub container: String,
    /// The bot's directory relative to the bots root, `/`-separated.
    pub dir: Option<String>,
    /// File name to contents, for the [`ALLOWED_FILES`] that exist.
    pub files: BTreeMap<String, String>,
    /// Why nothing was read, when `dir` is `None` or a file was refused, or
    /// why the change log was left out of a read over [`MAX_REPLY_BYTES`].
    pub error: Option<String>,
    /// Whether the bot's directory holds a usable RFQ maker key
    /// ([`setup::RFQ_API_KEY_FILE`]). A yes/no from [`maker_key_present`], never
    /// the key, its size or anything else about it.
    #[serde(default)]
    pub rfq_api_key: bool,
}

/// One side's spread: the form the config uses, and the value.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireSpread {
    pub abs: bool,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireSizing {
    pub total_liquidity: String,
    pub min_slice_debt: String,
    pub order_size: String,
    pub max_orders: String,
}

/// The fields a hosted quoting edit may carry: the ones the envelope bounds,
/// plus the pool it applies to. `None` leaves a field as it is. Anything else
/// in the request is refused when it's parsed.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuotingEdit {
    pub pool: usize,
    /// The pool's tokens as the client last read them, for a bot with more than
    /// one pool (an index alone can point at another pair after a renumber).
    pub collateral: Option<String>,
    pub debt: Option<String>,
    pub buy: Option<WireSpread>,
    pub sell: Option<WireSpread>,
    pub min_spread_bps: Option<String>,
    pub buy_sizing: Option<WireSizing>,
    pub sell_sizing: Option<WireSizing>,
    pub ttl_secs: Option<u64>,
    pub refresh_threshold_bps: Option<u32>,
    pub tick_interval_secs: Option<u64>,
}

/// A quoting save: the edit, and who the change log should say made it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireQuotingSave {
    pub edit: QuotingEdit,
    /// The signed-in panel actor, as the change log writes it.
    pub actor: String,
    /// The panel's name for the bot.
    pub bot: String,
}

/// A save's result: the files as they are now, and a change-log warning when
/// the config was written but couldn't be recorded.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireSaved {
    pub files: WireBotFiles,
    pub warning: Option<String>,
}

impl From<&SpreadEdit> for WireSpread {
    fn from(s: &SpreadEdit) -> Self {
        Self {
            abs: s.kind == SpreadKind::Abs,
            value: s.value.clone(),
        }
    }
}

impl From<&WireSpread> for SpreadEdit {
    fn from(s: &WireSpread) -> Self {
        Self {
            kind: if s.abs {
                SpreadKind::Abs
            } else {
                SpreadKind::Bps
            },
            value: s.value.clone(),
        }
    }
}

impl From<&SideSizing> for WireSizing {
    fn from(s: &SideSizing) -> Self {
        Self {
            total_liquidity: s.total_liquidity.clone(),
            min_slice_debt: s.min_slice_debt.clone(),
            order_size: s.order_size.clone(),
            max_orders: s.max_orders.clone(),
        }
    }
}

impl From<&WireSizing> for SideSizing {
    fn from(s: &WireSizing) -> Self {
        Self {
            total_liquidity: s.total_liquidity.clone(),
            min_slice_debt: s.min_slice_debt.clone(),
            order_size: s.order_size.clone(),
            max_orders: s.max_orders.clone(),
        }
    }
}

impl QuotingEdit {
    /// `current` with this edit's fields applied, everything else written back
    /// as it is.
    fn onto(&self, current: &SettingsView) -> setup::SettingsPatch {
        let mut patch = current.to_patch();
        if let Some(s) = &self.buy {
            patch.buy = s.into();
        }
        if let Some(s) = &self.sell {
            patch.sell = s.into();
        }
        if let Some(v) = &self.min_spread_bps {
            patch.min_spread_bps = Some(v.clone());
        }
        if let Some(s) = &self.buy_sizing {
            patch.buy_sizing = Some(s.into());
        }
        if let Some(s) = &self.sell_sizing {
            patch.sell_sizing = Some(s.into());
        }
        if let Some(v) = self.ttl_secs {
            patch.ttl_secs = Some(v);
        }
        if let Some(v) = self.refresh_threshold_bps {
            patch.refresh_threshold_bps = Some(v);
        }
        if let Some(v) = self.tick_interval_secs {
            patch.tick_interval_secs = Some(v);
        }
        patch
    }
}

/// `rel` as a path of plain names: no root, no `.` or `..`, not empty.
pub fn plain_relative(rel: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Refuse unless every component from `root` down to `root/rel` is a real
/// directory. `root` itself is the broker's own mount and trusted.
fn real_dir(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    for part in rel.components() {
        path.push(part);
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} is a symlink, so the broker won't use it",
                path.display()
            ));
        }
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
    }
    Ok(path)
}

impl ConfigPolicy {
    /// The bot's directory relative to the bots root, from its container's
    /// writable run-dir mount. Only the directory layout is served: a flat
    /// config file mounted on its own isn't.
    pub fn relative_dir(&self, container: &ContainerInfo) -> Result<PathBuf, String> {
        let mount = container
            .mounts
            .iter()
            .find(|m| m.destination == Path::new(RUN_DIR) && m.rw)
            .ok_or_else(|| {
                format!(
                    "{} has no writable {RUN_DIR} mount, so it has no bot directory the broker serves",
                    container.name
                )
            })?;
        mount
            .source
            .strip_prefix(&self.host_root)
            .ok()
            .and_then(plain_relative)
            .ok_or_else(|| {
                format!(
                    "{}'s bot directory {} is not under {}",
                    container.name,
                    mount.source.display(),
                    self.host_root.display()
                )
            })
    }

    /// The bot's directory inside the broker, checked for symlinks.
    pub fn bot_dir(&self, container: &ContainerInfo) -> Result<(PathBuf, PathBuf), String> {
        let rel = self.relative_dir(container)?;
        let dir = real_dir(&self.root, &rel)?;
        Ok((rel, dir))
    }
}

/// One allowed file out of `dir`, or `None` when it doesn't exist. Refuses
/// names outside [`ALLOWED_FILES`], links, non-regular files and oversized ones.
pub fn read_allowed(dir: &Path, name: &str) -> Result<Option<String>, String> {
    read_allowed_within(dir, name, MAX_FILE_BYTES).map_err(|e| match e {
        TooLarge(n) => format!("{name} is larger than {n} bytes"),
        Refused(why) => why,
    })
}

/// Why [`read_allowed_within`] didn't return a file.
enum NotRead {
    /// More than this many bytes, by the path's size or off the opened handle.
    TooLarge(u64),
    Refused(String),
}
use NotRead::{Refused, TooLarge};

/// [`read_allowed`] with at most `cap` bytes (never more than
/// [`MAX_FILE_BYTES`]), enforced on the opened file, not only on its size.
fn read_allowed_within(dir: &Path, name: &str, cap: u64) -> Result<Option<String>, NotRead> {
    let cap = cap.min(MAX_FILE_BYTES);
    if !ALLOWED_FILES.contains(&name) {
        return Err(Refused(format!("{name} is not a file the broker serves")));
    }
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Refused(format!("{name}: {e}"))),
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Refused(format!(
                "{name} is a symlink, so the broker won't read it"
            )));
        }
        Ok(meta) if meta.len() > cap => return Err(TooLarge(cap)),
        Ok(_) => {}
    }
    match files::read_at_most(&path, cap) {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Refused(format!("{name}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => Err(TooLarge(cap)),
        Err(e) => Err(Refused(format!("{name}: {e}"))),
    }
}

/// Largest maker key file the broker counts as a key. Real ones are one line.
const MAX_MAKER_KEY_BYTES: u64 = 4096;

/// Whether `dir` (a bot directory [`ConfigPolicy::bot_dir`] already checked)
/// holds a maker key the bot would load: [`setup::RFQ_API_KEY_FILE`] as a
/// regular, singly linked file, opened without following links, that isn't
/// blank. The name is fixed here, not taken from the caller. The contents are
/// read only to test them the way the bot's loader does (UTF-8, not blank once
/// trimmed), wiped, and never leave this function; a refusal of any kind reads
/// as "no key".
pub fn maker_key_present(dir: &Path) -> bool {
    use zeroize::Zeroize;
    let path = dir.join(setup::RFQ_API_KEY_FILE);
    let small_file = std::fs::symlink_metadata(&path)
        .is_ok_and(|m| m.file_type().is_file() && m.len() <= MAX_MAKER_KEY_BYTES);
    if !small_file {
        return false;
    }
    let Ok(Some(bytes)) = files::read_at_most(&path, MAX_MAKER_KEY_BYTES) else {
        return false;
    };
    // The loader's own test: UTF-8 text that isn't blank once trimmed
    // (`load_rfq_api_key`). Bytes it would refuse don't count as a key.
    match String::from_utf8(bytes) {
        Ok(mut text) => {
            let present = !text.trim().is_empty();
            text.zeroize();
            present
        }
        Err(e) => {
            e.into_bytes().zeroize();
            false
        }
    }
}

/// Everything the broker serves about one enrolled bot. Never fails as a whole:
/// a bot it can't read comes back with `error` set and no files.
pub fn read_bot_files(policy: &ConfigPolicy, container: &ContainerInfo) -> WireBotFiles {
    read_fleet_files(policy, std::slice::from_ref(container))
        .pop()
        .unwrap_or_default()
}

/// [`read_bot_files`] for every bot in `containers`, within [`MAX_REPLY_BYTES`]
/// together: configs first, then change logs while they fit. The budget is
/// held on the opened files, not only on their sizes, since a bot can grow or
/// swap a file in its own directory between the two.
pub fn read_fleet_files(policy: &ConfigPolicy, containers: &[ContainerInfo]) -> Vec<WireBotFiles> {
    read_fleet_files_within(policy, containers, MAX_REPLY_BYTES)
}

fn read_fleet_files_within(
    policy: &ConfigPolicy,
    containers: &[ContainerInfo],
    budget: u64,
) -> Vec<WireBotFiles> {
    let mut left = budget;
    let configs: Vec<(WireBotFiles, Option<PathBuf>)> = containers
        .iter()
        .map(|c| read_config(policy, c, &mut left))
        .collect();
    configs
        .into_iter()
        .scan(left, |left, (bot, dir)| {
            Some(add_change_log(bot, dir.as_deref(), left))
        })
        .collect()
}

/// A bot's directory and config, the config taken out of `left` bytes. The
/// directory is `None` when nothing more should be read for it.
fn read_config(
    policy: &ConfigPolicy,
    container: &ContainerInfo,
    left: &mut u64,
) -> (WireBotFiles, Option<PathBuf>) {
    let mut out = WireBotFiles {
        container: container.name.clone(),
        ..Default::default()
    };
    let (rel, dir) = match policy.bot_dir(container) {
        Ok(found) => found,
        Err(e) => {
            out.error = Some(e);
            return (out, None);
        }
    };
    out.dir = Some(rel.to_string_lossy().replace('\\', "/"));
    out.rfq_api_key = maker_key_present(&dir);
    match read_allowed_within(&dir, CONFIG_FILE, *left) {
        Ok(Some(text)) => {
            *left -= text.len() as u64;
            out.files.insert(CONFIG_FILE.to_string(), text);
            (out, Some(dir))
        }
        Ok(None) => (out, Some(dir)),
        Err(TooLarge(cap)) if cap < MAX_FILE_BYTES => {
            out.error = Some(format!(
                "config not served: the fleet's configs are over the {} MiB a read carries",
                MAX_REPLY_BYTES / (1024 * 1024)
            ));
            (out, None)
        }
        Err(TooLarge(cap)) => {
            out.error = Some(format!("{CONFIG_FILE} is larger than {cap} bytes"));
            (out, None)
        }
        Err(Refused(e)) => {
            out.error = Some(e);
            (out, None)
        }
    }
}

/// `bot` with its change-log files added, if they fit in `left` bytes. A file
/// the broker refuses clears the bot's files, as it always has: what's there
/// isn't what it should be.
fn add_change_log(mut bot: WireBotFiles, dir: Option<&Path>, left: &mut u64) -> WireBotFiles {
    let Some(dir) = dir else {
        return bot;
    };
    let over_budget = |mut bot: WireBotFiles| {
        bot.files.retain(|name, _| name == CONFIG_FILE);
        bot.error = Some(format!(
            "change log not served: the fleet's change logs are over the {} MiB a read carries",
            MAX_REPLY_BYTES / (1024 * 1024)
        ));
        bot
    };
    let logs = ALLOWED_FILES.iter().filter(|n| **n != CONFIG_FILE);
    // Sizes by path first, so an obvious overrun reads nothing. The reads
    // below hold the budget on the opened files too.
    let size: u64 = logs
        .clone()
        .map(|name| std::fs::symlink_metadata(dir.join(name)).map_or(0, |m| m.len()))
        .sum();
    if size > *left {
        return over_budget(bot);
    }
    let mut room = *left;
    for name in logs {
        match read_allowed_within(dir, name, room) {
            Ok(Some(text)) => {
                room -= text.len() as u64;
                bot.files.insert(name.to_string(), text);
            }
            Ok(None) => {}
            Err(TooLarge(cap)) if cap < MAX_FILE_BYTES => return over_budget(bot),
            Err(TooLarge(cap)) => {
                bot.error = Some(format!("{name} is larger than {cap} bytes"));
                bot.files.clear();
                return bot;
            }
            Err(Refused(e)) => {
                bot.error = Some(e);
                bot.files.clear();
                return bot;
            }
        }
    }
    *left = room;
    bot
}

/// How many times a save re-reads and re-applies its edit when the file
/// changed under it before giving up with 409.
const SAVE_ATTEMPTS: usize = 3;

/// Apply a quoting edit to the bot's `stitch.toml`, inside the envelope, and
/// record it in the change log. Errors carry the status the broker answers with.
///
/// The edit is merged onto the file outside the change-log lock, so the write
/// only goes ahead if the file still reads what the merge started from
/// ([`write_config_recorded_if_unchanged`]). If it doesn't, another save or a
/// host edit landed in between: the edit is applied again onto the new file,
/// envelope check included, so whatever changed there is kept.
///
/// [`write_config_recorded_if_unchanged`]: crate::changelog::journal::write_config_recorded_if_unchanged
pub fn save_quoting(
    policy: &ConfigPolicy,
    container: &ContainerInfo,
    save: &WireQuotingSave,
    now_ms: u64,
) -> Result<WireSaved, ApiError> {
    let (_, dir) = policy.bot_dir(container).map_err(ApiError::conflict)?;
    let path = dir.join("stitch.toml");
    let origin = Origin {
        source: Source::PanelSettings,
        actor: bounded(&save.actor),
        bot: Some(bounded(&save.bot)),
    };
    for _ in 0..SAVE_ATTEMPTS {
        let current_toml = read_allowed(&dir, "stitch.toml")
            .map_err(ApiError::conflict)?
            .ok_or_else(|| ApiError::conflict(format!("{} has no stitch.toml", container.name)))?;
        let edited = apply_quoting(policy, &current_toml, &save.edit)?;
        let written = crate::changelog::journal::write_config_recorded_if_unchanged(
            &path,
            &current_toml,
            &edited,
            &origin,
            now_ms,
            Some(policy.bot_uid),
        )
        .map_err(|e| ApiError::internal(&e))?;
        let Some(recorded) = written else {
            tracing::info!(container = %container.name, "stitch.toml changed during a quoting save; applying it again");
            continue;
        };
        // The broker writes as root. Hand the new file to the bot's uid, the
        // owner it had when the panel wrote it, without following a link.
        #[cfg(unix)]
        if let Err(e) = std::os::unix::fs::lchown(&path, Some(policy.bot_uid), None) {
            tracing::warn!(path = %path.display(), "couldn't hand the saved config to the bot's uid: {e}");
        }
        return Ok(WireSaved {
            files: read_bot_files(policy, container),
            warning: recorded.warning,
        });
    }
    Err(ApiError::conflict(format!(
        "{}'s stitch.toml kept changing during the save; reload and try again",
        container.name
    )))
}

/// `current_toml` with `edit` applied, checked against the envelope.
fn apply_quoting(
    policy: &ConfigPolicy,
    current_toml: &str,
    edit: &QuotingEdit,
) -> Result<String, ApiError> {
    crate::panel::http::settings::confirm_pool_identity(
        current_toml,
        edit.pool,
        edit.collateral.as_deref(),
        edit.debt.as_deref(),
    )?;
    let current =
        setup::read_settings_at(current_toml, edit.pool).map_err(ApiError::bad_request)?;
    let edited = setup::apply_settings(current_toml, &edit.onto(&current))
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let after = setup::read_settings_at(&edited, edit.pool).map_err(ApiError::bad_request)?;
    envelope::check(&current, &after, &policy.envelope)
        .map_err(|reason| ApiError::new(StatusCode::FORBIDDEN, reason))?;
    Ok(edited)
}

/// A label the panel sent for the change log, kept short and on one line.
fn bounded(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::fake::{container, dir_layout_mounts};
    use crate::panel::docker::ContainerState;

    const TEST_KEY: &str = crate::panel::http::testkit::TEST_KEY;

    struct Host {
        root: PathBuf,
        policy: ConfigPolicy,
        bot: ContainerInfo,
    }

    fn host(tag: &str, envelope: &str) -> Host {
        let root =
            std::env::temp_dir().join(format!("stitch-broker-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(root.join("bot-a"), corridor, TEST_KEY).unwrap();
        std::fs::write(
            root.join("bot-a").join(setup::RFQ_API_KEY_FILE),
            "tx_live_secret",
        )
        .unwrap();
        std::fs::write(root.join("bot-a").join("fireblocks-api.key"), "fb-secret").unwrap();
        let mut bot = container("stitch-bot-a", ContainerState::Running);
        bot.mounts = dir_layout_mounts(&root.join("bot-a").display().to_string());
        let policy = ConfigPolicy {
            host_root: root.clone(),
            root: root.clone(),
            envelope: Envelope::parse(envelope).unwrap(),
            bot_uid: nix_uid(),
        };
        Host { root, policy, bot }
    }

    fn nix_uid() -> u32 {
        // SAFETY: getuid has no preconditions and can't fail.
        unsafe { libc::getuid() }
    }

    const ENVELOPE: &str =
        r#"{"spread_bps": {"min": 5, "max": 300}, "ttl_secs": {"min": 30, "max": 600}}"#;

    fn save(edit: QuotingEdit) -> WireQuotingSave {
        WireQuotingSave {
            edit,
            actor: "panel password (shared)".into(),
            bot: "bot-a".into(),
        }
    }

    fn current(h: &Host) -> SettingsView {
        let text = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        setup::read_settings_at(&text, 0).unwrap()
    }

    #[test]
    fn a_fleet_read_serves_every_config_and_only_the_change_logs_that_fit() {
        let h = host("budget", ENVELOPE);
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(h.root.join("bot-b"), corridor, TEST_KEY).unwrap();
        let mut b = container("stitch-bot-b", ContainerState::Running);
        b.mounts = dir_layout_mounts(&h.root.join("bot-b").display().to_string());
        let log = "x".repeat(4096);
        for dir in ["bot-a", "bot-b"] {
            std::fs::write(h.root.join(dir).join("change-log.jsonl"), &log).unwrap();
        }
        let configs: u64 = ["bot-a", "bot-b"]
            .iter()
            .map(|d| {
                std::fs::metadata(h.root.join(d).join(CONFIG_FILE))
                    .unwrap()
                    .len()
            })
            .sum();
        // Room for both configs and one change log.
        let bots = [h.bot.clone(), b];
        let served = read_fleet_files_within(&h.policy, &bots, configs + 4096 + 100);
        assert!(served.iter().all(|b| b.files.contains_key(CONFIG_FILE)));
        assert_eq!(served[0].files.get("change-log.jsonl"), Some(&log));
        assert_eq!(served[0].error, None);
        assert!(!served[1].files.contains_key("change-log.jsonl"));
        assert!(served[1]
            .error
            .as_deref()
            .unwrap()
            .contains("change log not served"));
        // Room for one config only: the second bot gets no files and a reason.
        let first = std::fs::metadata(h.root.join("bot-a").join(CONFIG_FILE))
            .unwrap()
            .len();
        let served = read_fleet_files_within(&h.policy, &bots, first);
        assert_eq!(served[0].files.len(), 1, "{:?}", served[0].files.keys());
        assert!(served[1].files.is_empty());
        assert!(served[1]
            .error
            .as_deref()
            .unwrap()
            .contains("config not served"));
    }

    #[test]
    fn the_maker_key_is_reported_as_a_yes_or_no_and_nothing_else() {
        let h = host("maker-key", ENVELOPE);
        let dir = h.root.join("bot-a");
        let key = dir.join(setup::RFQ_API_KEY_FILE);
        let served = read_bot_files(&h.policy, &h.bot);
        assert!(served.rfq_api_key);
        let wire = serde_json::to_string(&served).unwrap();
        assert!(!wire.contains("tx_live_secret"), "{wire}");
        assert!(wire.contains("\"rfq_api_key\":true"), "{wire}");
        assert!(!served.files.contains_key(setup::RFQ_API_KEY_FILE));

        // Blank, a symlink, a hard link, a directory, oversized or missing: no.
        std::fs::write(&key, " \n").unwrap();
        assert!(!maker_key_present(&dir));
        let outside = h.root.join("outside.key");
        std::fs::write(&outside, "tx_live_elsewhere").unwrap();
        std::fs::remove_file(&key).unwrap();
        std::os::unix::fs::symlink(&outside, &key).unwrap();
        assert!(!maker_key_present(&dir));
        std::fs::remove_file(&key).unwrap();
        std::fs::hard_link(&outside, &key).unwrap();
        assert!(!maker_key_present(&dir));
        std::fs::remove_file(&key).unwrap();
        std::fs::create_dir(&key).unwrap();
        assert!(!maker_key_present(&dir));
        std::fs::remove_dir(&key).unwrap();
        std::fs::write(&key, "k".repeat(MAX_MAKER_KEY_BYTES as usize + 1)).unwrap();
        assert!(!maker_key_present(&dir));
        // What the bot's loader would refuse doesn't count either: bytes that
        // aren't UTF-8, and text that is only (Unicode) whitespace.
        std::fs::write(&key, [0xff, 0xfe, b'k', b'e', b'y']).unwrap();
        assert!(!maker_key_present(&dir));
        std::fs::write(&key, "\u{3000}\u{2003}\n").unwrap();
        assert!(!maker_key_present(&dir));
        std::fs::write(&key, " tx_live_x \n").unwrap();
        assert!(maker_key_present(&dir));
        std::fs::remove_file(&key).unwrap();
        assert!(!read_bot_files(&h.policy, &h.bot).rfq_api_key);
    }

    #[test]
    fn the_size_cap_is_held_on_the_opened_file() {
        let h = host("cap", ENVELOPE);
        let dir = h.root.join("bot-a");
        let log = dir.join("change-log.jsonl");
        std::fs::write(&log, "x".repeat(100)).unwrap();
        assert!(matches!(
            read_allowed_within(&dir, "change-log.jsonl", 100),
            Ok(Some(t)) if t.len() == 100
        ));
        assert!(matches!(
            read_allowed_within(&dir, "change-log.jsonl", 99),
            Err(TooLarge(99))
        ));
        // The handle check on its own, as when a file grows after the size
        // check on its path: never more than the cap plus one byte is read.
        let err = files::read_at_most(&log, 10).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::FileTooLarge);
        assert_eq!(files::read_at_most(&log, 100).unwrap().unwrap().len(), 100);
    }

    #[test]
    fn only_the_config_and_its_change_log_are_ever_read() {
        let h = host("read", ENVELOPE);
        let files = read_bot_files(&h.policy, &h.bot);
        assert_eq!(files.error, None);
        assert_eq!(files.dir.as_deref(), Some("bot-a"));
        assert!(files.files.contains_key("stitch.toml"));
        assert!(files
            .files
            .keys()
            .all(|k| ALLOWED_FILES.contains(&k.as_str())));
        let all = serde_json::to_string(&files).unwrap();
        assert!(
            !all.contains("tx_live_secret") && !all.contains("fb-secret"),
            "{all}"
        );
    }

    #[test]
    fn secret_names_and_traversal_are_refused_by_name() {
        let h = host("names", ENVELOPE);
        let dir = h.root.join("bot-a");
        for name in [
            "rfq-api.key",
            "stitch.key",
            "fireblocks-api.key",
            "stitch.env",
            "../bot-a/stitch.toml",
            "/etc/passwd",
            "change-log.lock",
            "",
        ] {
            assert!(read_allowed(&dir, name).is_err(), "{name}");
        }
    }

    #[test]
    fn a_bot_directory_outside_the_root_or_with_dot_dot_is_refused() {
        let h = host("outside", ENVELOPE);
        for source in [
            "/etc".to_string(),
            format!("{}/../elsewhere", h.root.display()),
            h.root.display().to_string(),
        ] {
            let mut bot = h.bot.clone();
            bot.mounts = dir_layout_mounts(&source);
            let files = read_bot_files(&h.policy, &bot);
            assert!(files.error.is_some() && files.files.is_empty(), "{source}");
            assert!(save_quoting(&h.policy, &bot, &save(QuotingEdit::default()), 0).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_configs_and_directories_are_refused() {
        let h = host("links", ENVELOPE);
        // A config that's a link to a secret.
        let dir = h.root.join("bot-a");
        std::fs::rename(dir.join("stitch.toml"), dir.join("real.toml")).unwrap();
        std::os::unix::fs::symlink(dir.join(setup::RFQ_API_KEY_FILE), dir.join("stitch.toml"))
            .unwrap();
        let files = read_bot_files(&h.policy, &h.bot);
        assert!(files.error.is_some() && files.files.is_empty(), "{files:?}");
        assert!(save_quoting(&h.policy, &h.bot, &save(QuotingEdit::default()), 0).is_err());
        // A bot directory that's a link.
        std::os::unix::fs::symlink(&dir, h.root.join("bot-link")).unwrap();
        let mut linked = h.bot.clone();
        linked.mounts = dir_layout_mounts(&h.root.join("bot-link").display().to_string());
        assert!(read_bot_files(&h.policy, &linked).error.is_some());
    }

    #[test]
    fn an_in_envelope_edit_round_trips_and_lands_in_the_change_log() {
        let h = host("save", ENVELOPE);
        let before = current(&h);
        assert_eq!(before.buy.kind, SpreadKind::Bps);
        let spread = if before.buy.value == "42" { "43" } else { "42" };
        let saved = save_quoting(
            &h.policy,
            &h.bot,
            &save(QuotingEdit {
                buy: Some(WireSpread {
                    abs: false,
                    value: spread.into(),
                }),
                ..Default::default()
            }),
            1_700_000_000_000,
        )
        .unwrap();
        let after = current(&h);
        assert_eq!(after.buy.value, spread);
        assert_eq!(after.sell, before.sell);
        assert_eq!(saved.warning, None);
        let journal = saved.files.files.get("change-log.jsonl").expect("recorded");
        let record = journal
            .lines()
            .find(|l| l.contains("\"source\":\"panel.settings\""))
            .unwrap_or_else(|| panic!("no panel record in {journal}"));
        assert!(record.contains("panel password (shared)"), "{record}");
        assert!(
            record.contains("\"parameter\":\"buy_offset_bps\""),
            "{record}"
        );
        assert!(record.contains("\"bot\":\"bot-a\""), "{record}");
        assert_eq!(
            saved.files.files.get("stitch.toml"),
            Some(&std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap())
        );
    }

    #[test]
    fn edits_outside_the_envelope_are_refused_and_nothing_is_written() {
        let h = host("refuse", ENVELOPE);
        let text = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        for edit in [
            // Out of bounds.
            QuotingEdit {
                ttl_secs: Some(5000),
                ..Default::default()
            },
            // No bound for it at all.
            QuotingEdit {
                tick_interval_secs: Some(7),
                ..Default::default()
            },
        ] {
            let err = save_quoting(&h.policy, &h.bot, &save(edit.clone()), 0).unwrap_err();
            assert_eq!(
                err.status,
                StatusCode::FORBIDDEN,
                "{edit:?}: {}",
                err.message
            );
        }
        assert_eq!(
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
            text
        );
        assert!(!h.root.join("bot-a/change-log.jsonl").exists());
    }

    #[test]
    fn a_broker_on_the_shipped_example_envelope_refuses_every_quoting_edit() {
        let h = host(
            "example-envelope",
            include_str!("../../deploy/hosted-envelope.example.json"),
        );
        let text = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        let mut by_envelope = 0;
        let spread = |v: &str| WireSpread {
            abs: false,
            value: v.into(),
        };
        let buy_orders = |v: &str| {
            let mut s = WireSizing::from(&current(&h).buy_sizing);
            s.max_orders = v.into();
            s
        };
        let sell_slice = |v: &str| {
            let mut s = WireSizing::from(&current(&h).sell_sizing);
            s.min_slice_debt = v.into();
            s
        };
        // The minimum spread isn't here: this config has no absolute side, so
        // the file drops the floor and an edit to it changes nothing. The
        // envelope's own tests cover it.
        for value in ["0", "45"] {
            let n: u64 = value.parse().unwrap();
            for edit in [
                QuotingEdit {
                    buy: Some(spread(value)),
                    ..Default::default()
                },
                QuotingEdit {
                    sell: Some(spread(value)),
                    ..Default::default()
                },
                QuotingEdit {
                    ttl_secs: Some(n),
                    ..Default::default()
                },
                QuotingEdit {
                    refresh_threshold_bps: Some(n as u32),
                    ..Default::default()
                },
                QuotingEdit {
                    tick_interval_secs: Some(n),
                    ..Default::default()
                },
                QuotingEdit {
                    buy_sizing: Some(buy_orders(value)),
                    ..Default::default()
                },
                QuotingEdit {
                    sell_sizing: Some(sell_slice(value)),
                    ..Default::default()
                },
            ] {
                let err = save_quoting(&h.policy, &h.bot, &save(edit.clone()), 0).unwrap_err();
                // Refused either way: by the envelope (403), or for some zeros
                // already by config validation (400) before it is consulted.
                match err.status {
                    StatusCode::FORBIDDEN => {
                        assert!(
                            err.message.contains("no bound"),
                            "{edit:?}: {}",
                            err.message
                        );
                        by_envelope += 1;
                    }
                    // Config validation got there first; still nothing written.
                    StatusCode::BAD_REQUEST => {}
                    other => panic!("{edit:?}: {other} {}", err.message),
                }
            }
        }
        assert_eq!(
            std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap(),
            text
        );
        assert!(!h.root.join("bot-a/change-log.jsonl").exists());
        assert!(by_envelope >= 7, "the envelope decided only {by_envelope}");
    }

    #[test]
    fn fields_outside_the_quoting_set_do_not_parse() {
        for body in [
            r#"{"edit":{"pool":0,"rpc_url":"https://attacker"},"actor":"a","bot":"b"}"#,
            r#"{"edit":{"pool":0,"vault_address":"0x00"},"actor":"a","bot":"b"}"#,
            r#"{"edit":{"pool":0},"actor":"a","bot":"b","toml":"x"}"#,
            r#"{"edit":{"pool":0,"buy":{"abs":false,"value":"5","kind":"x"}},"actor":"a","bot":"b"}"#,
        ] {
            assert!(
                serde_json::from_str::<WireQuotingSave>(body).is_err(),
                "{body}"
            );
        }
    }
}
