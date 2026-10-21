// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The jump guard's trusted print, kept on disk across restarts.
//!
//! Without a saved anchor, two agreeing fetches confirm a startup price.
//! Restarting while a feed prints 10x could therefore trust that bad level.
//! With an anchor, the first print after restart is judged against the last
//! trusted one: in line, it quotes at once; out of line, it waits for the
//! feed to return or hold the new level for `settle_secs`. To take a new
//! level at once, delete `price-guard` beside `stitch.toml` and restart.
//!
//! One JSON file per feed URL, rewritten at most once a minute or on a move
//! of more than 1 %, never with an older print than it holds. The shared
//! cache judges reads from both loops and owns the file. The on-disk format
//! also supports contradictory anchors written by independent owners: two
//! prices at one timestamp within `max_jump_bps` may replace each other;
//! beyond it, neither is kept ([`OnDisk::Contradicted`]). A restart trusts
//! nothing at or before that timestamp, and applies the startup rule to a
//! newer print. Existing contradiction tombstones follow the same rule.
//!
//! Persistence is best effort. Failed writes are logged and the guard keeps
//! its live history. A missing, unreadable or mismatched file starts from
//! the startup rule. Without a config directory, nothing is read or saved.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloy_primitives::{hex, keccak256};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::jump::{agrees, move_bps, JumpGuard, Print, PrintHistory};
use super::tick::is_price_usable;

/// Folder beside `stitch.toml`.
pub const DIR: &str = "price-guard";
const VERSION: u32 = 1;
const SAVE_EVERY_SECS: u64 = 60;
const SAVE_ON_MOVE_BPS: f64 = 100.0;

/// Atomic writes stage through one fixed temp name per target. Serialize
/// the read-compare-write in [`write`] even if separate cache owners share
/// a directory (for example in tests). The live runtime has one owner.
static WRITES: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    version: u32,
    url: String,
    /// Absent when `contradicted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    price: Option<f64>,
    observed_at: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    contradicted: bool,
}

/// What one feed's file holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OnDisk {
    /// The last saved trusted print: a restart's baseline.
    Trusted(Print),
    /// Writers offered prices more than `max_jump_bps` apart at this feed
    /// timestamp. No baseline, and no print at or before it is written.
    Contradicted { observed_at: u64 },
}

impl OnDisk {
    /// The history a restart starts the feed from.
    fn history(self) -> PrintHistory {
        match self {
            Self::Trusted(print) => PrintHistory {
                trusted: Some(print),
                ..PrintHistory::default()
            },
            Self::Contradicted { observed_at } => PrintHistory {
                contradicted_at: Some(observed_at),
                ..PrintHistory::default()
            },
        }
    }
}

/// The file for one feed URL. Hashed, so no URL characters reach the disk.
pub fn file_name(url: &str) -> String {
    format!("{}.json", hex::encode(&keccak256(url.as_bytes())[..12]))
}

/// Whether `trusted` is worth writing over what was last written. Pure. Any
/// newer print is worth writing over a contradiction: a baseline beats none.
pub fn worth_saving(saved: Option<OnDisk>, trusted: Print) -> bool {
    match saved {
        None => true,
        Some(OnDisk::Trusted(s)) => {
            trusted.observed_at >= s.observed_at.saturating_add(SAVE_EVERY_SECS)
                || move_bps(s.price, trusted.price) > SAVE_ON_MOVE_BPS
        }
        Some(OnDisk::Contradicted { observed_at }) => trusted.observed_at > observed_at,
    }
}

fn encode(url: &str, held: OnDisk) -> serde_json::Result<String> {
    let (price, observed_at, contradicted) = match held {
        OnDisk::Trusted(p) => (Some(p.price), p.observed_at, false),
        OnDisk::Contradicted { observed_at } => (None, observed_at, true),
    };
    serde_json::to_string(&Saved {
        version: VERSION,
        url: url.to_string(),
        price,
        observed_at,
        contradicted,
    })
}

