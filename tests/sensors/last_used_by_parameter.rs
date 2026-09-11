//! Scenario: a technician packing for a field trip asks which instruments were last used for the
//! parameters the trip will measure.
//!
//! Expected behaviour: the instruments come back most recently used first, one row each, naming
//! the parameter whose reading is the newest. The answer is ordered in SQL over the spot rows, so
//! an instrument outside the page a list would have selected is still ranked (M204).
//!
//! Run: cargo test --test sensors last_used_by_parameter -- --test-threads=1

use crate::common::*;
use serial_test::serial;

/// A spot reading of one parameter by one instrument at one instant.
async fn spot(
    db: &sea_orm::DatabaseConnection,
    stream: &uuid::Uuid,
    sensor: &str,
    parameter: &str,
    at: &str,
) {
    exec(
        db,
        &format!(
            "INSERT INTO readings \
             (stream_id, site_id, parameter_id, sensor_id, time, raw_value, replicate_index, \
              measurement_type) \
             VALUES ('{stream}', '{SITE1_ID}', '{parameter}', '{sensor}', '{at}', 1.0, 0, 'spot')"
        ),
    )
    .await;
}

async fn sensor_named(db: &sea_orm::DatabaseConnection, serial: &str) -> String {
    let id = uuid::Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, serial_number, name, data_frequency) \
             VALUES ('{id}', '{serial}', '{serial}', 'low')"
        ),
    )
    .await;
    id.to_string()
}

#[tokio::test]
#[serial]
async fn instruments_come_back_most_recently_used_first() {
    let fx = seeded_app().await;
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &fx.db,
        "last-used",
        PARAM_S1_TEMP_ID,
    )
    .await;

    let oldest = sensor_named(&fx.db, "LU-OLDEST").await;
    let middle = sensor_named(&fx.db, "LU-MIDDLE").await;
    let newest = sensor_named(&fx.db, "LU-NEWEST").await;
    let other_parameter = sensor_named(&fx.db, "LU-OTHER").await;

    spot(
        &fx.db,
        &stream,
        &oldest,
        GLOBAL_PARAM_TEMP_ID,
        "2024-01-01T09:00:00Z",
    )
    .await;
    spot(
        &fx.db,
        &stream,
        &middle,
        GLOBAL_PARAM_TEMP_ID,
        "2024-06-01T09:00:00Z",
    )
    .await;
    spot(
        &fx.db,
        &stream,
        &newest,
        GLOBAL_PARAM_TEMP_ID,
        "2025-01-01T09:00:00Z",
    )
    .await;
    // An instrument used only for something else is not part of this answer.
    spot(
        &fx.db,
        &stream,
        &other_parameter,
        GLOBAL_PARAM_DO_ID,
        "2026-01-01T09:00:00Z",
    )
    .await;
    // And an earlier reading by the newest instrument does not move it down the list.
    spot(
        &fx.db,
        &stream,
        &newest,
        GLOBAL_PARAM_TEMP_ID,
        "2023-01-01T09:00:00Z",
    )
    .await;

    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensors/last_used?parameter_ids={GLOBAL_PARAM_TEMP_ID}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "last_used ({status}): {body}");
    let ranked: Vec<&str> = body["instruments"]
        .as_array()
        .expect("instruments")
        .iter()
        .map(|i| i["serial_number"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        ranked,
        vec!["LU-NEWEST", "LU-MIDDLE", "LU-OLDEST"],
        "most recently used first, once each: {body}"
    );
    assert_eq!(
        body["instruments"][0]["last_used_at"]
            .as_str()
            .unwrap_or(""),
        "2025-01-01T09:00:00Z",
        "and the instant is the newest of that instrument's, not its first: {body}"
    );

    cleanup_test_db(&fx.db).await;
}

#[tokio::test]
#[serial]
async fn naming_no_parameter_is_refused_rather_than_ranking_everything() {
    let fx = seeded_app().await;
    let (status, body) = get_json_with_token(&fx.app, "/api/sensors/last_used", &fx.token).await;
    assert_eq!(
        status, 400,
        "an unbounded ranking is refused rather than served: {body}"
    );
    cleanup_test_db(&fx.db).await;
}

#[tokio::test]
#[serial]
async fn a_parameter_can_be_named_by_code() {
    let fx = seeded_app().await;
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &fx.db,
        "last-used-code",
        PARAM_S1_TEMP_ID,
    )
    .await;
    let sensor = sensor_named(&fx.db, "LU-BY-CODE").await;
    spot(
        &fx.db,
        &stream,
        &sensor,
        GLOBAL_PARAM_TEMP_ID,
        "2025-03-01T09:00:00Z",
    )
    .await;

    // The seeded catalog names this one; the point is that the code resolves to the same
    // parameter the id does, so the test reads it back rather than hard-coding it.
    let code = {
        use sea_orm::{ConnectionTrait, Statement};
        fx.db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!("SELECT code FROM parameters WHERE id = '{GLOBAL_PARAM_TEMP_ID}'"),
            ))
            .await
            .expect("the catalog row")
            .expect("a row")
            .try_get::<String>("", "code")
            .expect("code")
    };
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensors/last_used?parameter_codes={code}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "last_used by code ({status}): {body}");
    assert_eq!(
        body["instruments"][0]["serial_number"], "LU-BY-CODE",
        "the code resolves to the same parameter as its id: {body}"
    );
    cleanup_test_db(&fx.db).await;
}
