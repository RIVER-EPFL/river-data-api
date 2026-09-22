use super::{Backlog, Series, archive_line, backlog_for};
use chrono::{TimeZone, Utc};

#[test]
fn test_a_site_with_its_own_data_reads_back_to_it_and_no_further() {
    let floor = Utc.with_ymd_and_hms(2019, 6, 1, 0, 0, 0).unwrap();
    assert_eq!(
        backlog_for(Some(floor), true),
        Some(Backlog::Since(floor)),
        "a decade file is landed from the site's first reading"
    );
    assert_eq!(backlog_for(Some(floor), false), Some(Backlog::Since(floor)));
}

#[test]
fn test_a_site_with_no_data_of_its_own_takes_the_recent_file_only() {
    assert_eq!(backlog_for(None, true), None);
    assert_eq!(backlog_for(None, false), Some(Backlog::Everything));
}

/// Expected behaviour: the line names the archive by its own file name, not by the URL it was
/// fetched from, and carries the three counts the read produced.
#[test]
fn test_an_archive_line_names_the_file_and_what_came_out_of_it() {
    let series = Series {
        points: vec![
            super::Point {
                time: Utc.with_ymd_and_hms(2019, 6, 1, 0, 0, 0).unwrap(),
                value: 1.0,
            },
            super::Point {
                time: Utc.with_ymd_and_hms(2019, 6, 1, 0, 10, 0).unwrap(),
                value: 2.0,
            },
        ],
        blank: 1,
        unreadable: 0,
    };
    assert_eq!(
        archive_line(
            "https://data.geo.admin.ch/ch.meteoschweiz/ogd-smn/tae/ogd-smn_tae_t_historical_2010-2019.csv",
            &series
        ),
        "Read ogd-smn_tae_t_historical_2010-2019.csv: 2 rows, 1 blank, 0 unreadable"
    );
}
