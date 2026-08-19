// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! In-flight RFQ inventory reservations.
//!
//! Every signed quote is a live claim on the funding wallet until its order
//! deadline passes, unless the venue says the taker will never submit it.
//!
//! `selected` holds until `deadline + skew` or an explicit `quoteExpired`.
//! `lost_price`, `no_quote`, `invalid`, and `late` release immediately —
//! those signatures never leave the venue, so keeping them reserved makes
//! the corridor look empty for the rest of the TTL.
//!
//! `quoteExpired` is the selected-quote exception: the taker was handed the
//! winning quote and its accept window lapsed without a submit. The venue
//! un-counts that order at the same moment, so this ledger must drop it or
//! the next request on the same side keeps seeing a ghost reservation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alloy_primitives::U256;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// Sibling of `rfq-api.key` / `stitch.toml`. Survives panel restarts.
pub const RESERVATIONS_FILE: &str = "rfq-reservations.json";

/// Seconds past the order deadline a reservation lingers, covering clock skew
/// between the maker, the venue, and the chain.
pub const RELEASE_SKEW_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reservation {
    corridor: String,
    /// True: bid (maker pays debt). False: ask (maker pays collateral).
    bid: bool,
    /// The signed order's input — what the maker pays if it fills.
    input: U256,
    /// Unix seconds after which the reservation no longer counts.
    release_at: u64,
}

/// On-disk shape. `input` is a decimal string so a U256 never goes through JSON
/// number (which cannot hold 256-bit values).
#[derive(Debug, Serialize, Deserialize)]
struct StoredLedger {
    version: u32,
    entries: Vec<StoredEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredEntry {
    rfq_id: String,
    corridor: String,
    bid: bool,
    input: String,
    release_at: u64,
}

/// The reservation ledger. Owned by the responder task; no interior locking.
/// When `persist_path` is set, every reserve/prune writes the file so a
/// process restart (deploy, OOM, panel save) cannot forget live signatures.
#[derive(Debug, Default)]
pub struct Reservations {
    by_rfq: HashMap<String, Reservation>,
    persist_path: Option<PathBuf>,
}

impl Reservations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty ledger that will persist to `path` on the next reserve/prune.
    pub fn with_persist_path(path: impl Into<PathBuf>) -> Self {
        Self {
            persist_path: Some(path.into()),
            ..Self::default()
        }
    }

