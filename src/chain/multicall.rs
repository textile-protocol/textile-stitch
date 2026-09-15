// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Batched contract reads through Multicall3.
//!
//! Every read the bot makes used to be its own `eth_call`. That is fine once,
//! and expensive on a loop: the RFQ inventory refresh reads a balance and a
//! Permit2 allowance for every quotable token, and a vault maker reads ten
//! views, all of it on a timer that never stops. A bot seated on five
//! corridors was spending twelve round trips a second on nothing but "how much
//! can I quote". Packed into one `aggregate3` that is one round trip, and the
//! whole batch comes from a single block instead of drifting across ten.
//!
//! Multicall3 sits at [`CANONICAL_MULTICALL3`] on every chain Stitch runs on,
//! but an operator can point the bot at anything — a fresh L2, a local
//! Hardhat — so [`Batcher::detect`] asks the node whether the contract is
//! actually there and falls back to reading one call at a time when it is not.
//! Both paths return the same shape, so callers never branch.
//!
//! Sub-call failures are not batch failures: `allowFailure` is on and a
//! reverting view comes back as [`None`], so one bad token cannot take a
//! bot's whole inventory refresh dark — the same isolation the sequential
//! path gets from reading each call separately.

use alloy_primitives::{address, keccak256, Address, Bytes, U256};
use alloy_sol_types::{sol, SolValue};
use anyhow::{anyhow, Context};

use crate::chain::rpc::Rpc;

/// Multicall3's deterministic deployment address, the same on Celo, BNB Smart
/// Chain, Base, Ethereum and every other chain we quote on.
pub const CANONICAL_MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

// `sol!` generates these as public items. That is deliberate: the test node
// in `chain::mock_node` decodes a batch with the same types it is encoded
// with, so the two cannot drift.
sol! {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }

    struct Result3 {
        bool success;
        bytes returnData;
    }
}

fn selector(signature: &str) -> [u8; 4] {
    let h = keccak256(signature.as_bytes()).0;
    [h[0], h[1], h[2], h[3]]
}

/// One `eth_call`: where it goes and what it says.
#[derive(Debug, Clone)]
pub struct Call {
    pub target: Address,
    pub data: Vec<u8>,
}

impl Call {
    pub fn new(target: Address, data: Vec<u8>) -> Self {
        Self { target, data }
    }
}

/// A uint256 return, read exactly as `Wallet::read_uint` reads it: no data is
/// zero (a view that isn't there), otherwise the last 32 bytes big-endian.
/// Batched and sequential reads must not disagree about what a word means.
pub fn decode_uint(out: &Bytes) -> U256 {
    if out.is_empty() {
        return U256::ZERO;
    }
    let start = out.len().saturating_sub(32);
    U256::from_be_slice(&out[start..])
}

/// How this chain answers a set of reads. Cheap to copy; resolve it once with
/// [`Batcher::detect`] and keep it for the life of the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Batcher {
    multicall3: Option<Address>,
}

impl Batcher {
    /// One `eth_call` per read. What a chain without Multicall3 gets.
    pub fn sequential() -> Self {
        Self { multicall3: None }
    }

    pub fn at(multicall3: Address) -> Self {
        Self {
            multicall3: Some(multicall3),
        }
    }

    /// True when reads go out as one batched call.
    pub fn is_batched(&self) -> bool {
        self.multicall3.is_some()
    }

    /// Ask the node whether Multicall3 is deployed at the canonical address.
    /// An error here is the node being unreachable, not an answer — the caller
    /// should retry rather than pin itself to the sequential path forever.
    pub async fn detect(rpc: &Rpc) -> anyhow::Result<Self> {
        let code = rpc
            .get_code(CANONICAL_MULTICALL3)
            .await
            .context("probing for Multicall3")?;
        Ok(if code.is_empty() {
            Self::sequential()
        } else {
            Self::at(CANONICAL_MULTICALL3)
        })
    }

    /// Read every call, in order. `None` in the result is that one call
    /// failing (a revert, or a view the contract does not have); an `Err` is
    /// the node being unreachable, which fails every call together.
    pub async fn read(&self, rpc: &Rpc, calls: &[Call]) -> anyhow::Result<Vec<Option<Bytes>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        match self.multicall3 {
            Some(mc) => read_batched(rpc, mc, calls).await,
            None => read_sequential(rpc, calls).await,
        }
    }
}

