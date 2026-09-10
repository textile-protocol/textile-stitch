// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The public order book: the leftover ladder the bot quotes into (the "green
//! leg", `book_enabled`) and the resting limit orders it fills.
//!
//! [`maker`] runs one side end to end — requote gate, [`ladder`] sizing
//! against the [`funding`] budget, slot-keyed drafting through [`slots`], then
//! [`poster`] to sign and submit. [`taker`] is the other direction: filling
//! users' resting limit orders once their price reaches the operator's own
//! quote. Firm RFQ quotes are a separate path — see [`crate::rfq`].

pub mod funding;
pub mod ladder;
pub mod maker;
pub mod poster;
pub mod slots;
pub mod taker;
