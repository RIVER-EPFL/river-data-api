//! Scenario: a handler is handed the id of a job or an alarm event and has to decide whether the
//! caller's projects cover it. Neither table carries a project column.
//!
//! Expected behaviour: `common::scope`'s resolvers answer through the site, tell a job with no
//! target apart from an unattributable one, and report a missing row as missing rather than as
//! out-of-scope.
//!
//! Needs the database only, no Keycloak.
//!
//! Run: cargo test --test rbac project_of_row -- --test-threads=1

use chrono::Utc;
use river_db::common::scope::{self, RowProject};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{create_sensor, deploy_sensor, seed_base_entities};
use crate::common::{
    GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID, cleanup_test_db, exec, setup_test_db,
};

fn project_id() -> Uuid {
    PROJECT_ID.parse().expect("fixture project id")
}

async fn insert_job(db: &DatabaseConnection, site: Option<&str>, sensor: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let site = site.map_or("NULL".to_string(), |s| format!("'{s}'"));
    let sensor = sensor.map_or("NULL".to_string(), |s| format!("'{s}'"));
    exec(
        db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, site_id, sensor_id) \
             VALUES ('{id}', 'refresh_aggregates', 'completed', {site}, {sensor})"
        ),
    )
    .await;
    id
}

async fn insert_alarm_event(db: &DatabaseConnection) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    exec(
        db,
        &format!(
            "INSERT INTO alarm_events \
             (id, site_id, parameter_id, severity, max_severity, started_at, value_at_start, \
              last_seen_at, last_value) \
             VALUES ('{id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', 2, 2, '{now}', 99.0, '{now}', 99.0)"
        ),
    )
    .await;
    id
}

#[tokio::test]
#[serial]
async fn a_job_resolves_its_project_through_its_site_then_its_sensor() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let site_job = insert_job(&db, Some(SITE1_ID), None).await;
    assert_eq!(
        scope::project_of_job(&db, site_job).await.expect("resolve"),
        RowProject::In(vec![project_id()])
    );

    let sensor = create_sensor(&db, "Deployed sensor", GLOBAL_PARAM_TEMP_ID).await;
    deploy_sensor(
        &db,
        sensor.id,
        SITE1_ID,
        Utc::now() - chrono::Duration::days(1),
    )
    .await;
    let sensor_job = insert_job(&db, None, Some(sensor.id)).await;
    assert_eq!(
        scope::project_of_job(&db, sensor_job)
            .await
            .expect("resolve"),
        RowProject::In(vec![project_id()])
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_job_with_no_target_is_global_and_an_undeployed_sensors_job_is_unresolved() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let global = insert_job(&db, None, None).await;
    assert_eq!(
        scope::project_of_job(&db, global).await.expect("resolve"),
        RowProject::Global
    );

    let inventory = create_sensor(&db, "Never deployed", GLOBAL_PARAM_TEMP_ID).await;
    let inventory_job = insert_job(&db, None, Some(inventory.id)).await;
    assert_eq!(
        scope::project_of_job(&db, inventory_job)
            .await
            .expect("resolve"),
        RowProject::Unresolved
    );
    assert_eq!(
        scope::project_of_sensor(&db, inventory.id)
            .await
            .expect("resolve"),
        RowProject::Unresolved
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn an_alarm_event_resolves_through_its_site() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let event = insert_alarm_event(&db).await;
    assert_eq!(
        scope::project_of_alarm_event(&db, event)
            .await
            .expect("resolve"),
        RowProject::In(vec![project_id()])
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn an_unknown_id_is_missing_not_out_of_scope() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let ghost = Uuid::new_v4();
    assert_eq!(
        scope::project_of_job(&db, ghost).await.expect("resolve"),
        RowProject::Missing
    );
    assert_eq!(
        scope::project_of_alarm_event(&db, ghost)
            .await
            .expect("resolve"),
        RowProject::Missing
    );
    assert_eq!(
        scope::project_of_sensor(&db, ghost).await.expect("resolve"),
        RowProject::Missing
    );
    assert_eq!(
        scope::project_of_site(&db, ghost).await.expect("resolve"),
        RowProject::Missing
    );

    cleanup_test_db(&db).await;
}