    /// Load a previously persisted ledger. Missing file → empty (first run).
    /// Present but unreadable / unknown version → error so the responder
    /// refuses to start rather than quote over forgotten signatures.
    pub fn load(path: impl Into<PathBuf>, now_secs: u64) -> anyhow::Result<Self> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self::with_persist_path(path));
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stored: StoredLedger =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            stored.version == 1,
            "unsupported {} version {}",
            RESERVATIONS_FILE,
            stored.version
        );
        let mut by_rfq = HashMap::new();
        for entry in stored.entries {
            let input = entry
                .input
                .parse::<U256>()
                .with_context(|| format!("invalid reservation input {}", entry.input))?;
            if entry.release_at > now_secs {
                by_rfq.insert(
                    entry.rfq_id,
                    Reservation {
                        corridor: entry.corridor,
                        bid: entry.bid,
                        input,
                        release_at: entry.release_at,
                    },
                );
            }
        }
        Ok(Self {
            by_rfq,
            persist_path: Some(path),
        })
    }

    /// Record a quote's claim. `deadline_secs` is the signed order's deadline;
    /// the reservation survives it by [`RELEASE_SKEW_SECS`].
    pub fn reserve(
        &mut self,
        rfq_id: impl Into<String>,
        corridor: impl Into<String>,
        bid: bool,
        input: U256,
        deadline_secs: u64,
    ) {
        self.by_rfq.insert(
            rfq_id.into(),
            Reservation {
                corridor: corridor.into(),
                bid,
                input,
                release_at: deadline_secs.saturating_add(RELEASE_SKEW_SECS),
            },
        );
        self.persist();
    }

    /// Total input currently reserved on one side of a corridor. Expired
    /// entries never count (release is lazy; [`Self::prune`] reclaims memory).
    pub fn reserved(&self, corridor: &str, bid: bool, now_secs: u64) -> U256 {
        self.by_rfq
            .values()
            .filter(|r| r.corridor == corridor && r.bid == bid && r.release_at > now_secs)
            .fold(U256::ZERO, |sum, r| sum.saturating_add(r.input))
    }

    /// Corridor slug for a live reservation, if any. Peek before
    /// [`Self::release`] so a `quoteExpired` flush can wait for that
    /// book to actually publish, not a sibling with a fresh feed.
    pub fn corridor(&self, rfq_id: &str) -> Option<&str> {
        self.by_rfq.get(rfq_id).map(|r| r.corridor.as_str())
    }

    /// Drop one RFQ's claim immediately. Used when the venue says the
    /// winning quote expired unaccepted (`quoteExpired`). Missing id is a
    /// no-op so a duplicate or late frame cannot break the ledger.
    pub fn release(&mut self, rfq_id: &str) -> bool {
        let gone = self.by_rfq.remove(rfq_id).is_some();
        if gone {
            self.persist();
        }
        gone
    }

    /// Drop entries past their release time. Called on the 1s levels tick so
    /// the map can't grow unboundedly between quote bursts.
    pub fn prune(&mut self, now_secs: u64) {
        let before = self.by_rfq.len();
        self.by_rfq.retain(|_, r| r.release_at > now_secs);
        if self.by_rfq.len() != before {
            self.persist();
        }
    }

    fn persist(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        if let Err(e) = self.write(path) {
            tracing::warn!(
                error = %format!("{e:#}"),
                path = %path.display(),
                "RFQ reservations not persisted; a restart will forget live quotes"
            );
        }
    }

    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let stored = StoredLedger {
            version: 1,
            entries: self
                .by_rfq
                .iter()
                .map(|(rfq_id, r)| StoredEntry {
                    rfq_id: rfq_id.clone(),
                    corridor: r.corridor.clone(),
                    bid: r.bid,
                    input: r.input.to_string(),
                    release_at: r.release_at,
                })
                .collect(),
        };
        crate::setup::write_toml_atomic(path, &serde_json::to_string(&stored)?)
    }

    pub fn len(&self) -> usize {
        self.by_rfq.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_rfq.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corridor_peeks_before_release() {
        let mut r = Reservations::new();
        r.reserve("rfq_1", "cngn-usdc", true, U256::from(1u64), 1_000);
        assert_eq!(r.corridor("rfq_1"), Some("cngn-usdc"));
        assert!(r.release("rfq_1"));
        assert_eq!(r.corridor("rfq_1"), None);
    }

    #[test]
    fn reservations_hold_until_deadline_plus_skew_unless_released() {
        let mut r = Reservations::new();
        let deadline = 1_000u64;
        r.reserve("rfq_1", "cngn-usdc", true, U256::from(500u64), deadline);

        // The ledger itself does not watch venue frames — a selected quote
        // still claims inventory until deadline + skew if nobody calls release.
        assert_eq!(
            r.reserved("cngn-usdc", true, 900),
            U256::from(500u64),
            "an unreleased quote still claims inventory before its deadline"
        );
        // Still held through the deadline and the skew window…
        assert_eq!(
            r.reserved("cngn-usdc", true, deadline + RELEASE_SKEW_SECS - 1),
            U256::from(500u64)
        );
        // …and gone exactly at deadline + skew.
        assert_eq!(
            r.reserved("cngn-usdc", true, deadline + RELEASE_SKEW_SECS),
            U256::ZERO
        );
    }

    #[test]
    fn sides_and_corridors_are_tracked_independently() {
        let mut r = Reservations::new();
        r.reserve("a", "cngn-usdc", true, U256::from(100u64), 1_000);
        r.reserve("b", "cngn-usdc", true, U256::from(25u64), 1_000);
        r.reserve("c", "cngn-usdc", false, U256::from(7u64), 1_000);
        r.reserve("d", "kes-usdt", true, U256::from(9u64), 1_000);

        assert_eq!(r.reserved("cngn-usdc", true, 0), U256::from(125u64));
        assert_eq!(r.reserved("cngn-usdc", false, 0), U256::from(7u64));
        assert_eq!(r.reserved("kes-usdt", true, 0), U256::from(9u64));
        assert_eq!(r.reserved("kes-usdt", false, 0), U256::ZERO);
    }

    #[test]
    fn re_reserving_an_rfq_id_replaces_rather_than_stacks() {
        // A re-quote for the same rfqId supersedes the earlier claim; counting
        // both would double-reserve one request.
        let mut r = Reservations::new();
        r.reserve("a", "cngn-usdc", true, U256::from(100u64), 1_000);
        r.reserve("a", "cngn-usdc", true, U256::from(60u64), 1_200);
        assert_eq!(r.reserved("cngn-usdc", true, 0), U256::from(60u64));
    }

    #[test]
    fn quote_expired_releases_immediately_not_at_deadline_plus_skew() {
        let mut r = Reservations::new();
        r.reserve("rfq_1", "cngn-usdc", true, U256::from(500u64), 1_000);
        assert!(r.release("rfq_1"));
        assert_eq!(r.reserved("cngn-usdc", true, 0), U256::ZERO);
        assert!(!r.release("rfq_1"), "duplicate release is a no-op");
        r.reserve("rfq_2", "cngn-usdc", false, U256::from(9u64), 1_000);
        assert!(r.release("rfq_2"));
        assert_eq!(r.reserved("cngn-usdc", false, 0), U256::ZERO);
    }

    #[test]
    fn prune_reclaims_expired_entries() {
        let mut r = Reservations::new();
        r.reserve("a", "cngn-usdc", true, U256::from(1u64), 100);
        r.reserve("b", "cngn-usdc", true, U256::from(2u64), 10_000);
        r.prune(100 + RELEASE_SKEW_SECS);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.reserved("cngn-usdc", true, 100 + RELEASE_SKEW_SECS),
            U256::from(2u64)
        );
    }

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stitch-rfq-res-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(RESERVATIONS_FILE)
    }

    #[test]
    fn audit_m04_a_fresh_ledger_forgets_quotes_a_restart_must_not() {
        // The hole: Reservations::new() after a process death is empty, even
        // though the signed orders are still fillable until deadline + skew.
        let path = tmp_path("hole");
        let mut live = Reservations::with_persist_path(&path);
        live.reserve("rfq_1", "cngn-usdc", true, U256::from(500u64), 1_000);
        assert!(path.is_file(), "reserve must flush the ledger");

        let forgotten = Reservations::new();
        assert_eq!(
            forgotten.reserved("cngn-usdc", true, 0),
            U256::ZERO,
            "a restart that constructs Reservations::new() re-opens the hole"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn audit_m04_load_restores_live_quotes_and_drops_expired() {
        let path = tmp_path("load");
        let mut live = Reservations::with_persist_path(&path);
        live.reserve("live", "cngn-usdc", true, U256::from(500u64), 1_000);
        live.reserve("dead", "cngn-usdc", false, U256::from(9u64), 10);

        let restored = Reservations::load(&path, 10 + RELEASE_SKEW_SECS).unwrap();
        assert_eq!(
            restored.reserved("cngn-usdc", true, 10 + RELEASE_SKEW_SECS),
            U256::from(500u64)
        );
        assert_eq!(
            restored.reserved("cngn-usdc", false, 10 + RELEASE_SKEW_SECS),
            U256::ZERO,
            "expired entries must not come back after restart"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn persist_release_drops_the_entry_from_disk() {
        let path = tmp_path("release");
        let mut live = Reservations::with_persist_path(&path);
        live.reserve("rfq_1", "cngn-usdc", true, U256::from(500u64), 1_000);
        assert!(live.release("rfq_1"));
        let restored = Reservations::load(&path, 0).unwrap();
        assert!(restored.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_reservations_file_is_an_empty_ledger() {
        let path = tmp_path("missing");
        std::fs::remove_file(&path).ok();
        let loaded = Reservations::load(&path, 0).unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_reservations_file_is_an_error() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "not-json").unwrap();
        assert!(Reservations::load(&path, 0).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
