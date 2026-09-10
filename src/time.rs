// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Wall-clock helpers. Kept in one place so no module reaches for
//! `SystemTime` with its own error handling, and so a test can reason about
//! "now" from a single source.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in seconds. A clock before 1970 reads as 0, which fails
/// every freshness check closed rather than panicking.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current unix time in milliseconds. Same clamp as [`unix_now`].
pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
