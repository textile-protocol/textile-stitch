// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! In-flight RFQ inventory reservations.
//!
//! Every signed quote is a live claim on the funding wallet until its order
//! deadline passes — INCLUDING quotes the venue reports as lost
//! (`lost_price`): a losing quote is still a valid signed order the winner's
//! failure could route to, so its reservation holds until `deadline + skew`,
//! never until the loss notice. Releases are therefore purely time-based; the
//! venue's result frames are informational.

use std::collections::HashMap;

use alloy_primitives::U256;

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

/// The reservation ledger. Owned by the responder task; no interior locking.
#[derive(Debug, Default)]
pub struct Reservations {
    by_rfq: HashMap<String, Reservation>,
}

impl Reservations {
    pub fn new() -> Self {
        Self::default()
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
    }

    /// Total input currently reserved on one side of a corridor. Expired
    /// entries never count (release is lazy; [`Self::prune`] reclaims memory).
    pub fn reserved(&self, corridor: &str, bid: bool, now_secs: u64) -> U256 {
        self.by_rfq
            .values()
            .filter(|r| r.corridor == corridor && r.bid == bid && r.release_at > now_secs)
            .fold(U256::ZERO, |sum, r| sum.saturating_add(r.input))
    }

    /// Drop entries past their release time. Called on the 1s levels tick so
    /// the map can't grow unboundedly between quote bursts.
    pub fn prune(&mut self, now_secs: u64) {
        self.by_rfq.retain(|_, r| r.release_at > now_secs);
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
    fn reservations_hold_until_deadline_plus_skew_not_the_loss_notice() {
        let mut r = Reservations::new();
        let deadline = 1_000u64;
        r.reserve("rfq_1", "cngn-usdc", true, U256::from(500u64), deadline);

        // The venue reported lost_price at t=900 — the reservation must NOT
        // release then: the signed order is live until its deadline.
        assert_eq!(
            r.reserved("cngn-usdc", true, 900),
            U256::from(500u64),
            "a losing quote still claims inventory before its deadline"
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
}
