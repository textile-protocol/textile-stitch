// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Signing and posting operator order batches to the indexer.

use alloy_primitives::{Address, U256};
use k256::ecdsa::SigningKey;
use tracing::{info, warn};

use crate::indexer::Indexer;
use crate::submit::sign_submission;
use crate::tick::unix_now;
use crate::types::OrderParams;

/// Signs and posts one operator order to the indexer. Holds the static context
/// (key, reactor, permit2…) so the per-tick call sites stay small.
pub struct Poster<'a> {
    pub indexer: &'a Indexer,
    pub key: &'a SigningKey,
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
    pub spent_nonce: Option<u64>,
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
        let mut submissions = Vec::new();

        for draft in drafts {
            if draft.input_amount == U256::ZERO || draft.output_amount == U256::ZERO {
                warn!(label, "zero-size order; skipping");
                continue;
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
            match sign_submission(&order, self.permit2, self.chain_id, self.key) {
                Ok(mut s) => {
                    s.client_order_id = draft.client_order_id.clone();
                    submissions.push(s);
                }
                Err(e) => {
                    warn!(label, error = %e, "signing failed; skipping batch");
                    return PostResult::default();
                }
            }
        }

        if submissions.is_empty() {
            return PostResult::default();
        }

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
                spent_nonce: None,
                deadline,
            };
        }

        match self.indexer.submit_many(&submissions).await {
            Ok(ids) => {
                info!(label, price, orders = ids.len(), "posted order batch");
                PostResult {
                    posted: ids.len(),
                    spent_nonce: None,
                    deadline,
                }
            }
            Err(e) => {
                warn!(label, error = %e, "batch post failed");
                PostResult {
                    posted: 0,
                    spent_nonce: spent_nonce_from_error(&e.to_string()),
                    deadline,
                }
            }
        }
    }
}

/// Total input the drafts would commit (saturating sum).
pub fn drafted_input(drafts: &[OrderDraft]) -> U256 {
    drafts.iter().fold(U256::ZERO, |sum, draft| {
        sum.saturating_add(draft.input_amount)
    })
}

/// Pull the nonce out of the indexer's "Permit2 nonce already spent: <n>"
/// rejection. The marker must stay in sync with the API's `submitFillerOrder`
/// validation message (api/src/services/fillerOrders).
pub fn spent_nonce_from_error(error: &str) -> Option<u64> {
    const MARKER: &str = "Permit2 nonce already spent:";
    let tail = error.split(MARKER).nth(1)?;
    let digits: String = tail
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spent_nonce_errors_are_parsed_from_indexer_failures() {
        let error =
            r#"indexer rejected order batch: [{"message":"Permit2 nonce already spent: 1002"}]"#;

        assert_eq!(spent_nonce_from_error(error), Some(1002));
    }
}
