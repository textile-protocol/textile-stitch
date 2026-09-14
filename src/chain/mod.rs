// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Talking to an EVM node. [`rpc`] is the JSON-RPC client and the signing
//! wallet that lands transactions, [`tx`] is the EIP-1559 encoding underneath
//! it, and [`approve`] is the one-time Permit2 allowance every quoted token
//! needs before a filler can execute an order.

pub mod approve;
pub mod rpc;
pub mod tx;
pub mod withdraw;
