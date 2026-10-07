// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The bot's latest chain reads, on disk for the panel.
//!
//! A running RFQ bot reads its wallet (or vault) every couple of seconds to
//! size its quotes. The panel's funding screen asked the chain the same
//! questions again every five, so an open panel tab cost the operator's RPC
//! more than the bot did. Instead the bot writes what it read to
//! [`FILE`] next to `stitch.toml`, and the panel answers from there while the
//! file is fresh.
//!
//! What goes in is exactly what came back: the target, the calldata and the
//! raw return of each read that succeeded. The panel looks a read up by the
//! same two keys, so it can never take one answer for another question, and a
//! read the bot didn't make, or that failed, isn't there and goes to the chain.
//! The native balance is recorded as Multicall3's `getEthBalance(owner)`,
//! which is how the bot reads it.
//!
//! Writes go through a coalescing task off the refresh loop: a slow disk may
//! skip snapshots, but never delays a quote-inventory refresh.

use std::path::{Path, PathBuf};

use alloy_primitives::{hex, Address, Bytes};
use serde::{Deserialize, Serialize};

use crate::chain::multicall::Call;

/// `chain-reads.json`, beside `stitch.toml`.
pub const FILE: &str = "chain-reads.json";

const VERSION: u32 = 1;

/// The panel reads this file on a poll; anything bigger is not ours.
const MAX_BYTES: u64 = 256 * 1024;

/// One read and its raw return, all `0x` hex, lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedRead {
    pub to: String,
    pub data: String,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainReads {
    pub version: u32,
    pub chain_id: u64,
    /// Unix seconds the reads came back.
    pub at: u64,
    pub reads: Vec<RecordedRead>,
}

fn key(to: Address, data: &[u8]) -> (String, String) {
    (format!("{to:#x}"), hex::encode_prefixed(data))
}

/// Each call that answered, with its answer. Calls and results pair up in
/// order; a failed or missing result records nothing.
pub fn record(calls: &[Call], results: &[Option<Bytes>]) -> Vec<RecordedRead> {
    calls
        .iter()
        .zip(results)
        .filter_map(|(call, result)| {
            let result = result.as_ref()?;
            let (to, data) = key(call.target, &call.data);
            Some(RecordedRead {
                to,
                data,
                result: hex::encode_prefixed(result),
            })
        })
        .collect()
}

impl ChainReads {
    pub fn new(chain_id: u64, at: u64, reads: Vec<RecordedRead>) -> Self {
        Self {
            version: VERSION,
            chain_id,
            at,
            reads,
        }
    }

    /// Whether these reads may stand in for the chain's answer now: same
    /// chain, and no older than `max_age_secs`. A timestamp from the future is
    /// a clock that can't be trusted either way.
    pub fn is_fresh(&self, chain_id: u64, now: u64, max_age_secs: u64) -> bool {
        self.version == VERSION
            && self.chain_id == chain_id
            && self.at <= now
            && now - self.at <= max_age_secs
    }

    /// The recorded return of `data` at `to`, if the bot made that read.
    pub fn lookup(&self, to: Address, data: &[u8]) -> Option<Bytes> {
        let (to, data) = key(to, data);
        self.reads
            .iter()
            .find(|r| r.to == to && r.data == data)
            .and_then(|r| hex::decode(&r.result).ok())
            .map(Bytes::from)
    }
}

/// The snapshot in `dir`, if there is a readable one. Any problem reads as
/// none: the caller asks the chain instead.
pub fn load(dir: &Path) -> Option<ChainReads> {
    let path = dir.join(FILE);
    if std::fs::metadata(&path).ok()?.len() > MAX_BYTES {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Where the bot publishes. Cheap to clone; every clone feeds the same writer.
#[derive(Clone)]
pub struct Publisher {
    chain_id: u64,
    sender: tokio::sync::watch::Sender<Option<ChainReads>>,
}

impl Publisher {
    /// Start the writer for `dir`. Must run inside a Tokio runtime.
    pub fn spawn(dir: &Path, chain_id: u64) -> Self {
        let (sender, mut receiver) = tokio::sync::watch::channel(None::<ChainReads>);
        let path: PathBuf = dir.join(FILE);
        tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let Some(snapshot) = receiver.borrow_and_update().clone() else {
                    continue;
                };
                let path = path.clone();
                let written = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    crate::setup::write_file_atomic(&path, serde_json::to_vec(&snapshot)?)
                })
                .await;
                if !matches!(written, Ok(Ok(()))) {
                    tracing::debug!("could not write the chain-read snapshot for the panel");
                }
            }
        });
        Self { chain_id, sender }
    }

    /// Replace the snapshot with these reads, taken at `at`.
    pub fn publish(&self, at: u64, reads: Vec<RecordedRead>) {
        let _ = self
            .sender
            .send(Some(ChainReads::new(self.chain_id, at, reads)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const TOKEN: Address = address!("00000000000000000000000000000000000000aA");

    fn reads() -> ChainReads {
        let calls = [
            Call::new(TOKEN, vec![0x70, 0xa0, 0x82, 0x31, 1]),
            Call::new(TOKEN, vec![0xdd, 0x62, 0xed, 0x3e, 2]),
        ];
        let results = [Some(Bytes::from(vec![7u8; 32])), None];
        ChainReads::new(42220, 100, record(&calls, &results))
    }

    #[test]
    fn only_answered_reads_are_recorded_and_found_by_target_and_calldata() {
        let r = reads();
        assert_eq!(r.reads.len(), 1, "the failed read is not recorded");
        assert_eq!(
            r.lookup(TOKEN, &[0x70, 0xa0, 0x82, 0x31, 1]),
            Some(Bytes::from(vec![7u8; 32]))
        );
        // Same target, other calldata: a different question.
        assert_eq!(r.lookup(TOKEN, &[0x70, 0xa0, 0x82, 0x31, 9]), None);
        assert_eq!(r.lookup(TOKEN, &[0xdd, 0x62, 0xed, 0x3e, 2]), None);
    }

    #[test]
    fn freshness_needs_the_same_chain_a_recent_stamp_and_no_future() {
        let r = reads();
        assert!(r.is_fresh(42220, 105, 10));
        assert!(r.is_fresh(42220, 110, 10));
        assert!(!r.is_fresh(42220, 111, 10), "too old");
        assert!(!r.is_fresh(56, 105, 10), "another chain");
        assert!(!r.is_fresh(42220, 99, 10), "stamped in the future");
    }

    #[tokio::test]
    async fn a_published_snapshot_loads_back() {
        let dir =
            std::env::temp_dir().join(format!("stitch-chain-reads-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let publisher = Publisher::spawn(&dir, 42220);
        publisher.publish(100, reads().reads);
        let loaded = async {
            loop {
                if let Some(r) = load(&dir) {
                    return r;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        let loaded = tokio::time::timeout(std::time::Duration::from_secs(5), loaded)
            .await
            .expect("snapshot written");
        assert_eq!(loaded, reads());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_or_garbled_file_is_no_snapshot() {
        let dir =
            std::env::temp_dir().join(format!("stitch-chain-reads-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).is_none());
        std::fs::write(dir.join(FILE), "not json").unwrap();
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
