//! Subscribing a site to a MeteoSwiss station and variable is the whole operator action: the slot,
//! the paired stream and the station instrument follow from it, and a re-published interval is a
//! duplicate rather than a correction.
//!
//! Run with: cargo test --test data_streams meteoswiss

use chrono::{DateTime, TimeZone, Utc};
use river_db::routes::private::meteoswiss::models::subscription::MeteoswissSubscriptionCreate;
use river_db::routes::private::meteoswiss::models::{Point, Subscriber};
use river_db::routes::private::meteoswiss::service::{
    MeteoswissSubscriptionOperations, advance_cursor, cursor, instrument, provision, subscribers,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const STATION: &str = "MOB";
const VARIABLE: &str = "prestas0";

/// The catalog row the migration seeds; the fixture cleanup truncates `parameters`, so a suite
/// that has already run once starts without it.
async fn ensure_pressure_parameter(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        "INSERT INTO parameters (code, name, default_units, category) \
         SELECT 'barometric_pressure', 'Barometric Pressure', 'hPa', 'measurement' \
          WHERE NOT EXISTS (SELECT 1 FROM parameters WHERE lower(code) = 'barometric_pressure')",
    )
    .await;
}

async fn subscribe(db: &DatabaseConnection, site_id: &str, station: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO meteoswiss_subscriptions (site_id, station_abbr, variable, parameter_id) \
             SELECT '{site_id}', '{station}', '{VARIABLE}', p.id FROM parameters p \
              WHERE lower(p.code) = 'barometric_pressure'"
        ),
    )
    .await;
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

fn points(start: DateTime<Utc>, values: &[f64]) -> Vec<Point> {
    values
        .iter()
        .enumerate()
        .map(|(i, value)| Point {
            time: start + chrono::Duration::minutes(10 * i as i64),
            value: *value,
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn meteoswiss_declaration_provisions_a_paired_pressure_stream() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    ensure_pressure_parameter(&db).await;
    subscribe(&db, crate::common::fixtures::SITE1_ID, "  mob  ").await;

    // The subscription is read back trimmed and upper-cased, so the URL and the instrument key are
    // the same whatever an operator typed.
    let declared = subscribers(&db).await.unwrap();
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].station, STATION);
    assert_eq!(declared[0].variable, VARIABLE);

    let parameter = declared[0].parameter_id;
    let stream_id = provision(&db, &declared[0], parameter).await.unwrap();

    let paired = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM data_streams s \
               JOIN site_parameters sp ON sp.id = s.site_parameter_id \
              WHERE s.id = '{stream_id}' AND s.source_system = 'meteoswiss' \
                AND s.measurement_type = 'continuous' AND s.paired_at IS NOT NULL \
                AND sp.site_id = '{}' AND sp.parameter_id = '{parameter}'",
            crate::common::fixtures::SITE1_ID
        ),
    )
    .await;
    assert_eq!(
        paired, 1,
        "the stream is paired to the site's pressure slot"
    );

    // Provisioning is what every tick runs, so it has to converge rather than accumulate.
    let again = provision(&db, &declared[0], parameter).await.unwrap();
    assert_eq!(again, stream_id);
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM site_parameters \
                  WHERE site_id = '{}' AND parameter_id = '{parameter}'",
                crate::common::fixtures::SITE1_ID
            ),
        )
        .await,
        1
    );
}

