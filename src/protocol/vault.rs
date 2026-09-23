// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! OperatorVault view calldata and the epoch-prefixed Permit2 nonce.
//!
//! Mirrors `packages/constants/src/operatorVaultMath.ts` (`tradingNonce`) and
//! the no-arg views on `IOperatorVault`. Encoding only — RPC lives with the
//! caller so the RFQ inventory loop can share [`crate::chain::rpc::Wallet`].

use alloy_primitives::{keccak256, Address, U256};

fn selector(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes()).0;
    [h[0], h[1], h[2], h[3]]
}

fn encode_view(sig: &str) -> Vec<u8> {
    selector(sig).to_vec()
}

pub fn encode_free_settlement() -> Vec<u8> {
    encode_view("freeSettlement()")
}

pub fn encode_free_corridor() -> Vec<u8> {
    encode_view("freeCorridor()")
}

pub fn encode_last_settled_nav() -> Vec<u8> {
    encode_view("lastSettledNav()")
}

pub fn encode_settlement_decimals() -> Vec<u8> {
    encode_view("settlementDecimals()")
}

pub fn encode_corridor_decimals() -> Vec<u8> {
    encode_view("corridorDecimals()")
}

pub fn encode_trading_epoch() -> Vec<u8> {
    encode_view("tradingEpoch()")
}

pub fn encode_quotable_settlement() -> Vec<u8> {
    encode_view("quotableSettlement()")
}

pub fn encode_liquid_settlement() -> Vec<u8> {
    encode_view("liquidSettlement()")
}

pub fn encode_quotable_corridor() -> Vec<u8> {
    encode_view("quotableCorridor()")
}

pub fn encode_settlement_asset() -> Vec<u8> {
    encode_view("settlementAsset()")
}

pub fn encode_corridor_asset() -> Vec<u8> {
    encode_view("corridorAsset()")
}

pub fn encode_close_only() -> Vec<u8> {
    encode_view("closeOnly()")
}

pub fn encode_max_order_input_settlement() -> Vec<u8> {
    encode_view("maxOrderInputSettlement()")
}

pub fn encode_max_order_input_corridor() -> Vec<u8> {
    encode_view("maxOrderInputCorridor()")
}

pub fn encode_paused() -> Vec<u8> {
    encode_view("paused()")
}

pub fn encode_max_order_lifetime() -> Vec<u8> {
    encode_view("maxOrderLifetime()")
}

pub fn encode_redemption_epoch_duration() -> Vec<u8> {
    encode_view("redemptionEpochDuration()")
}

pub fn encode_closed_redeem_epoch_id() -> Vec<u8> {
    encode_view("closedRedeemEpochId()")
}

/// `epochs(uint256)` — the public getter on the epoch map. Every member is a
/// value type, so the return is a flat run of 32-byte words in declaration
/// order; see [`RedeemEpochView::decode`].
pub fn encode_epochs(epoch_id: U256) -> Vec<u8> {
    let mut out = encode_view("epochs(uint256)");
    out.extend_from_slice(&epoch_id.to_be_bytes::<32>());
    out
}

/// `closeRedeemEpoch(uint256)` — a write, not a view. The operator admin and
/// the strategy signer may call it whenever they like; everyone else waits
/// `redemptionEpochDuration + valuationTimeout` (audit v0.2 L-02).
pub fn encode_close_redeem_epoch(epoch_id: U256) -> Vec<u8> {
    let mut out = encode_view("closeRedeemEpoch(uint256)");
    out.extend_from_slice(&epoch_id.to_be_bytes::<32>());
    out
}

/// `EpochState.Open` — the only state a close may act on.
const EPOCH_STATE_OPEN: u64 = 1;

/// `EpochState.Closed` — the only state an attestation can settle.
const EPOCH_STATE_CLOSED: u64 = 2;

/// The part of an `Epoch` a close decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedeemEpochView {
    pub state: u64,
    pub is_deposit: bool,
    pub opened_at: u64,
    pub units: U256,
}

