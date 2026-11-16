// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Reading a bot's change log back, for the panel's Change log view.
//!
//! Read-only and lock-free: the journal is append-only, so reading it while
//! the bot or a panel save appends can at worst miss the line being written,
//! which [`ship::pending`] already leaves for later. Pure apart from
//! [`read_bot`], which does the file reads.
//!
//! Whether Textile has a record comes from the `change-log.shipped` cursor,
//! never from `[change_log] ship`: a record Textile acknowledged stays
//! acknowledged after shipping is turned off.

use std::path::Path;

use serde::Serialize;

use super::files::{self, LogFile};
use super::record::ChangeRecord;
use super::ship;

/// Change sets per page. A set is never split across two pages.
pub const PAGE_SETS: usize = 100;

/// One journal record as the panel serves it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(flatten)]
    pub record: ChangeRecord,
    /// Textile acknowledged it: the line ends at or before the shipped cursor.
    pub acknowledged: bool,
}

/// Everything read from one bot's log files.
#[derive(Debug, Clone, PartialEq)]
pub struct BotLog {
    pub entries: Vec<Entry>,
    /// When the last acknowledged record took effect, for display. `None`
    /// when Textile has nothing from this bot.
    pub shipped_through: Option<String>,
    /// Ids from `change-log.refused`.
    pub refused: Vec<String>,
}

/// Pure: the journal's records, each marked acknowledged or not.
///
/// `bot` overrides each record's own `bot`, which only the panel fills in
/// (the bot doesn't know what the panel calls it). The view groups by it, so
/// every record of one bot needs the same value.
///
/// A cursor past the end of the journal means the journal was replaced; the
/// shipper starts over from zero then, so nothing counts as acknowledged.
pub fn entries(journal: &[u8], shipped_offset: u64, bot: &str) -> Vec<Entry> {
    let cursor_valid = shipped_offset <= journal.len() as u64;
    ship::pending(journal, 0, usize::MAX)
        .entries
        .into_iter()
        .map(|e| Entry {
            acknowledged: cursor_valid && e.end <= shipped_offset,
            record: ChangeRecord {
                bot: Some(bot.to_string()),
                ..e.record
            },
        })
        .collect()
}

/// When a record took effect: a host edit when it was detected, anything
/// else when it was made.
fn effective_at(record: &ChangeRecord) -> &str {
    record.detected_at.as_deref().unwrap_or(&record.changed_at)
}

/// Pure: when the last acknowledged record took effect.
pub fn shipped_through(entries: &[Entry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| e.acknowledged)
        .map(|e| effective_at(&e.record).to_string())
}

/// Pure: the ids in `change-log.refused`, one JSON object per line. A line
/// that isn't one is skipped: the file is the bot's, and a bad line shouldn't
/// hide the rest.
pub fn refused_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| v.get("id")?.as_str().map(str::to_string))
        .collect()
}

/// Read one bot's log beside `config_path`. A bot with no journal yet has an
/// empty log, not an error.
pub fn read_bot(config_path: &Path, bot: &str) -> anyhow::Result<BotLog> {
    let journal = files::read(&files::path(config_path, LogFile::Journal))?.unwrap_or_default();
    let entries = entries(&journal, ship::shipped_offset(config_path), bot);
    let refused = files::read_text(&files::path(config_path, LogFile::Refused))?
        .map(|text| refused_ids(&text))
        .unwrap_or_default();
    Ok(BotLog {
        shipped_through: shipped_through(&entries),
        entries,
        refused,
    })
}

/// Where a change set sits in the merged, newest-first order. Compared as a
/// tuple, so sets that took effect in the same millisecond still have one
/// order and a cursor never skips or repeats one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SetKey {
    at: String,
    bot: String,
    change_set_id: String,
}

impl SetKey {
    fn of(record: &ChangeRecord) -> Self {
        Self {
            at: effective_at(record).to_string(),
            bot: record.bot.clone().unwrap_or_default(),
            change_set_id: record.change_set_id.clone(),
        }
    }

