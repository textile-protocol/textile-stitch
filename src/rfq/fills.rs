// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Releasing a filled quote's claim once the chain shows the fill.
//!
//! A signed quote claims its inventory until its deadline plus
//! [`RELEASE_SKEW_SECS`](super::reserve::RELEASE_SKEW_SECS): a `selected`
//! result says a taker holds the signature, not that they used it. Once they
//! do, the balances show the fill while the claim still counts it, so the next
//! quote on that side is capped, and on a Live inventory curve priced, as if
//! the trade were still to come. That lasts up to the quote TTL plus 30 s,
//! right after the large fills that carry most of the volume.
//!
//! Every RFQ order spends one Permit2 nonce under its swapper (the wallet, or
//! the vault) when it fills, and nothing else spends it: RFQ nonces have their
//! own namespace ([`super::nonce`]) and a vault's carry its trading epoch. So
//! the balance loop reads `nonceBitmap` for every exposed claim in the same
//! Multicall3 batch as the balances, with the batch's block number, and hands
//! the spent nonces to the responder, which releases those claims.
//!
//! A release is only safe against balances that already show the fill, and
//! two rules make every balance read after a release one that does:
//!
//! - The nonces, the balances and the block number come from one batch, so
//!   one block. The loop publishes the balances (the wallet's, or the vault's
//!   free balances) before the spent nonces, and the responder takes the
//!   spent nonces before it reads any balance.
//! - The loop never publishes a batch from an older block than one it already
//!   published ([`BlockClock`]). A load-balanced RPC can answer from a node a
//!   block or two behind; without this, a later read could hand back
//!   pre-fill balances after the claim was gone, and the next same-side quote
//!   would be priced as if the fill had never happened.
//!
//! Without Multicall3 the reads can't share a block, so nothing is released
//! early and claims hold to their deadline.

use std::sync::{Arc, Mutex};

use alloy_primitives::{keccak256, Address, Bytes, U256};

use crate::chain::multicall::{decode_uint, Batcher, Call};

/// The most distinct bitmap words one refresh reads. Claims past it hold to
/// their deadline, as every claim did before fills were read.
pub const MAX_WATCHED_WORDS: usize = 64;

/// The exposed claims the balance loop watches and the fills it has seen.
/// Shared between the loop, which lives as long as the process, and the
/// responder, which is rebuilt every session.
#[derive(Clone, Default)]
pub struct FillWatch(Arc<Mutex<Watch>>);

#[derive(Default)]
struct Watch {
    /// The exposed claims' nonces, as the responder last published them.
    watched: Vec<U256>,
    /// Watched nonces a published reading showed spent, not yet released.
    spent: Vec<U256>,
}

impl FillWatch {
    /// Replace the nonces the next refresh reads.
    pub fn watch(&self, nonces: Vec<U256>) {
        if let Ok(mut w) = self.0.lock() {
            w.watched = nonces;
        }
    }

    /// The nonces to read this refresh.
    pub fn watched(&self) -> Vec<U256> {
        self.0.lock().map(|w| w.watched.clone()).unwrap_or_default()
    }

    /// Hand over nonces a reading showed spent. Called only after that
    /// reading's balances are published.
    pub fn saw_spent(&self, nonces: Vec<U256>) {
        if nonces.is_empty() {
            return;
        }
        if let Ok(mut w) = self.0.lock() {
            for nonce in nonces {
                if !w.spent.contains(&nonce) {
                    w.spent.push(nonce);
                }
            }
        }
    }

    /// Take every spent nonce handed over since the last call.
    pub fn take_spent(&self) -> Vec<U256> {
        self.0
            .lock()
            .map(|mut w| std::mem::take(&mut w.spent))
            .unwrap_or_default()
    }
}

/// The fill reads that ride one balance batch: Multicall3's block number,
/// then one Permit2 `nonceBitmap(owner, word)` per distinct word the watched
/// nonces sit in. Empty when the batch can't carry them.
pub struct FillReads {
    watched: Vec<U256>,
    words: Vec<U256>,
    calls: Vec<Call>,
}

/// What one batch's fill reads said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillReading {
    /// The block the whole batch ran at, when the node answered.
    pub block: Option<u64>,
    /// The watched nonces that block shows spent.
    pub spent: Vec<U256>,
}

