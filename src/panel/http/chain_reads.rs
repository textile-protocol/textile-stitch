// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The panel's chain reads: every question one screen asks, in one request.
//!
//! The funding screen polls every few seconds per bot, and used to send each
//! balance, allowance and vault view as its own `eth_call` — five to ten
//! requests a poll, on the operator's RPC. Through Multicall3's `aggregate3`
//! that is one request, and every answer comes from the same block.
//!
//! Each read still fails on its own. `allowFailure` turns a reverting call into
//! that one row's error, not the batch's. When the batch itself fails for any
//! reason other than the time budget (no Multicall3 on this chain, a node that
//! rejects it), the reads go out one by one, concurrently, each under its own
//! budget, which is what the panel did before. A batch that runs out of time
//! does not retry: the node is not answering, and a second round would double
//! how long the screen waits to say so.
//!
//! The native balance rides the batch as Multicall3's `getEthBalance`; one by
//! one it is a plain `eth_getBalance`.
//!
//! Some answers never change for a contract: a token's `symbol()`, a vault's
//! two assets. A read marked [`Ask::constant`] is asked once per chain and
//! kept in [`ChainConstants`] for the life of the panel.
//!
//! And a running bot already reads most of the rest every couple of seconds.
//! [`read_cached`] takes the bot's snapshot ([`crate::chain::snapshot`]) and
//! answers from it where the bot made exactly that read, so only what the bot
//! didn't ask goes to the chain.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, U256};

use crate::chain::multicall::{encode_get_eth_balance, Batcher, Call, CANONICAL_MULTICALL3};
use crate::chain::rpc::Rpc;
use crate::chain::snapshot::ChainReads;

/// Each chain read gets this long. The shared RPC client allows fifteen
/// seconds per request, which is fine for a bot and far too long for a
/// screen polling every five.
pub(super) const CHAIN_BUDGET: Duration = Duration::from_secs(6);

/// What one read asks the chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Read {
    /// `eth_call` of `data` at `to`, against the latest block.
    Call { to: Address, data: Vec<u8> },
    /// The native (gas coin) balance of an address.
    Native(Address),
}

/// One read and the words a failure should name it by, e.g.
/// `balanceOf() on 0x…`.
#[derive(Debug, Clone)]
pub(super) struct Ask {
    pub read: Read,
    pub what: String,
    /// The answer can't change, so a cached one stands in for the read.
    pub constant: bool,
}

impl Ask {
    pub fn call(to: Address, data: Vec<u8>, what: impl Into<String>) -> Self {
        Self {
            read: Read::Call { to, data },
            what: what.into(),
            constant: false,
        }
    }

    /// Mark a read whose answer is fixed when the contract is deployed.
    pub fn constant(self) -> Self {
        Self {
            constant: true,
            ..self
        }
    }

    pub fn native(owner: Address) -> Self {
        Self {
            read: Read::Native(owner),
            what: format!("the gas balance of {owner}"),
            constant: false,
        }
    }
}

/// Answers to [`Ask::constant`] reads, by chain. Only non-empty successes go
/// in: an empty or failed answer is a contract that isn't there yet, or a node
/// that blipped, and either may read differently next time.
#[derive(Default)]
pub struct ChainConstants(Mutex<HashMap<(u64, Read), Bytes>>);

impl ChainConstants {
    pub(super) fn get(&self, chain_id: u64, read: &Read) -> Option<Bytes> {
        let map = self.0.lock().ok()?;
        map.get(&(chain_id, read.clone())).cloned()
    }

    fn remember(&self, chain_id: u64, read: &Read, out: &Bytes) {
        if out.is_empty() {
            return;
        }
        if let Ok(mut map) = self.0.lock() {
            map.insert((chain_id, read.clone()), out.clone());
        }
    }
}

/// [`read_all`], answering first from `constants` (for constant reads), then
/// from the bot's `snapshot`, and only then from the chain. Constant reads
/// that reach the chain are remembered. The caller vouches for the snapshot
/// being this chain's and fresh enough.
pub(super) async fn read_cached(
    rpc_url: &str,
    chain_id: u64,
    constants: &ChainConstants,
    snapshot: Option<&ChainReads>,
    asks: &[Ask],
) -> Vec<Answer> {
    let cached: Vec<Option<Bytes>> = asks
        .iter()
        .map(|ask| {
            ask.constant
                .then(|| constants.get(chain_id, &ask.read))
                .flatten()
                .or_else(|| snapshot.and_then(|s| recorded(s, &ask.read)))
        })
        .collect();
    let misses: Vec<Ask> = asks
        .iter()
        .zip(&cached)
        .filter(|(_, hit)| hit.is_none())
        .map(|(ask, _)| ask.clone())
        .collect();
    let mut fresh = read_all(rpc_url, &misses).await.into_iter();
    asks.iter()
        .zip(cached)
        .map(|(ask, hit)| match hit {
            Some(out) => Ok(out),
            None => {
                let answer = fresh
                    .next()
                    .unwrap_or_else(|| Err(format!("{} got no answer", ask.what)));
                if let (true, Ok(out)) = (ask.constant, &answer) {
                    constants.remember(chain_id, &ask.read, out);
                }
                answer
            }
        })
        .collect()
}