#[tokio::test]
#[serial]
async fn meteoswiss_readings_land_attributed_and_a_replay_inserts_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    ensure_pressure_parameter(&db).await;
    subscribe(&db, crate::common::fixtures::SITE1_ID, STATION).await;

    let parameter = subscribers(&db).await.unwrap()[0].parameter_id;
    let site = Subscriber {
        subscription_id: Uuid::new_v4(),
        site_id: Uuid::parse_str(crate::common::fixtures::SITE1_ID).unwrap(),
        site_name: "Site 1".to_string(),
        station: STATION.to_string(),
        variable: VARIABLE.to_string(),
        parameter_id: parameter,
    };
    let stream_id = provision(&db, &site, parameter).await.unwrap();
    let sensor_id = instrument(&db, STATION).await.unwrap();

    assert!(cursor(&db, stream_id).await.unwrap().is_none());

    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let series = points(start, &[922.0, 921.7, 921.4]);
    let refs: Vec<&Point> = series.iter().collect();
    let written = river_db::routes::private::meteoswiss::service::insert(
        &db,
        stream_id,
        site.site_id,
        parameter,
        sensor_id,
        &refs,
    )
    .await
    .unwrap();
    assert_eq!(written, 3);

    let attributed = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
              WHERE stream_id = '{stream_id}' AND site_id = '{}' AND parameter_id = '{parameter}' \
                AND sensor_id = '{sensor_id}' AND measurement_type = 'continuous' \
                AND replicate_index = 0",
            crate::common::fixtures::SITE1_ID
        ),
    )
    .await;
    assert_eq!(attributed, 3, "every reading carries its slot and station");

    // The SMN file re-publishes what it already held; the same instant is not a correction.
    let replayed = river_db::routes::private::meteoswiss::service::insert(
        &db,
        stream_id,
        site.site_id,
        parameter,
        sensor_id,
        &refs,
    )
    .await
    .unwrap();
    assert_eq!(replayed, 0);
    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT count(*) AS n FROM readings WHERE stream_id = '{stream_id}'"),
        )
        .await,
        3
    );

    // The cursor only ever moves forward, so a file that re-publishes an older tail cannot rewind it.
    let newest = series.last().unwrap().time;
    advance_cursor(&db, stream_id, newest).await.unwrap();
    assert_eq!(cursor(&db, stream_id).await.unwrap(), Some(newest));
    advance_cursor(&db, stream_id, start).await.unwrap();
    assert_eq!(cursor(&db, stream_id).await.unwrap(), Some(newest));

    // One station serves every site that named it.
    assert_eq!(instrument(&db, STATION).await.unwrap(), sensor_id);
}

#[tokio::test]
#[serial]
async fn a_site_with_no_enabled_subscription_is_not_a_subscriber() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    ensure_pressure_parameter(&db).await;
    assert!(subscribers(&db).await.unwrap().is_empty());
    subscribe(&db, crate::common::fixtures::SITE1_ID, STATION).await;
    crate::common::exec(&db, "UPDATE meteoswiss_subscriptions SET enabled = false").await;
    assert!(
        subscribers(&db).await.unwrap().is_empty(),
        "a subscription switched off is not fetched for"
    );
}

