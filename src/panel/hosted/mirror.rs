// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The hosted panel's view of bot configs.
//!
//! A hosted panel mounts no bot directory. Its `STITCH_PANEL_BOTS_DIR` is a
//! private tmpfs, and before each fleet read [`refresh`] fills it from the
//! runtime broker with the only files the broker serves: each enrolled bot's
//! `stitch.toml` and change-log files. Every read the panel already does
//! (inventory, settings, change log) then works unchanged, on copies. The copy
//! holds nothing secret, and writing to it changes nothing on the host: the one
//! hosted write, a quoting save, goes to the broker through [`ConfigStore::save`].
//!
//! The one thing the panel needs to know about the files it doesn't get is
//! whether a bot has an RFQ maker key. The broker answers yes/no
//! ([`WireBotFiles::rfq_api_key`]), and the copy records a yes as
//! [`MAKER_KEY_MARKER`] beside the config, holding no key. The panel's
//! "connected" checks read it through [`maker_key_reported`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use crate::panel::broker_config::{
    plain_relative, WireBotFiles, WireQuotingSave, WireSaved, ALLOWED_FILES,
};

/// Written into a bot's copied directory when the broker says the bot has a
/// maker key. Its contents are fixed and say nothing about the key. Only
/// [`write_bot`] writes it, so it exists only in a hosted panel's own copy; a
/// full panel's bots directory never has one.
pub const MAKER_KEY_MARKER: &str = "rfq-api.key.present";

const MAKER_KEY_MARKER_TEXT: &str = "the runtime broker reports a maker key for this bot\n";

/// Whether the hosted copy in `dir` records a maker key. False anywhere the
/// mirror didn't write the marker, including every full-profile directory.
pub fn maker_key_reported(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(MAKER_KEY_MARKER)).is_ok_and(|t| t == MAKER_KEY_MARKER_TEXT)
}

/// Where a hosted panel gets bot configs and saves quoting edits.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    async fn configs(&self) -> Result<Vec<WireBotFiles>>;
    async fn save(&self, container: &str, save: &WireQuotingSave) -> Result<WireSaved>;
}

/// A broker refusal, keeping its status so the panel answers with it.
#[derive(Debug, thiserror::Error)]
#[error("the runtime broker refused ({status}): {message}")]
pub struct StoreRefused {
    pub status: u16,
    pub message: String,
}

/// The panel's answer for a failed store call: the broker's own status when it
/// refused, 502 when it couldn't be reached.
pub fn api_error(e: &anyhow::Error) -> crate::panel::http::ApiError {
    use axum::http::StatusCode;
    match e.chain().find_map(|c| c.downcast_ref::<StoreRefused>()) {
        Some(r) => crate::panel::http::ApiError::new(
            StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY),
            r.message.clone(),
        ),
        None => crate::panel::http::ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

/// Refill the panel's copy under `root` from `store`.
pub async fn refresh(store: &dyn ConfigStore, root: &Path) -> Result<()> {
    apply_snapshot(root, &store.configs().await?)
}

/// Make the copy under `root` match one complete broker reply: each served
/// bot written, and every directory no served bot owns emptied of the files
/// the mirror writes (and removed once that leaves it empty). A bot whose
/// container went away, or whose mount stopped qualifying, so disappears from
/// the panel instead of lingering as a config-only bot.
pub fn apply_snapshot(root: &Path, bots: &[WireBotFiles]) -> Result<()> {
    for bot in bots {
        if let Some(e) = &bot.error {
            tracing::warn!(container = %bot.container, error = %e, "runtime broker didn't serve this bot's config");
        }
        write_bot(root, bot)?;
    }
    let owned: HashSet<PathBuf> = bots
        .iter()
        .filter_map(|b| b.dir.as_deref())
        .filter_map(|d| plain_relative(Path::new(d)))
        .collect();
    prune(root, Path::new(""), &owned, 0)
}

/// How deep [`prune`] looks. Bot directories are one or two levels down.
const PRUNE_DEPTH: usize = 4;

/// Under `root/rel`, clear the mirror's files out of every directory not in
/// `owned`. Only names the mirror writes are removed, and a directory only
/// when nothing else is left in it; links are never followed.
fn prune(root: &Path, rel: &Path, owned: &HashSet<PathBuf>, depth: usize) -> Result<()> {
    if depth >= PRUNE_DEPTH {
        return Ok(());
    }
    let dir = root.join(rel);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child = rel.join(entry.file_name());
        prune(root, &child, owned, depth + 1)?;
        if owned.contains(&child) {
            continue;
        }
        let path = root.join(&child);
        for name in ALLOWED_FILES.iter().copied().chain([MAKER_KEY_MARKER]) {
            put(&path.join(name), None)?;
        }
        // Fails while anything else is in it, which is the point.
        let _ = std::fs::remove_dir(&path);
    }
    Ok(())
}

