// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! An issuer's spread from a Textile feed switch, checked by this bot before
//! it quotes at it.
//!
//! A feed switch lets an issuer (IDRX for `idrx-usdt`) pick the venue a
//! group of pools prices from, and set the buy and sell offsets they quote
//! at. Textile's `/price` serves the spread next to the price, with the
//! issuer's signature. Textile only relays it: a bot that opts in with an
//! `[issuer_spread]` table applies a spread only when
//!
//! - its signer is one of the wallets in that table, and the signature is
//!   theirs over the exact message [`spread_message`] rebuilds from this
//!   bot's own switch, environment and chain (EIP-191; ERC-1271 on this
//!   chain for a contract wallet such as a Safe);
//! - the nonce secret served with it hashes to the signed nonce. Only the
//!   issuer's browser had that secret until it submitted the message, so a
//!   message the issuer signed and abandoned (a Safe publishes every one)
//!   can't be relayed;
//! - its switch revision is above the highest this bot accepted, or equal
//!   to it over the same message. The bot keeps that across restarts
//!   ([`state_path`]), so an older spread the issuer once signed can't be
//!   replayed, and applies nothing while it can't keep it;
//! - it came on a response naming this bot's switch at a switch revision
//!   no older than any response seen before, before and after the check
//!   ([`Gate::spread_for`]). That revision is kept across restarts too, so
//!   a response from before a withdrawal can't bring the spread back;
//! - per pool, both offsets sit inside the pool's `issuer_spread_min_bps`
//!   and `issuer_spread_max_bps` ([`applicable`]).
//!
//! Otherwise the pool quotes the offsets in `stitch.toml`, exactly as it
//! would without the table. Textile can withdraw a spread (the field goes
//! away) but can't make one up, and the operator's bounds hold either way.
//!
//! The message format is shared with `@textile/constants`
//! (`feedSwitchMessage`). `tests/fixtures/feed_switch_spread.json` holds the
//! two builders to the same bytes. `Expires` bounds when Textile may accept
//! the message, not how long a spread lasts, so it isn't checked here.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use alloy_primitives::{hex, keccak256, Address, Bytes, B256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::chain::rpc::Rpc;
use crate::config::{IssuerSignerKind, IssuerSpreadConfig};
use crate::rfq::iso8601::format_iso_ms;
use crate::signer::recover_address;

/// Offsets are below this, as the bid side requires (`buy_offset_bps`).
pub const SPREAD_LIMIT_BPS: u32 = 10_000;

/// Longest `Expires` minus `Issued` the API allows. A wider window is a
/// message the API never accepted.
const MAX_VALIDITY_MS: u64 = 24 * 60 * 60 * 1000;

/// ERC-1271's magic value: `isValidSignature(bytes32,bytes)`'s selector.
const ERC1271_MAGIC: [u8; 4] = [0x16, 0x26, 0xba, 0x7e];

/// Shared with `@textile/constants`; part of the signed bytes.
const FOOTER: &str = "Signing this moves no funds. It sets the spread the bots \
                      behind this switch quote at, within limits each bot sets for itself.";

/// Where the highest accepted revision is kept: beside the config, scoped
/// by its name and the chain, like the other per-bot state files, e.g.
/// `stitch.42220.issuer-spread.json`.
pub fn state_path(config_path: &Path, chain_id: u64) -> PathBuf {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("stitch");
    dir.join(format!("{stem}.{chain_id}.issuer-spread.json"))
}

/// How long a contract signer's spread waits before its ERC-1271 check runs
/// again, after the check failed or said no. That answer comes from this
/// bot's RPC, which can be down or a few blocks behind the node Textile
/// checked it on (a Safe that signs on chain), so it isn't final. One
/// `eth_call` a minute at most while it stays unsettled.
pub const RETRY_UNSETTLED: Duration = Duration::from_secs(60);

/// The spread as `/price` serves it. Nothing in it is trusted until
/// [`Gate::verify`] says so.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedSpread {
    pub buy_bps: u32,
    pub sell_bps: u32,
    pub revision: u64,
    pub nonce: String,
    pub nonce_secret: String,
    /// Epoch milliseconds.
    pub issued_at: u64,
    /// Epoch milliseconds.
    pub expires_at: u64,
    pub signer: String,
    /// Hex. Empty (`0x`) from a Safe that signed on chain.
    pub signature: String,
}

/// A spread this bot checked the issuer signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssuerSpread {
    pub buy_bps: u32,
    pub sell_bps: u32,
    pub revision: u64,
}

/// What a print carries about an issuer spread.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SpreadSlot {
    /// The feed served none, or served something unreadable.
    #[default]
    None,
    /// As served; not checked, never applied.
    Served(Box<SignedSpread>),
    /// Checked by [`Gate::verify`].
    Verified(IssuerSpread),
}

impl SpreadSlot {
    /// The spread to quote at, if this bot checked one.
    pub fn verified(&self) -> Option<IssuerSpread> {
        match self {
            SpreadSlot::Verified(spread) => Some(*spread),
            _ => None,
        }
    }

    /// The verified offsets, `(buy_bps, sell_bps)`, or none. Two prints that
    /// agree here price a pool alike. A path that signs after pricing
    /// compares the print it priced from with the one the cache holds after
    /// signing: a spread an issuer set, or Textile withdrew, meanwhile moves
    /// the price by less than the band would notice.
    pub fn offsets(&self) -> Option<(u32, u32)> {
        self.verified().map(|s| (s.buy_bps, s.sell_bps))
    }
}

/// `spread` on a print, read leniently: anything malformed reads as none,
/// so an optional field can never take a corridor dark.
pub fn lenient_slot<'de, D: serde::Deserializer<'de>>(d: D) -> Result<SpreadSlot, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(serde_json::from_value::<SignedSpread>(value)
        .map(|spread| SpreadSlot::Served(Box::new(spread)))
        .unwrap_or_default())
}

/// The range, inclusive, a pool takes an issuer's offsets in, for each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpreadBounds {
    pub min_bps: u32,
    pub max_bps: u32,
}

/// The spread a pool quotes at: `spread` when the pool takes one, both
/// offsets sit inside its bounds, and, for a pool that needs an even spread
/// (the inventory curve), they match. Pure.
pub fn applicable(
    bounds: Option<SpreadBounds>,
    even_only: bool,
    spread: Option<IssuerSpread>,
) -> Option<IssuerSpread> {
    let (bounds, spread) = (bounds?, spread?);
    let inside = |bps: u32| (bounds.min_bps..=bounds.max_bps).contains(&bps);
    (inside(spread.buy_bps)
        && inside(spread.sell_bps)
        && (!even_only || spread.buy_bps == spread.sell_bps))
        .then_some(spread)
}

