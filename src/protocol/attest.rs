// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Countersigning an OperatorVault NAV attestation.
//!
//! The vault converts deposit and redeem epochs at whatever the attestation
//! says, and takes it only when both its signers signed the same digest: the
//! risk signer (Warp) builds it, and this bot, as the strategy signer, checks
//! it and countersigns. The check is the whole point — a bot that signs
//! whatever it is handed turns two keys back into one. So before signing the
//! bot reads the vault itself and compares every figure, and prices the
//! corridor leg off its own feed.
//!
//! Pure functions here; the RPC read and the signing live with the session
//! task in `rfq`.

use alloy_primitives::{Address, U256};

use crate::rfq::wire::{AttestRejectReason, NavAttestationWire};

const WAD: u128 = 1_000_000_000_000_000_000;

/// What the vault reads when it checks the same attestation. `free_*` are
/// the balances net of pending and reserved; `last_settled_nav` is the
/// replay guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveVault {
    pub free_settlement: U256,
    pub free_corridor: U256,
    pub last_settled_nav: U256,
    pub settlement_decimals: u8,
    pub corridor_decimals: u8,
}

/// Decoded `NavAttestation`, field for field the struct the vault hashes.
///
/// Two versions are live. Vaults built before the struct dropped `nav` hash
/// ten fields under domain version `1`; newer ones derive NAV from the floors
/// and the price, and hash the other nine under version `2`. `nav` being set
/// is what says which, and the venue sends it only for the older vaults.
/// Nothing here reads the chain to confirm the version: a signature over the
/// wrong one is worthless rather than dangerous, since the vault hashes its
/// own domain and struct and simply rejects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavAttestation {
    pub vault: Address,
    pub chain_id: U256,
    pub epoch_id: U256,
    pub corridor_asset_price: U256,
    /// Version `1` only. `None` is a version `2` attestation.
    pub nav: Option<U256>,
    pub last_settled_nav: U256,
    pub free_settlement: U256,
    pub free_corridor: U256,
    pub valid_after: U256,
    pub valid_until: U256,
}

impl NavAttestation {
    pub fn parse(w: &NavAttestationWire) -> anyhow::Result<Self> {
        let u = |s: &str| -> anyhow::Result<U256> {
            s.parse::<U256>()
                .map_err(|e| anyhow::anyhow!("bad uint {s:?}: {e}"))
        };
        Ok(Self {
            vault: w.vault.parse()?,
            chain_id: u(&w.chain_id)?,
            epoch_id: u(&w.epoch_id)?,
            corridor_asset_price: u(&w.corridor_asset_price)?,
            nav: w.nav.as_deref().map(u).transpose()?,
            last_settled_nav: u(&w.last_settled_nav)?,
            free_settlement: u(&w.free_settlement)?,
            free_corridor: u(&w.free_corridor)?,
            valid_after: u(&w.valid_after)?,
            valid_until: u(&w.valid_until)?,
        })
    }

    /// The EIP-712 domain version of the vault this attestation is for.
    pub fn version(&self) -> &'static str {
        if self.nav.is_some() {
            "1"
        } else {
            "2"
        }
    }

    /// What the signed floors are worth at the signed price: the NAV a
    /// version `2` vault derives, and the one a version `1` attestation must
    /// carry.
    pub fn derived_nav(&self, live: &LiveVault) -> U256 {
        nav(
            self.free_settlement,
            self.free_corridor,
            self.corridor_asset_price,
            live.settlement_decimals,
            live.corridor_decimals,
        )
    }

    /// The attested NAV, or the derived one where the struct has none. For logs.
    pub fn nav_or_derived(&self, live: &LiveVault) -> U256 {
        self.nav.unwrap_or_else(|| self.derived_nav(live))
    }
}

/// How far the attested corridor price may sit from this bot's own mark,
/// either way. Warp and the bot price off different feeds, so some gap is
/// normal; a gap this wide is a feed disagreement the operator should look
/// at, not one to sign through.
pub const MAX_PRICE_GAP_BPS: u128 = 100;