impl FillReads {
    /// The reads for `watched` nonces under `owner`. A sequential reader gets
    /// none: separate calls can land on separate blocks.
    pub fn new(reader: Batcher, permit2: Address, owner: Address, watched: Vec<U256>) -> Self {
        let Some(multicall3) = reader.multicall3() else {
            return Self {
                watched: Vec::new(),
                words: Vec::new(),
                calls: Vec::new(),
            };
        };
        let words = distinct_words(&watched);
        let calls = std::iter::once(Call::new(multicall3, encode_get_block_number()))
            .chain(
                words
                    .iter()
                    .map(|word| Call::new(permit2, encode_nonce_bitmap(owner, *word))),
            )
            .collect();
        Self {
            watched,
            words,
            calls,
        }
    }

    pub fn calls(&self) -> &[Call] {
        &self.calls
    }

    /// Read this batch's answers to [`Self::calls`], in order.
    pub fn decode(&self, results: &[Option<Bytes>]) -> FillReading {
        if self.calls.is_empty() {
            return FillReading {
                block: None,
                spent: Vec::new(),
            };
        }
        let block = results.first().and_then(|r| decode_block(r.as_ref()?));
        let bitmaps = results.get(1..).unwrap_or_default();
        FillReading {
            block,
            // A batch without its block can't be ordered against the
            // balances it rode with, so it releases nothing.
            spent: match block {
                Some(_) => spent_nonces(&self.watched, &self.words, bitmaps),
                None => Vec::new(),
            },
        }
    }
}

/// The distinct bitmap words `nonces` sit in, first seen first, at most
/// [`MAX_WATCHED_WORDS`].
fn distinct_words(nonces: &[U256]) -> Vec<U256> {
    nonces.iter().fold(Vec::new(), |mut words, nonce| {
        let word = *nonce >> 8;
        if words.len() < MAX_WATCHED_WORDS && !words.contains(&word) {
            words.push(word);
        }
        words
    })
}

/// The nonces whose bit is set in the bitmap read for their word. A word that
/// wasn't read or didn't come back spends nothing.
fn spent_nonces(nonces: &[U256], words: &[U256], bitmaps: &[Option<Bytes>]) -> Vec<U256> {
    nonces
        .iter()
        .filter(|nonce| {
            let word = **nonce >> 8;
            let bit = (**nonce & U256::from(0xffu8)).to::<usize>();
            words
                .iter()
                .position(|w| *w == word)
                .and_then(|i| bitmaps.get(i)?.as_ref())
                .filter(|raw| raw.len() >= 32)
                .is_some_and(|raw| decode_uint(raw).bit(bit))
        })
        .copied()
        .collect()
}

/// A block number from Multicall3's `getBlockNumber()`. No data, or zero, is
/// no answer.
fn decode_block(raw: &Bytes) -> Option<u64> {
    (raw.len() >= 32)
        .then(|| decode_uint(raw))
        .filter(|block| !block.is_zero())
        .and_then(|block| u64::try_from(block).ok())
}

/// Multicall3 `getBlockNumber()`: the block the whole batch runs at.
fn encode_get_block_number() -> Vec<u8> {
    keccak256(b"getBlockNumber()")[..4].to_vec()
}

/// Permit2 `nonceBitmap(address owner, uint256 wordPos)`.
fn encode_nonce_bitmap(owner: Address, word: U256) -> Vec<u8> {
    let mut data = keccak256(b"nonceBitmap(address,uint256)")[..4].to_vec();
    data.extend_from_slice(&owner.into_word().0);
    data.extend_from_slice(&word.to_be_bytes::<32>());
    data
}

/// The newest block the balance loop has published. Readings only move
/// forward: one from an older block than a published one is dropped.
#[derive(Debug, Default)]
pub struct BlockClock(Option<u64>);