/// [`applicable`], noting once per pool and revision when a pool that takes
/// issuer spreads passes on one, so the operator can see why it quotes its
/// own offsets.
pub fn applied(
    scope: &str,
    bounds: Option<SpreadBounds>,
    even_only: bool,
    spread: Option<IssuerSpread>,
) -> Option<IssuerSpread> {
    let chosen = applicable(bounds, even_only, spread);
    if let (Some(bounds), Some(spread), None) = (bounds, spread, chosen) {
        note_passed_over(scope, bounds, even_only, spread);
    }
    chosen
}

fn note_passed_over(scope: &str, bounds: SpreadBounds, even_only: bool, spread: IssuerSpread) {
    static NOTED: StdMutex<Option<HashSet<(String, u64)>>> = StdMutex::new(None);
    let fresh = NOTED
        .lock()
        .map(|mut noted| {
            noted
                .get_or_insert_with(HashSet::new)
                .insert((scope.to_string(), spread.revision))
        })
        .unwrap_or(false);
    if fresh {
        warn!(
            pool = %scope,
            revision = spread.revision,
            buy_bps = spread.buy_bps,
            sell_bps = spread.sell_bps,
            min_bps = bounds.min_bps,
            max_bps = bounds.max_bps,
            even_only,
            "issuer spread outside this pool's issuer_spread bounds; quoting its own offsets"
        );
    }
}

/// The exact text the issuer signed, from this bot's own switch,
/// environment and chain and the served offsets, revision, nonce and window.
/// A message signed for another switch or environment can't match.
pub fn spread_message(table: &IssuerSpreadConfig, chain_id: u64, spread: &SignedSpread) -> String {
    let norm = |s: &str| s.trim().to_lowercase();
    [
        "Textile feed switch".to_string(),
        String::new(),
        "Action: spread".to_string(),
        format!("Environment: {}", norm(&table.environment)),
        format!("Switch: {}", norm(&table.switch)),
        format!("Chain: {chain_id}"),
        format!("Bid: {} bps below the price", spread.buy_bps),
        format!("Ask: {} bps above the price", spread.sell_bps),
        format!("Revision: {}", spread.revision),
        format!("Nonce: {}", norm(&spread.nonce)),
        format!("Issued: {}", format_iso_ms(spread.issued_at)),
        format!("Expires: {}", format_iso_ms(spread.expires_at)),
        String::new(),
        FOOTER.to_string(),
    ]
    .join("\n")
}

/// The digest `personal_sign` signs: EIP-191 over the message's bytes.
pub fn eip191_hash(message: &str) -> B256 {
    let mut bytes = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
    bytes.extend_from_slice(message.as_bytes());
    keccak256(&bytes)
}

fn bytes32(raw: &str) -> Option<[u8; 32]> {
    let hex = raw.trim().strip_prefix("0x")?;
    let bytes = hex::decode(hex).ok()?;
    bytes.try_into().ok()
}

fn signature_bytes(raw: &str) -> Option<Vec<u8>> {
    hex::decode(raw.trim().strip_prefix("0x")?).ok()
}

/// What's wrong with a served spread on its face, before any signature work:
/// offsets out of range, a nonce secret that doesn't hash to the nonce, or a
/// window the API would never have accepted. Pure.
pub fn shape_problem(spread: &SignedSpread) -> Option<&'static str> {
    if spread.buy_bps >= SPREAD_LIMIT_BPS || spread.sell_bps >= SPREAD_LIMIT_BPS {
        return Some("offsets out of range");
    }
    let (Some(nonce), Some(secret)) = (bytes32(&spread.nonce), bytes32(&spread.nonce_secret))
    else {
        return Some("nonce or nonce secret is not 32 bytes of hex");
    };
    if keccak256(secret).0 != nonce {
        return Some("nonce secret doesn't match the signed nonce");
    }
    let window = spread.expires_at.checked_sub(spread.issued_at);
    if !window.is_some_and(|w| w > 0 && w <= MAX_VALIDITY_MS) {
        return Some("validity window the API never accepts");
    }
    None
}

/// The replay rule: a revision above the highest accepted, or the same
/// revision over the same message (the spread in force, served again). The
/// message is compared by its EIP-191 `digest`, never by signature: a Safe
/// that signs on chain hands back the same empty signature for every
/// message it approved, so a signature can't tell two spreads apart. Pure.
pub fn replay_problem(revision: u64, digest: B256, last: Option<&Accepted>) -> Option<String> {
    let last = last?;
    if revision > last.revision {
        return None;
    }
    if revision == last.revision
        && last
            .digest
            .eq_ignore_ascii_case(&hex::encode_prefixed(digest))
    {
        return None;
    }
    Some(format!(
        "revision {revision} doesn't move past the {} already accepted",
        last.revision
    ))
}

/// The spread a bot last accepted for one switch: its revision, and the
/// EIP-191 digest of its message, `0x` hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub revision: u64,
    pub digest: String,
}

/// What the state file keeps for one switch identity (environment, switch,
/// chain; see [`identity_key`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Kept {
    /// The replay floor ([`replay_problem`]).
    pub accepted: Option<Accepted>,
    /// The newest switch revision any `/price` response carried, with or
    /// without a spread. A response below it is out of date, its spread
    /// included: kept, so a restart doesn't forget a withdrawal.
    #[serde(default)]
    pub newest_switch_revision: u64,
}

/// The state file: one entry per switch identity this config has taken
/// spreads from, so pointing the config at another switch and back keeps
/// each one's floor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFile {
    pub identities: BTreeMap<String, Kept>,
}

/// The first format: the one spread accepted, with the switch it was for.
#[derive(Deserialize)]
struct LegacyAccepted {
    environment: String,
    switch: String,
    chain_id: u64,
    revision: u64,
    digest: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OnDisk {
    Current(StateFile),
    Legacy(LegacyAccepted),
}

/// The key a switch identity is kept under. Pure.
pub fn identity_key(environment: &str, switch: &str, chain_id: u64) -> String {
    let norm = |s: &str| s.trim().to_lowercase();
    format!("{}|{}|{chain_id}", norm(environment), norm(switch))
}

fn read_state(path: &Path) -> anyhow::Result<StateFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(match serde_json::from_str(&text)? {
            OnDisk::Current(file) => file,
            OnDisk::Legacy(old) => StateFile {
                identities: BTreeMap::from([(
                    identity_key(&old.environment, &old.switch, old.chain_id),
                    Kept {
                        accepted: Some(Accepted {
                            revision: old.revision,
                            digest: old.digest,
                        }),
                        newest_switch_revision: 0,
                    },
                )]),
            },
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::default()),
        Err(e) => Err(e.into()),
    }
}

