// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch — the Textile filler-network operator bot.
//!
//! The bot is the buy side of the Settlement v2/v3 market: it keeps live,
//! funded limit orders to buy collateral (e.g. cNGN) with debt (e.g. USDT),
//! priced off the operator's own feed, and also closes settlement auctions.
//!
//! This is a standalone crate, deliberately not derived from the TypeScript
//! reference closer. v1 lands the security-critical core first — UniswapX
//! `LimitOrder` EIP-712 + Permit2 witness signing ([`eip712`], [`signer`]) and
//! the pricing rule ([`quote`]) — then the feed, indexer client, and auction
//! closer wire on top.

pub mod banner;
pub mod cli;
pub mod closer;
pub mod config;
pub mod eip712;
pub mod feed;
pub mod indexer;
pub mod ladder;
pub mod net;
pub mod quote;
pub mod rpc;
pub mod signer;
pub mod submit;
pub mod tick;
pub mod tx;
pub mod types;
pub mod update;
