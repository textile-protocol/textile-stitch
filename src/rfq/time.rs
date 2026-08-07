// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Minimal ISO-8601 (UTC/Zulu) ↔ unix-millisecond conversion for the RFQ wire.
//!
//! The venue timestamps every frame with `Date#toISOString()`-style strings
//! (`2026-08-05T10:00:00.000Z`). That's the only shape we need, so this is a
//! hand-rolled converter (Howard Hinnant's civil-days algorithm) instead of a
//! chrono dependency — the crate's date needs start and end here.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in milliseconds; 0 if the clock is before the epoch.
pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from((m + 9) % 12); // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01 (inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff…]Z` to unix milliseconds. Fractional digits
/// beyond milliseconds are truncated. Anything else — offsets, missing `Z`,
/// dates before the epoch — is `None`; the caller treats an unreadable venue
/// timestamp as an unquotable request, never a guess.
pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;

    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let m: u32 = date_parts.next()?.parse().ok()?;
    let d: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }

    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (time, ""),
    };
    let mut time_parts = hms.split(':');
    let h: u64 = time_parts.next()?.parse().ok()?;
    let min: u64 = time_parts.next()?.parse().ok()?;
    let sec: u64 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Milliseconds: first three fraction digits, right-padded with zeros.
    let mut ms: u64 = 0;
    if !frac.is_empty() {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut padded = frac.to_string();
        padded.truncate(3);
        while padded.len() < 3 {
            padded.push('0');
        }
        ms = padded.parse().ok()?;
    }

    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86_400 + h * 3_600 + min * 60 + sec) * 1_000 + ms)
}

/// Format unix milliseconds as `YYYY-MM-DDTHH:MM:SS.fffZ` — the venue's own
/// shape, millisecond precision always present.
pub fn format_iso_ms(ms: u64) -> String {
    let secs = ms / 1_000;
    let ms = ms % 1_000;
    let (y, mo, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_the_golden_session_timestamp() {
        // 1754388000000 ms is the maker-session golden vector's issuedAt.
        assert_eq!(
            parse_iso_ms("2025-08-05T10:00:00.000Z"),
            Some(1_754_388_000_000)
        );
        assert_eq!(format_iso_ms(1_754_388_000_000), "2025-08-05T10:00:00.000Z");
    }

    #[test]
    fn fractions_are_optional_truncated_and_padded() {
        assert_eq!(
            parse_iso_ms("2025-08-05T10:00:00Z"),
            Some(1_754_388_000_000)
        );
        assert_eq!(
            parse_iso_ms("2025-08-05T10:00:00.5Z"),
            Some(1_754_388_000_500)
        );
        // Microseconds truncate to milliseconds.
        assert_eq!(
            parse_iso_ms("2025-08-05T10:00:00.123456Z"),
            Some(1_754_388_000_123)
        );
    }

    #[test]
    fn round_trips_across_month_and_leap_boundaries() {
        for iso in [
            "1970-01-01T00:00:00.000Z",
            "2024-02-29T23:59:59.999Z", // leap day
            "2025-12-31T23:59:59.001Z",
            "2026-03-01T00:00:00.000Z",
        ] {
            let ms = parse_iso_ms(iso).unwrap();
            assert_eq!(format_iso_ms(ms), iso, "round trip for {iso}");
        }
    }

    #[test]
    fn rejects_shapes_the_venue_never_sends() {
        for bad in [
            "2025-08-05T10:00:00",       // no Z
            "2025-08-05 10:00:00Z",      // no T
            "2025-08-05T10:00:00+00:00", // offset form
            "2025-13-05T10:00:00Z",      // bad month
            "not a date",
            "",
        ] {
            assert_eq!(parse_iso_ms(bad), None, "{bad:?} must not parse");
        }
    }
}