impl RedeemEpochView {
    /// Words 0..=6 of the getter's return: state, isDeposit, openedAt,
    /// cutoff, closedAt, inCorridor, units. A short return is a vault that
    /// does not speak this ABI, which is not something to guess at.
    pub fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < 7 * 32 {
            return None;
        }
        let word = |i: usize| -> &[u8] { &raw[i * 32..(i + 1) * 32] };
        let small = |i: usize| -> Option<u64> {
            let w = word(i);
            if w[..24].iter().any(|b| *b != 0) {
                return None;
            }
            Some(u64::from_be_bytes(w[24..32].try_into().ok()?))
        };
        Some(Self {
            state: small(0)?,
            is_deposit: word(1)[31] != 0,
            opened_at: small(2)?,
            units: U256::from_be_slice(word(6)),
        })
    }

    /// Whether an attestation for this epoch could settle it now. Deposit and
    /// redeem epochs share the getter layout, so this holds for both.
    pub fn is_closed(&self) -> bool {
        self.state == EPOCH_STATE_CLOSED
    }

    /// Whether the bot should close this epoch now: an open redeem epoch with
    /// shares in it, past its own duration.
    ///
    /// The duration is the venue's gate too, so this is not second-guessing
    /// the keeper — it is refusing to bump the trading epoch (and kill every
    /// signed order) on a frame that does not match the chain. `units == 0`
    /// closes nothing and costs a re-quote, so it waits.
    pub fn closable_now(&self, redemption_epoch_duration: u64, now_secs: u64) -> bool {
        self.state == EPOCH_STATE_OPEN
            && !self.is_deposit
            && self.units > U256::ZERO
            && now_secs.saturating_sub(self.opened_at) >= redemption_epoch_duration
    }
}

/// Live vault limits used on the quote path. Inventory stays quotable;
/// per-order caps and lifetime clamp each signed order separately.
#[derive(Debug, Clone, Copy)]
pub struct VaultQuotePolicy {
    pub settlement: Address,
    pub corridor: Address,
    pub max_input_settlement: U256,
    pub max_input_corridor: U256,
    pub max_lifetime_secs: u64,
}

impl VaultQuotePolicy {
    pub fn max_input_for(&self, token: Address) -> U256 {
        if token == self.settlement {
            self.max_input_settlement
        } else if token == self.corridor {
            self.max_input_corridor
        } else {
            U256::ZERO
        }
    }

    /// VaultPolicy requires the exact settlement↔corridor pair, either way.
    pub fn matches_pair(&self, input: Address, output: Address) -> bool {
        (input == self.settlement && output == self.corridor)
            || (input == self.corridor && output == self.settlement)
    }
}

/// `paused` zeros both sides. `closeOnly` zeros settlement (`VaultPolicy`
/// rejects selling settlement while closed). Per-order caps stay off this
/// path so reservations still see the full quotable balance.
pub fn apply_vault_order_policy(
    settlement_qty: U256,
    corridor_qty: U256,
    close_only: bool,
    paused: bool,
) -> (U256, U256) {
    if paused {
        return (U256::ZERO, U256::ZERO);
    }
    let settlement = if close_only {
        U256::ZERO
    } else {
        settlement_qty
    };
    (settlement, corridor_qty)
}

/// How much settlement the bot may publish and sign for, given how its
/// orders will be filled.
///
/// `quotableSettlement()` prices the vault's whole economic inventory: what
/// sits idle in the vault plus what `allocateIdle` has parked in the yield
/// adapter (Aave). `liquidSettlement()` is only the idle part — the balance a
/// plain Permit2 pull can reach, and what `VaultPolicy.validateEnvelope` caps
/// a settlement-input order at when it runs.
///
/// Whether the adapter position counts depends on the route, not the vault.
/// A fill through the chain's `VaultOrderExecutor` calls `prepareSettlement`
/// before the reactor's Permit2 pull, so by the time the envelope is checked
/// the adapter position is back in the vault and only the economic figure
/// binds. A direct `reactor.execute` never recalls, so anything above liquid
/// reverts on the pull. Publishing the liquid figure for an executor-routed
/// vault pins a mostly-staked vault to its idle floor — a vault holding 2
/// USDT with 1.99 in Aave would advertise 0.01 — and takes it off the market.
pub fn quotable_settlement_for_route(quotable: U256, liquid: U256, executor_routed: bool) -> U256 {
    if executor_routed {
        quotable
    } else {
        quotable.min(liquid)
    }
}

/// Clamp a requested deadline to `now + maxOrderLifetime`. None if that
/// leaves no usable life.
pub fn clamp_vault_deadline(
    now_secs: u64,
    deadline_secs: u64,
    max_lifetime_secs: u64,
) -> Option<u64> {
    let cap = now_secs.saturating_add(max_lifetime_secs);
    let deadline = deadline_secs.min(cap);
    (deadline > now_secs).then_some(deadline)
}

/// `(epoch << 128) | counter` — VaultPolicy requires `epochFromNonce == tradingEpoch`.
pub fn trading_nonce(epoch: u64, counter: u128) -> U256 {
    (U256::from(epoch) << 128) | U256::from(counter)
}

