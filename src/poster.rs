// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Signing and posting operator order batches to the indexer.

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::indexer::Indexer;
use crate::signer::DynSigner;
use crate::submit::{sign_submission, SubmitOrder};
use crate::tick::unix_now;
use crate::types::OrderParams;

/// Signs and posts one operator order to the indexer. Holds the static context
/// (signer, reactor, permit2…) so the per-tick call sites stay small. The signer
/// is shared (`Arc`) with the blue-leg wallet.
pub struct Poster<'a> {
    pub indexer: &'a Indexer,
    pub signer: DynSigner,
    pub permit2: Address,
    pub chain_id: u64,
    pub maker: Address,
    pub reactor: Address,
    pub dry_run: bool,
}

/// One order slice of a side's ladder, keyed by its stable replacement slot.
pub struct OrderDraft {
    pub nonce: u64,
    pub slot_key: String,
    pub input_amount: U256,
    pub output_amount: U256,
    pub client_order_id: Option<String>,
}

#[derive(Default)]
pub struct PostResult {
    pub posted: usize,
    /// Every nonce the indexer reported as already spent. The whole atomic
    /// batch is rejected when any one slot is spent, so we rotate all of them
    /// at once rather than one-per-tick.
    pub spent_nonces: Vec<u64>,
    /// Unix-seconds order deadline the posted batch was signed with; 0 when
    /// nothing was posted. The slot ledger records it so a replacement only
    /// reuses input the indexer still counts as live.
    pub deadline: u64,
}

impl Poster<'_> {
    /// Build, sign, and POST a side's order batch. Returns the number of orders
    /// posted (or that would be posted in dry-run). The indexer writes the batch
    /// atomically, so a ladder refresh cannot partially replace live slots.
    pub async fn post_many(
        &self,
        ttl_secs: u64,
        input_token: Address,
        output_token: Address,
        drafts: &[OrderDraft],
        label: &str,
        price: f64,
    ) -> PostResult {
        let deadline = unix_now().saturating_add(ttl_secs);

        // Build the orders to sign, preserving draft order; drop zero-size slices.
        let jobs: Vec<(OrderParams, Option<String>)> = drafts
            .iter()
            .filter_map(|draft| {
                if draft.input_amount == U256::ZERO || draft.output_amount == U256::ZERO {
                    warn!(label, "zero-size order; skipping");
                    return None;
                }
                let order = OrderParams {
                    reactor: self.reactor,
                    swapper: self.maker,
                    nonce: U256::from(draft.nonce),
                    deadline: U256::from(deadline),
                    input_token,
                    input_amount: draft.input_amount,
                    output_token,
                    output_amount: draft.output_amount,
                    recipient: self.maker,
                };
                Some((order, draft.client_order_id.clone()))
            })
            .collect();

        if jobs.is_empty() {
            return PostResult::default();
        }

        let submissions = match self.sign_batch(jobs, label).await {
            Some(s) if !s.is_empty() => s,
            _ => return PostResult::default(),
        };

        if self.dry_run {
            for submission in &submissions {
                info!(
                    label,
                    price,
                    input = %submission.input_amount,
                    output = %submission.output_amount,
                    "[dry-run] would post order"
                );
            }
            return PostResult {
                posted: submissions.len(),
                spent_nonces: Vec::new(),
                deadline,
            };
        }

        match self.indexer.submit_many(&submissions).await {
            Ok(ids) => {
                info!(label, price, orders = ids.len(), "posted order batch");
                PostResult {
                    posted: ids.len(),
                    spent_nonces: Vec::new(),
                    deadline,
                }
            }
            Err(e) => {
                warn!(label, error = %e, "batch post failed");
                PostResult {
                    posted: 0,
                    spent_nonces: spent_nonces_from_error(&e.to_string()),
                    deadline,
                }
            }
        }
    }

    /// Sign all orders concurrently, bounded by the backend's limit, returning
    /// them in the original `jobs` order. Remote MPC signing is a round trip per
    /// order, so signing a ladder serially would blow the tick budget. `None`
    /// means at least one order failed to sign — the caller skips the whole
    /// (atomically-posted) batch.
    async fn sign_batch(
        &self,
        jobs: Vec<(OrderParams, Option<String>)>,
        label: &str,
    ) -> Option<Vec<SubmitOrder>> {
        let n = jobs.len();
        let permit2 = self.permit2;
        let chain_id = self.chain_id;
        let limit = self.signer.max_concurrent_signs().max(1);
        let sem = Arc::new(Semaphore::new(limit));
        let mut set: JoinSet<(usize, Option<String>, anyhow::Result<SubmitOrder>)> = JoinSet::new();
        for (idx, (order, client_order_id)) in jobs.into_iter().enumerate() {
            let signer = self.signer.clone();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .expect("signing semaphore is open");
                let res = sign_submission(&order, permit2, chain_id, signer.as_ref()).await;
                (idx, client_order_id, res)
            });
        }

        let mut slots: Vec<Option<SubmitOrder>> = (0..n).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            let (idx, client_order_id, res) = match joined {
                Ok(t) => t,
                Err(e) => {
                    warn!(label, error = %e, "sign task failed; skipping batch");
                    return None;
                }
            };
            match res {
                Ok(mut s) => {
                    s.client_order_id = client_order_id;
                    slots[idx] = Some(s);
                }
                Err(e) => {
                    warn!(label, error = %e, "signing failed; skipping batch");
                    return None;
                }
            }
        }
        Some(slots.into_iter().flatten().collect())
    }
}