/// The bot's answer to this read, if it made exactly this one.
fn recorded(snapshot: &ChainReads, read: &Read) -> Option<Bytes> {
    match read {
        Read::Call { to, data } => snapshot.lookup(*to, data),
        Read::Native(owner) => {
            snapshot.lookup(CANONICAL_MULTICALL3, &encode_get_eth_balance(*owner))
        }
    }
}

/// The raw return of one read, or why there isn't one. A string rather than an
/// error so a whole batch's failure can be handed to every row.
pub(super) type Answer = Result<Bytes, String>;

/// The `aggregate3` entry for one read.
fn batched_call(read: &Read) -> Call {
    match read {
        Read::Call { to, data } => Call::new(*to, data.clone()),
        Read::Native(owner) => Call::new(CANONICAL_MULTICALL3, encode_get_eth_balance(*owner)),
    }
}

/// The message every read gets when the node ran out of time.
pub(super) fn timed_out(rpc_url: &str) -> String {
    format!(
        "the RPC at {rpc_url} didn't answer within {} seconds",
        CHAIN_BUDGET.as_secs()
    )
}

/// Every read, in order, one answer each. One request where the chain has
/// Multicall3; one request per read where it doesn't.
pub(super) async fn read_all(rpc_url: &str, asks: &[Ask]) -> Vec<Answer> {
    let rpc = Rpc::new(rpc_url.to_string());
    match asks {
        [] => Vec::new(),
        // Nothing to batch with: wrapping one call in `aggregate3` saves
        // nothing and costs the fallback round trip on a chain without it.
        [ask] => vec![read_one(&rpc, rpc_url, ask).await],
        _ => {
            let calls: Vec<Call> = asks.iter().map(|a| batched_call(&a.read)).collect();
            let batch = Batcher::at(CANONICAL_MULTICALL3);
            match tokio::time::timeout(CHAIN_BUDGET, batch.read(&rpc, &calls)).await {
                Ok(Ok(results)) => answers_of(asks, results),
                Ok(Err(e)) => {
                    tracing::debug!(error = %format!("{e:#}"), "batched read failed; reading one by one");
                    read_each(&rpc, rpc_url, asks).await
                }
                Err(_) => asks.iter().map(|_| Err(timed_out(rpc_url))).collect(),
            }
        }
    }
}

/// A batch's slots as answers: a reverted slot is that read's error.
fn answers_of(asks: &[Ask], results: Vec<Option<Bytes>>) -> Vec<Answer> {
    asks.iter()
        .zip(results)
        .map(|(ask, out)| out.ok_or_else(|| format!("{} reverted", ask.what)))
        .collect()
}

async fn read_each(rpc: &Rpc, rpc_url: &str, asks: &[Ask]) -> Vec<Answer> {
    futures_util::future::join_all(asks.iter().map(|ask| read_one(rpc, rpc_url, ask))).await
}

/// One read on its own, under the budget.
async fn read_one(rpc: &Rpc, rpc_url: &str, ask: &Ask) -> Answer {
    let read = async {
        match &ask.read {
            Read::Call { to, data } => rpc.eth_call(*to, &Bytes::from(data.clone())).await,
            Read::Native(owner) => rpc
                .get_balance(*owner)
                .await
                .map(|wei| Bytes::from(wei.to_be_bytes::<32>().to_vec())),
        }
    };
    match tokio::time::timeout(CHAIN_BUDGET, read).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err(timed_out(rpc_url)),
    }
}

/// A one-word return (a view's `uint`, `address` or `bool`).
pub(super) fn word(answer: &Answer, what: &str) -> anyhow::Result<U256> {
    let out = answer.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        out.len() >= 32,
        "{what} returned {} bytes, not a word",
        out.len()
    );
    Ok(U256::from_be_slice(&out[out.len() - 32..]))
}

