//! UTC time helpers.
//!
//! Mirrors `src/dbs/core/timeutil.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). All timestamps in the database are TEXT ISO-8601
//! in UTC with a trailing `Z`, so lexicographic string comparison of
//! timestamps is also chronological. `chrono::DateTime<Utc>` is already
//! always UTC and always timezone-aware in the type system, so this module
//! is smaller than the reference — it doesn't need the reference's
//! naive-vs-aware branch on the *output* side, only on parsing untrusted
//! input text.

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};

/// Formats `dt` as canonical ISO-8601 UTC with a `Z` suffix (fractional
/// seconds included only when non-zero, matching Python's
/// `datetime.isoformat()`).
pub fn iso_z(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Parses an ISO-8601 timestamp (`Z`, an explicit offset, or no offset at
/// all) into an aware UTC datetime. A `None` value, an empty/
/// whitespace-only string, or unparseable text all return `None`. A
/// naive (no-offset) string is treated as already being UTC, matching the
/// reference's behavior for a timezone-less input.
pub fn parse_iso(value: Option<&str>) -> Option<DateTime<Utc>> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(dt.with_timezone(&Utc));
    }
    // No offset/Z present in the input — treat as naive UTC.
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, fmt) {
            return Some(naive.and_utc());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    #[test]
    fn iso_z_formats_with_z_suffix_no_fractional_when_zero() {
        let dt = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(iso_z(dt), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn iso_z_includes_fractional_seconds_when_present() {
        let dt = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .unwrap()
            .with_nanosecond(500_000_000)
            .unwrap();
        assert!(iso_z(dt).ends_with("Z"));
        assert!(iso_z(dt).contains('.'));
    }

    #[test]
    fn parse_iso_none_and_empty_return_none() {
        assert!(parse_iso(None).is_none());
        assert!(parse_iso(Some("")).is_none());
        assert!(parse_iso(Some("   ")).is_none());
    }

    #[test]
    fn parse_iso_handles_z_suffix() {
        let parsed = parse_iso(Some("2024-01-01T00:00:00Z")).unwrap();
        assert_eq!(parsed, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn parse_iso_converts_explicit_offset_to_utc() {
        let parsed = parse_iso(Some("2024-01-01T05:00:00+05:00")).unwrap();
        assert_eq!(parsed, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn parse_iso_treats_naive_input_as_utc() {
        let parsed = parse_iso(Some("2024-01-01T00:00:00")).unwrap();
        assert_eq!(parsed, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn parse_iso_rejects_garbage() {
        assert!(parse_iso(Some("not a timestamp")).is_none());
    }

    #[test]
    fn round_trips_through_iso_z_and_parse_iso() {
        let dt = Utc.with_ymd_and_hms(2026, 8, 12, 16, 30, 45).unwrap();
        let text = iso_z(dt);
        assert_eq!(parse_iso(Some(&text)).unwrap(), dt);
    }
}