impl BlockClock {
    /// Whether a batch from `block` may be published, recording it if so.
    ///
    /// Until a batch has carried a block (a sequential reader never does),
    /// everything is published, as before fills were read. After that a batch
    /// without one is dropped too, since it can't be ordered.
    pub fn admits(&mut self, block: Option<u64>) -> bool {
        match (self.0, block) {
            (None, None) => true,
            (Some(_), None) => false,
            (Some(last), Some(block)) if block < last => false,
            (_, Some(block)) => {
                self.0 = Some(block);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
    const OWNER: Address = address!("70997970c51812dc3a010c7d01b50e0d17dc79c8");

    fn word(value: U256) -> Option<Bytes> {
        Some(Bytes::from(value.to_be_bytes::<32>().to_vec()))
    }

    fn batched() -> Batcher {
        Batcher::at(crate::chain::multicall::CANONICAL_MULTICALL3)
    }

    #[test]
    fn one_bitmap_read_per_word_after_the_block_number() {
        // Two nonces in word 1, one in word 2.
        let nonces = vec![
            U256::from(256 + 3),
            U256::from(256 + 9),
            U256::from(512 + 1),
        ];
        let reads = FillReads::new(batched(), PERMIT2, OWNER, nonces);
        let calls = reads.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[0].target,
            crate::chain::multicall::CANONICAL_MULTICALL3
        );
        assert_eq!(calls[0].data, keccak256(b"getBlockNumber()")[..4].to_vec());
        assert!(calls[1..].iter().all(|c| c.target == PERMIT2));
        assert_eq!(
            &calls[1].data[68 - 32..],
            &U256::from(1).to_be_bytes::<32>()
        );
        assert_eq!(
            &calls[2].data[68 - 32..],
            &U256::from(2).to_be_bytes::<32>()
        );
    }

    #[test]
    fn a_set_bit_is_a_spent_nonce_and_nothing_else_is() {
        let filled = U256::from(256 + 3);
        let open = U256::from(256 + 9);
        let unread = U256::from(512 + 1);
        let reads = FillReads::new(batched(), PERMIT2, OWNER, vec![filled, open, unread]);
        let reading = reads.decode(&[word(U256::from(1_234u64)), word(U256::from(1u8) << 3), None]);
        assert_eq!(
            reading,
            FillReading {
                block: Some(1_234),
                spent: vec![filled],
            }
        );
    }

    #[test]
    fn a_batch_without_its_block_releases_nothing() {
        let filled = U256::from(7u8);
        let reads = FillReads::new(batched(), PERMIT2, OWNER, vec![filled]);
        for block in [None, Some(Bytes::new()), word(U256::ZERO)] {
            let reading = reads.decode(&[block, word(U256::from(1u8) << 7)]);
            assert_eq!(reading.block, None);
            assert!(reading.spent.is_empty());
        }
    }

    #[test]
    fn a_sequential_reader_reads_no_fills() {
        let reads = FillReads::new(Batcher::sequential(), PERMIT2, OWNER, vec![U256::from(7u8)]);
        assert!(reads.calls().is_empty());
        assert_eq!(reads.decode(&[]).spent, Vec::<U256>::new());
    }

    #[test]
    fn rfq_and_vault_nonces_land_in_their_own_words() {
        // The real nonce shapes: a millisecond counter under bit 200, and a
        // vault nonce carrying its epoch. Each quote gets its own word.
        let a = super::super::nonce::rfq_nonce(1_754_388_000_000, 0);
        let b = super::super::nonce::rfq_nonce(1_754_388_000_001, 0);
        let reads = FillReads::new(batched(), PERMIT2, OWNER, vec![a, b]);
        assert_eq!(reads.calls().len(), 3);
        let bit = (a & U256::from(0xffu8)).to::<usize>();
        let reading = reads.decode(&[
            word(U256::from(9u8)),
            word(U256::from(1u8) << bit),
            word(U256::ZERO),
        ]);
        assert_eq!(reading.spent, vec![a]);
    }

    #[test]
    fn the_watch_caps_the_words_it_reads() {
        let nonces: Vec<U256> = (0..100u64).map(|i| U256::from(i) << 8).collect();
        let reads = FillReads::new(batched(), PERMIT2, OWNER, nonces);
        assert_eq!(reads.calls().len(), 1 + MAX_WATCHED_WORDS);
    }

    #[test]
    fn readings_only_move_forward() {
        let mut clock = BlockClock::default();
        assert!(
            clock.admits(None),
            "a sequential reader publishes as before"
        );
        assert!(clock.admits(Some(100)));
        assert!(clock.admits(Some(100)), "the same block again is fine");
        assert!(!clock.admits(Some(99)), "a node a block behind is dropped");
        assert!(
            !clock.admits(None),
            "a batch that can't be ordered is dropped"
        );
        assert!(clock.admits(Some(101)));
    }

    #[test]
    fn spent_nonces_are_handed_over_once() {
        let watch = FillWatch::default();
        watch.watch(vec![U256::from(1u8), U256::from(2u8)]);
        assert_eq!(watch.watched().len(), 2);
        watch.saw_spent(vec![U256::from(1u8)]);
        watch.saw_spent(vec![U256::from(1u8)]);
        assert_eq!(watch.take_spent(), vec![U256::from(1u8)]);
        assert!(watch.take_spent().is_empty());
    }
}
