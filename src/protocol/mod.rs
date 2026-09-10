// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The contract-shaped types and encodings, with no I/O in them.
//!
//! [`types`] is the operator order the rest of the crate passes around,
//! [`eip712`] hashes it into the UniswapX + Permit2 witness digest the
//! operator signs, and [`vault`] encodes the OperatorVault views and the
//! epoch-prefixed trading nonce. All three are ported byte-for-byte from the
//! vendored Solidity and `packages/constants`, so a drift on either side
//! shows up as a failing digest test rather than a rejected fill.

pub mod eip712;
pub mod types;
pub mod vault;