/// The most a settlement position could plausibly accrue in a year, as an
/// upper bound on the yield adapter. Warp reads at one block and the bot a
/// few later, and the adapter accrues every block, so live settlement can
/// run a hair ahead of the signed floor. The allowance is that hair and
/// nothing more: a flat percentage here would be a mark-down the risk key
/// alone could choose, and the whole point of the second signature is that
/// it cannot.
pub const MAX_YIELD_APY_BPS: u128 = 2_000;
const SECONDS_PER_YEAR: u128 = 365 * 24 * 60 * 60;

/// How long an attestation is at most in flight between Warp's read and
/// ours: the keeper's whole tick is under a minute, so ten minutes is
/// generous. A constant, deliberately. The attestation carries `validAfter`,
/// but that is the risk key's own claim about when it read the chain, and a
/// compromised key could set it to zero and buy itself years of "accrual".
pub const ATTESTATION_IN_FLIGHT_SECS: u128 = 10 * 60;

/// The longest validity window this bot co-signs. Warp's honest attestations
/// run `attest.max_validity_secs` from the block it read (3600 in the deployed
/// config), and the vault accepts an attestation at any moment inside its
/// window. Without a ceiling here, a compromised risk key could get a
/// co-signature on a window that never ends, hold it, and settle later at a
/// price that suited it (audit 2026-09-22, attestation-validity-window). If
/// Warp's ceiling (`MAX_ATTEST_VALIDITY_SECS` in packages/warp/src/config/mod.rs)
/// is ever raised, raise this with it.
pub const MAX_ATTESTATION_VALIDITY_SECS: u64 = 60 * 60;

/// Slack for this host's clock against the chain's block timestamps.
pub const ATTESTATION_CLOCK_SKEW_SECS: u64 = 60;

/// The window must be current and short: already open, not yet over, and
/// ending no later than [`MAX_ATTESTATION_VALIDITY_SECS`] from now. A window
/// that opens in the future is refused too, since it would let a signature
/// made at today's price be used later without ever being long.
pub fn check_window(att: &NavAttestation, now_secs: u64) -> Result<(), AttestRejectReason> {
    let now = U256::from(now_secs);
    let skew = U256::from(ATTESTATION_CLOCK_SKEW_SECS);
    if att.valid_after > now + skew || att.valid_until <= now {
        return Err(AttestRejectReason::Window);
    }
    if att.valid_until > now + U256::from(MAX_ATTESTATION_VALIDITY_SECS) + skew {
        return Err(AttestRejectReason::Window);
    }
    Ok(())
}

/// What live settlement may exceed the signed floor by, plus one unit for
/// rounding: the yield ceiling over the in-flight window.
pub fn accrual_allowance(live_settlement: U256) -> U256 {
    live_settlement * U256::from(MAX_YIELD_APY_BPS * ATTESTATION_IN_FLIGHT_SECS)
        / U256::from(10_000u128 * SECONDS_PER_YEAR)
        + U256::from(1u64)
}

/// `VaultLib.nav`: settlement-denominated NAV from free balances and a WAD
/// price of settlement per corridor.
pub fn nav(
    free_settlement: U256,
    free_corridor: U256,
    price_wad: U256,
    settlement_decimals: u8,
    corridor_decimals: u8,
) -> U256 {
    if free_corridor.is_zero() || price_wad.is_zero() {
        return free_settlement;
    }
    let wad = U256::from(WAD);
    if settlement_decimals >= corridor_decimals {
        let exp = u32::from(settlement_decimals - corridor_decimals);
        let denom = wad / U256::from(10u64).pow(U256::from(exp));
        return free_settlement + mul_div(free_corridor, price_wad, denom);
    }
    let scale = U256::from(10u64).pow(U256::from(u32::from(
        corridor_decimals - settlement_decimals,
    )));
    free_settlement + mul_div(free_corridor, price_wad, scale * wad)
}

