use super::{
    ATTRIBUTION, VARIABLES, attributions, distance_km, insert_chunk, latest, latin1, rank_stations,
    recent_url, require_declared, series, stations, stations_url, variable,
};
use crate::routes::private::meteoswiss::models::{Point, station, subscription};
use chrono::{TimeZone, Utc};
use uuid::Uuid;

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

/// Scenario: the SMN feed re-publishes an instant it already sent.
/// Expected behaviour: the insert names every column the feed knows, tags the rows continuous, and
/// does nothing on conflict, because a replayed instant is a duplicate and not a correction.
#[test]
fn test_the_insert_tags_the_rows_continuous_and_does_nothing_on_conflict() {
    use sea_orm::QueryTrait;
    let point = Point {
        time: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        value: 963.4,
    };
    let sql = insert_chunk(
        Uuid::nil(),
        Uuid::nil(),
        Uuid::nil(),
        Uuid::nil(),
        &[&point],
    )
    .into_query()
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        str::starts_with(
            &sql,
            r#"INSERT INTO "readings" ("stream_id", "time", "replicate_index", "site_id", "parameter_id", "raw_value", "sensor_id", "measurement_type")"#
        ),
        "{sql}"
    );
    assert!(str::contains(&sql, "'continuous'"), "{sql}");
    assert!(str::ends_with(&sql, "DO NOTHING"), "{sql}");
}

/// Scenario: a site is subscribed to a variable name somebody typed.
/// Expected behaviour: only a declared variable is accepted, whatever its case and spacing, and a
/// refusal names what is on offer.
#[test]
fn test_only_a_declared_variable_can_be_subscribed_to() {
    assert_eq!(
        variable("  PRESTAS0 ").map(|v| v.code),
        Some("barometric_pressure")
    );
    assert!(require_declared("prestas0").is_ok());

    let refusal =
        require_declared("tre200s0").expect_err("the feed publishes no such subscription");
    let message = format!("{refusal:?}");
    assert!(str::contains(&message, "tre200s0"), "{message}");
    for declared in VARIABLES {
        assert!(str::contains(&message, declared.name), "{message}");
    }
}

/// The published header, in the order MeteoSwiss ship it.
const META_HEADER: &str = "station_abbr;station_name;station_canton;station_wigos_id;station_type_de;station_type_fr;station_type_it;station_type_en;station_dataowner;station_data_since;station_height_masl;station_height_barometer_masl;station_coordinates_lv95_east;station_coordinates_lv95_north;station_coordinates_wgs84_lat;station_coordinates_wgs84_lon";

fn meta_row(abbr: &str, name: &str, since: &str, height: &str, barometer: &str) -> String {
    format!(
        "{abbr};{name};VS;0-20000-0-06735;a;b;c;d;MeteoSchweiz;{since};{height};{barometer};0;0;46.071019;7.225272"
    )
}

#[test]
fn test_stations_url_hangs_the_metadata_file_off_the_collection() {
    assert_eq!(
        stations_url("https://data.geo.admin.ch/ch.meteoschweiz.ogd-smn/"),
        "https://data.geo.admin.ch/ch.meteoschweiz.ogd-smn/ogd-smn_meta_stations.csv"
    );
}

#[test]
fn test_stations_reads_a_published_row() {
    let csv = format!(
        "{META_HEADER}\n{}",
        meta_row("mob", "Montagnier, Bagnes", "01.03.1906", "839.0", "840.0")
    );
    let rows = stations(&csv).unwrap();
    assert_eq!(rows.len(), 1);
    let station = &rows[0];
    // The abbreviation is the key a subscription names, so it is held in one case.
    assert_eq!(station.abbr, "MOB");
    assert_eq!(station.name, "Montagnier, Bagnes");
    assert_eq!(
        station.data_since,
        Some(chrono::NaiveDate::from_ymd_opt(1906, 3, 1).unwrap())
    );
    assert_eq!(station.height_masl, Some(839.0));
    assert_eq!(station.height_barometer_masl, Some(840.0));
    assert_eq!(station.latitude, Some(46.071019));
    assert_eq!(station.longitude, Some(7.225272));
}

#[test]
fn test_stations_keeps_a_station_with_no_barometer_height() {
    // 19 of the 158 published stations leave it blank; they are still stations a picker lists.
    let csv = format!(
        "{META_HEADER}\n{}",
        meta_row("AEG", "Oberaegeri", "01.02.1993", "724.0", "")
    );
    let rows = stations(&csv).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].height_masl, Some(724.0));
    assert_eq!(rows[0].height_barometer_masl, None);
}

#[test]
fn test_stations_skips_a_row_with_no_abbreviation() {
    let csv = format!(
        "{META_HEADER}\n{}\n{}",
        meta_row("", "Nowhere", "01.01.2000", "100.0", "100.0"),
        meta_row("SIO", "Sion", "01.01.1958", "482.0", "483.0")
    );
    let rows = stations(&csv).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].abbr, "SIO");
}

#[test]
fn test_stations_refuses_a_file_that_cannot_name_the_station() {
    let err = stations("station_name;station_canton\nSion;VS").unwrap_err();
    assert!(err.contains("station_abbr"), "{err}");
}

#[test]
fn test_latin1_reads_a_station_name_the_source_publishes() {
    // Oberägeri, as the published bytes carry it: 0xE4 is the character, not a broken UTF-8 byte.
    let bytes = b"Ober\xe4geri";
    assert_eq!(latin1(bytes), "Oberägeri");
    assert_eq!(String::from_utf8_lossy(bytes), "Ober\u{fffd}geri");
}

