//! Parsing for the MeteoSwiss Open Government Data SMN CSV.
//!
//! The published files are semicolon-separated with a header row naming every variable the station
//! reports, one row per ten-minute interval. `reference_timestamp` is `DD.MM.YYYY HH:MM` in UTC,
//! and a variable the station did not report at that interval is an empty cell.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

/// One parsed interval: the instant and the value of the requested variable.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub time: DateTime<Utc>,
    pub value: f64,
}

/// What one file yielded. `blank` and `unreadable` are counted rather than raised so a station that
/// stops reporting one variable does not stop the sync for the rest.
#[derive(Debug, Default, PartialEq)]
pub struct Series {
    pub points: Vec<Point>,
    /// Rows whose variable cell was empty.
    pub blank: usize,
    /// Rows whose timestamp or value could not be read.
    pub unreadable: usize,
}

const TIMESTAMP_COLUMN: &str = "reference_timestamp";
const TIMESTAMP_FORMAT: &str = "%d.%m.%Y %H:%M";

/// Read one variable out of an SMN CSV. Errors only when the file cannot name the columns asked
/// for, which is a changed publication format rather than a gap in the data.
pub fn series(csv: &str, variable: &str) -> Result<Series, String> {
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("file is empty")?;
    let header = header.strip_prefix('\u{feff}').unwrap_or(header);

    let columns: Vec<&str> = header.split(';').map(str::trim).collect();
    let time_at = column(&columns, TIMESTAMP_COLUMN)?;
    let value_at = column(&columns, variable)?;

    let mut series = Series::default();
    for line in lines {
        let cells: Vec<&str> = line.split(';').map(str::trim).collect();
        let (Some(raw_time), Some(raw_value)) = (cells.get(time_at), cells.get(value_at)) else {
            series.unreadable += 1;
            continue;
        };
        if raw_value.is_empty() {
            series.blank += 1;
            continue;
        }
        let (Ok(naive), Ok(value)) = (
            NaiveDateTime::parse_from_str(raw_time, TIMESTAMP_FORMAT),
            raw_value.parse::<f64>(),
        ) else {
            series.unreadable += 1;
            continue;
        };
        if !value.is_finite() {
            series.unreadable += 1;
            continue;
        }
        series.points.push(Point {
            time: Utc.from_utc_datetime(&naive),
            value,
        });
    }
    Ok(series)
}

fn column(columns: &[&str], name: &str) -> Result<usize, String> {
    columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("column {name:?} is not in the header"))
}

/// The published path for one station's recent file, under the OGD collection base URL.
#[must_use]
pub fn recent_url(base: &str, station_abbr: &str) -> String {
    let station = station_abbr.trim().to_lowercase();
    format!(
        "{}/{station}/ogd-smn_{station}_t_recent.csv",
        base.trim_end_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::{Point, recent_url, series};
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
}
