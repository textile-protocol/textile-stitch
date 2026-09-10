// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The program around the bot: what the operator types ([`cli`]), what they
//! see on the way up ([`banner`]), and how the binary replaces itself
//! ([`update`]). None of it knows anything about quoting — it is the shell the
//! trading modules run inside, and `stitch-desktop` reuses the same pieces.

pub mod banner;
pub mod cli;
pub mod update;
