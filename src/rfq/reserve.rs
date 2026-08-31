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
    /// Maker-pays token, lowercased. `None` on ledgers written before we
    /// stored it — those still count via corridor + side.
    input_token: Option<String>,
}

/// On-disk shape. `input` is a decimal string so a U256 never goes through JSON
/// number (which cannot hold 256-bit values).
#[derive(Debug, Serialize, Deserialize)]
struct StoredLedger {
    version: u32,
    entries: Vec<StoredEntry>,
    /// `tradingEpoch` the entries were signed under. Absent on files written
    /// before vault mode, and for plain EOA makers (no epoch to bind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vault_epoch: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredEntry {
    rfq_id: String,
    corridor: String,
    bid: bool,
    input: String,
    release_at: u64,
    /// Absent on v1 files written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_token: Option<String>,
}

/// The reservation ledger. Owned by the responder task; no interior locking.
/// When `persist_path` is set, every reserve/prune writes the file so a
/// process restart (deploy, OOM, panel save) cannot forget live signatures.
#[derive(Debug, Default)]
pub struct Reservations {
    by_rfq: HashMap<String, Reservation>,
    persist_path: Option<PathBuf>,
    /// Last synced vault `tradingEpoch`. Persisted so a restart can tell
    /// whether the loaded claims were signed under the current epoch.
    vault_epoch: Option<u64>,
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
                        input_token: normalize_token_opt(entry.input_token),
                    },
                );
            }
        }
        Ok(Self {
            by_rfq,
            persist_path: Some(path),
            vault_epoch: stored.vault_epoch,
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
        self.reserve_paying(rfq_id, corridor, bid, input, deadline_secs, None::<String>);
    }

    /// Same as [`Self::reserve`], tagging the claim with the token the maker
    /// pays so a later pool removal cannot drop it from that token's total.
    pub fn reserve_paying(
        &mut self,
        rfq_id: impl Into<String>,
        corridor: impl Into<String>,
        bid: bool,
        input: U256,
        deadline_secs: u64,
        input_token: Option<impl AsRef<str>>,
    ) {
        self.by_rfq.insert(
            rfq_id.into(),
            Reservation {
                corridor: corridor.into(),
                bid,
                input,
                release_at: deadline_secs.saturating_add(RELEASE_SKEW_SECS),
                input_token: normalize_token_opt(input_token.map(|t| t.as_ref().to_string())),
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

    /// Sum of [`Self::reserved`] across every named corridor on one side.
    ///
    /// Two pools that pay the same token (Celo cNGN and wBRL both bid USDT)
    /// share one wallet claim. Dedupes slugs so two unbound books with an
    /// empty label cannot count the same entries twice.
    pub fn reserved_across<'a, I>(&self, corridors: I, bid: bool, now_secs: u64) -> U256
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut seen = std::collections::HashSet::new();
        corridors.into_iter().fold(U256::ZERO, |sum, corridor| {
            if !seen.insert(corridor) {
                return sum;
            }
            sum.saturating_add(self.reserved(corridor, bid, now_secs))
        })
    }

    /// Fill `input_token` on tokenless entries for `corridor`. Bids get
    /// `bid_token`, asks get `ask_token`. No-op on already-tagged rows.
    /// Persists when anything changed.
    pub fn tag_corridor_tokens(
        &mut self,
        corridor: &str,
        bid_token: Option<&str>,
        ask_token: Option<&str>,
    ) -> usize {
        let n = self.tag_corridor_tokens_mem(corridor, bid_token, ask_token);
        if n > 0 {
            self.persist();
        }
        n
    }

    /// Stamp every live book so an upgrade that still has the quoted pool
    /// writes the token before anyone can remove it.
    pub fn tag_books<'a, I>(&mut self, books: I) -> usize
    where
        I: IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    {
        let mut n = 0;
        for (slug, bid_token, ask_token) in books {
            if slug.is_empty() {
                continue;
            }
            n += self.tag_corridor_tokens_mem(slug, Some(bid_token), Some(ask_token));
        }
        if n > 0 {
            self.persist();
        }
        n
    }

    /// Stamp tokenless claims we can attribute to a pool about to disappear.
    ///
    /// `known_slugs` are labels that belong to that pool (`rfq_corridor`,
    /// catalog id). Venue-assigned slugs that match neither are left alone —
    /// guessing the owner can stamp a kept pool's ask with the removed
    /// pool's collateral. Session start (`tag_books`) covers those while
    /// the book is still live.
    ///
    /// The write is fallible here, unlike [`Self::persist`] elsewhere. The
    /// caller decides whether the pool may go by counting what is left
    /// tokenless *in memory*, so a swallowed write error would let it act on a
    /// stamp the restarted bot never reads — the untagged claim would come
    /// back and go missing from the corridors that remain.
    pub fn tag_for_removed_pool(
        &mut self,
        known_slugs: &[&str],
        bid_token: &str,
        ask_token: &str,
    ) -> anyhow::Result<usize> {
        let mut n = 0;
        for slug in known_slugs {
            let slug = slug.trim();
            if slug.is_empty() {
                continue;
            }
            n += self.tag_corridor_tokens_mem(slug, Some(bid_token), Some(ask_token));
        }
        if n > 0 {
            if let Some(path) = &self.persist_path {
                self.write(path)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
        Ok(n)
    }

    /// Live rows that still have no `input_token`. Those disappear from a
    /// shared-token total once their corridor slug is gone, so a remove must
    /// not proceed while any remain.
    pub fn live_tokenless_count(&self, now_secs: u64) -> usize {
        self.by_rfq
            .values()
            .filter(|r| r.release_at > now_secs && r.input_token.is_none())
            .count()
    }

    fn tag_corridor_tokens_mem(
        &mut self,
        corridor: &str,
        bid_token: Option<&str>,
        ask_token: Option<&str>,
    ) -> usize {
        let mut n = 0;
        for r in self.by_rfq.values_mut() {
            if r.input_token.is_some() || r.corridor != corridor {
                continue;
            }
            let raw = if r.bid { bid_token } else { ask_token };
            let Some(token) = normalize_token_opt(raw.map(str::to_string)) else {
                continue;
            };
            r.input_token = Some(token);
            n += 1;
        }
        n
    }

    /// Live claim on `token` — every tagged reservation paying it, plus
    /// untagged (pre-token) entries on `fallback_slugs` for this side.
    ///
    /// Tagged entries stay in the total after their corridor is removed from
    /// the live books. Untagged entries still need a matching slug — call
    /// [`Self::tag_for_removed_pool`] or [`Self::tag_books`] before that slug
    /// can disappear.
    pub fn reserved_paying<'a, I>(
        &self,
        token: &str,
        fallback_slugs: I,
        bid: bool,
        now_secs: u64,
    ) -> U256
    where
        I: IntoIterator<Item = &'a str>,
    {
        let token = normalize_token(token);
        if token.is_empty() {
            return self.reserved_across(fallback_slugs, bid, now_secs);
        }
        let fallback: std::collections::HashSet<&str> = fallback_slugs.into_iter().collect();
        self.by_rfq.values().fold(U256::ZERO, |sum, r| {
            if r.release_at <= now_secs {
                return sum;
            }
            if let Some(t) = &r.input_token {
                if t == &token {
                    return sum.saturating_add(r.input);
                }
                return sum;
            }
            if r.bid == bid && fallback.contains(r.corridor.as_str()) {
                return sum.saturating_add(r.input);
            }
            sum
        })
    }

    /// Whether any live untagged claim belongs to no book we can see.
    ///
    /// [`Self::reserved_paying`] counts an untagged row only against a slug it
    /// was handed, so these are dropped from every total. They are real signed
    /// quotes, and nothing about them says which token they spend: the row's
    /// side names a leg of a book we can't identify, and the amount is
    /// denominated in that book's token, whose decimals we don't know either.
    /// So there is no amount to add — summing one token's units into another's
    /// balance would under-count as easily as over-count.
    ///
    /// A caller that can't account for a claim must not size against the
    /// balance it may have spent. The window is narrow by construction —
    /// untagged rows only exist until a session start tags them or they
    /// expire — and it closes on its own.
    pub fn has_unattributable_claim<'a, I>(&self, known_slugs: I, now_secs: u64) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        let known: std::collections::HashSet<&str> = known_slugs.into_iter().collect();
        self.by_rfq.values().any(|r| {
            r.release_at > now_secs
                && r.input_token.is_none()
                && !known.contains(r.corridor.as_str())
        })
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

    /// Bind the ledger to the vault's on-chain `tradingEpoch`. A bump
    /// invalidates every outstanding signature, so a mismatch drops every
    /// claim — whether the bump happened live or while the process was down
    /// (the ledger persists the epoch it was written under). A ledger with no
    /// recorded epoch (pre-epoch file) keeps its entries: they cannot be
    /// attributed, and keeping them only under-quotes until they expire,
    /// while wrongly dropping a live claim would double-spend inventory.
    pub fn sync_vault_epoch(&mut self, epoch: u64) {
        if self.vault_epoch == Some(epoch) {
            return;
        }
        if self.vault_epoch.is_some() {
            self.by_rfq.clear();
        }
        self.vault_epoch = Some(epoch);
        self.persist();
    }

    /// Last synced epoch — restart diagnostics and tests.
    pub fn vault_epoch(&self) -> Option<u64> {
        self.vault_epoch
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
            vault_epoch: self.vault_epoch,
            entries: self
                .by_rfq
                .iter()
                .map(|(rfq_id, r)| StoredEntry {
                    rfq_id: rfq_id.clone(),
                    corridor: r.corridor.clone(),
                    bid: r.bid,
                    input: r.input.to_string(),
                    release_at: r.release_at,
                    input_token: r.input_token.clone(),
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

fn normalize_token(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn normalize_token_opt(raw: Option<String>) -> Option<String> {
    raw.map(|s| normalize_token(&s)).filter(|s| !s.is_empty())
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
        assert_eq!(
            r.reserved_across(["cngn-usdc", "kes-usdt"], true, 0),
            U256::from(134u64)
        );
        assert_eq!(
            r.reserved_across(["cngn-usdc", "cngn-usdc"], true, 0),
            U256::from(125u64),
            "a repeated slug must not double-count"
        );
    }

    #[test]
    fn a_tagged_claim_survives_after_its_corridor_is_gone() {
        let mut r = Reservations::new();
        r.reserve_paying(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
            Some("0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e"),
        );
        assert_eq!(
            r.reserved_paying(
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                ["wbrl-usdt-celo"],
                true,
                0
            ),
            U256::from(400u64),
            "USDT claims stay after cNGN is dropped from the live books"
        );
        assert_eq!(
            r.reserved_across(["wbrl-usdt-celo"], true, 0),
            U256::ZERO,
            "slug-only totals must not invent a wBRL claim"
        );
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
    fn tag_for_removed_pool_stamps_a_tokenless_claim() {
        let mut r = Reservations::new();
        r.reserve(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
        );
        assert_eq!(
            r.tag_for_removed_pool(
                &["cngn-usdt-celo"],
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                "0x00000000000000000000000000000000000000c1",
            )
            .unwrap(),
            1
        );
        assert_eq!(
            r.reserved_paying(
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                ["wbrl-usdt-celo"],
                true,
                0
            ),
            U256::from(400u64),
            "the leftover cNGN bid must still count against USDT after the slug is gone"
        );
    }

    #[test]
    fn live_tokenless_count_ignores_tagged_and_expired_rows() {
        let mut r = Reservations::new();
        r.reserve("rfq_live", "venue-cngn", true, U256::from(10u64), 1_000);
        r.reserve("rfq_dead", "venue-old", true, U256::from(10u64), 1);
        r.reserve_paying(
            "rfq_tagged",
            "cngn-usdt-celo",
            true,
            U256::from(10u64),
            1_000,
            Some("0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e"),
        );
        assert_eq!(r.live_tokenless_count(10 + RELEASE_SKEW_SECS), 1);
        r.tag_for_removed_pool(
            &["venue-cngn"],
            "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
            "0x00000000000000000000000000000000000000c1",
        )
        .unwrap();
        assert_eq!(r.live_tokenless_count(10 + RELEASE_SKEW_SECS), 0);
    }

    /// The caller gates the removal on the in-memory count, so a write that
    /// didn't land has to be an error — otherwise the pool goes and the restart
    /// reloads the untagged claim.
    #[test]
    fn tag_for_removed_pool_reports_a_failed_write() {
        let dir = std::env::temp_dir().join(format!("stitch-tag-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RESERVATIONS_FILE);
        let mut r = Reservations::with_persist_path(&path);
        r.reserve(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
        );

        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        let original = perms.clone();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o500);
        std::fs::set_permissions(&dir, perms).unwrap();
        let out = r.tag_for_removed_pool(
            &["cngn-usdt-celo"],
            "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
            "0x00000000000000000000000000000000000000c1",
        );
        std::fs::set_permissions(&dir, original).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(out.is_err(), "a failed write must not report success");
    }

    /// A claim signed under a corridor nothing names is still live, and its
    /// side says nothing about which of *our* tokens it spent — the side names
    /// a leg of a book we can't identify. So it is a flag, not an amount.
    #[test]
    fn an_unattributable_claim_is_flagged_whatever_its_side() {
        let mut r = Reservations::new();
        r.reserve(
            "rfq_gone",
            "venue-dropped",
            false,
            U256::from(400u64),
            1_000,
        );
        let now = 10 + RELEASE_SKEW_SECS;

        assert!(r.has_unattributable_claim(["cngn-usdt-celo"], now));
        // An ask-side orphan still counts: a remaining pool sizing its bid may
        // be sizing the very token that ask paid.
        assert!(r.has_unattributable_claim(["cngn-usdt-celo"], now));
        // Known book: `reserved_paying` can place it, so it is not orphaned.
        assert!(!r.has_unattributable_claim(["venue-dropped"], now));
        // Expired rows are nobody's problem.
        assert!(!r.has_unattributable_claim(["other"], 2_000));

        // A tagged row is attributable by definition, even on an unknown slug.
        let mut tagged = Reservations::new();
        tagged.reserve_paying(
            "rfq_tagged",
            "venue-dropped-too",
            true,
            U256::from(9u64),
            1_000,
            Some("0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e"),
        );
        assert!(!tagged.has_unattributable_claim(["cngn-usdt-celo"], now));
    }

    #[test]
    fn tag_for_removed_pool_does_not_guess_a_venue_slug() {
        let mut r = Reservations::new();
        r.reserve("rfq_kept", "venue-wbrl", false, U256::from(50u64), 1_000);
        assert_eq!(
            r.tag_for_removed_pool(
                &["cngn-usdt-celo"],
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                "0x00000000000000000000000000000000000000c1",
            )
            .unwrap(),
            0,
            "a kept pool's venue slug must not inherit the removed pool's tokens"
        );
        assert_eq!(
            r.reserved_paying(
                "0x00000000000000000000000000000000000000c1",
                std::iter::empty(),
                false,
                0
            ),
            U256::ZERO,
            "the unidentified ask must not be stamped as the removed collateral"
        );
    }

    #[test]
    fn tag_books_backfills_a_tokenless_file() {
        let path = tmp_path("tag-books");
        let mut live = Reservations::with_persist_path(&path);
        live.reserve(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
        );
        live.tag_books([(
            "cngn-usdt-celo",
            "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
            "0x00000000000000000000000000000000000000c1",
        )]);
        let restored = Reservations::load(&path, 0).unwrap();
        assert_eq!(
            restored.reserved_paying(
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                std::iter::empty(),
                true,
                0
            ),
            U256::from(400u64)
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_epoch_bump_across_a_restart_drops_the_loaded_ledger() {
        let path = tmp_path("epoch-restart");
        let mut live = Reservations::with_persist_path(&path);
        live.sync_vault_epoch(4);
        live.reserve("rfq_old", "cngn-usdt-celo", true, U256::from(400u64), 1_000);

        // Same epoch after restart: the claims are still live signatures.
        let mut same = Reservations::load(&path, 0).unwrap();
        same.sync_vault_epoch(4);
        assert_eq!(same.len(), 1, "same epoch must keep loaded claims");

        // The vault bumped to 5 while the process was down: every loaded
        // signature is dead, and must not subtract inventory.
        let mut bumped = Reservations::load(&path, 0).unwrap();
        bumped.sync_vault_epoch(5);
        assert!(bumped.is_empty(), "a stale-epoch ledger must be dropped");
        assert_eq!(bumped.vault_epoch(), Some(5));

        // And the drop persists: a second restart loads the new epoch, empty.
        let reloaded = Reservations::load(&path, 0).unwrap();
        assert!(reloaded.is_empty());
        assert_eq!(reloaded.vault_epoch(), Some(5));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_ledger_without_an_epoch_keeps_its_claims_on_first_sync() {
        // Pre-epoch file (or in-memory EOA ledger): the claims cannot be
        // attributed to an epoch, and keeping them only under-quotes.
        let mut r = Reservations::new();
        r.reserve(
            "rfq_legacy",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
        );
        r.sync_vault_epoch(3);
        assert_eq!(r.len(), 1);
        // A live bump after binding still clears.
        r.sync_vault_epoch(4);
        assert!(r.is_empty());
        let _ = r;
    }

    #[test]
    fn a_token_tag_survives_a_restart() {
        let path = tmp_path("token");
        let mut live = Reservations::with_persist_path(&path);
        live.reserve_paying(
            "rfq_cngn",
            "cngn-usdt-celo",
            true,
            U256::from(400u64),
            1_000,
            Some("0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e"),
        );
        let restored = Reservations::load(&path, 0).unwrap();
        assert_eq!(
            restored.reserved_paying(
                "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e",
                std::iter::empty(),
                true,
                0
            ),
            U256::from(400u64)
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
