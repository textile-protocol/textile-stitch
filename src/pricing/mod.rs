// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Where a quote comes from, before anything is signed.
//!
//! [`feed`] is the price source, [`twap`] smooths it, [`quote`] turns the mid
//! into a two-sided bid/ask with the operator's spread, [`lean`] tilts those
//! spreads against live inventory so the book self-rebalances, [`curve`] is
//! the opt-in cNGN quote curve for stitch pricing (off unless a corridor asks
//! for it), and [`tick`]
//! decides when the feed is too stale to quote and when a move is worth
//! re-signing. [`jump`] holds quoting when a print jumps away from the last
//! one trusted, and [`band`] is the last check on a firm order before it is
//! signed. Pure math over plain numbers — no chain, no network — except
//! [`cache`], which shares live references, and [`anchor`], which keeps the jump guard's trusted print on disk.

pub mod anchor;
pub mod band;
pub mod cache;
pub mod curve;
pub mod feed;
pub mod jump;
pub mod lean;
pub mod quote;
pub mod tick;
pub mod twap;