/// Scenario: instruments registered by serial carry no provenance, and two MeteoSwiss stations
/// are declared.
/// Expected behaviour: the station registration is keyed on `(source_system, source_key)`, which
/// is unique only where both are set, so the rows with neither are not the same row as each other
/// and are left where they are. One row per station, registered twice or once.
#[tokio::test]
#[serial]
async fn a_station_registers_once_and_leaves_the_provenance_less_instruments_alone() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        "INSERT INTO sensors (id, serial_number, kind, data_frequency) VALUES \
         (gen_random_uuid(), 'BENCH-1', 'device', 'low'), \
         (gen_random_uuid(), 'BENCH-2', 'device', 'low')",
    )
    .await;
    let bench_before = scalar_i64(
        &db,
        "SELECT COUNT(*) AS n FROM sensors WHERE source_system IS NULL AND source_key IS NULL",
    )
    .await;
    assert_eq!(bench_before, 2, "both serial instruments are stored");

    let first = instrument(&db, STATION)
        .await
        .expect("register the station");
    let again = instrument(&db, STATION)
        .await
        .expect("register the same station again");
    assert_eq!(first, again, "the station registers onto its own row");

    let other = instrument(&db, "PAY")
        .await
        .expect("register a second station");
    assert_ne!(other, first, "a different station is a different row");

    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS n FROM sensors WHERE source_system = 'meteoswiss'",
        )
        .await,
        2,
        "two stations, two rows, three registrations",
    );
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS n FROM sensors WHERE source_system IS NULL AND source_key IS NULL",
        )
        .await,
        bench_before,
        "the instruments with no provenance are untouched",
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a blank catalog, and a site subscribed to station pressure.
/// Expected behaviour: the subscription mints the catalog row its variable lands on and points at
/// it, and a second site subscribing to the same variable reuses that row.
#[tokio::test]
#[serial]
async fn subscribing_mints_the_catalog_parameter_the_variable_lands_on() {
    use crudcrate::CRUDOperations;

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    assert_eq!(
        scalar_i64(
            &db,
            "SELECT count(*) AS n FROM parameters WHERE lower(code) = 'barometric_pressure'",
        )
        .await,
        0,
        "the catalog does not hold it before anyone subscribes"
    );

    let subscribe = |site_id: &'static str| MeteoswissSubscriptionCreate {
        site_id: Uuid::parse_str(site_id).unwrap(),
        station_abbr: STATION.to_string(),
        variable: VARIABLE.to_string(),
        enabled: Some(true),
    };
    let first = MeteoswissSubscriptionOperations
        .perform_create(&db, subscribe(crate::common::fixtures::SITE1_ID))
        .await
        .expect("subscribe the first site");
    let second = MeteoswissSubscriptionOperations
        .perform_create(&db, subscribe(crate::common::fixtures::SITE2_ID))
        .await
        .expect("subscribe the second site");

    assert_eq!(
        first.parameter_id, second.parameter_id,
        "one catalog row serves every site reading the variable"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM parameters \
                  WHERE id = '{}' AND lower(code) = 'barometric_pressure' \
                    AND default_units = 'hPa' AND category = 'measurement'",
                first.parameter_id
            ),
        )
        .await,
        1,
        "the minted row is the variable's declaration"
    );

    let subscriptions = subscribers(&db).await.unwrap();
    assert_eq!(subscriptions.len(), 2);
    assert!(
        subscriptions
            .iter()
            .all(|s| s.parameter_id == first.parameter_id)
    );
}

/// Scenario: the SMN file has not been republished since the last tick.
/// Expected behaviour: the stored ETag goes out as `If-None-Match`, the source answers 304, and
/// the fetch reports the body unchanged rather than downloading it again.
#[tokio::test]
#[serial]
async fn a_second_fetch_is_conditional_and_a_304_reads_as_unchanged() {
    use axum::http::{HeaderMap, StatusCode};
    use river_db::routes::private::meteoswiss::models::Fetched;
    use river_db::routes::private::meteoswiss::service::fetch;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    const ETAG: &str = "\"smn-1\"";
    let bodies_served = Arc::new(AtomicUsize::new(0));
    let served = bodies_served.clone();
    let app = axum::Router::new().route(
        "/file.csv",
        axum::routing::get(move |headers: HeaderMap| {
            let served = served.clone();
            async move {
                if headers
                    .get(axum::http::header::IF_NONE_MATCH)
                    .and_then(|v| v.to_str().ok())
                    == Some(ETAG)
                {
                    return (StatusCode::NOT_MODIFIED, [("etag", ETAG)], String::new());
                }
                served.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("etag", ETAG)],
                    "station;value".to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file.csv", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let client = reqwest::Client::new();
    let first = fetch(&db, &client, &url).await.expect("the first fetch");
    assert!(matches!(first, Fetched::Body(ref body) if body == "station;value"));
    let second = fetch(&db, &client, &url).await.expect("the second fetch");
    assert!(
        matches!(second, Fetched::Unchanged),
        "the source holds what we hold"
    );
    assert_eq!(
        bodies_served.load(Ordering::SeqCst),
        1,
        "the body is downloaded once"
    );

    server.abort();
}
