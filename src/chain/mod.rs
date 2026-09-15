// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Talking to an EVM node. [`rpc`] is the JSON-RPC client and the signing
//! wallet that lands transactions, [`tx`] is the EIP-1559 encoding underneath
//! it, [`multicall`] batches read-only calls so a polling loop costs one round
//! trip instead of one per view, and [`approve`] is the one-time Permit2
//! allowance every quoted token needs before a filler can execute an order.

pub mod approve;
#[cfg(test)]
pub(crate) mod mock_node;
pub mod multicall;
pub mod rpc;
pub mod tx;
pub mod withdraw;
