//! Scenario: a database holds deployments pairing opened at the pairing instant, months after the
//! streams they cover started measuring.
//!
//! Expected behaviour: each of those opens at the history it should cover, clamped to the end of
//! whatever covered the slot before it. A hand-dated deployment is left alone, and so is one with
//! no readings to cover.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{
    cleanup_test_db, seed_test_data, setup_test_db, GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_TEMP_ID,
    SITE1_ID, SITE2_ID,
};

const AUTO_NOTE: &str = "Auto-created during stream pairing";

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn sensor(db: &DatabaseConnection, serial: &str) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, serial_number, manufacturer, model, data_frequency) \
             VALUES ('{id}', '{serial}', 'test', 'test', 'high')"
        ),
    )
    .await;
    id
}

async fn deployment(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    site_id: &str,
    parameter_id: &str,
    from: &str,
    until: Option<&str>,
    note: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    let until = until.map_or("NULL".to_string(), |u| format!("'{u}'"));
    let note = note.map_or("NULL".to_string(), |n| format!("'{n}'"));
    exec(
        db,
        &format!(
            "INSERT INTO sensor_deployments \
                 (id, sensor_id, site_id, parameter_id, deployed_from, deployed_until, \
                  deployment_type, notes) \
             VALUES ('{id}', '{sensor_id}', '{site_id}', '{parameter_id}', '{from}', {until}, \
                     'permanent', {note})"
        ),
    )
    .await;
    id
}

/// Readings the deployment should have covered, reached through the readings' own attribution.
async fn attributed_readings(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    site_id: &str,
    parameter_id: &str,
    times: &[&str],
) {
    let stream = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, sensor_id) \
             VALUES ('{stream}', 'test', 'bd-{stream}', 'bd', true, '{sensor_id}')"
        ),
    )
    .await;
    for (i, t) in times.iter().enumerate() {
        exec(
            db,
            &format!(
                "INSERT INTO readings \
                     (stream_id, time, replicate_index, raw_value, site_id, parameter_id, sensor_id) \
                 VALUES ('{stream}', '{t}', 0, {}, '{site_id}', '{parameter_id}', '{sensor_id}')",
                1.0 + i as f64
            ),
        )
        .await;
    }
}

async fn deployed_from(db: &DatabaseConnection, id: Uuid) -> String {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT deployed_from FROM sensor_deployments WHERE id = '{id}'"),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "deployed_from")
        .expect("deployed_from")
        .to_rfc3339()
}

#[tokio::test]
#[serial]
async fn each_auto_deployment_opens_at_the_history_it_covers() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;

    // Paired long after its stream started: moves back to its earliest reading.
    let late = sensor(&db, "bd-late").await;
    let late_dep = deployment(
        &db,
        late,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2026-01-01T00:00:00Z",
        None,
        Some(AUTO_NOTE),
    )
    .await;
    attributed_readings(
        &db,
        late,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        &["2025-02-01T00:00:00Z", "2025-03-01T00:00:00Z"],
    )
    .await;

    // Hand-dated: not the pairing's row, so it is not the migration's business.
    let hand = sensor(&db, "bd-hand").await;
    let hand_dep = deployment(
        &db,
        hand,
        SITE2_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2026-01-01T00:00:00Z",
        None,
        Some("Deployed by hand"),
    )
    .await;
    attributed_readings(
        &db,
        hand,
        SITE2_ID,
        GLOBAL_PARAM_TEMP_ID,
        &["2025-02-01T00:00:00Z"],
    )
    .await;

    // A slot whose previous instrument was recalled six months ago: clamped to that recall.
    let earlier = sensor(&db, "bd-earlier").await;
    deployment(
        &db,
        earlier,
        SITE1_ID,
        GLOBAL_PARAM_DEPTH_ID,
        "2024-01-01T00:00:00Z",
        Some("2025-06-01T00:00:00Z"),
        None,
    )
    .await;
    let successor = sensor(&db, "bd-successor").await;
    let successor_dep = deployment(
        &db,
        successor,
        SITE1_ID,
        GLOBAL_PARAM_DEPTH_ID,
        "2026-01-01T00:00:00Z",
        None,
        Some(AUTO_NOTE),
    )
    .await;
    attributed_readings(
        &db,
        successor,
        SITE1_ID,
        GLOBAL_PARAM_DEPTH_ID,
        &["2025-02-01T00:00:00Z"],
    )
    .await;

    // Nothing to cover: left where it is rather than moved to an empty history.
    let idle = sensor(&db, "bd-idle").await;
    let idle_dep = deployment(
        &db,
        idle,
        SITE2_ID,
        GLOBAL_PARAM_DEPTH_ID,
        "2026-01-01T00:00:00Z",
        None,
        Some(AUTO_NOTE),
    )
    .await;

    db.execute_unprepared(
        &migration::m20260907_000002_backdate_auto_deployments::backdate_auto_deployments(),
    )
    .await
    .expect("the backdate applies");

    assert_eq!(
        deployed_from(&db, late_dep).await,
        "2025-02-01T00:00:00+00:00",
        "the auto deployment opens at its earliest reading"
    );
    assert_eq!(
        deployed_from(&db, hand_dep).await,
        "2026-01-01T00:00:00+00:00",
        "a hand-dated deployment is left alone"
    );
    assert_eq!(
        deployed_from(&db, successor_dep).await,
        "2025-06-01T00:00:00+00:00",
        "the successor opens where the recalled instrument ended, not over it"
    );
    assert_eq!(
        deployed_from(&db, idle_dep).await,
        "2026-01-01T00:00:00+00:00",
        "a deployment with no readings to cover is left alone"
    );

    cleanup_test_db(&db).await;
}
