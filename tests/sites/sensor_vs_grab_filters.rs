//! The sensor-vs-grab export averages the same continuous population every other serving path
//! uses: unflagged rows at replicate_index 0.

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const GRAB_TIME: &str = "2025-04-01T08:00:00Z";

async fn make_stream(db: &DatabaseConnection) -> Uuid {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'test', '{}', 'turbidity', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    stream_id
}

async fn insert_continuous(
    db: &DatabaseConnection,
    stream_id: Uuid,
    time: &str,
    replicate_index: i16,
    value: f64,
    flagged: bool,
) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings \
                (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                 calibrated_value, logged, measurement_type, is_flagged) \
             VALUES ('{stream_id}', '{site}', '{param}', '{time}', {replicate_index}, {value}, \
                     {value}, false, 'continuous', {flagged})",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn flagged_and_replicate_rows_are_excluded_from_the_sensor_average() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [{
                "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
                "value": 12.0,
                "time": GRAB_TIME,
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {body}");

    // Window is [T+2h, T+6h]: two clean points, one flagged spike, one stray replicate.
    let stream = make_stream(&db).await;
    insert_continuous(&db, stream, "2025-04-01T11:00:00Z", 0, 10.0, false).await;
    insert_continuous(&db, stream, "2025-04-01T12:00:00Z", 0, 14.0, false).await;
    insert_continuous(&db, stream, "2025-04-01T13:00:00Z", 0, 900.0, true).await;
    insert_continuous(&db, stream, "2025-04-01T12:00:00Z", 1, 900.0, false).await;

    let uri = format!(
        "/api/sites/{}/export/sensor-vs-grab?parameter_id={}\
         &start=2025-04-01T00:00:00Z&end=2025-04-01T23:59:59Z",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TURB_ID,
    );
    let (status, body) = crate::common::get_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "export ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = resp["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("rows in export: {resp}"));
    assert_eq!(rows.len(), 1, "one grab, one comparison row: {resp}");
    let row = &rows[0];
    assert_eq!(row["sensor_n"], 2, "clean replicate-0 points only: {row}");
    assert!(
        (row["sensor_avg"].as_f64().unwrap() - 12.0).abs() < 1e-9,
        "(10 + 14) / 2, flagged spike and stray replicate excluded: {row}"
    );
}