/// Low 128 bits of a vault nonce: per-process salt in the high 64, wall-clock
/// milliseconds ×1000 plus the process counter in the low 64. `ms * 1000`
/// stays under 2^64 until the year ~2554, so the fields never overlap. The
/// salt is what keeps two bots quoting one vault from colliding when both
/// sign in the same millisecond with equal counters — the venue serializes
/// nonce reservations per vault and would reject the second as
/// `nonce_reserved`.
pub fn vault_nonce_low(salt: u64, unix_ms: u64, counter: u64) -> u128 {
    (u128::from(salt) << 64) | (u128::from(unix_ms) * 1000 + u128::from(counter))
}

pub fn address_from_word(word: U256) -> Address {
    let bytes = word.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    #[test]
    fn view_selectors_match_cast_keccak() {
        assert_eq!(&encode_trading_epoch(), &hex::decode("e3c85e7e").unwrap());
        assert_eq!(
            &encode_quotable_settlement(),
            &hex::decode("0f8769a0").unwrap()
        );
        assert_eq!(
            &encode_liquid_settlement(),
            &hex::decode("84d4fc64").unwrap()
        );
        assert_eq!(
            &encode_quotable_corridor(),
            &hex::decode("2e1741f9").unwrap()
        );
        assert_eq!(
            &encode_settlement_asset(),
            &hex::decode("d3781d58").unwrap()
        );
        assert_eq!(&encode_corridor_asset(), &hex::decode("f0f85843").unwrap());
        assert_eq!(&encode_close_only(), &hex::decode("c7dc844d").unwrap());
        assert_eq!(
            &encode_max_order_input_settlement(),
            &hex::decode("cb06c682").unwrap()
        );
        assert_eq!(
            &encode_max_order_input_corridor(),
            &hex::decode("65e36cd1").unwrap()
        );
        assert_eq!(&encode_paused(), &hex::decode("5c975abb").unwrap());
        assert_eq!(
            &encode_max_order_lifetime(),
            &hex::decode("9c454e9d").unwrap()
        );
        assert_eq!(
            &encode_redemption_epoch_duration(),
            &hex::decode("de2ab60a").unwrap()
        );
        assert_eq!(
            &encode_closed_redeem_epoch_id(),
            &hex::decode("944eb53d").unwrap()
        );
    }

    #[test]
    fn epoch_calls_carry_the_id_after_the_selector() {
        let id = U256::from(3u64);
        let epochs = encode_epochs(id);
        assert_eq!(&epochs[..4], &hex::decode("c6b61e4c").unwrap()[..]);
        assert_eq!(epochs.len(), 36);
        assert_eq!(U256::from_be_slice(&epochs[4..]), id);

        let close = encode_close_redeem_epoch(id);
        assert_eq!(&close[..4], &hex::decode("c05c4a5b").unwrap()[..]);
        assert_eq!(U256::from_be_slice(&close[4..]), id);
    }

    /// The real return of `epochs(3)` on the BSC tvUSDT-cNGN vault
    /// (0x180c8ef3…b77a): an open redeem epoch holding ~3.002 shares.
    const LIVE_OPEN_REDEEM_EPOCH: &str = concat!(
        "0000000000000000000000000000000000000000000000000000000000000001",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000006ab24783",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "00000000000000000000000000000000000000000000000029a923ad4738b03f",
    );

    #[test]
    fn decodes_a_live_open_redeem_epoch() {
        let raw = hex::decode(LIVE_OPEN_REDEEM_EPOCH).unwrap();
        let view = RedeemEpochView::decode(&raw).expect("seven words is enough");
        assert_eq!(view.state, 1, "Open");
        assert!(!view.is_deposit);
        assert_eq!(view.opened_at, 1_790_068_611);
        assert_eq!(view.units, U256::from(3_001_969_853_750_358_079u64));
        assert!(!view.is_closed(), "an open epoch can't be attested");
    }

    #[test]
    fn only_a_closed_epoch_can_be_attested() {
        let mut raw = hex::decode(LIVE_OPEN_REDEEM_EPOCH).unwrap();
        raw[31] = 2; // EpochState.Closed
        assert!(RedeemEpochView::decode(&raw).unwrap().is_closed());
        raw[31] = 3; // Processed: already settled
        assert!(!RedeemEpochView::decode(&raw).unwrap().is_closed());
    }

    #[test]
    fn a_short_return_decodes_to_nothing() {
        // A vault that does not speak this ABI. Guessing at a close that
        // bumps the trading epoch is not the move.
        let raw = hex::decode(LIVE_OPEN_REDEEM_EPOCH).unwrap();
        assert!(RedeemEpochView::decode(&raw[..6 * 32]).is_none());
    }

    #[test]
    fn only_a_due_open_redeem_epoch_with_shares_is_closable() {
        let opened_at = 1_790_068_611u64;
        let base = RedeemEpochView {
            state: 1,
            is_deposit: false,
            opened_at,
            units: U256::from(5u64),
        };
        assert!(base.closable_now(300, opened_at + 300));
        assert!(
            !base.closable_now(300, opened_at + 299),
            "inside the duration"
        );
        assert!(
            !RedeemEpochView {
                units: U256::ZERO,
                ..base
            }
            .closable_now(300, opened_at + 300),
            "an empty epoch closes nothing and costs a re-quote"
        );
        assert!(
            !RedeemEpochView {
                is_deposit: true,
                ..base
            }
            .closable_now(300, opened_at + 300),
            "deposit epochs close on their own cutoff, permissionlessly"
        );
        assert!(
            !RedeemEpochView { state: 2, ..base }.closable_now(300, opened_at + 300),
            "already Closed"
        );
    }

    #[test]
    fn executor_routed_vaults_quote_the_adapter_position_too() {
        // 2 USDT of economic inventory, 0.01 idle, the rest in Aave.
        let quotable = U256::from(2_000_000u64);
        let liquid = U256::from(10_000u64);
        assert_eq!(
            quotable_settlement_for_route(quotable, liquid, true),
            quotable,
            "the executor unstakes before the Permit2 pull, so the whole position is fillable"
        );
        assert_eq!(
            quotable_settlement_for_route(quotable, liquid, false),
            liquid,
            "a direct reactor fill can only pull what is idle"
        );
    }

    #[test]
    fn the_direct_route_never_publishes_more_than_quotable() {
        // Liquid above quotable happens when minReserveSettlement bites:
        // quotable nets the reserve out, liquid does not.
        let quotable = U256::from(500u64);
        let liquid = U256::from(900u64);
        assert_eq!(
            quotable_settlement_for_route(quotable, liquid, false),
            quotable
        );
        assert_eq!(
            quotable_settlement_for_route(quotable, liquid, true),
            quotable
        );
    }

    #[test]
    fn close_only_zeros_settlement_and_pause_zeros_both() {
        let (open_s, open_c) =
            apply_vault_order_policy(U256::from(1_000u64), U256::from(2_000u64), false, false);
        assert_eq!(open_s, U256::from(1_000u64));
        assert_eq!(open_c, U256::from(2_000u64));
        let (closed, still) =
            apply_vault_order_policy(U256::from(1_000u64), U256::from(2_000u64), true, false);
        assert_eq!(closed, U256::ZERO);
        assert_eq!(still, U256::from(2_000u64));
        let (paused_s, paused_c) =
            apply_vault_order_policy(U256::from(1_000u64), U256::from(2_000u64), false, true);
        assert_eq!(paused_s, U256::ZERO);
        assert_eq!(paused_c, U256::ZERO);
    }

    #[test]
    fn matches_pair_is_the_vault_assets_in_either_direction() {
        let policy = VaultQuotePolicy {
            settlement: Address::from([1u8; 20]),
            corridor: Address::from([2u8; 20]),
            max_input_settlement: U256::ZERO,
            max_input_corridor: U256::ZERO,
            max_lifetime_secs: 0,
        };
        let other = Address::from([3u8; 20]);
        assert!(policy.matches_pair(policy.settlement, policy.corridor));
        assert!(policy.matches_pair(policy.corridor, policy.settlement));
        assert!(!policy.matches_pair(policy.settlement, other));
        assert!(!policy.matches_pair(other, policy.corridor));
    }

    #[test]
    fn vault_deadline_clamps_to_max_lifetime() {
        assert_eq!(clamp_vault_deadline(1_000, 2_000, 30), Some(1_030));
        assert_eq!(clamp_vault_deadline(1_000, 1_010, 30), Some(1_010));
        assert_eq!(clamp_vault_deadline(1_000, 1_000, 30), None);
    }

    #[test]
    fn vault_nonce_low_namespaces_by_process_salt() {
        let a = vault_nonce_low(1, 1_754_388_000_000, 0);
        let b = vault_nonce_low(2, 1_754_388_000_000, 0);
        // Same millisecond, same counter, different process — distinct nonces.
        assert_ne!(a, b);
        // Salt sits above the ms field; ms*1000 + counter survives below it.
        assert_eq!(a >> 64, 1u128);
        assert_eq!(a & u128::from(u64::MAX), 1_754_388_000_000u128 * 1000);
        // Epoch still lands in the high half of the full nonce.
        let n = trading_nonce(7, a);
        assert_eq!(n >> 128, U256::from(7u64));
    }

    #[test]
    fn trading_nonce_embeds_the_epoch_in_the_high_half() {
        let n = trading_nonce(7, 42);
        assert_eq!(n >> 128, U256::from(7u64));
        assert_eq!(
            n & ((U256::from(1u8) << 128) - U256::from(1u8)),
            U256::from(42u64)
        );
    }
}