fn decode(raw: &str, url: &str) -> Option<OnDisk> {
    let saved: Saved = serde_json::from_str(raw).ok()?;
    if saved.version != VERSION || saved.url != url || saved.observed_at == 0 {
        return None;
    }
    if saved.contradicted {
        return Some(OnDisk::Contradicted {
            observed_at: saved.observed_at,
        });
    }
    saved.price.filter(|p| is_price_usable(*p)).map(|price| {
        OnDisk::Trusted(Print {
            price,
            observed_at: saved.observed_at,
        })
    })
}

/// One feed's saved trusted print.
#[derive(Debug)]
pub struct Anchor {
    path: Option<PathBuf>,
    url: String,
    saved: Option<OnDisk>,
}

impl Anchor {
    /// Read the feed's saved print and the history to start it with. `dir`
    /// is the folder holding `stitch.toml`; `None` keeps nothing.
    pub fn load(dir: Option<&Path>, url: &str) -> (Self, PrintHistory) {
        let path = dir.map(|d| d.join(DIR).join(file_name(url)));
        let saved = path.as_deref().and_then(|p| read(p, url));
        let history = saved.map(OnDisk::history).unwrap_or_default();
        let anchor = Self {
            path,
            url: url.to_string(),
            saved,
        };
        (anchor, history)
    }

    /// Write the history's trusted print if it is worth it. If the file
    /// already holds a newer one from another writer, or a
    /// contradiction at or after it, that stays and becomes what this anchor
    /// compares against. `guard` is the one that judged `history`.
    pub fn record(&mut self, history: &PrintHistory, guard: &JumpGuard) {
        let (Some(path), Some(trusted)) = (&self.path, history.trusted) else {
            return;
        };
        if !worth_saving(self.saved, trusted) {
            return;
        }
        match write(path, &self.url, trusted, guard) {
            Ok(kept) => {
                if let OnDisk::Contradicted { observed_at } = kept {
                    if self.saved != Some(kept) {
                        warn!(feed = %self.url, observed_at, price = trusted.price, "saved anchors disagree by more than max_jump_bps at one feed timestamp; a restart trusts neither until a newer print is saved")
                    }
                }
                self.saved = Some(kept);
            }
            Err(e) => {
                warn!(feed = %self.url, error = %format!("{e:#}"), "could not save the trusted reference price; the jump guard keeps it in memory only")
            }
        }
    }
}

fn read(path: &Path, url: &str) -> Option<OnDisk> {
    decode(&std::fs::read_to_string(path).ok()?, url)
}

/// What the file holds once `print` is offered. Pure.
///
/// Saved timestamps never go backwards, so a writer offering an older
/// print cannot hand a restart an older baseline. At the saved print's
/// timestamp, a price within `max_jump_bps` of it replaces it: either is a
/// baseline the guard would judge the next print against the same way. One
/// beyond it means the saved anchors disagree:
/// neither price is a baseline, and none is until a print newer than that
/// timestamp is offered.
fn keep(on_disk: Option<OnDisk>, print: Print, guard: &JumpGuard) -> OnDisk {
    match on_disk {
        Some(OnDisk::Trusted(d)) if d.observed_at > print.observed_at => OnDisk::Trusted(d),
        Some(OnDisk::Trusted(d))
            if d.observed_at == print.observed_at && !agrees(d.price, print.price, guard) =>
        {
            OnDisk::Contradicted {
                observed_at: d.observed_at,
            }
        }
        Some(c @ OnDisk::Contradicted { observed_at }) if print.observed_at <= observed_at => c,
        _ => OnDisk::Trusted(print),
    }
}

