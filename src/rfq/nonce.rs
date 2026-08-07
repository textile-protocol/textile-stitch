// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! RFQ Permit2 nonces live in their own namespace: bit 200 set, plus a
//! time-seeded counter. The ladder's slot ledger mints plain `u64` nonces
//! (seeded `unix_now() × 1000`), so with bit 200 the two can never collide on
//! one funding wallet — Permit2 treats every nonce in the same per-owner
//! bitmap, and a collision would let a ladder fill burn a live RFQ quote (or
//! vice versa). The venue also reserves per `(chain, swapper)` as a backstop,
//! but the namespace split is what makes collisions impossible by
//! construction rather than merely detected.
//!
//! RFQ nonces are deliberately not persisted: they're single-shot (one per
//! quote), and the millisecond seed makes a restart's nonces strictly larger
//! than anything minted before it.

use alloy_primitives::U256;

/// The namespace bit. Anything at or above 2^200 is RFQ; the ladder's u64
/// nonces top out 136 bits below it.
pub const RFQ_NONCE_BIT: u32 = 200;

/// Mint the nonce for one RFQ quote: `(1 << 200) | (unix_ms × 1000 + counter)`.
///
/// `counter` is a process-lifetime monotonic count, so quotes inside the same
/// millisecond stay distinct; ×1000 gives each millisecond a thousand slots
/// before the next millisecond's seed catches up.
pub fn rfq_nonce(unix_ms: u64, counter: u64) -> U256 {
    (U256::from(1u8) << RFQ_NONCE_BIT)
        | U256::from(u128::from(unix_ms) * 1000 + u128::from(counter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::unix_now;

    #[test]
    fn rfq_nonces_carry_the_namespace_bit() {
        let n = rfq_nonce(1_754_388_000_000, 0);
        assert_ne!(n & (U256::from(1u8) << RFQ_NONCE_BIT), U256::ZERO);
        // And the low part survives alongside it.
        assert_eq!(
            n & !(U256::from(1u8) << RFQ_NONCE_BIT),
            U256::from(1_754_388_000_000u128 * 1000)
        );
    }

    #[test]
    fn rfq_and_ladder_namespaces_are_disjoint() {
        // The ladder seeds `next_nonce = unix_now() * 1000` and increments —
        // always a u64. Every possible ladder nonce is below 2^200; every RFQ
        // nonce is at or above it.
        let ladder_seed = unix_now().saturating_mul(1000);
        let ladder_max = U256::from(u64::MAX);
        let boundary = U256::from(1u8) << RFQ_NONCE_BIT;
        assert!(U256::from(ladder_seed) < boundary);
        assert!(
            ladder_max < boundary,
            "no u64 ladder nonce can reach bit 200"
        );

        let rfq_min = rfq_nonce(0, 0);
        assert!(
            rfq_min >= boundary,
            "even the degenerate RFQ nonce is above"
        );
        assert!(rfq_nonce(unix_now() * 1000, u64::MAX) >= boundary);
    }

    #[test]
    fn nonces_are_distinct_within_a_millisecond_and_across_time() {
        let a = rfq_nonce(1_754_388_000_000, 0);
        let b = rfq_nonce(1_754_388_000_000, 1);
        let c = rfq_nonce(1_754_388_000_001, 0);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert!(a < b && b < c, "time-then-counter ordering holds");
    }
}
