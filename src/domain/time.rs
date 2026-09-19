//! UTC RFC3339 timestamp helpers.

use chrono::{DateTime, SecondsFormat, Utc};

/// Current time as a UTC RFC3339 string with millisecond precision.
///
/// Example: `2026-01-19T12:34:56.789Z`
pub fn now_utc_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Check whether `value` is a valid RFC3339 timestamp at UTC offset.
pub fn is_valid_utc_rfc3339(value: &str) -> bool {
    match DateTime::parse_from_rfc3339(value) {
        Ok(stamp) => stamp.offset().local_minus_utc() == 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_valid_utc_rfc3339() {
        let stamp = now_utc_rfc3339();
        assert!(stamp.ends_with('Z'), "expected Z suffix, got {stamp}");
        assert!(is_valid_utc_rfc3339(&stamp));
    }

    #[test]
    fn timestamps_advance() {
        let a = now_utc_rfc3339();
        let b = now_utc_rfc3339();
        assert!(a <= b);
    }

    #[test]
    fn validator_accepts_utc_forms() {
        assert!(is_valid_utc_rfc3339("2026-01-19T12:34:56.789Z"));
        assert!(is_valid_utc_rfc3339("2026-01-19T12:34:56Z"));
        assert!(is_valid_utc_rfc3339("2026-01-19T12:34:56+00:00"));
    }

    #[test]
    fn validator_rejects_non_utc_or_malformed_values() {
        assert!(!is_valid_utc_rfc3339("2026-01-19T12:34:56+02:00")); // non-UTC offset
        assert!(!is_valid_utc_rfc3339("2026-01-19T12:34:56")); // no offset
        assert!(!is_valid_utc_rfc3339("2026-01-19T12:34:56")); // no offset
        assert!(!is_valid_utc_rfc3339("2026-13-40T12:34:56Z")); // invalid date
        assert!(!is_valid_utc_rfc3339(""));
        assert!(!is_valid_utc_rfc3339("yesterday"));
    }
}
