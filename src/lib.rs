// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch — the Textile filler-network operator bot.
//!
//! New bots quote Swap via RFQ: they answer venue quote requests with firm,
//! taker-bound Permit2 orders priced off the operator's feed. The leftover
//! public ladder (`book_enabled`) and the limit-order taker ([`book::taker`]) are
//! separate switches. The bot also closes settlement auctions.
//!
//! This is a standalone crate, deliberately not derived from the TypeScript
//! reference closer. Signing is UniswapX `LimitOrder` EIP-712 + Permit2
//! witness ([`protocol::eip712`], [`signer`]) with the pricing rule in
//! [`pricing::quote`].
//!
//! # Layout
//!
//! - [`config`] — the operator's TOML, and [`app`] the CLI, banner and self-update
//! - [`pricing`] — feed, TWAP, spread, inventory lean: what a quote should be
//! - [`protocol`] — order types and the EIP-712 / vault encodings, from the contracts
//! - [`chain`] — JSON-RPC, transaction signing, Permit2 approvals
//! - [`rfq`] — firm quotes over the venue's maker stream (the main path today)
//! - [`book`] — the public ladder and the limit-order taker; [`closer`] the auctions
//! - [`venue`] — the Textile indexer and maker enrollment; [`signer`] the key backends
//! - [`setup`] — the interactive installer; `panel` the local web UI (feature-gated)

pub mod app;
pub mod book;
pub mod chain;
pub mod closer;
pub mod config;
pub mod net;
#[cfg(feature = "panel")]
pub mod panel;
pub mod pricing;
pub mod protocol;
pub mod rfq;
pub mod setup;
pub mod signer;
pub mod time;
pub mod venue;