/// Write, flush to disk, rename over, then flush the directory, so a crash
/// leaves the old state or the new one, never a torn file.
fn write_state(path: &Path, file: &StateFile) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let mut out = std::fs::File::create(&tmp)?;
    out.write_all(&serde_json::to_vec(file)?)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// The state file as a gate holds it, and where it's written.
struct Persisted {
    /// `None` in a dry run: nothing is written.
    path: Option<PathBuf>,
    file: StateFile,
    /// This gate's identity ([`identity_key`]).
    key: String,
    /// What's held isn't all on disk: a write failed, or the file couldn't
    /// be written at startup. A restart would only know what's on disk,
    /// which may be from before a withdrawal, so no spread applies until a
    /// write succeeds ([`Persisted::flush`] retries on every response).
    unsaved: bool,
}

impl Persisted {
    fn own(&self) -> Kept {
        self.file
            .identities
            .get(&self.key)
            .cloned()
            .unwrap_or_default()
    }

    /// Write `kept` as this identity's entry, the others as they are, then
    /// hold it. Nothing held changes when the file can't be written.
    fn save(&mut self, kept: Kept) -> anyhow::Result<()> {
        let mut next = self.file.clone();
        next.identities.insert(self.key.clone(), kept);
        if let Some(path) = &self.path {
            write_state(path, &next)?;
        }
        self.file = next;
        self.unsaved = false;
        Ok(())
    }

    /// Hold `kept` whether or not it can be written, then write everything
    /// held. A newer switch revision has to count at once, written or not:
    /// the write failing only stops spreads ([`Persisted::unsaved`]).
    fn hold(&mut self, kept: Kept) {
        let key = self.key.clone();
        self.file.identities.insert(key, kept);
        self.flush();
    }

    /// Write everything held. False, with [`Persisted::unsaved`] set, when
    /// the file can't be written.
    fn flush(&mut self) -> bool {
        let Some(path) = &self.path else {
            self.unsaved = false;
            return true;
        };
        match write_state(path, &self.file) {
            Ok(()) => {
                if self.unsaved {
                    info!(path = %path.display(), "the issuer spread state is written again; spreads can apply");
                }
                self.unsaved = false;
            }
            Err(e) => {
                if !self.unsaved {
                    error!(
                        error = %e,
                        path = %path.display(),
                        "can't write the issuer spread state; taking no issuer spread until it can"
                    );
                }
                self.unsaved = true;
            }
        }
        !self.unsaved
    }
}

#[derive(Debug, Default)]
struct GateState {
    /// The served spread last accepted, so a repeat costs no crypto.
    verified: Option<(SignedSpread, IssuerSpread)>,
    /// The served spread last refused for good, so a repeat is refused
    /// quietly.
    refused: Option<SignedSpread>,
    /// A served spread whose contract check didn't pass yet, and when to
    /// run it again.
    unsettled: Option<(SignedSpread, Instant)>,
}

/// Why a served spread isn't applied.
#[derive(Debug, PartialEq)]
enum Refusal {
    /// Decided from the spread and this bot's config alone; the same
    /// spread will never pass.
    Final(String),
    /// The contract signer's check failed or said no on this bot's RPC. It
    /// can pass later, so it's retried after [`RETRY_UNSETTLED`].
    Unsettled(String),
}

/// Checks served spreads for one `[issuer_spread]` table. Shared by every
/// feed this bot reads, so both price loops agree on what's accepted.
pub struct Gate {
    table: IssuerSpreadConfig,
    chain_id: u64,
    signers: Vec<(Address, IssuerSignerKind)>,
    rpc: Rpc,
    /// `None` in a dry run: nothing is kept or recorded.
    config_path: Option<PathBuf>,
    /// Why this gate applies nothing until the bot restarts: its kept
    /// revision couldn't be read, so it can't tell a replay from a new
    /// spread. The pools quote their own offsets meanwhile.
    blocked: Option<String>,
    /// [`RETRY_UNSETTLED`], shorter in tests.
    retry_unsettled: Duration,
    /// The accepted spread and newest switch revision, as kept on disk.
    /// Shared by both price loops; never held across an await.
    persisted: StdMutex<Persisted>,
    state: Mutex<GateState>,
}

