// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! An automatic record of every change to a bot's quoting parameters.
//!
//! Spreads, size caps, the price guard, the feed and its freshness windows,
//! the RFQ / ladder / modules switches and every module setting: see
//! [`params`] for the exact list and how each is read. A change is recorded
//! with the parameter, its value before and after, when it happened (UTC),
//! who did it and through which path.
//!
//! Three ways a config changes, three ways it gets recorded:
//!
//! - **The panel** writes through [`journal::write_config_recorded`], which
//!   records the save under the signed-in operator.
//! - **Anything else** (an editor on the host, `stitch connect`, a restored
//!   backup) is caught by diffing the file against the last recorded
//!   snapshot. The bot does that every time it starts, which is the only
//!   time a config takes effect, and the panel does it before each save.
//! - **A bot seen for the first time** gets a baseline: every value as found.
//!
//! Records stay on the host in `change-log.jsonl` and, when the
//! operator opts in with `[change_log] ship = true`, the bot ships them to
//! Textile ([`ship`]). Whether a change was inside the client's agreed
//! envelope is decided there, not here: the envelope and the approvals
//! register live with Textile, and a third-party Stitch has neither.

pub mod files;
pub mod journal;
pub mod params;
pub mod record;
pub mod ship;

pub use journal::{record_startup, write_config_recorded, Recorded};
pub use record::{ChangeRecord, Origin, Source, HOST_EDIT_ACTOR};