/// Write one bot's served files under `root`, and remove served files the
/// broker no longer reports. Names outside [`ALLOWED_FILES`] are ignored.
pub fn write_bot(root: &Path, bot: &WireBotFiles) -> Result<()> {
    let Some(rel) = bot.dir.as_deref() else {
        return Ok(());
    };
    let rel = plain_relative(Path::new(rel))
        .ok_or_else(|| anyhow!("the broker sent an unusable bot directory {rel:?}"))?;
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for name in ALLOWED_FILES {
        put(&dir.join(name), bot.files.get(name).map(String::as_str))?;
    }
    put(
        &dir.join(MAKER_KEY_MARKER),
        bot.rfq_api_key.then_some(MAKER_KEY_MARKER_TEXT),
    )
}

/// Make `path` hold `text`, or not exist when `text` is `None`.
fn put(path: &Path, text: Option<&str>) -> Result<()> {
    match text {
        Some(text) => replace(path, text),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        },
    }
}

/// Replace `path` with `text`, skipping the write when it's already there.
/// Concurrent refreshes each stage their own temp file.
fn replace(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = PathBuf::from(format!(
        "{}.mirror-{}-{}",
        path.display(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn root(tag: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("stitch-mirror-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(&r).unwrap();
        r
    }

    #[test]
    fn only_served_names_are_written_and_dropped_ones_removed() {
        let r = root("names");
        let mut files = BTreeMap::new();
        files.insert("stitch.toml".to_string(), "a = 1".to_string());
        files.insert("change-log.jsonl".to_string(), "{}".to_string());
        files.insert("rfq-api.key".to_string(), "secret".to_string());
        let bot = WireBotFiles {
            container: "stitch-bot-a".into(),
            dir: Some("bot-a".into()),
            files,
            error: None,
            rfq_api_key: false,
        };
        write_bot(&r, &bot).unwrap();
        assert!(r.join("bot-a/stitch.toml").is_file());
        assert!(!r.join("bot-a/rfq-api.key").exists());
        let mut later = bot.clone();
        later.files.remove("change-log.jsonl");
        write_bot(&r, &later).unwrap();
        assert!(!r.join("bot-a/change-log.jsonl").exists());
    }

    #[test]
    fn the_maker_key_flag_becomes_a_marker_with_no_key_in_it() {
        let r = root("marker");
        let mut bot = WireBotFiles {
            container: "stitch-bot-a".into(),
            dir: Some("bot-a".into()),
            rfq_api_key: true,
            ..Default::default()
        };
        write_bot(&r, &bot).unwrap();
        let dir = r.join("bot-a");
        assert!(maker_key_reported(&dir));
        assert!(!dir.join(crate::setup::RFQ_API_KEY_FILE).exists());
        bot.rfq_api_key = false;
        write_bot(&r, &bot).unwrap();
        assert!(!maker_key_reported(&dir));
        assert!(!dir.join(MAKER_KEY_MARKER).exists());
        // Anything but the mirror's own text doesn't count.
        std::fs::write(dir.join(MAKER_KEY_MARKER), "yes").unwrap();
        assert!(!maker_key_reported(&dir));
    }

    #[test]
    fn bots_missing_from_a_reply_leave_the_copy_and_nothing_else_does() {
        let r = root("prune");
        let bot = |dir: &str| WireBotFiles {
            container: format!("stitch-{dir}"),
            dir: Some(dir.into()),
            files: BTreeMap::from([("stitch.toml".to_string(), "a = 1".to_string())]),
            rfq_api_key: true,
            ..Default::default()
        };
        apply_snapshot(&r, &[bot("bot-a"), bot("group/bot-b"), bot("bot-c")]).unwrap();
        std::fs::write(r.join(".panel-state.json"), "{}").unwrap();
        std::fs::write(r.join("bot-c/notes.txt"), "not the mirror's").unwrap();
        // bot-b's container went away; bot-c's mount stopped qualifying.
        let gone = WireBotFiles {
            container: "stitch-bot-c".into(),
            error: Some("no writable mount".into()),
            ..Default::default()
        };
        apply_snapshot(&r, &[bot("bot-a"), gone]).unwrap();
        assert!(r.join("bot-a/stitch.toml").is_file());
        assert!(!r.join("group").exists(), "an emptied directory is removed");
        assert!(!r.join("bot-c/stitch.toml").exists());
        assert!(!r.join("bot-c").join(MAKER_KEY_MARKER).exists());
        assert!(
            r.join("bot-c/notes.txt").is_file(),
            "only the mirror's files go"
        );
        assert!(r.join(".panel-state.json").is_file());
    }

    #[test]
    fn a_directory_that_climbs_out_is_refused() {
        let r = root("climb");
        for dir in ["../x", "/etc", "a/../../b", ""] {
            let bot = WireBotFiles {
                container: "c".into(),
                dir: Some(dir.into()),
                ..Default::default()
            };
            assert!(write_bot(&r, &bot).is_err(), "{dir}");
        }
    }
}