impl Gate {
    /// Reads the accepted revision kept at [`state_path`]. One that can't be
    /// read blocks the gate rather than the bot: starting without it would
    /// let an older spread back in, and stopping would take every pool down
    /// with it.
    pub fn new(
        table: IssuerSpreadConfig,
        chain_id: u64,
        rpc: Rpc,
        config_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let signers = table
            .signers
            .iter()
            .map(|s| Ok((s.address.trim().parse::<Address>()?, s.kind)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let path = config_path.map(|path| state_path(path, chain_id));
        let (file, blocked) = match path.as_deref().map(read_state) {
            Some(Err(e)) => {
                let problem = format!(
                    "{}: {e}",
                    path.as_deref().unwrap_or(Path::new("")).display()
                );
                error!(
                    %problem,
                    "can't read the kept issuer spread state; taking no issuer spread until \
                     the file is fixed or removed and the bot restarts"
                );
                (StateFile::default(), Some(problem))
            }
            Some(Ok(file)) => (file, None),
            None => (StateFile::default(), None),
        };
        let key = identity_key(&table.environment, &table.switch, chain_id);
        let mut persisted = Persisted {
            path,
            file,
            key,
            unsaved: false,
        };
        // A file this bot can't write can't keep a withdrawal across the
        // next restart either, so find out now. An unreadable one is left
        // alone: the gate is blocked, and writing would lose it.
        if blocked.is_none() {
            persisted.flush();
        }
        Ok(Self {
            table,
            chain_id,
            signers,
            rpc,
            config_path: config_path.map(Path::to_path_buf),
            blocked,
            retry_unsettled: RETRY_UNSETTLED,
            persisted: StdMutex::new(persisted),
            state: Mutex::new(GateState::default()),
        })
    }

    fn is_own_switch(&self, switch: &str) -> bool {
        switch.trim().to_lowercase() == self.table.switch.trim().to_lowercase()
    }

    /// Record a `/price` response from `switch` at `revision`, and say
    /// whether it's as new as any this gate has seen. Both price loops fetch
    /// on their own and the cache orders prints by the price, not the
    /// switch, so a response answered before a withdrawal can land after one
    /// answered after it. A newer revision counts at once and is written to
    /// disk; while it can't be, no spread applies ([`Persisted::unsaved`]). A response naming another switch is never current:
    /// the message is rebuilt from this bot's own switch, so a spread on it
    /// would verify, and its revision says nothing about this switch.
    pub fn observe(&self, switch: &str, revision: u64) -> bool {
        if self.blocked.is_some() || !self.is_own_switch(switch) {
            return false;
        }
        let Ok(mut persisted) = self.persisted.lock() else {
            return false;
        };
        let mut kept = persisted.own();
        if revision < kept.newest_switch_revision {
            return false;
        }
        if revision > kept.newest_switch_revision {
            kept.newest_switch_revision = revision;
            persisted.hold(kept);
        } else if persisted.unsaved {
            persisted.flush();
        }
        true
    }

    /// Whether a spread from a response at `revision` may apply now: it's
    /// still the newest seen (a response that was current before a slow
    /// check may not be after it), and everything held is on disk.
    fn still_current(&self, revision: u64) -> bool {
        self.persisted
            .lock()
            .is_ok_and(|p| !p.unsaved && revision >= p.own().newest_switch_revision)
    }

    fn accepted(&self) -> Option<Accepted> {
        self.persisted.lock().ok().and_then(|p| p.own().accepted)
    }

    /// The spread a print may apply: `slot` as a response served it, with
    /// the switch and revision it answered at (`feedSwitch` and `revision`,
    /// unsigned). Only a response that names this gate's switch, at a
    /// revision no older than any seen, both before and after the
    /// signature check, applies its spread. A served spread without them is
    /// never applied: a relay that drops them can't step around the order.
    pub async fn spread_for(&self, slot: &SpreadSlot, switch: Option<(String, u64)>) -> SpreadSlot {
        let current = switch
            .as_ref()
            .is_some_and(|(name, revision)| self.observe(name, *revision));
        let (SpreadSlot::Served(served), Some((_, revision)), true) = (slot, switch, current)
        else {
            return SpreadSlot::None;
        };
        // Nothing can apply while the state isn't on disk, so don't check
        // (or try to keep) a spread until it is.
        if !self.still_current(revision) {
            return SpreadSlot::None;
        }
        match self.verify(served).await {
            Some(spread) if self.still_current(revision) => SpreadSlot::Verified(spread),
            _ => SpreadSlot::None,
        }
    }

    /// The spread to apply from what a print served, or `None` to quote the
    /// configured offsets. Never fails the print.
    pub async fn verify(&self, served: &SignedSpread) -> Option<IssuerSpread> {
        if self.blocked.is_some() {
            return None;
        }
        let last = {
            let state = self.state.lock().await;
            if let Some((seen, spread)) = &state.verified {
                if seen == served {
                    return Some(*spread);
                }
            }
            if state.refused.as_ref() == Some(served) {
                return None;
            }
            if let Some((seen, retry_at)) = &state.unsettled {
                if seen == served && Instant::now() < *retry_at {
                    return None;
                }
            }
            self.accepted()
        };
        // A contract signer's check is an RPC call that can take seconds, and
        // both price loops read through this gate: it runs without the lock,
        // and the replay rule runs again under it before anything is kept.
        let digest = eip191_hash(&spread_message(&self.table, self.chain_id, served));
        let checked = self.check(served, digest, last.as_ref()).await;
        let mut state = self.state.lock().await;
        let outcome = checked.and_then(|spread| {
            match replay_problem(spread.revision, digest, self.accepted().as_ref()) {
                Some(problem) => Err(Refusal::Final(problem)),
                None => self.accept(&mut state, served, spread, digest),
            }
        });
        match outcome {
            Ok(spread) => Some(spread),
            Err(Refusal::Final(reason)) => {
                warn!(
                    revision = served.revision,
                    signer = %served.signer,
                    %reason,
                    "issuer spread refused; quoting the stitch.toml offsets"
                );
                state.refused = Some(served.clone());
                state.unsettled = None;
                None
            }
            Err(Refusal::Unsettled(reason)) => {
                warn!(
                    revision = served.revision,
                    signer = %served.signer,
                    %reason,
                    retry_in_secs = self.retry_unsettled.as_secs(),
                    "issuer spread not verified yet; quoting the stitch.toml offsets until it is"
                );
                state.unsettled = Some((served.clone(), Instant::now() + self.retry_unsettled));
                None
            }
        }
    }

    async fn check(
        &self,
        served: &SignedSpread,
        digest: B256,
        last: Option<&Accepted>,
    ) -> Result<IssuerSpread, Refusal> {
        let fail = |reason: &str| Refusal::Final(reason.to_string());
        if let Some(problem) = shape_problem(served) {
            return Err(fail(problem));
        }
        if let Some(problem) = replay_problem(served.revision, digest, last) {
            return Err(Refusal::Final(problem));
        }
        let signer: Address = served
            .signer
            .trim()
            .parse()
            .map_err(|_| fail("signer is not an address"))?;
        let kind = self
            .signers
            .iter()
            .find(|(address, _)| *address == signer)
            .map(|(_, kind)| *kind)
            .ok_or_else(|| fail("signer isn't listed in [issuer_spread].signers"))?;
        let signature =
            signature_bytes(&served.signature).ok_or_else(|| fail("signature is not hex"))?;
        match kind {
            IssuerSignerKind::Eoa if recovers_to(digest, &signature, signer) => {}
            IssuerSignerKind::Eoa => return Err(fail("signature doesn't match the signer")),
            IssuerSignerKind::Contract => match erc1271_valid(&self.rpc, signer, digest, signature)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return Err(Refusal::Unsettled(
                        "the contract signer's isValidSignature said no".to_string(),
                    ))
                }
                Err(e) => return Err(Refusal::Unsettled(format!("ERC-1271 check failed: {e}"))),
            },
        }
        Ok(IssuerSpread {
            buy_bps: served.buy_bps,
            sell_bps: served.sell_bps,
            revision: served.revision,
        })
    }

