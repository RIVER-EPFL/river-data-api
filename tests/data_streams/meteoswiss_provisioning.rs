//! Declaring a MeteoSwiss station on a site is the whole operator action: the pressure slot, the
//! paired stream and the station instrument follow from it, and a re-published interval is a
//! duplicate rather than a correction.
//!
//! Run with: cargo test --test data_streams meteoswiss

use chrono::{DateTime, TimeZone, Utc};
use river_db::routes::private::meteoswiss::parse::Point;
use river_db::routes::private::meteoswiss::sync::{
    Subscriber, advance_cursor, cursor, instrument, parameter_id, provision, subscribers,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const STATION: &str = "MOB";

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

async fn declare_station(db: &DatabaseConnection, site_id: &str, station: &str) {
    crate::common::exec(
        db,
        &format!("UPDATE sites SET meteoswiss_station_abbr = '{station}' WHERE id = '{site_id}'"),
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
    declare_station(&db, crate::common::fixtures::SITE1_ID, "  mob  ").await;

    // The declaration is read back trimmed and upper-cased, so the URL and the instrument key are
    // the same whatever an operator typed.
    let declared = subscribers(&db).await.unwrap();
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].station, STATION);

    let parameter = parameter_id(&db)
        .await
        .unwrap()
        .expect("the migration seeds barometric_pressure");
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
    assert_eq!(paired, 1, "the stream is paired to the site's pressure slot");

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
    declare_station(&db, crate::common::fixtures::SITE1_ID, STATION).await;

    let site = Subscriber {
        site_id: Uuid::parse_str(crate::common::fixtures::SITE1_ID).unwrap(),
        site_name: "Site 1".to_string(),
        station: STATION.to_string(),
    };
    let parameter = parameter_id(&db).await.unwrap().unwrap();
    let stream_id = provision(&db, &site, parameter).await.unwrap();
    let sensor_id = instrument(&db, STATION).await.unwrap();

    assert!(cursor(&db, stream_id).await.unwrap().is_none());

    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let series = points(start, &[922.0, 921.7, 921.4]);
    let refs: Vec<&Point> = series.iter().collect();
    let written = river_db::routes::private::meteoswiss::sync::insert(
        &db, stream_id, site.site_id, parameter, sensor_id, &refs,
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
    let replayed = river_db::routes::private::meteoswiss::sync::insert(
        &db, stream_id, site.site_id, parameter, sensor_id, &refs,
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
async fn a_site_with_no_station_is_not_a_subscriber() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    assert!(subscribers(&db).await.unwrap().is_empty());
    declare_station(&db, crate::common::fixtures::SITE1_ID, "   ").await;
    assert!(
        subscribers(&db).await.unwrap().is_empty(),
        "a blank declaration is not a declaration"
    );

}