/// Offer `print` to the file (see [`keep`]). Returns what the file holds
/// afterwards.
fn write(path: &Path, url: &str, print: Print, guard: &JumpGuard) -> anyhow::Result<OnDisk> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _one_at_a_time = WRITES.lock().unwrap_or_else(|p| p.into_inner());
    let on_disk = read(path, url);
    let kept = keep(on_disk, print, guard);
    if on_disk != Some(kept) {
        crate::setup::write_file_atomic(path, encode(url, kept)?)?;
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::jump::{observe, Hold, Verdict};

    const URL: &str = "https://api.example/price?pair=cngn-usdt";
    const JUMP: JumpGuard = JumpGuard {
        max_jump_bps: 1_000,
        settle_secs: 300,
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("stitch-anchor-{tag}-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn p(price: f64, observed_at: u64) -> Print {
        Print { price, observed_at }
    }

    fn saved(price: f64, observed_at: u64) -> Option<OnDisk> {
        Some(OnDisk::Trusted(p(price, observed_at)))
    }

    fn trusting(print: Print) -> PrintHistory {
        PrintHistory {
            trusted: Some(print),
            ..PrintHistory::default()
        }
    }

    #[test]
    fn a_restart_judges_the_first_print_against_the_last_trusted_one() {
        let dir = temp_dir("restart");
        let (mut anchor, fresh) = Anchor::load(Some(&dir), URL);
        assert_eq!(fresh, PrintHistory::default(), "nothing saved yet");
        anchor.record(&trusting(p(0.5, 100)), &JUMP);

        // The bot restarts while the feed prints 10x: held, and still held
        // a second later, where an empty history would have trusted it.
        let (_, history) = Anchor::load(Some(&dir), URL);
        assert_eq!(history.trusted, Some(p(0.5, 100)));
        let (history, v) = observe(history, p(5.0, 200), &JUMP);
        assert!(matches!(v, Verdict::Held(Hold::Jumped { .. })), "{v:?}");
        let (_, v) = observe(history, p(5.0, 201), &JUMP);
        assert!(matches!(v, Verdict::Held(_)), "{v:?}");

        // In line, it quotes on the first read.
        let (_, history) = Anchor::load(Some(&dir), URL);
        assert_eq!(observe(history, p(0.51, 200), &JUMP).1, Verdict::Trusted);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn writes_are_throttled_to_a_minute_or_a_one_percent_move() {
        assert!(worth_saving(None, p(1.0, 100)));
        assert!(!worth_saving(saved(1.0, 100), p(1.005, 159)));
        assert!(worth_saving(saved(1.0, 100), p(1.0, 160)));
        assert!(worth_saving(saved(1.0, 100), p(1.02, 101)));

        let dir = temp_dir("throttle");
        let (mut anchor, _) = Anchor::load(Some(&dir), URL);
        anchor.record(&trusting(p(1.0, 100)), &JUMP);
        anchor.record(&trusting(p(1.001, 130)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(1.0, 100)));
        anchor.record(&trusting(p(1.0, 161)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(1.0, 161)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_loop_holding_an_older_print_never_moves_the_saved_one_back() {
        let trusted = |price, at| OnDisk::Trusted(p(price, at));
        assert_eq!(keep(None, p(1.0, 100), &JUMP), trusted(1.0, 100));
        assert_eq!(keep(saved(1.0, 100), p(1.0, 100), &JUMP), trusted(1.0, 100));
        assert_eq!(keep(saved(1.0, 200), p(5.0, 150), &JUMP), trusted(1.0, 200));

        // The RFQ loop and the tick loop each hold an anchor for one feed.
        let dir = temp_dir("two-loops");
        let (mut rfq, _) = Anchor::load(Some(&dir), URL);
        let (mut tick, _) = Anchor::load(Some(&dir), URL);
        rfq.record(&trusting(p(1.0, 200)), &JUMP);
        // The tick loop last trusted an older print from a lagging read,
        // and it moved more than 1 %, so the throttle alone would write it.
        tick.record(&trusting(p(1.05, 150)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(1.0, 200)));
        // It now compares against the newer print, and saves newer ones.
        tick.record(&trusting(p(1.0, 230)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(1.0, 200)));
        tick.record(&trusting(p(1.0, 260)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(1.0, 260)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_nearby_price_at_the_same_timestamp_is_an_ordinary_live_print() {
        // Two API tasks, one second: the same market, a few bps apart. The
        // later write stands; a restart has a baseline and no warning fires.
        assert_eq!(
            keep(saved(1.0, 200), p(1.05, 200), &JUMP),
            OnDisk::Trusted(p(1.05, 200))
        );
        let dir = temp_dir("same-second");
        let (mut rfq, _) = Anchor::load(Some(&dir), URL);
        let (mut tick, _) = Anchor::load(Some(&dir), URL);
        rfq.record(&trusting(p(1.0, 200)), &JUMP);
        tick.record(&trusting(p(1.02, 200)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(1.02, 200)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn two_prices_at_one_timestamp_leave_no_baseline_until_a_newer_print() {
        let contradicted = OnDisk::Contradicted { observed_at: 200 };
        assert_eq!(keep(saved(1.0, 200), p(5.0, 200), &JUMP), contradicted);
        // Neither price, nor anything older, brings a baseline back.
        assert_eq!(keep(Some(contradicted), p(1.0, 200), &JUMP), contradicted);
        assert_eq!(keep(Some(contradicted), p(5.0, 200), &JUMP), contradicted);
        assert_eq!(keep(Some(contradicted), p(1.0, 150), &JUMP), contradicted);
        assert_eq!(
            keep(Some(contradicted), p(5.0, 201), &JUMP),
            OnDisk::Trusted(p(5.0, 201))
        );
        // A newer print is worth writing at once, not a minute later.
        assert!(!worth_saving(Some(contradicted), p(5.0, 200)));
        assert!(worth_saving(Some(contradicted), p(5.0, 201)));

        // Both loops start with nothing saved, and the feed hands each of
        // them a different price for the same timestamp.
        let dir = temp_dir("contradicted");
        let (mut rfq, _) = Anchor::load(Some(&dir), URL);
        let (mut tick, _) = Anchor::load(Some(&dir), URL);
        rfq.record(&trusting(p(1.0, 200)), &JUMP);
        tick.record(&trusting(p(5.0, 200)), &JUMP);
        // A restart trusts neither, however often the feed serves one.
        let (_, history) = Anchor::load(Some(&dir), URL);
        assert_eq!(history.trusted, None);
        let (history, v) = observe(history, p(5.0, 200), &JUMP);
        assert_eq!(v, Verdict::Held(Hold::Contradicted { at: 200 }));
        let (_, v) = observe(history, p(5.0, 200), &JUMP);
        assert_eq!(v, Verdict::Held(Hold::Contradicted { at: 200 }));
        tick.record(&trusting(p(5.0, 201)), &JUMP);
        assert_eq!(Anchor::load(Some(&dir), URL).1.trusted, Some(p(5.0, 201)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_contradiction_survives_the_round_trip_through_the_file() {
        let contradicted = OnDisk::Contradicted { observed_at: 200 };
        let raw = encode(URL, contradicted).unwrap();
        assert!(!raw.contains(r#""price""#), "{raw}");
        assert_eq!(decode(&raw, URL), Some(contradicted));
        let print = OnDisk::Trusted(p(0.5, 100));
        let raw = encode(URL, print).unwrap();
        assert!(!raw.contains(r#""contradicted""#), "{raw}");
        assert_eq!(decode(&raw, URL), Some(print));
    }

    #[test]
    fn a_bad_or_foreign_file_is_ignored_and_no_dir_keeps_nothing() {
        let dir = temp_dir("bad");
        let path = dir.join(DIR).join(file_name(URL));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for raw in [
            "not json".to_string(),
            r#"{"version":1,"url":"https://other","price":0.5,"observedAt":100}"#.to_string(),
            format!(r#"{{"version":2,"url":"{URL}","price":0.5,"observedAt":100}}"#),
            format!(r#"{{"version":1,"url":"{URL}","price":0,"observedAt":100}}"#),
            format!(r#"{{"version":1,"url":"{URL}","price":0.5,"observedAt":0}}"#),
            format!(r#"{{"version":1,"url":"{URL}","observedAt":100}}"#),
        ] {
            std::fs::write(&path, &raw).unwrap();
            assert_eq!(
                Anchor::load(Some(&dir), URL).1,
                PrintHistory::default(),
                "{raw}"
            );
        }
        let (mut anchor, history) = Anchor::load(None, URL);
        assert_eq!(history, PrintHistory::default());
        anchor.record(&trusting(p(1.0, 100)), &JUMP);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn feeds_get_their_own_files() {
        assert_ne!(
            file_name(URL),
            file_name("https://api.example/price?pair=kes-usdt")
        );
        assert_eq!(file_name(URL), file_name(URL));
        let name = file_name(URL);
        let stem = name.strip_suffix(".json").unwrap();
        assert!(stem.len() == 24 && stem.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
