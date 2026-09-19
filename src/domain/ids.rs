//! ULID helpers.
//!
//! Durable record IDs are ULIDs: 26-character Crockford Base32 strings.

use ulid::Ulid;

/// Generate a new random ULID in canonical string form.
pub fn new_ulid() -> String {
    Ulid::new().to_string()
}

/// Check whether `value` is a canonical ULID string.
pub fn is_valid_ulid(value: &str) -> bool {
    Ulid::from_string(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ulid_is_canonical() {
        let id = new_ulid();
        assert_eq!(id.len(), 26);
        assert!(is_valid_ulid(&id));
    }

    #[test]
    fn new_ulids_are_distinct() {
        let a = new_ulid();
        let b = new_ulid();
        assert_ne!(a, b);
    }

    #[test]
    fn ulids_encode_time_lexicographically() {
        // ULIDs sort lexicographically by their millisecond time component;
        // randomness only breaks ties within the same millisecond, so we
        // compare ULIDs built from explicit datetimes.
        use chrono::{TimeZone, Utc};

        let earlier = Utc.with_ymd_and_hms(2026, 1, 19, 12, 0, 0).unwrap();
        let later = earlier + chrono::Duration::milliseconds(5);
        let a = ulid::Ulid::from_datetime(earlier.into()).to_string();
        let b = ulid::Ulid::from_datetime(later.into()).to_string();
        assert!(a < b);
    }

    #[test]
    fn is_valid_ulid_rejects_malformed_values() {
        assert!(is_valid_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(!is_valid_ulid(""));
        assert!(!is_valid_ulid("01ARZ3NDEKTSV4RRFFQ69G5FA")); // too short
        assert!(!is_valid_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAVA")); // too long
        assert!(!is_valid_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAU")); // non-crockford letter
        assert!(!is_valid_ulid("not a ulid"));
    }
}
