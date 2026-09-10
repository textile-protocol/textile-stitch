// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Where a quote comes from, before anything is signed.
//!
//! [`feed`] is the price source, [`twap`] smooths it, [`quote`] turns the mid
//! into a two-sided bid/ask with the operator's spread, [`lean`] tilts those
//! spreads against live inventory so the book self-rebalances, and [`tick`]
//! decides when the feed is too stale to quote and when a move is worth
//! re-signing. Pure math over plain numbers — no chain, no network.

pub mod feed;
pub mod lean;
pub mod quote;
pub mod tick;
pub mod twap;