    /// Opaque to the client: a JSON array, so no separator can collide with
    /// a bot name.
    fn encode(&self) -> String {
        serde_json::json!([self.at, self.bot, self.change_set_id]).to_string()
    }

    fn decode(cursor: &str) -> Option<Self> {
        let [at, bot, change_set_id]: [String; 3] = serde_json::from_str(cursor).ok()?;
        Some(Self {
            at,
            bot,
            change_set_id,
        })
    }
}

/// One page of the merged log.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub next_cursor: Option<String>,
}

/// Pure: up to `limit` change sets older than `before`, newest first across
/// every bot, each set whole. A cursor that doesn't parse is refused rather
/// than read as "from the top", which would show the first page again as if
/// it were older.
pub fn page(
    entries: impl IntoIterator<Item = Entry>,
    before: Option<&str>,
    limit: usize,
) -> Result<Page, String> {
    let before = before
        .map(|c| SetKey::decode(c).ok_or_else(|| format!("not a change log cursor: {c}")))
        .transpose()?;
    let mut sets: std::collections::BTreeMap<SetKey, Vec<Entry>> = Default::default();
    for e in entries {
        sets.entry(SetKey::of(&e.record)).or_default().push(e);
    }
    let mut older = sets
        .into_iter()
        .rev()
        .filter(|(key, _)| before.as_ref().is_none_or(|b| key < b));
    let taken: Vec<(SetKey, Vec<Entry>)> = older.by_ref().take(limit).collect();
    let next_cursor = older.next().and(taken.last()).map(|(key, _)| key.encode());
    Ok(Page {
        entries: taken.into_iter().flat_map(|(_, e)| e).collect(),
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, set: &str, at: &str, detected: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "id": id,
            "changeSetId": set,
            "changedAt": at,
            "source": if detected.is_some() { "host-edit" } else { "panel.settings" },
            "actor": "tailnet:ana@acme.io",
            "chainId": 42220,
            "scope": "bot",
            "parameter": "max_loss_bps",
            "unit": "bps",
            "before": "1500",
            "after": "500",
            "configSha256Before": "a",
            "configSha256After": "b",
        });
        if let Some(d) = detected {
            v["detectedAt"] = serde_json::json!(d);
        }
        format!("{v}\n")
    }

    fn journal(lines: &[String]) -> Vec<u8> {
        lines.concat().into_bytes()
    }

    #[test]
    fn records_up_to_the_cursor_are_acknowledged_and_the_rest_are_not() {
        let a = line("a", "s1", "2026-11-13T10:00:00.000Z", None);
        let b = line("b", "s2", "2026-11-13T11:00:00.000Z", None);
        let j = journal(&[a.clone(), b]);
        let acked = |offset: u64| {
            entries(&j, offset, "lagos")
                .iter()
                .map(|e| e.acknowledged)
                .collect::<Vec<_>>()
        };
        assert_eq!(acked(0), [false, false]);
        assert_eq!(acked(a.len() as u64), [true, false]);
        assert_eq!(acked(j.len() as u64), [true, true]);
        assert_eq!(
            acked(j.len() as u64 + 1),
            [false, false],
            "a replaced journal starts over"
        );
    }

    #[test]
    fn every_record_carries_the_panel_name_of_its_bot() {
        let j = journal(&[line("a", "s1", "2026-11-13T10:00:00.000Z", None)]);
        let e = entries(&j, 0, "lagos-cngn");
        assert_eq!(e[0].record.bot.as_deref(), Some("lagos-cngn"));
    }

    #[test]
    fn a_line_still_being_written_is_left_out_and_a_torn_one_skipped() {
        let whole = line("a", "s1", "2026-11-13T10:00:00.000Z", None);
        let torn = &line("b", "s2", "2026-11-13T11:00:00.000Z", None)[..30];
        let j = journal(&[whole.clone(), format!("{torn}\n"), whole[..20].to_string()]);
        let ids: Vec<_> = entries(&j, 0, "x")
            .into_iter()
            .map(|e| e.record.id)
            .collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn shipped_through_is_when_the_last_acknowledged_record_took_effect() {
        let host = line(
            "a",
            "s1",
            "2026-11-13T09:00:00.000Z",
            Some("2026-11-13T09:30:00.000Z"),
        );
        let later = line("b", "s2", "2026-11-13T11:00:00.000Z", None);
        let j = journal(&[host.clone(), later]);
        assert_eq!(shipped_through(&entries(&j, 0, "x")), None);
        assert_eq!(
            shipped_through(&entries(&j, host.len() as u64, "x")).as_deref(),
            Some("2026-11-13T09:30:00.000Z"),
            "a host edit counts from when it was detected"
        );
    }

    #[test]
    fn refused_ids_reads_each_line_and_skips_junk() {
        let text = "{\"id\":\"r1\",\"reason\":\"too big\"}\nnot json\n\n{\"id\":\"r2\"}\n";
        assert_eq!(refused_ids(text), ["r1", "r2"]);
    }

    fn fleet() -> Vec<Entry> {
        let lagos = journal(&[
            line("l1", "a", "2026-11-13T10:00:00.000Z", None),
            line("l2", "a", "2026-11-13T10:00:00.000Z", None),
            line("l3", "b", "2026-11-13T12:00:00.000Z", None),
        ]);
        let jakarta = journal(&[
            line(
                "j1",
                "c",
                "2026-11-13T09:00:00.000Z",
                Some("2026-11-13T11:00:00.000Z"),
            ),
            // Same millisecond as lagos' set a: the tuple order still splits them.
            line("j2", "d", "2026-11-13T10:00:00.000Z", None),
        ]);
        [entries(&lagos, 0, "lagos"), entries(&jakarta, 0, "jakarta")].concat()
    }

    fn ids(p: &Page) -> Vec<&str> {
        p.entries.iter().map(|e| e.record.id.as_str()).collect()
    }

    #[test]
    fn a_page_is_newest_first_across_bots_and_keeps_sets_whole() {
        let p = page(fleet(), None, 10).unwrap();
        assert_eq!(ids(&p), ["l3", "j1", "l1", "l2", "j2"]);
        assert_eq!(p.next_cursor, None);
    }

    #[test]
    fn paging_with_the_cursor_visits_every_set_once() {
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let p = page(fleet(), cursor.as_deref(), 2).unwrap();
            seen.extend(ids(&p).into_iter().map(str::to_string));
            match p.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(seen, ["l3", "j1", "l1", "l2", "j2"]);
    }

    #[test]
    fn the_last_page_has_no_cursor_even_when_it_is_exactly_full() {
        assert_eq!(page(fleet(), None, 4).unwrap().next_cursor, None);
        let p = page(fleet(), None, 3).unwrap();
        assert!(p.next_cursor.is_some());
        let last = page(fleet(), p.next_cursor.as_deref(), 1).unwrap();
        assert_eq!(ids(&last), ["j2"]);
        assert_eq!(last.next_cursor, None);
    }

    #[test]
    fn a_cursor_that_does_not_parse_is_refused() {
        assert!(page(fleet(), Some("yesterday"), 10).is_err());
        assert!(page(fleet(), Some("[\"a\",\"b\"]"), 10).is_err());
    }

    #[test]
    fn entries_serialize_as_the_journal_line_plus_acknowledged() {
        let j = journal(&[line("a", "s1", "2026-11-13T10:00:00.000Z", None)]);
        let v = serde_json::to_value(&entries(&j, j.len() as u64, "lagos")[0]).unwrap();
        assert_eq!(v["id"], "a");
        assert_eq!(v["changeSetId"], "s1");
        assert_eq!(v["bot"], "lagos");
        assert_eq!(v["acknowledged"], true);
        assert!(v.get("detectedAt").is_none());
        assert_eq!(v["before"], "1500");
    }
}
