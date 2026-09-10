use super::{recent_url, series};
use crate::routes::private::meteoswiss::models::Point;
use chrono::{TimeZone, Utc};

const HEADER: &str = "station_abbr;reference_timestamp;tre200s0;prestas0";

#[test]
fn test_series_reads_the_named_variable() {
    let csv = format!("{HEADER}\nMOB;01.01.2026 00:00;-8.1;922\nMOB;01.01.2026 00:10;-8.2;921.7");
    let parsed = series(&csv, "prestas0").unwrap();
    assert_eq!(
        parsed.points,
        vec![
            Point {
                time: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                value: 922.0,
            },
            Point {
                time: Utc.with_ymd_and_hms(2026, 1, 1, 0, 10, 0).unwrap(),
                value: 921.7,
            },
        ]
    );
    assert_eq!((parsed.blank, parsed.unreadable), (0, 0));
}

#[test]
fn test_series_counts_blank_cells_without_dropping_the_rest() {
    let csv = format!("{HEADER}\nMOB;01.01.2026 00:00;-8.1;\nMOB;01.01.2026 00:10;-8.2;921.7");
    let parsed = series(&csv, "prestas0").unwrap();
    assert_eq!(parsed.points.len(), 1);
    assert_eq!(parsed.blank, 1);
}

#[test]
fn test_series_counts_an_unreadable_row_rather_than_failing() {
    let csv = format!(
        "{HEADER}\nMOB;not a date;-8.1;922\nMOB;01.01.2026 00:10;-8.2;not a number\nMOB;01.01.2026 00:20;-8.2;921.7"
    );
    let parsed = series(&csv, "prestas0").unwrap();
    assert_eq!(parsed.points.len(), 1);
    assert_eq!(parsed.unreadable, 2);
}

#[test]
fn test_series_counts_a_short_row() {
    let csv = format!("{HEADER}\nMOB;01.01.2026 00:00");
    let parsed = series(&csv, "prestas0").unwrap();
    assert!(parsed.points.is_empty());
    assert_eq!(parsed.unreadable, 1);
}

#[test]
fn test_series_tolerates_a_byte_order_mark_and_crlf() {
    let csv = format!("\u{feff}{HEADER}\r\nMOB;01.01.2026 00:00;-8.1;922\r\n");
    let parsed = series(&csv, "prestas0").unwrap();
    assert_eq!(parsed.points.len(), 1);
}

#[test]
fn test_series_rejects_a_file_missing_the_variable() {
    let csv = format!("{HEADER}\nMOB;01.01.2026 00:00;-8.1;922");
    assert!(series(&csv, "pp0qnhs0").is_err());
}

#[test]
fn test_series_rejects_an_empty_file() {
    assert!(series("", "prestas0").is_err());
    assert!(series("   \n\n", "prestas0").is_err());
}

#[test]
fn test_series_of_a_header_alone_is_empty() {
    let parsed = series(HEADER, "prestas0").unwrap();
    assert_eq!(parsed, super::Series::default());
}

#[test]
fn test_recent_url_lowercases_the_station() {
    assert_eq!(
        recent_url("https://data.geo.admin.ch/ch.meteoschweiz.ogd-smn/", "MOB"),
        "https://data.geo.admin.ch/ch.meteoschweiz.ogd-smn/mob/ogd-smn_mob_t_recent.csv"
    );
}