    /// Take a checked spread. A revision that moves past the last one is
    /// kept on disk first: if it can't be, the spread isn't applied (and is
    /// tried again later), since after a restart this bot couldn't refuse
    /// an older one.
    fn accept(
        &self,
        state: &mut GateState,
        served: &SignedSpread,
        spread: IssuerSpread,
        digest: B256,
    ) -> Result<IssuerSpread, Refusal> {
        let moved = self
            .accepted()
            .is_none_or(|last| spread.revision > last.revision);
        if moved {
            {
                let mut persisted = self
                    .persisted
                    .lock()
                    .map_err(|_| Refusal::Unsettled("the kept state is unavailable".to_string()))?;
                let mut kept = persisted.own();
                kept.accepted = Some(Accepted {
                    revision: spread.revision,
                    digest: hex::encode_prefixed(digest),
                });
                persisted.save(kept).map_err(|e| {
                    Refusal::Unsettled(format!(
                        "couldn't keep revision {}{}: {e}",
                        spread.revision,
                        persisted
                            .path
                            .as_deref()
                            .map(|p| format!(" at {}", p.display()))
                            .unwrap_or_default()
                    ))
                })?;
            }
            if let Some(config_path) = self.config_path.as_deref() {
                // A missed record is a gap in the history, not a wrong price.
                if let Err(e) = record_accepted(config_path, self.chain_id, served) {
                    warn!(error = %e, "couldn't record the issuer spread in the change log");
                }
            }
            info!(
                revision = spread.revision,
                buy_bps = spread.buy_bps,
                sell_bps = spread.sell_bps,
                signer = %served.signer,
                "issuer spread accepted"
            );
        }
        state.verified = Some((served.clone(), spread));
        state.refused = None;
        state.unsettled = None;
        Ok(spread)
    }
}

fn recovers_to(digest: B256, signature: &[u8], signer: Address) -> bool {
    let Ok(mut sig) = <[u8; 65]>::try_from(signature) else {
        return false;
    };
    // Wallets return v as 27/28 or as 0/1.
    if sig[64] < 27 {
        sig[64] += 27;
    }
    recover_address(digest, &sig).is_ok_and(|recovered| recovered == signer)
}

/// `isValidSignature(bytes32,bytes)` on the signer's contract, on this
/// chain. A revert or an empty answer is "no". ERC-6492 (a wallet not yet
/// deployed) isn't supported: list deployed contracts only.
async fn erc1271_valid(
    rpc: &Rpc,
    signer: Address,
    digest: B256,
    signature: Vec<u8>,
) -> anyhow::Result<bool> {
    let mut data = keccak256("isValidSignature(bytes32,bytes)".as_bytes()).0[..4].to_vec();
    data.extend((digest, Bytes::from(signature)).abi_encode_params());
    let out = rpc.eth_call(signer, &Bytes::from(data)).await?;
    // The full word, as OpenZeppelin's SignatureChecker reads it: the magic
    // value then zeros. A fallback that echoes calldata starts with the
    // same four bytes.
    Ok(out.len() >= 32 && out[..4] == ERC1271_MAGIC && out[4..32].iter().all(|b| *b == 0))
}

