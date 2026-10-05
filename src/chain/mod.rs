// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Talking to an EVM node. [`rpc`] is the JSON-RPC client and the signing
//! wallet that lands transactions, [`tx`] is the EIP-1559 encoding underneath
//! it, [`gas_caps`] bounds what a send may bid whatever the node suggests,
//! [`gas_reserve`] keeps gas back when a corridor trades the gas coin itself,
//! [`multicall`] batches read-only calls so a polling loop costs one round
//! trip instead of one per view, and [`approve`] is the one-time Permit2
//! allowance every quoted token needs before a filler can execute an order.

pub mod approve;
pub mod gas_caps;
pub mod gas_reserve;
#[cfg(test)]
pub(crate) mod mock_node;
pub mod multicall;
pub mod rpc;
pub mod tx;
pub mod withdraw;