fn subscribed(parameter: Uuid, station: &str, enabled: bool) -> subscription::Model {
    subscription::Model {
        id: Uuid::new_v4(),
        site_id: Uuid::from_u128(1),
        station_abbr: station.to_string(),
        variable: "prestas0".to_string(),
        parameter_id: parameter,
        enabled,
        created_at: None,
    }
}

/// Expected behaviour: the feed attributes the parameters its subscriptions name, and nothing
/// else. Matching on the parameter's code instead would attribute a site's own pressure sensor to
/// MeteoSwiss, and a subscription switched off would keep attributing after the feed stopped.
#[test]
fn test_only_a_subscribed_parameter_is_attributed_to_the_feed() {
    let pressure = Uuid::from_u128(10);
    let own = Uuid::from_u128(11);
    let stopped = Uuid::from_u128(12);

    let sources = attributions(&[
        subscribed(pressure, " mob ", true),
        subscribed(stopped, "SIO", false),
    ]);

    let landed = sources.get(&pressure).expect("the subscribed parameter");
    assert_eq!(landed.system, "meteoswiss");
    assert_eq!(
        landed.station, "MOB",
        "the station is named as SMN spells it"
    );
    assert_eq!(landed.attribution, ATTRIBUTION);
    assert!(!sources.contains_key(&own), "a slot with no subscription");
    assert!(
        !sources.contains_key(&stopped),
        "a subscription switched off stops attributing"
    );
}

/// The all-stations latest-values file, as published: the station is `Station/Location`, the
/// instant `YYYYMMDDHHMM`, and a variable a station did not report is a dash.
const LATEST: &str = "Station/Location;Date;tre200s0;prestas0\n\
TAE;202609141510;23.70;961.60\n\
MOB;202609141510;24.30;926.40\n\
ABO;202609141510;17.30;-";

#[test]
fn test_latest_keys_the_newest_interval_by_station() {
    let reported = latest(LATEST, "prestas0").expect("the file names the variable");
    assert_eq!(
        reported.len(),
        2,
        "the dash is no measurement: {reported:?}"
    );
    let mob = reported.get("MOB").expect("MOB reported pressure");
    assert!((mob.value - 926.4).abs() < f64::EPSILON);
    assert_eq!(
        mob.time,
        Utc.with_ymd_and_hms(2026, 9, 14, 15, 10, 0).unwrap()
    );
    assert!(!reported.contains_key("ABO"));
}

#[test]
fn test_latest_rejects_a_file_missing_the_variable() {
    let refusal = latest(LATEST, "rre150z0").expect_err("the column is not in the header");
    assert!(str::contains(&refusal, "rre150z0"), "{refusal}");
}

/// Sion, Montagnier and Adelboden as the published list carries them.
const SIO: (f64, f64) = (46.218790, 7.330250);
const MOB: (f64, f64) = (46.071019, 7.225272);
const ABO: (f64, f64) = (46.491703, 7.560703);

fn listed(abbr: &str, name: &str, at: Option<(f64, f64)>) -> station::Model {
    station::Model {
        station_abbr: abbr.to_string(),
        name: name.to_string(),
        data_since: None,
        height_masl: Some(500.0),
        height_barometer_masl: Some(501.0),
        latitude: at.map(|p| p.0),
        longitude: at.map(|p| p.1),
        updated_at: chrono::Utc::now(),
    }
}

#[test]
fn test_distance_km_over_a_known_pair() {
    // Montagnier to Sion, 18.31 km great-circle.
    assert!(
        (distance_km(MOB, SIO) - 18.3139).abs() < 0.01,
        "{}",
        distance_km(MOB, SIO)
    );
    assert!(
        (distance_km(MOB, ABO) - 53.4102).abs() < 0.01,
        "{}",
        distance_km(MOB, ABO)
    );
    assert_eq!(distance_km(MOB, MOB), 0.0);
}

#[test]
fn test_rank_stations_offers_the_nearest_first() {
    let ranked = rank_stations(
        vec![
            listed("ABO", "Adelboden", Some(ABO)),
            listed("SIO", "Sion", Some(SIO)),
            listed("MOB", "Montagnier, Bagnes", Some(MOB)),
        ],
        Some(MOB),
    );
    let order: Vec<&str> = ranked.iter().map(|c| c.station_abbr.as_str()).collect();
    assert_eq!(order, vec!["MOB", "SIO", "ABO"]);
    assert_eq!(ranked[0].distance_km, Some(0.0));
}

#[test]
fn test_rank_stations_without_site_coordinates_lists_by_name() {
    // The coordinates are hand-entered, so a site without them gets the list, not an empty one.
    let ranked = rank_stations(
        vec![
            listed("SIO", "Sion", Some(SIO)),
            listed("ABO", "Adelboden", Some(ABO)),
        ],
        None,
    );
    let order: Vec<&str> = ranked.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(order, vec!["Adelboden", "Sion"]);
    assert!(ranked.iter().all(|c| c.distance_km.is_none()));
}

#[test]
fn test_rank_stations_puts_a_station_with_no_coordinates_last() {
    let ranked = rank_stations(
        vec![
            listed("XXX", "Anywhere", None),
            listed("ABO", "Adelboden", Some(ABO)),
        ],
        Some(MOB),
    );
    let order: Vec<&str> = ranked.iter().map(|c| c.station_abbr.as_str()).collect();
    assert_eq!(order, vec!["ABO", "XXX"]);
    assert_eq!(ranked[1].distance_km, None);
}

#[test]
fn test_a_candidate_carries_both_elevations() {
    let ranked = rank_stations(vec![listed("SIO", "Sion", Some(SIO))], None);
    assert_eq!(ranked[0].height_masl, Some(500.0));
    assert_eq!(ranked[0].height_barometer_masl, Some(501.0));
}
