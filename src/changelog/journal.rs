// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The per-bot change log on disk, beside `stitch.toml` (see [`files`] for
//! the file names and how they're opened).
//!
//! The snapshot is what makes hand edits visible. Whoever touches the config
//! next (the bot starting, or a panel save) compares the file with the
//! snapshot first and records any difference as a host edit before doing
//! anything of its own. Both sides are read with the same extractor, so a
//! release that tracks a new parameter doesn't invent a diff.
//!
//! Everything that compares and records holds the lock file, so the bot
//! starting and a panel save can't both record the same hand edit. The
//! panel's own per-config lock only covers the panel racing itself.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::files::{self, LogFile};
use super::params::{self, ParamMap};
use super::record::{
    random_uuid, sha256_hex, to_records, ChangeRecord, Hashes, Origin, Source, Timing,
};

/// An exclusive hold on one bot's change log. Released on drop.
#[derive(Debug)]
pub struct Journal {
    config_path: PathBuf,
    /// The uid files this journal creates are handed to: the bot's, when a
    /// root panel is the one writing. `None` leaves ownership alone.
    owner: Option<u32>,
    _lock: std::fs::File,
}

impl Journal {
    /// Take the lock, waiting for whoever holds it. Holders only ever do
    /// local file work, so the wait is short.
    pub fn open(config_path: &Path, owner: Option<u32>) -> Result<Self> {
        let lock_path = files::path(config_path, LogFile::Lock);
        let lock = files::open_lock(&lock_path, owner)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        Ok(Self {
            config_path: config_path.to_path_buf(),
            owner,
            _lock: lock,
        })
    }

    fn file(&self, file: LogFile) -> PathBuf {
        files::path(&self.config_path, file)
    }
}

/// The config as of the last record, or `None` before the first one.
pub fn read_snapshot(journal: &Journal) -> Result<Option<String>> {
    let path = journal.file(LogFile::Snapshot);
    files::read_text(&path).with_context(|| format!("reading {}", path.display()))
}

/// Staged and renamed into place by the same writer as `stitch.toml`, which
/// never writes through a link. Root-owned when the panel writes it; the bot
/// only reads it and replaces it by rename, which its directory allows.
fn write_snapshot(journal: &Journal, text: &str) -> Result<()> {
    crate::setup::write_file_atomic(&journal.file(LogFile::Snapshot), text)
}

