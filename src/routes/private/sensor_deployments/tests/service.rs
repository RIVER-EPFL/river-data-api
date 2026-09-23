use chrono::{Duration, TimeZone, Utc};

use uuid::Uuid;

use super::{reject_inverted_window, vacated};

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

#[test]
fn test_vacated_an_edit_keeping_instrument_and_site_leaves_nothing() {
    let (sensor, site) = (Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(vacated((sensor, site), (sensor, site)), None);
}

#[test]
fn test_vacated_another_instrument_leaves_the_old_one_at_its_site() {
    let (first, second, site) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(vacated((first, site), (second, site)), Some((first, site)));
}

#[test]
fn test_vacated_another_site_leaves_the_instrument_at_the_old_site() {
    let (sensor, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(vacated((sensor, a), (sensor, b)), Some((sensor, a)));
}

#[test]
fn test_vacated_both_moved_leaves_the_old_pair() {
    let (x, y, a, b) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    assert_eq!(vacated((x, a), (y, b)), Some((x, a)));
}