/// An ERC-20 `uint256` return. Same as [`word`], with the likelier cause of a
/// short answer spelled out.
pub(super) fn token_uint(answer: &Answer, what: &str) -> anyhow::Result<U256> {
    let out = answer.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        out.len() >= 32,
        "{what} returned {} bytes, not a uint256 — is that address an ERC-20 on this chain?",
        out.len()
    );
    Ok(U256::from_be_slice(&out[out.len() - 32..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::mock_node::{mock_rpc, MockChain};
    use crate::closer::executor::encode_balance_of;
    use alloy_primitives::address;
    use std::sync::atomic::Ordering;

    const TOKEN: &str = "0x0000000000000000000000000000000000000aaa";
    const OWNER: Address = address!("0000000000000000000000000000000000000bbb");

    fn asks() -> Vec<Ask> {
        vec![
            Ask::call(
                TOKEN.parse().unwrap(),
                encode_balance_of(OWNER),
                "balanceOf()",
            ),
            Ask::native(OWNER),
            // A view the token doesn't have: no return data.
            Ask::call(TOKEN.parse().unwrap(), vec![1, 2, 3, 4], "missing()"),
        ]
    }

    fn values(answers: &[Answer]) -> (U256, U256, bool) {
        (
            token_uint(&answers[0], "balanceOf()").unwrap(),
            word(&answers[1], "gas").unwrap(),
            word(&answers[2], "missing()").is_err(),
        )
    }

    #[test]
    fn get_eth_balance_selector_matches_cast_sig() {
        // cast sig "getEthBalance(address)"
        assert_eq!(encode_get_eth_balance(OWNER)[..4], [0x4d, 0x23, 0x01, 0xcc]);
    }

    #[tokio::test]
    async fn with_multicall3_every_read_is_one_request() {
        let node = mock_rpc(
            MockChain::default()
                .balance(TOKEN, 25)
                .native(7)
                .with_multicall3(),
        )
        .await;
        let answers = read_all(&node.url, &asks()).await;
        assert_eq!(node.hits.load(Ordering::SeqCst), 1);
        assert_eq!(values(&answers), (U256::from(25), U256::from(7), true));
    }

    #[tokio::test]
    async fn without_multicall3_the_same_reads_go_one_by_one() {
        let node = mock_rpc(MockChain::default().balance(TOKEN, 25).native(7)).await;
        let answers = read_all(&node.url, &asks()).await;
        // The failed batch, then one request per read.
        assert_eq!(node.hits.load(Ordering::SeqCst), 4);
        assert_eq!(values(&answers), (U256::from(25), U256::from(7), true));
    }

    #[tokio::test]
    async fn a_single_read_skips_the_batch() {
        let node = mock_rpc(MockChain::default().native(7).with_multicall3()).await;
        let answers = read_all(&node.url, &[Ask::native(OWNER)]).await;
        assert_eq!(node.hits.load(Ordering::SeqCst), 1);
        assert_eq!(word(&answers[0], "gas").unwrap(), U256::from(7));
    }

    #[tokio::test]
    async fn a_constant_is_read_once_and_a_balance_every_time() {
        let node = mock_rpc(
            MockChain::default()
                .balance(TOKEN, 25)
                .symbol(TOKEN, "cNGN")
                .with_multicall3(),
        )
        .await;
        let constants = ChainConstants::default();
        let token: Address = TOKEN.parse().unwrap();
        let asks = [
            Ask::call(token, encode_balance_of(OWNER), "balanceOf()"),
            Ask::call(token, vec![0x95, 0xd8, 0x9b, 0x41], "symbol()").constant(),
        ];
        let first = read_cached(&node.url, 1, &constants, None, &asks).await;
        let second = read_cached(&node.url, 1, &constants, None, &asks).await;
        assert_eq!(first, second);
        // Both polls read the balance; only the first asked for the symbol, so
        // the second was a single call rather than a batch.
        assert_eq!(node.hits.load(Ordering::SeqCst), 2);
        assert!(constants
            .get(1, &asks[1].read)
            .is_some_and(|out| !out.is_empty()));
        // Another chain's constant is another chain's.
        assert!(constants.get(2, &asks[1].read).is_none());
    }

    #[tokio::test]
    async fn an_empty_constant_is_not_kept() {
        // A token with no symbol() answers no data: maybe not deployed yet.
        let node = mock_rpc(MockChain::default().with_multicall3()).await;
        let constants = ChainConstants::default();
        let ask = Ask::call(
            TOKEN.parse().unwrap(),
            vec![0x95, 0xd8, 0x9b, 0x41],
            "symbol()",
        )
        .constant();
        read_cached(&node.url, 1, &constants, None, std::slice::from_ref(&ask)).await;
        assert!(constants.get(1, &ask.read).is_none());
    }

    #[tokio::test]
    async fn the_bots_reads_answer_and_only_the_rest_go_to_the_chain() {
        use crate::chain::snapshot::record;
        let node = mock_rpc(MockChain::default().balance(TOKEN, 1).native(1)).await;
        let token: Address = TOKEN.parse().unwrap();
        // The bot read the balance and the gas; not the missing view.
        let calls = [
            Call::new(token, encode_balance_of(OWNER)),
            Call::new(CANONICAL_MULTICALL3, encode_get_eth_balance(OWNER)),
        ];
        let words = [U256::from(25), U256::from(7)]
            .map(|w| Some(Bytes::from(w.to_be_bytes::<32>().to_vec())));
        let snapshot = ChainReads::new(1, 100, record(&calls, &words));
        let answers = read_cached(
            &node.url,
            1,
            &ChainConstants::default(),
            Some(&snapshot),
            &asks(),
        )
        .await;
        // The bot's numbers, not the node's; one request, for the missing view.
        assert_eq!(values(&answers), (U256::from(25), U256::from(7), true));
        assert_eq!(node.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_reverted_slot_names_its_read() {
        let answers = answers_of(&asks()[..2], vec![None, Some(Bytes::new())]);
        assert_eq!(answers[0], Err("balanceOf() reverted".to_string()));
        assert!(answers[1].is_ok());
    }
}