/// Append records as one write, then sync, so a crash leaves either all of a
/// change set or none of it plus at most one torn last line (which the
/// shipper skips).
pub fn append(journal: &Journal, records: &[ChangeRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let lines = records
        .iter()
        .map(|r| serde_json::to_string(r).map(|line| line + "\n"))
        .collect::<Result<String, _>>()?;
    let path = journal.file(LogFile::Journal);
    let mut file = files::open_append(&path, journal.owner)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(lines.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

/// Every complete record in a bot's journal, oldest first. Unreadable lines
/// are skipped; see [`super::ship::pending`] for why one can exist.
pub fn read_all(config_path: &Path) -> Result<Vec<ChangeRecord>> {
    let path = files::path(config_path, LogFile::Journal);
    let text = files::read_text(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .unwrap_or_default();
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// What the file on disk says that the snapshot doesn't.
///
/// - No snapshot (or one that no longer parses): a baseline, every value as
///   found, so the log starts from a known state.
/// - Same text: nothing.
/// - Different text: the diff, as a host edit dated by the file's mtime.
///
/// Empty when `current` isn't a config: the bot won't start on it and the
/// panel won't write one, so there is nothing in effect to record. The
/// snapshot is left alone so the next reader tries again.
pub fn catch_up_records(
    snapshot: Option<&str>,
    current: &str,
    file_modified_ms: u64,
    now_ms: u64,
    bot: Option<String>,
    next_id: impl FnMut() -> String,
) -> Vec<ChangeRecord> {
    let Some((current_params, chain_id)) = read(current) else {
        return Vec::new();
    };
    let current_hash = Some(sha256_hex(current));
    let previous = snapshot.and_then(|s| read(s).map(|(p, _)| (s, p)));
    match previous {
        None => to_records(
            &params::diff(&ParamMap::new(), &current_params),
            &Origin::baseline(bot),
            Timing::at(now_ms),
            chain_id,
            &Hashes {
                before: None,
                after: current_hash,
            },
            next_id,
        ),
        Some((text, _)) if text == current => Vec::new(),
        Some((text, previous_params)) => to_records(
            &params::diff(&previous_params, &current_params),
            &Origin::host_edit(bot),
            Timing {
                changed_at_ms: file_modified_ms,
                detected_at_ms: Some(now_ms),
            },
            chain_id,
            &Hashes {
                before: Some(sha256_hex(text)),
                after: current_hash,
            },
            next_id,
        ),
    }
}

fn read(text: &str) -> Option<(ParamMap, u64)> {
    let cfg = toml::from_str::<crate::config::Config>(text).ok()?;
    Some((params::extract(&cfg), cfg.chain_id))
}

/// The config hash the newest record in the journal left the file at.
fn last_recorded_hash(journal: &Journal) -> Result<Option<String>> {
    Ok(read_all(&journal.config_path)?
        .into_iter()
        .next_back()
        .and_then(|r| r.config_sha256_after))
}

/// Record whatever changed in `current` since the snapshot, then move the
/// snapshot to it. Returns how many records it wrote. See
/// [`catch_up_records`].
pub fn catch_up(
    journal: &Journal,
    current: &str,
    file_modified_ms: u64,
    now_ms: u64,
    bot: Option<String>,
) -> Result<usize> {
    let snapshot = read_snapshot(journal)?;
    if snapshot.as_deref() == Some(current) || read(current).is_none() {
        return Ok(0);
    }
    // The journal already ends at this exact file: an earlier record landed
    // but its snapshot write didn't. Repair the snapshot instead of recording
    // the same change again as a host edit.
    if last_recorded_hash(journal)?.as_deref() == Some(sha256_hex(current).as_str()) {
        write_snapshot(journal, current)?;
        return Ok(0);
    }
    let records = catch_up_records(
        snapshot.as_deref(),
        current,
        file_modified_ms,
        now_ms,
        bot,
        random_uuid,
    );
    append(journal, &records)?;
    // Also when nothing tracked changed (a comment, an RPC URL): the snapshot
    // follows the file so the next comparison starts from here.
    write_snapshot(journal, current)?;
    Ok(records.len())
}

/// Record the move from `before` to `after` under `origin` and make `after`
/// the snapshot. `before` is `None` for a config that didn't exist.
///
/// `after` that isn't a config records nothing and leaves the snapshot, so
/// the next catch-up compares against the last state that was one.
///
/// Journal first, snapshot second: a snapshot that moved without its records
/// would hide the change for good, while records without their snapshot are
/// repaired by the next catch-up (see [`catch_up`]).
pub fn record(
    journal: &Journal,
    before: Option<&str>,
    after: &str,
    origin: &Origin,
    now_ms: u64,
) -> Result<usize> {
    let Some((after_params, chain_id)) = read(after) else {
        anyhow::bail!("the new config doesn't parse, so its parameters can't be read");
    };
    let before_params = before.and_then(read).map(|(p, _)| p).unwrap_or_default();
    let records = to_records(
        &params::diff(&before_params, &after_params),
        origin,
        Timing::at(now_ms),
        chain_id,
        &Hashes {
            before: before.map(sha256_hex),
            after: Some(sha256_hex(after)),
        },
        random_uuid,
    );
    append(journal, &records)?;
    write_snapshot(journal, after)?;
    Ok(records.len())
}

/// The outcome of a recorded write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// Records written for this save, not counting a host edit caught first.
    pub records: usize,
    /// Host-edit or baseline records written before this save's own.
    pub caught_up: usize,
    /// Set when the config was written but recording it failed. The snapshot
    /// is then behind the file, so the next catch-up records the change as a
    /// host edit: it is late and unattributed, not lost.
    pub warning: Option<String>,
}

/// Write a config and record what the write changed, in that order, under
/// the change-log lock.
///
/// The write is the operation; recording is bookkeeping about it. A change
/// log that can't be opened or written doesn't stop the save, it comes back
/// as [`Recorded::warning`]. Whether a bot may *run* without its change log is
/// decided at startup ([`record_startup`]), where it can refuse.
///
/// `owner` is the uid files the log creates are handed to (the bot's, from a
/// root panel); see [`Journal::open`].
pub fn write_config_recorded(
    config_path: &Path,
    next: &str,
    origin: &Origin,
    now_ms: u64,
    owner: Option<u32>,
) -> Result<Recorded> {
    // Lock first, then read: a hand edit landing between the two would
    // otherwise be compared against a stale copy and recorded wrong.
    let journal = Journal::open(config_path, owner);
    let current = std::fs::read_to_string(config_path).ok();
    let prepared = journal.and_then(|journal| {
        let caught_up = match &current {
            Some(text) => catch_up(
                &journal,
                text,
                modified_ms(config_path).unwrap_or(now_ms),
                now_ms,
                origin.bot.clone(),
            )?,
            None => 0,
        };
        Ok((journal, caught_up))
    });

    crate::setup::write_toml_atomic(config_path, next)?;

    let outcome = prepared.and_then(|(journal, caught_up)| {
        record(&journal, current.as_deref(), next, origin, now_ms).map(|n| (n, caught_up))
    });
    Ok(match outcome {
        Ok((records, caught_up)) => Recorded {
            records,
            caught_up,
            warning: None,
        },
        Err(e) => {
            tracing::error!(
                path = %config_path.display(),
                "config saved, but the change log couldn't record it: {e:#}"
            );
            Recorded {
                records: 0,
                caught_up: 0,
                warning: Some(format!(
                    "The change log couldn't record this save ({e:#}). The bot records it as \
                     an unattributed host edit when it next starts."
                )),
            }
        }
    })
}

/// Catch the change log up with the config the bot is about to run, and
/// return how many records that wrote.
///
/// This is the one place no config change can get past: Stitch reads its
/// config once, here, so anything that takes effect is on this path. `text`
/// is the exact text the bot parsed.
pub fn record_startup(config_path: &Path, text: &str, now_ms: u64) -> Result<usize> {
    let journal = Journal::open(config_path, None)?;
    catch_up(
        &journal,
        text,
        modified_ms(config_path).unwrap_or(now_ms),
        now_ms,
        None,
    )
}

/// A file's mtime in unix milliseconds.
pub fn modified_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// The origin of a panel action on a named bot.
pub fn panel_origin(source: Source, actor: &str, bot: &str) -> Origin {
    Origin {
        source,
        actor: actor.to_string(),
        bot: Some(bot.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changelog::record::{BASELINE_ACTOR, HOST_EDIT_ACTOR};

    const CONFIG: &str = include_str!("testdata/zaru.toml");

    fn temp_config(tag: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stitch-changelog-{}-{tag}-{}",
            std::process::id(),
            random_uuid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stitch.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn ids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("id-{n}")
        }
    }

    #[test]
    fn a_bot_seen_for_the_first_time_gets_a_baseline_of_its_set_values() {
        let records = catch_up_records(None, CONFIG, 5, 10, None, ids());
        assert!(!records.is_empty());
        assert!(records.iter().all(|r| r.source == "baseline"));
        assert!(records.iter().all(|r| r.actor == BASELINE_ACTOR));
        assert!(records.iter().all(|r| r.before.is_none()));
        // Unset values aren't in a baseline: nothing changed from nothing.
        assert!(records.iter().all(|r| r.after.is_some()));
        assert!(records.iter().any(|r| r.parameter == "buy_offset_bps"));
    }

    #[test]
    fn an_unchanged_file_records_nothing() {
        assert!(catch_up_records(Some(CONFIG), CONFIG, 5, 10, None, ids()).is_empty());
    }

    #[test]
    fn a_hand_edit_is_recorded_as_one_and_dated_by_the_file() {
        let edited = CONFIG.replace("sell_offset_bps = 45", "sell_offset_bps = 90");
        let records = catch_up_records(Some(CONFIG), &edited, 1_000, 9_000, None, ids());
        assert_eq!(records.len(), 1, "{records:?}");
        let r = &records[0];
        assert_eq!(r.parameter, "sell_offset_bps");
        assert_eq!(r.before.as_deref(), Some("45"));
        assert_eq!(r.after.as_deref(), Some("90"));
        assert_eq!(r.actor, HOST_EDIT_ACTOR);
        assert_eq!(r.source, "host-edit");
        assert_eq!(r.changed_at, "1970-01-01T00:00:01.000Z");
        assert_eq!(r.detected_at.as_deref(), Some("1970-01-01T00:00:09.000Z"));
        assert_eq!(r.config_sha256_before, Some(sha256_hex(CONFIG)));
        assert_eq!(r.config_sha256_after, Some(sha256_hex(&edited)));
    }

    #[test]
    fn an_edit_that_touches_no_tracked_parameter_records_nothing() {
        let edited = CONFIG.replace("https://base.example", "https://base-2.example");
        assert!(catch_up_records(Some(CONFIG), &edited, 1, 2, None, ids()).is_empty());
    }

    #[test]
    fn a_broken_file_records_nothing_until_it_parses() {
        assert!(catch_up_records(Some(CONFIG), "chain_id = ", 1, 2, None, ids()).is_empty());
    }

    #[test]
    fn startup_records_a_hand_edit_once_and_moves_the_snapshot() {
        let path = temp_config("startup", CONFIG);
        let first = record_startup(&path, CONFIG, 1_000).unwrap();
        assert!(first > 0, "baseline on first start");
        let baseline = read_all(&path).unwrap().len();

        let edited = CONFIG.replace("buy_offset_bps = 40", "buy_offset_bps = 15");
        std::fs::write(&path, &edited).unwrap();
        assert_eq!(record_startup(&path, &edited, 2_000).unwrap(), 1);
        // A restart on the same file finds nothing new.
        assert_eq!(record_startup(&path, &edited, 3_000).unwrap(), 0);

        let all = read_all(&path).unwrap();
        assert_eq!(all.len(), baseline + 1);
        let last = all.last().unwrap();
        assert_eq!(last.actor, HOST_EDIT_ACTOR);
        assert_eq!(last.after.as_deref(), Some("15"));
    }

    #[test]
    fn a_recorded_write_attributes_its_change_and_a_hand_edit_before_it_separately() {
        let path = temp_config("write", CONFIG);
        record_startup(&path, CONFIG, 1_000).unwrap();
        let before = read_all(&path).unwrap().len();

        // Someone edits the file on the host...
        let hand = CONFIG.replace("sell_offset_bps = 45", "sell_offset_bps = 60");
        std::fs::write(&path, &hand).unwrap();
        // ...then an operator saves a spread from the panel.
        let saved = hand.replace("buy_offset_bps = 40", "buy_offset_bps = 30");
        let origin = panel_origin(Source::PanelSettings, "tailnet:ops@textile.example", "acme");
        let outcome = write_config_recorded(&path, &saved, &origin, 5_000, None).unwrap();
        assert_eq!(outcome.warning, None);
        assert_eq!(outcome.caught_up, 1);
        assert_eq!(outcome.records, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);

        let all = read_all(&path).unwrap();
        let new = &all[before..];
        assert_eq!(new.len(), 2, "{new:?}");
        assert_eq!(new[0].parameter, "sell_offset_bps");
        assert_eq!(new[0].actor, HOST_EDIT_ACTOR);
        assert_eq!(new[1].parameter, "buy_offset_bps");
        assert_eq!(new[1].actor, "tailnet:ops@textile.example");
        assert_eq!(new[1].source, "panel.settings");
        assert_eq!(new[1].bot.as_deref(), Some("acme"));
        assert_ne!(new[0].change_set_id, new[1].change_set_id);

        // And the bot starting on that file has nothing left to record.
        assert_eq!(record_startup(&path, &saved, 6_000).unwrap(), 0);
    }

    #[test]
    fn a_save_still_lands_when_the_change_log_cannot_be_written() {
        let path = temp_config("blocked", CONFIG);
        // A directory where the lock file should be: the log can't open.
        std::fs::create_dir(files::path(&path, LogFile::Lock)).unwrap();
        let next = CONFIG.replace("buy_offset_bps = 40", "buy_offset_bps = 30");
        let origin = panel_origin(Source::PanelRaw, "panel password (shared)", "acme");
        let outcome = write_config_recorded(&path, &next, &origin, 1, None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), next);
        assert_eq!(outcome.records, 0);
        assert!(outcome.warning.is_some());
        // The bot refuses to pretend: startup can't record either.
        assert!(record_startup(&path, &next, 2).is_err());
    }

    #[test]
    fn a_save_whose_snapshot_did_not_land_is_not_recorded_twice() {
        let path = temp_config("heal", CONFIG);
        record_startup(&path, CONFIG, 1).unwrap();
        let saved = CONFIG.replace("buy_offset_bps = 40", "buy_offset_bps = 30");
        let origin = panel_origin(Source::PanelSettings, "tailnet:ops@textile.example", "acme");
        write_config_recorded(&path, &saved, &origin, 2, None).unwrap();
        // Simulate the snapshot write failing after the records landed.
        let snapshot = files::path(&path, LogFile::Snapshot);
        std::fs::write(&snapshot, CONFIG).unwrap();
        let before = read_all(&path).unwrap().len();

        assert_eq!(record_startup(&path, &saved, 3).unwrap(), 0);
        assert_eq!(read_all(&path).unwrap().len(), before);
        assert_eq!(
            std::fs::read_to_string(&snapshot).unwrap(),
            saved,
            "the snapshot is repaired"
        );
    }

    #[test]
    fn the_log_is_written_beside_the_config_not_under_it() {
        let path = temp_config("layout", CONFIG);
        record_startup(&path, CONFIG, 1).unwrap();
        let dir = path.parent().unwrap();
        for name in ["change-log.jsonl", "change-log.snapshot", "change-log.lock"] {
            assert!(dir.join(name).is_file(), "{name}");
        }
        assert!(!dir.join("change-log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_swapped_for_a_link_is_refused_not_followed() {
        let path = temp_config("swap", CONFIG);
        record_startup(&path, CONFIG, 1).unwrap();
        let dir = path.parent().unwrap();
        let target = dir.join("not-the-journal");
        std::fs::write(&target, "untouched").unwrap();
        let journal = files::path(&path, LogFile::Journal);
        std::fs::remove_file(&journal).unwrap();
        std::os::unix::fs::symlink(&target, &journal).unwrap();

        let next = CONFIG.replace("buy_offset_bps = 40", "buy_offset_bps = 30");
        let origin = panel_origin(Source::PanelRaw, "panel password (shared)", "acme");
        let outcome = write_config_recorded(&path, &next, &origin, 2, None).unwrap();
        assert!(outcome.warning.is_some(), "the save lands, the log refuses");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    }
}
