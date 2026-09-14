// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! `stitch withdraw`: move tokens out of the bot's wallet.
//!
//! The panel's Funds tab calls this the way it calls `approve` — as a one-shot
//! run of the bot binary against the bot's own config — so the private key is
//! read by the same code, in the same place, that reads it to quote. The panel
//! never signs anything itself.
//!
//! Two shapes: an ERC-20 `transfer` for a corridor token, or a plain value
//! transfer for the chain's gas coin (`native`). Amounts are the operator's
//! decimal, converted with the decimals the config already carries for that
//! token, or `all`. `all` on the gas coin keeps back what the transfer itself
//! bids for gas (the wallet's own fee plan, one bump of headroom), or the node
//! would refuse it for lack of gas and the operator would read that as
//! "withdraw is broken".
use std::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::{bail, Context};
use tracing::info;

use crate::chain::rpc::Wallet;
use crate::closer::executor::{encode_balance_of, encode_transfer};
use crate::config::Config;

/// What the operator asked to move: resolved token, atomic amount, target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawPlan {
    pub token: Option<Address>,
    pub symbol: String,
    pub amount: U256,
    pub to: Address,
}

/// The decimals of a corridor token the config knows, by address. `native`
/// is not in here: it is a value transfer, not a token. The config carries no
/// symbols (the panel looks those up in the catalog), so the plan names the
/// token by address and the panel's screen puts the ticker on it.
fn known_token_decimals(cfg: &Config, wanted: Address) -> Option<u8> {
    cfg.pools.iter().find_map(|p| {
        let collateral: Address = p.collateral.parse().ok()?;
        let debt: Address = p.debt.parse().ok()?;
        if collateral == wanted {
            Some(p.collateral_decimals)
        } else if debt == wanted {
            Some(p.debt_decimals)
        } else {
            None
        }
    })
}

/// A human decimal amount ("12.5") in atomic units for `decimals`, or `None`
/// when it isn't one. Whole-number and fractional parts only, no exponent,
/// no sign: the same strictness the venue applies to amounts.
pub fn parse_decimal_amount(text: &str, decimals: u8) -> Option<U256> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('-') || text.starts_with('+') {
        return None;
    }
    let (whole, frac) = match text.split_once('.') {
        Some((w, f)) => (w, f),
        None => (text, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if frac.len() > usize::from(decimals) {
        // More precision than the token has is a typo, not a rounding job.
        return None;
    }
    let whole_units: U256 = if whole.is_empty() {
        U256::ZERO
    } else {
        whole.parse().ok()?
    };
    let mut padded = frac.to_string();
    while padded.len() < usize::from(decimals) {
        padded.push('0');
    }
    let frac_units: U256 = if padded.is_empty() {
        U256::ZERO
    } else {
        padded.parse().ok()?
    };
    let scale = U256::from(10u64).pow(U256::from(decimals));
    whole_units
        .checked_mul(scale)
        .and_then(|w| w.checked_add(frac_units))
}

/// Resolve what to send. Reads the wallet for `all`; otherwise no chain call.
pub async fn plan_withdraw(
    cfg: &Config,
    wallet: &Wallet,
    token: &str,
    amount: &str,
    to: &str,
) -> anyhow::Result<WithdrawPlan> {
    let to: Address = to
        .trim()
        .parse()
        .with_context(|| format!("--to {to:?} is not an address"))?;
    if to == Address::ZERO {
        bail!("refusing to send to the zero address");
    }
    if to == wallet.address() {
        bail!("that is the bot's own wallet; nothing to move");
    }

    let native = token.trim().eq_ignore_ascii_case("native");
    let (token_addr, symbol, decimals) = if native {
        (None, "native".to_string(), 18u8)
    } else {
        let addr: Address = token
            .trim()
            .parse()
            .with_context(|| format!("--token {token:?} is not an address (or `native`)"))?;
        let decimals = known_token_decimals(cfg, addr).ok_or_else(|| {
            anyhow::anyhow!(
                "{addr} is not a token this bot's corridors trade; withdraw only moves those \
                 (and `native` for gas)"
            )
        })?;
        (Some(addr), addr.to_string(), decimals)
    };

    let amount = if amount.trim().eq_ignore_ascii_case("all") {
        match token_addr {
            Some(t) => {
                let data = Bytes::from(encode_balance_of(wallet.address()));
                wallet
                    .read_uint(t, &data)
                    .await
                    .with_context(|| format!("reading the {symbol} balance"))?
            }
            None => {
                let (balance, reserve) = tokio::try_join!(
                    wallet.rpc().get_balance(wallet.address()),
                    wallet.native_transfer_reserve(to),
                )
                .context("reading the gas balance and the transfer's cost")?;
                balance.saturating_sub(reserve)
            }
        }
    } else {
        parse_decimal_amount(amount, decimals).ok_or_else(|| {
            anyhow::anyhow!(
                "--amount {amount:?} is not a plain decimal with at most {decimals} decimal \
                 places, or `all`"
            )
        })?
    };
    if amount.is_zero() {
        bail!("nothing to withdraw: the amount is zero");
    }

    Ok(WithdrawPlan {
        token: token_addr,
        symbol,
        amount,
        to,
    })
}

/// Send it. Returns the transaction hash as the RPC reports it.
pub async fn send_withdraw(wallet: &Wallet, plan: &WithdrawPlan) -> anyhow::Result<String> {
    info!(
        from = %wallet.address(), to = %plan.to, token = %plan.symbol,
        amount_atomic = %plan.amount, "withdrawing"
    );
    let receipt = match plan.token {
        Some(token) => wallet
            .send_and_wait(
                token,
                Bytes::from(encode_transfer(plan.to, plan.amount)),
                U256::ZERO,
                Duration::from_secs(120),
            )
            .await
            .with_context(|| format!("sending {} to {}", plan.symbol, plan.to))?,
        None => wallet
            .send_and_wait(plan.to, Bytes::new(), plan.amount, Duration::from_secs(120))
            .await
            .with_context(|| format!("sending the gas coin to {}", plan.to))?,
    };
    // `send_and_wait` already bails on a reverted receipt; what comes back landed.
    let hash = receipt
        .get("transactionHash")
        .and_then(|h| h.as_str())
        .unwrap_or("")
        .to_string();
    info!(%hash, "withdraw landed");
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_amounts_scale_by_the_tokens_decimals() {
        assert_eq!(parse_decimal_amount("1", 6), Some(U256::from(1_000_000u64)));
        assert_eq!(
            parse_decimal_amount("12.5", 6),
            Some(U256::from(12_500_000u64))
        );
        assert_eq!(parse_decimal_amount("0.000001", 6), Some(U256::from(1u64)));
        assert_eq!(parse_decimal_amount(".5", 2), Some(U256::from(50u64)));
        assert_eq!(parse_decimal_amount("7.", 0), Some(U256::from(7u64)));
    }

    #[test]
    fn amounts_the_token_cannot_represent_are_refused() {
        // One decimal place too many is a typo, not a rounding job.
        assert_eq!(parse_decimal_amount("0.0000001", 6), None);
        assert_eq!(parse_decimal_amount("-1", 6), None);
        assert_eq!(parse_decimal_amount("1e6", 6), None);
        assert_eq!(parse_decimal_amount("", 6), None);
        assert_eq!(parse_decimal_amount(".", 6), None);
        assert_eq!(parse_decimal_amount("1,000", 6), None);
    }
}