/// Floor `a * b / c` without overflow (512-bit intermediate).
fn mul_div(a: U256, b: U256, c: U256) -> U256 {
    let wide = a.widening_mul::<256, 4, 512, 8>(b);
    let q = wide / alloy_primitives::ruint::Uint::<512, 8>::from(c);
    U256::from_limbs([
        q.as_limbs()[0],
        q.as_limbs()[1],
        q.as_limbs()[2],
        q.as_limbs()[3],
    ])
}

/// The bot's own price as a WAD of settlement per corridor.
pub fn price_wad(price: f64) -> U256 {
    U256::from((price * WAD as f64).round() as u128)
}

fn bps_of(value: U256, bps: u128) -> U256 {
    value * U256::from(bps) / U256::from(10_000u64)
}

fn abs_diff(a: U256, b: U256) -> U256 {
    if a > b {
        a - b
    } else {
        b - a
    }
}

/// Everything the bot can check before it signs.
///
/// The signed figures must be what the bot reads live: `lastSettledNav` and
/// the corridor floor exactly (nothing moves them between Warp's read and
/// ours except a fill, which the vault rejects anyway), and the settlement
/// floor within what the yield adapter could have accrued while the
/// attestation was in flight. A version `1` attestation's signed NAV must be
/// what those floors are worth at the signed price (a version `2` vault
/// derives it the same way, so there is nothing to check), and the price must
/// sit near the bot's own mark; the chain has no price to check it against.
pub fn check_attestation(
    att: &NavAttestation,
    live: &LiveVault,
    own_price_wad: U256,
) -> Result<(), AttestRejectReason> {
    if att.last_settled_nav != live.last_settled_nav || att.free_corridor != live.free_corridor {
        return Err(AttestRejectReason::Figures);
    }
    if att.free_settlement > live.free_settlement
        || live.free_settlement - att.free_settlement > accrual_allowance(live.free_settlement)
    {
        return Err(AttestRejectReason::Figures);
    }
    if att
        .nav
        .is_some_and(|signed| signed != att.derived_nav(live))
    {
        return Err(AttestRejectReason::Figures);
    }
    if abs_diff(att.corridor_asset_price, own_price_wad) > bps_of(own_price_wad, MAX_PRICE_GAP_BPS)
    {
        return Err(AttestRejectReason::Price);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn live() -> LiveVault {
        LiveVault {
            free_settlement: U256::from(10_000_000u64),
            free_corridor: U256::from(4_000_000_000_000_000_000u128),
            last_settled_nav: U256::from(12_345u64),
            settlement_decimals: 6,
            corridor_decimals: 18,
        }
    }

    fn honest(price: U256) -> NavAttestation {
        let l = live();
        NavAttestation {
            vault: address!("2222222222222222222222222222222222222222"),
            chain_id: U256::from(8453u64),
            epoch_id: U256::from(7u64),
            corridor_asset_price: price,
            nav: Some(nav(l.free_settlement, l.free_corridor, price, 6, 18)),
            last_settled_nav: l.last_settled_nav,
            free_settlement: l.free_settlement,
            free_corridor: l.free_corridor,
            valid_after: U256::from(1_700_000_000u64),
            valid_until: U256::from(1_700_003_600u64),
        }
    }

    const PRICE: U256 = U256::from_limbs([1_500_000_000_000_000_000, 0, 0, 0]);

    #[test]
    fn nav_matches_vault_lib() {
        // 10 USDT + 4 cNGN at 1.5 USDT/cNGN = 16 USDT, in 6dp.
        assert_eq!(
            nav(
                U256::from(10_000_000u64),
                U256::from(4_000_000_000_000_000_000u128),
                PRICE,
                6,
                18
            ),
            U256::from(16_000_000u64)
        );
        // Zero corridor or zero price is settlement only.
        assert_eq!(
            nav(U256::from(5u64), U256::ZERO, PRICE, 6, 18),
            U256::from(5u64)
        );
        assert_eq!(
            nav(U256::from(5u64), U256::from(9u64), U256::ZERO, 6, 18),
            U256::from(5u64)
        );
    }

    /// `honest()` runs 1_700_000_000..1_700_003_600, exactly Warp's shape.
    const SIGNED_AT: u64 = 1_700_000_000;

    fn with_window(after: u64, until: U256) -> NavAttestation {
        NavAttestation {
            valid_after: U256::from(after),
            valid_until: until,
            ..honest(PRICE)
        }
    }

    #[test]
    fn a_warp_shaped_window_passes_while_it_is_open() {
        assert_eq!(check_window(&honest(PRICE), SIGNED_AT), Ok(()));
        assert_eq!(check_window(&honest(PRICE), SIGNED_AT + 3_599), Ok(()));
        // This host's clock a little behind the chain is fine.
        assert_eq!(check_window(&honest(PRICE), SIGNED_AT - 30), Ok(()));
    }

    #[test]
    fn a_window_that_never_ends_is_refused() {
        // The banked-attestation shape from the audit: validUntil = 2^256 - 1.
        let banked = with_window(SIGNED_AT, U256::MAX);
        assert_eq!(
            check_window(&banked, SIGNED_AT),
            Err(AttestRejectReason::Window)
        );
        // Just past the ceiling (plus skew) is refused too.
        let long = with_window(
            SIGNED_AT,
            U256::from(SIGNED_AT + MAX_ATTESTATION_VALIDITY_SECS + ATTESTATION_CLOCK_SKEW_SECS + 1),
        );
        assert_eq!(
            check_window(&long, SIGNED_AT),
            Err(AttestRejectReason::Window)
        );
    }

    #[test]
    fn a_future_dated_window_is_refused() {
        // Short, but it opens in a year: today's price, usable next year.
        let year = 365 * 24 * 60 * 60;
        let later = with_window(SIGNED_AT + year, U256::from(SIGNED_AT + year + 3_600));
        assert_eq!(
            check_window(&later, SIGNED_AT),
            Err(AttestRejectReason::Window)
        );
    }

    #[test]
    fn an_expired_window_is_refused() {
        assert_eq!(
            check_window(&honest(PRICE), SIGNED_AT + 3_600),
            Err(AttestRejectReason::Window)
        );
    }

    #[test]
    fn an_honest_attestation_passes() {
        assert_eq!(check_attestation(&honest(PRICE), &live(), PRICE), Ok(()));
    }

    #[test]
    fn a_marked_down_attestation_is_refused() {
        // The zero attestation the vault alone would accept.
        let mut att = honest(PRICE);
        att.nav = Some(U256::ZERO);
        att.free_settlement = U256::ZERO;
        att.free_corridor = U256::ZERO;
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
        // Floors that add up but sit a little under live, on either leg.
        // 50 bps on 10 USDT is 5,000 units; ten minutes of 20% APY is 38.
        let l = live();
        let mut att = honest(PRICE);
        att.free_settlement = l.free_settlement * U256::from(9_950u64) / U256::from(10_000u64);
        att.nav = Some(nav(att.free_settlement, att.free_corridor, PRICE, 6, 18));
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
        let mut att = honest(PRICE);
        att.free_corridor -= U256::from(1u64);
        att.nav = Some(nav(att.free_settlement, att.free_corridor, PRICE, 6, 18));
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
    }

    #[test]
    fn yield_accrued_in_flight_is_allowed_and_nothing_the_signer_can_widen() {
        // 10 USDT at 20% APY accrues ~38 units in ten minutes; allow 39 (+1
        // rounding). 40 is past what any adapter could have earned.
        assert_eq!(
            accrual_allowance(U256::from(10_000_000u64)),
            U256::from(39u64)
        );
        let mut l = live();
        l.free_settlement += U256::from(39u64);
        assert_eq!(check_attestation(&honest(PRICE), &l, PRICE), Ok(()));
        l.free_settlement += U256::from(1u64);
        assert_eq!(
            check_attestation(&honest(PRICE), &l, PRICE),
            Err(AttestRejectReason::Figures)
        );
        // A signer cannot buy itself a wider window: `validAfter` at zero
        // and a zero settlement floor is still refused.
        let mut att = honest(PRICE);
        att.valid_after = U256::ZERO;
        att.free_settlement = U256::ZERO;
        att.nav = Some(nav(att.free_settlement, att.free_corridor, PRICE, 6, 18));
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
    }

    #[test]
    fn floors_above_live_or_a_stale_replay_guard_are_refused() {
        let mut att = honest(PRICE);
        att.free_corridor += U256::from(1u64);
        att.nav = Some(nav(att.free_settlement, att.free_corridor, PRICE, 6, 18));
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
        let mut att = honest(PRICE);
        att.last_settled_nav += U256::from(1u64);
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
    }

    #[test]
    fn a_nav_that_disagrees_with_its_own_floors_is_refused() {
        let mut att = honest(PRICE);
        att.nav = att.nav.map(|n| n - U256::from(1u64));
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
    }

    /// A version `2` vault derives NAV itself, so its attestation has none.
    fn honest_v2(price: U256) -> NavAttestation {
        NavAttestation {
            nav: None,
            ..honest(price)
        }
    }

    #[test]
    fn the_version_follows_whether_nav_is_there() {
        assert_eq!(honest(PRICE).version(), "1");
        assert_eq!(honest_v2(PRICE).version(), "2");
    }

    #[test]
    fn a_v2_attestation_passes_without_a_nav() {
        assert_eq!(check_attestation(&honest_v2(PRICE), &live(), PRICE), Ok(()));
        assert_eq!(
            honest_v2(PRICE).nav_or_derived(&live()),
            U256::from(16_000_000u64)
        );
    }

    #[test]
    fn a_v2_attestation_gets_every_other_check() {
        let mut att = honest_v2(PRICE);
        att.free_settlement = U256::ZERO;
        att.free_corridor = U256::ZERO;
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
        let mut att = honest_v2(PRICE);
        att.last_settled_nav += U256::from(1u64);
        assert_eq!(
            check_attestation(&att, &live(), PRICE),
            Err(AttestRejectReason::Figures)
        );
        assert_eq!(
            check_attestation(&honest_v2(PRICE), &live(), price_wad(1.5 * 1.02)),
            Err(AttestRejectReason::Price)
        );
        let banked = NavAttestation {
            valid_until: U256::MAX,
            ..honest_v2(PRICE)
        };
        assert_eq!(
            check_window(&banked, SIGNED_AT),
            Err(AttestRejectReason::Window)
        );
    }

    #[test]
    fn the_price_must_sit_within_the_band_of_our_own_mark() {
        // 0.9% off: inside 100 bps.
        let near = price_wad(1.5 * 0.991);
        assert_eq!(check_attestation(&honest(near), &live(), PRICE), Ok(()));
        // 2% off either way: out.
        for own in [price_wad(1.5 * 1.02), price_wad(1.5 * 0.98)] {
            assert_eq!(
                check_attestation(&honest(PRICE), &live(), own),
                Err(AttestRejectReason::Price)
            );
        }
    }

    #[test]
    fn parses_the_wire_shape() {
        let w = NavAttestationWire {
            vault: "0x2222222222222222222222222222222222222222".into(),
            chain_id: "8453".into(),
            epoch_id: "7".into(),
            corridor_asset_price: "1500000000000000000".into(),
            nav: Some("16000000".into()),
            last_settled_nav: "12345".into(),
            free_settlement: "10000000".into(),
            free_corridor: "4000000000000000000".into(),
            valid_after: "1700000000".into(),
            valid_until: "1700003600".into(),
        };
        assert_eq!(NavAttestation::parse(&w).unwrap(), honest(PRICE));
        let mut bad = w.clone();
        bad.nav = Some("sixteen".into());
        assert!(NavAttestation::parse(&bad).is_err());
        let v2 = NavAttestationWire { nav: None, ..w };
        assert_eq!(NavAttestation::parse(&v2).unwrap(), honest_v2(PRICE));
    }
}