/// `selector ++ abi.encode(args)`.
fn calldata(signature: &str, args: Vec<u8>) -> Vec<u8> {
    let mut out = selector(signature).to_vec();
    out.extend_from_slice(&args);
    out
}

/// The `aggregate3` calldata for a batch — pure, so the encoding is asserted
/// in a test rather than on a live node.
pub fn encode_aggregate3(calls: &[Call]) -> Vec<u8> {
    let batch: Vec<Call3> = calls
        .iter()
        .map(|c| Call3 {
            target: c.target,
            allowFailure: true,
            callData: Bytes::from(c.data.clone()),
        })
        .collect();
    calldata(
        "aggregate3((address,bool,bytes)[])",
        (batch,).abi_encode_params(),
    )
}

/// Decode `aggregate3`'s return into one slot per call, `None` where the call
/// reverted. A length mismatch is a lying node, not a partial answer.
pub fn decode_aggregate3(raw: &Bytes, expected: usize) -> anyhow::Result<Vec<Option<Bytes>>> {
    let (results,) = <(Vec<Result3>,)>::abi_decode_params_validate(raw)
        .map_err(|_| anyhow!("multicall returned undecodable data"))?;
    if results.len() != expected {
        return Err(anyhow!(
            "multicall answered {} of {expected} calls",
            results.len()
        ));
    }
    Ok(results
        .into_iter()
        .map(|r| r.success.then_some(r.returnData))
        .collect())
}

async fn read_batched(
    rpc: &Rpc,
    mc: Address,
    calls: &[Call],
) -> anyhow::Result<Vec<Option<Bytes>>> {
    let data = Bytes::from(encode_aggregate3(calls));
    let raw = rpc
        .eth_call(mc, &data)
        .await
        .context("multicall aggregate3")?;
    decode_aggregate3(&raw, calls.len())
}

