// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Blue-leg auction closer: the bot closes settlement positions via the
//! existing FIFO `pool.fill()` to earn the closer fee.
//!
//! This module holds the pure, verifiable core: a Rust port of the protocol's
//! `FeeLogic` ([`feemath`], cross-checked against the canonical Solidity + its
//! TS mirror) and the close strategy ([`strategy`]). Subgraph discovery and the
//! on-chain `fill()` submission (the I/O) wire on top.

pub mod discover;
pub mod executor;
pub mod feemath;
pub mod runner;
pub mod strategy;
