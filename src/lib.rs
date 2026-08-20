// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch — the Textile filler-network operator bot.
//!
//! New bots quote Swap via RFQ: they answer venue quote requests with firm,
//! taker-bound Permit2 orders priced off the operator's feed. The leftover
//! public ladder (`book_enabled`) and the limit-order taker ([`taker`]) are
//! separate switches. The bot also closes settlement auctions.
//!
//! This is a standalone crate, deliberately not derived from the TypeScript
//! reference closer. Signing is UniswapX `LimitOrder` EIP-712 + Permit2
//! witness ([`eip712`], [`signer`]) with the pricing rule in [`quote`].

pub mod approve;
pub mod banner;
pub mod cli;
pub mod closer;
pub mod config;
pub mod eip712;
pub mod enroll;
pub mod feed;
pub mod funding;
pub mod indexer;
pub mod ladder;
pub mod lean;
pub mod maker;
pub mod net;
#[cfg(feature = "panel")]
pub mod panel;
pub mod poster;
pub mod quote;
pub mod rfq;
pub mod rpc;
pub mod setup;
pub mod signer;
pub mod slots;
pub mod submit;
pub mod taker;
pub mod tick;
pub mod twap;
pub mod tx;
pub mod types;
pub mod update;