/// Total input the drafts would commit (saturating sum).
pub fn drafted_input(drafts: &[OrderDraft]) -> U256 {
    drafts.iter().fold(U256::ZERO, |sum, draft| {
        sum.saturating_add(draft.input_amount)
    })
}

/// Pull every nonce out of the indexer's "Permit2 nonce already spent: <n>,
/// <n>, ..." rejection. The whole atomic batch is rejected when any slot is
/// spent, and the indexer names all of them, so we rotate the full set in one
/// pass. The marker must stay in sync with the API's `submitFillerOrders`
/// validation message (api/src/services/fillerOrders/submit.ts). A single-nonce
/// message (older API) parses to a one-element vec.
pub fn spent_nonces_from_error(error: &str) -> Vec<u64> {
    const MARKER: &str = "Permit2 nonce already spent:";
    let Some(tail) = error.split(MARKER).nth(1) else {
        return Vec::new();
    };
    // Read the comma/space-separated digit runs right after the marker, stopping
    // at the first char that isn't a digit or list separator (e.g. the closing
    // quote of the JSON message).
    let mut nonces = Vec::new();
    let mut current = String::new();
    for c in tail.chars() {
        if c.is_ascii_digit() {
            current.push(c);
        } else if c == ',' || c.is_whitespace() {
            if let Ok(nonce) = current.parse::<u64>() {
                nonces.push(nonce);
            }
            current.clear();
        } else {
            break;
        }
    }
    if let Ok(nonce) = current.parse::<u64>() {
        nonces.push(nonce);
    }
    nonces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spent_nonce_errors_are_parsed_from_indexer_failures() {
        let error =
            r#"indexer rejected order batch: [{"message":"Permit2 nonce already spent: 1002"}]"#;

        assert_eq!(spent_nonces_from_error(error), vec![1002]);
    }

    #[test]
    fn every_spent_nonce_in_a_batch_rejection_is_parsed() {
        let error = r#"indexer rejected order batch: [{"message":"Permit2 nonce already spent: 1781638093041, 1781638093043, 1781638093044"}]"#;

        assert_eq!(
            spent_nonces_from_error(error),
            vec![1781638093041, 1781638093043, 1781638093044]
        );
    }

    #[test]
    fn a_rejection_without_the_marker_yields_no_nonces() {
        let error = "indexer rejected order batch: [{\"message\":\"Order batch is not funded\"}]";

        assert!(spent_nonces_from_error(error).is_empty());
    }

    use crate::signer::{parse_private_key, LocalSigner, Signer};
    use alloy_primitives::B256;
    use async_trait::async_trait;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn order_with_nonce(nonce: u64) -> OrderParams {
        OrderParams {
            reactor: Address::ZERO,
            swapper: Address::ZERO,
            nonce: U256::from(nonce),
            deadline: U256::from(1u64),
            input_token: Address::ZERO,
            input_amount: U256::from(1u64),
            output_token: Address::ZERO,
            output_amount: U256::from(1u64),
            recipient: Address::ZERO,
        }
    }

    fn test_poster(indexer: &Indexer, signer: DynSigner) -> Poster<'_> {
        Poster {
            indexer,
            signer,
            permit2: Address::ZERO,
            chain_id: 8453,
            maker: Address::ZERO,
            reactor: Address::ZERO,
            dry_run: true,
        }
    }

    #[tokio::test]
    async fn sign_batch_preserves_draft_order() {
        let indexer = Indexer::new("http://localhost/graphql".to_string());
        let signer: DynSigner = Arc::new(LocalSigner::new(parse_private_key(TEST_KEY).unwrap()));
        let poster = test_poster(&indexer, signer);

        let jobs: Vec<(OrderParams, Option<String>)> = (0..8)
            .map(|i| (order_with_nonce(1000 + i), Some(format!("coid-{i}"))))
            .collect();

        let signed = poster.sign_batch(jobs, "test").await.expect("all sign");
        assert_eq!(signed.len(), 8);
        for (i, s) in signed.iter().enumerate() {
            assert_eq!(s.nonce, (1000 + i as u64).to_string(), "order preserved");
            assert_eq!(s.client_order_id, Some(format!("coid-{i}")));
        }
    }

    struct FailingSigner;

    #[async_trait]
    impl Signer for FailingSigner {
        async fn sign_digest(&self, _digest: B256) -> anyhow::Result<[u8; 65]> {
            anyhow::bail!("signer unavailable")
        }
        fn address(&self) -> Address {
            Address::ZERO
        }
    }

    #[tokio::test]
    async fn sign_batch_skips_the_whole_batch_on_any_failure() {
        let indexer = Indexer::new("http://localhost/graphql".to_string());
        let poster = test_poster(&indexer, Arc::new(FailingSigner));
        let jobs = vec![
            (order_with_nonce(1), None),
            (order_with_nonce(2), None),
            (order_with_nonce(3), None),
        ];
        assert!(poster.sign_batch(jobs, "test").await.is_none());
    }
}
