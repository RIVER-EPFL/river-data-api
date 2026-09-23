use chrono::{Duration, TimeZone, Utc};

use super::reject_inverted_window;

#[test]
fn test_an_open_or_forward_window_is_accepted() {
    let from = Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap();
    assert!(reject_inverted_window(from, None).is_ok());
    assert!(reject_inverted_window(from, Some(from + Duration::hours(1))).is_ok());
}

#[test]
fn test_a_window_ending_where_it_starts_is_accepted() {
    let from = Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap();
    assert!(reject_inverted_window(from, Some(from)).is_ok());
}

#[test]
fn test_a_window_ending_before_it_starts_is_refused() {
    let from = Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap();
    assert!(reject_inverted_window(from, Some(from - Duration::seconds(1))).is_err());
}