/// One change-log row per accepted revision, at bot scope, so Textile can
/// see which bots apply which spread. Shipped with the rest when
/// `[change_log] ship = true`.
fn record_accepted(config_path: &Path, chain_id: u64, served: &SignedSpread) -> anyhow::Result<()> {
    use crate::changelog::journal::{append, Journal};
    use crate::changelog::record::{random_uuid, ChangeRecord};
    let record = ChangeRecord {
        id: random_uuid(),
        change_set_id: random_uuid(),
        changed_at: format_iso_ms(crate::time::unix_now_ms()),
        detected_at: None,
        source: "bot.issuer-spread".to_string(),
        actor: format!("feed-switch-signer:{}", served.signer.trim().to_lowercase()),
        bot: None,
        chain_id,
        scope: "bot".to_string(),
        scope_label: None,
        parameter: "issuer_spread.revision".to_string(),
        unit: "revision".to_string(),
        before: None,
        after: Some(served.revision.to_string()),
        config_sha256_before: None,
        config_sha256_after: None,
    };
    append(&Journal::open(config_path, None)?, &[record])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IssuerSignerConfig;

    #[derive(Deserialize)]
    struct Fixture {
        signer: FixtureSigner,
        cases: Vec<FixtureCase>,
    }
    #[derive(Deserialize)]
    struct FixtureSigner {
        address: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureCase {
        name: String,
        intent: FixtureIntent,
        message: String,
        signature: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureIntent {
        environment: String,
        key: String,
        chain_id: u64,
        buy_bps: u32,
        sell_bps: u32,
        revision: u64,
        nonce: String,
        issued_at: u64,
        expires_at: u64,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../../tests/fixtures/feed_switch_spread.json")).unwrap()
    }

    fn table(env: &str, switch: &str, signer: &str, kind: IssuerSignerKind) -> IssuerSpreadConfig {
        IssuerSpreadConfig {
            switch: switch.to_string(),
            environment: env.to_string(),
            signers: vec![IssuerSignerConfig {
                address: signer.to_string(),
                kind,
            }],
        }
    }

    /// The fixture's first case as served, with the nonce secret behind its
    /// nonce (`0xab…ab`, as the fixture generator used).
    fn served(case: &FixtureCase) -> SignedSpread {
        let secret = match case.name.as_str() {
            "whole seconds" => "ab",
            "milliseconds and zero offsets" => "01",
            _ => "ff",
        };
        SignedSpread {
            buy_bps: case.intent.buy_bps,
            sell_bps: case.intent.sell_bps,
            revision: case.intent.revision,
            nonce: case.intent.nonce.clone(),
            nonce_secret: format!("0x{}", secret.repeat(32)),
            issued_at: case.intent.issued_at,
            expires_at: case.intent.expires_at,
            signer: fixture().signer.address,
            signature: case.signature.clone(),
        }
    }

    fn gate_for(fx: &Fixture, config_path: Option<&Path>) -> Gate {
        Gate::new(
            table(
                "app.textilecredit.com",
                "idrx-usdt",
                &fx.signer.address,
                IssuerSignerKind::Eoa,
            ),
            42220,
            Rpc::new("http://127.0.0.1:1"),
            config_path,
        )
        .unwrap()
    }

    #[test]
    fn rebuilds_the_exact_text_the_web_signs() {
        let fx = fixture();
        for case in &fx.cases {
            let t = table(
                &case.intent.environment,
                &case.intent.key,
                &fx.signer.address,
                IssuerSignerKind::Eoa,
            );
            let spread = served(case);
            assert_eq!(
                spread_message(&t, case.intent.chain_id, &spread),
                case.message,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn the_fixture_signatures_recover_to_the_signer() {
        let fx = fixture();
        let signer: Address = fx.signer.address.parse().unwrap();
        for case in &fx.cases {
            let sig = signature_bytes(&case.signature).unwrap();
            assert!(
                recovers_to(eip191_hash(&case.message), &sig, signer),
                "{}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn accepts_a_listed_signer_and_repeats_without_crypto() {
        let fx = fixture();
        let gate = gate_for(&fx, None);
        let spread = served(&fx.cases[0]);
        let got = gate.verify(&spread).await.unwrap();
        assert_eq!((got.buy_bps, got.sell_bps, got.revision), (25, 30, 12));
        assert_eq!(gate.verify(&spread).await, Some(got));
    }

    #[tokio::test]
    async fn refuses_anything_the_issuer_didnt_sign() {
        let fx = fixture();
        let gate = gate_for(&fx, None);
        let base = served(&fx.cases[0]);
        // Textile changing the offsets breaks the signature.
        let widened = SignedSpread {
            sell_bps: 31,
            ..base.clone()
        };
        assert_eq!(gate.verify(&widened).await, None);
        // So does relaying it for another switch or environment.
        let other = Gate::new(
            table(
                "staging.textilecredit.com",
                "idrx-usdt",
                &fx.signer.address,
                IssuerSignerKind::Eoa,
            ),
            42220,
            Rpc::new("http://127.0.0.1:1"),
            None,
        )
        .unwrap();
        assert_eq!(other.verify(&base).await, None);
        // A signer this bot doesn't list.
        let stranger = Gate::new(
            table(
                "app.textilecredit.com",
                "idrx-usdt",
                "0x0000000000000000000000000000000000000001",
                IssuerSignerKind::Eoa,
            ),
            42220,
            Rpc::new("http://127.0.0.1:1"),
            None,
        )
        .unwrap();
        assert_eq!(stranger.verify(&base).await, None);
    }

    #[tokio::test]
    async fn refuses_a_message_relayed_without_its_secret() {
        let fx = fixture();
        let gate = gate_for(&fx, None);
        let abandoned = SignedSpread {
            nonce_secret: format!("0x{}", "cd".repeat(32)),
            ..served(&fx.cases[0])
        };
        assert_eq!(gate.verify(&abandoned).await, None);
    }

    #[tokio::test]
    async fn never_goes_back_to_an_older_revision_even_after_a_restart() {
        let fx = fixture();
        let dir = std::env::temp_dir().join(format!("issuer-spread-{}", random_uuid_for_test()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("stitch.toml");
        // Revision 12 accepted and kept.
        let newer = served(&fx.cases[0]);
        assert!(gate_for(&fx, Some(&config_path))
            .verify(&newer)
            .await
            .is_some());
        // A restarted bot refuses revision 7, and still takes 12 again.
        let restarted = gate_for(&fx, Some(&config_path));
        assert_eq!(restarted.verify(&served(&fx.cases[1])).await, None);
        assert!(restarted.verify(&newer).await.is_some());
        let kept = read_state(&dir.join("stitch.42220.issuer-spread.json")).unwrap();
        let own = &kept.identities[&identity_key("app.textilecredit.com", "idrx-usdt", 42220)];
        assert_eq!(own.accepted.as_ref().map(|a| a.revision), Some(12));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn each_switch_keeps_its_own_floor_in_the_one_file() {
        let fx = fixture();
        let dir = std::env::temp_dir().join(format!("issuer-spread-{}", random_uuid_for_test()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("stitch.toml");
        let staging_key = identity_key("staging.textilecredit.com", "idrx-usdt", 42220);
        let staging = Kept {
            accepted: Some(Accepted {
                revision: 300,
                digest: format!("0x{}", "00".repeat(32)),
            }),
            newest_switch_revision: 301,
        };
        write_state(
            &state_path(&config_path, 42220),
            &StateFile {
                identities: BTreeMap::from([(staging_key.clone(), staging.clone())]),
            },
        )
        .unwrap();
        // Staging's floor says nothing about production's switch.
        assert!(gate_for(&fx, Some(&config_path))
            .verify(&served(&fx.cases[0]))
            .await
            .is_some());
        // Production's acceptance kept staging's entry, so pointing the config
        // back at staging still refuses what it already moved past.
        let kept = read_state(&state_path(&config_path, 42220)).unwrap();
        assert_eq!(kept.identities[&staging_key], staging);
        assert_eq!(kept.identities.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_the_first_state_format() {
        let dir = std::env::temp_dir().join(format!("issuer-spread-{}", random_uuid_for_test()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stitch.42220.issuer-spread.json");
        std::fs::write(
            &path,
            r#"{"environment":"app.textilecredit.com","switch":"idrx-usdt","chain_id":42220,"revision":12,"digest":"0xab"}"#,
        )
        .unwrap();
        let kept = read_state(&path).unwrap();
        assert_eq!(
            kept.identities[&identity_key("app.textilecredit.com", "idrx-usdt", 42220)],
            Kept {
                accepted: Some(Accepted {
                    revision: 12,
                    digest: "0xab".into()
                }),
                newest_switch_revision: 0,
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn applies_nothing_while_the_revision_can_t_be_read_or_kept() {
        let fx = fixture();
        let spread = served(&fx.cases[0]);
        // Unreadable: the gate is blocked, the bot isn't.
        let dir = std::env::temp_dir().join(format!("issuer-spread-{}", random_uuid_for_test()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("stitch.toml");
        std::fs::write(state_path(&config_path, 42220), b"{torn").unwrap();
        assert_eq!(
            gate_for(&fx, Some(&config_path)).verify(&spread).await,
            None
        );
        std::fs::remove_dir_all(&dir).ok();
        // Can't be written (no such directory): not applied.
        let nowhere = std::env::temp_dir()
            .join(format!("issuer-spread-missing-{}", random_uuid_for_test()))
            .join("stitch.toml");
        assert_eq!(gate_for(&fx, Some(&nowhere)).verify(&spread).await, None);
    }

    fn random_uuid_for_test() -> String {
        crate::changelog::record::random_uuid()
    }

    #[tokio::test]
    async fn a_contract_signer_answers_erc1271_on_this_chain() {
        use crate::chain::mock_node::{mock_rpc, MockChain};
        let fx = fixture();
        let safe = "0x5afe000000000000000000000000000000000001";
        let mut word = [0u8; 32];
        word[..4].copy_from_slice(&ERC1271_MAGIC);
        let yes = mock_rpc(MockChain::on(42220).view(
            safe,
            "isValidSignature(bytes32,bytes)",
            alloy_primitives::U256::from_be_bytes(word),
        ))
        .await;
        let no = mock_rpc(MockChain::on(42220)).await;
        // A Safe that signed on chain returns an empty signature.
        let spread = SignedSpread {
            signer: safe.to_string(),
            signature: "0x".to_string(),
            ..served(&fx.cases[0])
        };
        let gate = |url: &str| {
            Gate::new(
                table(
                    "app.textilecredit.com",
                    "idrx-usdt",
                    safe,
                    IssuerSignerKind::Contract,
                ),
                42220,
                Rpc::new(url),
                None,
            )
            .unwrap()
        };
        assert!(gate(&yes.url).verify(&spread).await.is_some());
        assert_eq!(gate(&no.url).verify(&spread).await, None);
    }

    #[test]
    fn a_switch_response_older_than_one_seen_is_out_of_date() {
        let fx = fixture();
        let gate = gate_for(&fx, None);
        // After a withdrawal at revision 15 was seen, a response answered at
        // 13, before it, mustn't bring the spread back.
        assert!(gate.observe("idrx-usdt", 13));
        assert!(gate.observe("IDRX-USDT", 15));
        assert!(!gate.observe("idrx-usdt", 13));
        assert!(
            gate.observe("idrx-usdt", 15),
            "the same revision again is current"
        );
        // A response naming another switch never is: the message is rebuilt
        // from this bot's switch, so its spread would verify here.
        assert!(!gate.observe("cngn-usdt", 99));
    }

    #[tokio::test]
    async fn a_withdrawal_seen_before_a_restart_still_holds_after_it() {
        let fx = fixture();
        let dir = std::env::temp_dir().join(format!("issuer-spread-{}", random_uuid_for_test()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("stitch.toml");
        let with_spread = SpreadSlot::Served(Box::new(served(&fx.cases[0])));
        let gate = gate_for(&fx, Some(&config_path));
        // Set at switch revision 13, then withdrawn at 14 (no spread served).
        assert!(matches!(
            gate.spread_for(&with_spread, Some(("idrx-usdt".into(), 13)))
                .await,
            SpreadSlot::Verified(_)
        ));
        assert_eq!(
            gate.spread_for(&SpreadSlot::None, Some(("idrx-usdt".into(), 14)))
                .await,
            SpreadSlot::None
        );
        // Restarted, a response from before the withdrawal applies nothing.
        let restarted = gate_for(&fx, Some(&config_path));
        assert_eq!(
            restarted
                .spread_for(&with_spread, Some(("idrx-usdt".into(), 13)))
                .await,
            SpreadSlot::None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn nothing_applies_while_the_state_can_t_be_written() {
        let fx = fixture();
        let dir = std::env::temp_dir().join(format!("issuer-spread-{}", random_uuid_for_test()));
        let config_path = dir.join("stitch.toml");
        let with_spread = SpreadSlot::Served(Box::new(served(&fx.cases[0])));
        // No directory to write to: the startup write fails, so nothing applies.
        let gate = gate_for(&fx, Some(&config_path));
        assert_eq!(
            gate.spread_for(&with_spread, Some(("idrx-usdt".into(), 13)))
                .await,
            SpreadSlot::None
        );
        // Once it can be written, the next response writes it and applies.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            gate.spread_for(&with_spread, Some(("idrx-usdt".into(), 13)))
                .await,
            SpreadSlot::Verified(_)
        ));
        // A withdrawal at 14 that can't be written stops the spread outright,
        // and a restart onto the same disk applies nothing either.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            gate.spread_for(&SpreadSlot::None, Some(("idrx-usdt".into(), 14)))
                .await,
            SpreadSlot::None
        );
        assert_eq!(
            gate.spread_for(&with_spread, Some(("idrx-usdt".into(), 14)))
                .await,
            SpreadSlot::None
        );
        let restarted = gate_for(&fx, Some(&config_path));
        assert_eq!(
            restarted
                .spread_for(&with_spread, Some(("idrx-usdt".into(), 13)))
                .await,
            SpreadSlot::None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_served_spread_without_its_switch_and_revision_is_never_applied() {
        let fx = fixture();
        let gate = gate_for(&fx, None);
        let with_spread = SpreadSlot::Served(Box::new(served(&fx.cases[0])));
        assert!(matches!(
            gate.spread_for(&with_spread, Some(("idrx-usdt".into(), 13)))
                .await,
            SpreadSlot::Verified(_)
        ));
        // The same signed spread, with the unsigned fields dropped or
        // renamed: the cached check doesn't let it through.
        assert_eq!(gate.spread_for(&with_spread, None).await, SpreadSlot::None);
        assert_eq!(
            gate.spread_for(&with_spread, Some(("other".into(), 99)))
                .await,
            SpreadSlot::None
        );
    }

    #[tokio::test]
    async fn a_withdrawal_during_a_slow_contract_check_wins() {
        use crate::chain::mock_node::{mock_rpc, MockChain};
        let fx = fixture();
        let safe = "0x5afe000000000000000000000000000000000001";
        let mut word = [0u8; 32];
        word[..4].copy_from_slice(&ERC1271_MAGIC);
        let node = mock_rpc(
            MockChain::on(42220)
                .view(
                    safe,
                    "isValidSignature(bytes32,bytes)",
                    alloy_primitives::U256::from_be_bytes(word),
                )
                .hung_for(Duration::from_millis(300)),
        )
        .await;
        let gate = std::sync::Arc::new(
            Gate::new(
                table(
                    "app.textilecredit.com",
                    "idrx-usdt",
                    safe,
                    IssuerSignerKind::Contract,
                ),
                42220,
                Rpc::new(&node.url),
                None,
            )
            .unwrap(),
        );
        let slot = SpreadSlot::Served(Box::new(SignedSpread {
            signer: safe.to_string(),
            signature: "0x".to_string(),
            ..served(&fx.cases[0])
        }));
        // A response at 13 starts its check; the withdrawal at 14 lands
        // while the Safe's node is slow to answer.
        let slow = {
            let gate = gate.clone();
            let slot = slot.clone();
            tokio::spawn(
                async move { gate.spread_for(&slot, Some(("idrx-usdt".into(), 13))).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            gate.spread_for(&SpreadSlot::None, Some(("idrx-usdt".into(), 14)))
                .await,
            SpreadSlot::None
        );
        assert_eq!(slow.await.unwrap(), SpreadSlot::None);
    }

    #[tokio::test]
    async fn a_contract_check_that_fails_is_retried_later_not_refused_for_good() {
        use crate::chain::mock_node::{mock_rpc, MockChain};
        use std::sync::atomic::Ordering;
        let fx = fixture();
        let safe = "0x5afe000000000000000000000000000000000001";
        // A node that doesn't know the Safe: every eth_call says no.
        let node = mock_rpc(MockChain::on(42220)).await;
        let spread = SignedSpread {
            signer: safe.to_string(),
            signature: "0x".to_string(),
            ..served(&fx.cases[0])
        };
        let gate = |retry: Duration| Gate {
            retry_unsettled: retry,
            ..Gate::new(
                table(
                    "app.textilecredit.com",
                    "idrx-usdt",
                    safe,
                    IssuerSignerKind::Contract,
                ),
                42220,
                Rpc::new(&node.url),
                None,
            )
            .unwrap()
        };
        // Within the retry window a repeat costs no call.
        let waiting = gate(RETRY_UNSETTLED);
        assert_eq!(waiting.verify(&spread).await, None);
        let after_first = node.hits.load(Ordering::SeqCst);
        assert_eq!(waiting.verify(&spread).await, None);
        assert_eq!(node.hits.load(Ordering::SeqCst), after_first);
        // Once it's up, the same spread is checked again.
        let retrying = gate(Duration::ZERO);
        assert_eq!(retrying.verify(&spread).await, None);
        let before = node.hits.load(Ordering::SeqCst);
        assert_eq!(retrying.verify(&spread).await, None);
        assert!(node.hits.load(Ordering::SeqCst) > before);
    }

    #[test]
    fn checks_the_shape_before_any_signature_work() {
        let fx = fixture();
        let base = served(&fx.cases[0]);
        assert_eq!(shape_problem(&base), None);
        assert!(shape_problem(&SignedSpread {
            buy_bps: 10_000,
            ..base.clone()
        })
        .is_some());
        assert!(shape_problem(&SignedSpread {
            nonce: "0x12".into(),
            ..base.clone()
        })
        .is_some());
        assert!(shape_problem(&SignedSpread {
            expires_at: base.issued_at + MAX_VALIDITY_MS + 1,
            ..base.clone()
        })
        .is_some());
    }

    #[test]
    fn replay_needs_a_higher_revision_or_the_same_message() {
        let a = B256::repeat_byte(0xaa);
        let b = B256::repeat_byte(0xbb);
        let last = Accepted {
            revision: 12,
            digest: hex::encode_prefixed(a).to_uppercase().replace("0X", "0x"),
        };
        assert_eq!(replay_problem(12, a, None), None);
        assert_eq!(replay_problem(12, a, Some(&last)), None);
        assert!(replay_problem(12, b, Some(&last)).is_some());
        assert!(replay_problem(11, a, Some(&last)).is_some());
        assert_eq!(replay_problem(13, b, Some(&last)), None);
    }

    #[tokio::test]
    async fn a_safe_that_signs_on_chain_can_t_have_another_spread_swapped_in() {
        use crate::chain::mock_node::{mock_rpc, MockChain};
        let fx = fixture();
        let safe = "0x5afe000000000000000000000000000000000001";
        let mut word = [0u8; 32];
        word[..4].copy_from_slice(&ERC1271_MAGIC);
        // The Safe approved both messages on chain: every check says yes,
        // and both carry the same empty signature.
        let node = mock_rpc(MockChain::on(42220).view(
            safe,
            "isValidSignature(bytes32,bytes)",
            alloy_primitives::U256::from_be_bytes(word),
        ))
        .await;
        let gate = Gate::new(
            table(
                "app.textilecredit.com",
                "idrx-usdt",
                safe,
                IssuerSignerKind::Contract,
            ),
            42220,
            Rpc::new(&node.url),
            None,
        )
        .unwrap();
        let taken = SignedSpread {
            signer: safe.to_string(),
            signature: "0x".to_string(),
            ..served(&fx.cases[0])
        };
        assert!(gate.verify(&taken).await.is_some());
        let swapped = SignedSpread {
            buy_bps: 3,
            sell_bps: 3,
            ..taken.clone()
        };
        assert_eq!(gate.verify(&swapped).await, None);
        assert!(gate.verify(&taken).await.is_some());
    }

    #[test]
    fn a_pool_takes_a_spread_only_inside_its_bounds() {
        let bounds = Some(SpreadBounds {
            min_bps: 10,
            max_bps: 50,
        });
        let spread = |buy, sell| {
            Some(IssuerSpread {
                buy_bps: buy,
                sell_bps: sell,
                revision: 3,
            })
        };
        assert_eq!(applicable(bounds, false, spread(10, 50)), spread(10, 50));
        assert_eq!(applicable(bounds, false, spread(9, 30)), None);
        assert_eq!(applicable(bounds, false, spread(30, 51)), None);
        assert_eq!(applicable(None, false, spread(30, 30)), None);
        assert_eq!(applicable(bounds, false, None), None);
        // The inventory curve quotes one even spread.
        assert_eq!(applicable(bounds, true, spread(20, 30)), None);
        assert_eq!(applicable(bounds, true, spread(25, 25)), spread(25, 25));
    }

    #[test]
    fn a_malformed_field_reads_as_none() {
        let read = |json: &str| -> SpreadSlot {
            #[derive(Deserialize)]
            struct Wrap {
                #[serde(default, deserialize_with = "lenient_slot")]
                spread: SpreadSlot,
            }
            serde_json::from_str::<Wrap>(json).unwrap().spread
        };
        assert_eq!(read("{}"), SpreadSlot::None);
        assert_eq!(read(r#"{"spread": null}"#), SpreadSlot::None);
        assert_eq!(read(r#"{"spread": {"buyBps": "wide"}}"#), SpreadSlot::None);
        assert!(matches!(
            read(
                r#"{"spread": {"buyBps": 5, "sellBps": 5, "revision": 1, "nonce": "0x", "nonceSecret": "0x",
                    "issuedAt": 1, "expiresAt": 2, "signer": "0x", "signature": "0x"}}"#
            ),
            SpreadSlot::Served(_)
        ));
    }
}