async fn read_sequential(rpc: &Rpc, calls: &[Call]) -> anyhow::Result<Vec<Option<Bytes>>> {
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        out.push(Some(
            rpc.eth_call(call.target, &Bytes::from(call.data.clone()))
                .await?,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::mock_node::{mock_rpc, MockChain};
    use alloy_primitives::hex;

    const TOKEN: &str = "0x0000000000000000000000000000000000000aaa";
    const VAULT: &str = "0x0000000000000000000000000000000000000bbb";

    #[test]
    fn aggregate3_selector_matches_cast_sig() {
        assert_eq!(
            hex::encode(selector("aggregate3((address,bool,bytes)[])")),
            "82ad56cb"
        );
    }

    /// The encoder is the piece that would silently mis-read a balance, so
    /// pin the wire bytes: head offset, array length, per-element offsets,
    /// then each `(target, allowFailure, callData)`.
    #[test]
    fn aggregate3_encodes_one_call_to_the_expected_layout() {
        let target = Address::from([0x11u8; 20]);
        let encoded = encode_aggregate3(&[Call::new(target, vec![0xde, 0xad, 0xbe, 0xef])]);
        let body = hex::encode(&encoded[4..]);
        let words: Vec<&str> = (0..body.len() / 64)
            .map(|i| &body[i * 64..(i + 1) * 64])
            .collect();
        assert_eq!(hex::encode(&encoded[..4]), "82ad56cb");
        assert_eq!(
            words[0].trim_start_matches('0'),
            "20",
            "offset to the array"
        );
        assert_eq!(words[1].trim_start_matches('0'), "1", "one call");
        assert_eq!(
            words[2].trim_start_matches('0'),
            "20",
            "offset to element 0"
        );
        assert_eq!(&words[3][24..], "1111111111111111111111111111111111111111");
        assert_eq!(words[4].trim_start_matches('0'), "1", "allowFailure is on");
        assert_eq!(words[5].trim_start_matches('0'), "60", "offset to callData");
        assert_eq!(words[6].trim_start_matches('0'), "4", "four bytes of it");
        assert!(words[7].starts_with("deadbeef"));
    }

    #[test]
    fn a_reverting_call_is_none_and_the_rest_still_decode() {
        let raw = Bytes::from(
            (vec![
                Result3 {
                    success: true,
                    returnData: Bytes::from(U256::from(7u64).to_be_bytes::<32>().to_vec()),
                },
                Result3 {
                    success: false,
                    returnData: Bytes::new(),
                },
            ],)
                .abi_encode_params(),
        );
        let out = decode_aggregate3(&raw, 2).unwrap();
        assert_eq!(decode_uint(out[0].as_ref().unwrap()), U256::from(7u64));
        assert!(out[1].is_none(), "the reverting call must not read as zero");
    }

    #[test]
    fn a_short_answer_is_an_error_rather_than_a_silent_partial() {
        let raw = Bytes::from(
            (vec![Result3 {
                success: true,
                returnData: Bytes::new(),
            }],)
                .abi_encode_params(),
        );
        assert!(decode_aggregate3(&raw, 2).is_err());
    }

    #[test]
    fn decode_uint_matches_read_uint_on_the_empty_return() {
        assert_eq!(decode_uint(&Bytes::new()), U256::ZERO);
        assert_eq!(
            decode_uint(&Bytes::from(U256::from(42u64).to_be_bytes::<32>().to_vec())),
            U256::from(42u64)
        );
    }

    fn views() -> MockChain {
        MockChain::default()
            .balance(TOKEN, 1_000)
            .view(VAULT, "quotableSettlement()", U256::from(500u64))
            .view(VAULT, "tradingEpoch()", U256::from(3u64))
    }

    fn batch_calls() -> Vec<Call> {
        vec![
            Call::new(
                TOKEN.parse().unwrap(),
                crate::closer::executor::encode_balance_of(Address::ZERO),
            ),
            Call::new(
                VAULT.parse().unwrap(),
                crate::protocol::vault::encode_quotable_settlement(),
            ),
            Call::new(
                VAULT.parse().unwrap(),
                crate::protocol::vault::encode_trading_epoch(),
            ),
        ]
    }

    /// The property that matters: batching must not change any answer. Same
    /// node, same calls, one round trip instead of three.
    #[tokio::test]
    async fn batched_and_sequential_reads_agree() {
        let node = mock_rpc(views().with_multicall3()).await;
        let rpc = Rpc::new(&node.url);
        let calls = batch_calls();

        let detected = Batcher::detect(&rpc).await.unwrap();
        assert!(detected.is_batched(), "the mock deploys Multicall3");

        let before = node.hits.load(std::sync::atomic::Ordering::SeqCst);
        let batched = detected.read(&rpc, &calls).await.unwrap();
        let batched_requests = node.hits.load(std::sync::atomic::Ordering::SeqCst) - before;

        let before = node.hits.load(std::sync::atomic::Ordering::SeqCst);
        let one_at_a_time = Batcher::sequential().read(&rpc, &calls).await.unwrap();
        let sequential_requests = node.hits.load(std::sync::atomic::Ordering::SeqCst) - before;

        let words = |r: &[Option<Bytes>]| -> Vec<U256> {
            r.iter().map(|b| decode_uint(b.as_ref().unwrap())).collect()
        };
        assert_eq!(words(&batched), words(&one_at_a_time));
        assert_eq!(
            words(&batched),
            vec![U256::from(1_000u64), U256::from(500u64), U256::from(3u64)]
        );
        assert_eq!(batched_requests, 1, "three reads, one request");
        assert_eq!(sequential_requests, 3);
    }

    /// A chain without Multicall3 must still work — the bot runs on local
    /// Hardhat and on whatever an operator points it at.
    #[tokio::test]
    async fn a_chain_without_multicall3_detects_as_sequential() {
        let node = mock_rpc(views()).await;
        let rpc = Rpc::new(&node.url);
        let detected = Batcher::detect(&rpc).await.unwrap();
        assert!(!detected.is_batched());
        let out = detected.read(&rpc, &batch_calls()).await.unwrap();
        assert_eq!(decode_uint(out[0].as_ref().unwrap()), U256::from(1_000u64));
    }

    #[tokio::test]
    async fn an_empty_batch_costs_no_request() {
        let node = mock_rpc(views().with_multicall3()).await;
        let rpc = Rpc::new(&node.url);
        let before = node.hits.load(std::sync::atomic::Ordering::SeqCst);
        assert!(Batcher::at(CANONICAL_MULTICALL3)
            .read(&rpc, &[])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(node.hits.load(std::sync::atomic::Ordering::SeqCst), before);
    }
}
