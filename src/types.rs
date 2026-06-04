// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shared types for the operator bot.

pub use alloy_primitives::{Address, B256, U256};

/// One operator limit order: pay `input_amount` debt (USDT) to buy
/// `output_amount` collateral (cNGN). For a limit order (no Dutch decay) the
/// permitted max equals `input_amount`. `swapper` and `recipient` are the
/// operator's own wallet — the bought cNGN goes back to the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderParams {
    /// The LimitOrderReactor this order targets.
    pub reactor: Address,
    /// The operator wallet (UniswapX swapper).
    pub swapper: Address,
    /// Permit2 nonce (replay protection / cancellation).
    pub nonce: U256,
    /// Unix-seconds order expiry.
    pub deadline: U256,
    /// Debt asset the operator pays in (e.g. USDT).
    pub input_token: Address,
    /// Debt atomic units the operator pays (== permitted max for a limit order).
    pub input_amount: U256,
    /// Collateral asset the operator buys (e.g. cNGN).
    pub output_token: Address,
    /// Collateral atomic units the operator receives.
    pub output_amount: U256,
    /// Where the bought collateral lands (the operator's wallet).
    pub recipient: Address,
}
