//! The two health probes are built rather than written out, so what proves them is Postgres
//! accepting them: a LATERAL with a window function, and a correlated regression over a night
//! window. Both are syntax no unit test can check.
//!
//! Run: cargo test --test notifications probe_sql -- --test-threads=1

use river_db::routes::private::notifications::flows;
use sea_orm::ConnectionTrait;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn the_health_probes_are_sql_postgres_runs() {
    let db = crate::common::setup_test_db().await;

    db.query_all_raw(flows::stale_slots_query())
        .await
        .expect("the stale-slot probe runs");
    db.query_all_raw(flows::battery_trend_query(uuid::Uuid::nil()))
        .await
        .expect("the battery-trend probe runs");

    crate::common::cleanup_test_db(&db).await;
}

async fn stream_with_reading(
    db: &sea_orm::DatabaseConnection,
    slot: &str,
    parameter: &str,
    stream_type: &str,
    at: &str,
) {
    let stream = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams \
             (id, source_system, source_key, source_name, site_parameter_id, paired_at, is_active, \
              measurement_type) \
             VALUES ('{stream}', 'test', '{stream}', '{stream}', '{slot}', NOW(), true, \
              '{stream_type}')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, replicate_index, measurement_type) \
             VALUES ('{stream}', '{}', '{parameter}', '{at}', 1, 0, 'derived')",
            crate::common::SITE1_ID
        ),
    )
    .await;
}

async fn last_continuous(
    db: &sea_orm::DatabaseConnection,
    parameter: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let rows = db
        .query_all_raw(flows::stale_slots_query())
        .await
        .expect("the stale-slot probe runs");
    let row = rows
        .iter()
        .find(|r| {
            r.try_get::<uuid::Uuid>("", "site_id").unwrap().to_string() == crate::common::SITE1_ID
                && r.try_get::<uuid::Uuid>("", "parameter_id")
                    .unwrap()
                    .to_string()
                    == parameter
        })
        .expect("the slot is probed");
    row.try_get("", "last_continuous").unwrap()
}

/// Scenario: one slot is fed only by a spot stream, another by a non-spot stream, and both hold
/// non-spot readings.
/// Expected behaviour: the continuous lookup runs only at the slot with a non-spot stream, so the
/// spot-only slot reads no continuous time.
#[tokio::test]
#[serial]
async fn the_continuous_lookup_runs_only_at_slots_with_a_non_spot_stream() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = NULL, paired_at = NULL \
             WHERE site_parameter_id IN ('{}', '{}')",
            crate::common::PARAM_S1_TEMP_ID,
            crate::common::PARAM_S1_DO_ID
        ),
    )
    .await;
    stream_with_reading(
        &db,
        crate::common::PARAM_S1_TEMP_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "spot",
        "2025-01-01T00:00:00Z",
    )
    .await;
    stream_with_reading(
        &db,
        crate::common::PARAM_S1_DO_ID,
        crate::common::GLOBAL_PARAM_DO_ID,
        "derived",
        "2030-01-02T00:00:00Z",
    )
    .await;

    assert_eq!(
        last_continuous(&db, crate::common::GLOBAL_PARAM_TEMP_ID).await,
        None
    );
    assert_eq!(
        last_continuous(&db, crate::common::GLOBAL_PARAM_DO_ID)
            .await
            .map(|t| t.to_rfc3339()),
        Some("2030-01-02T00:00:00+00:00".to_string())
    );

    crate::common::cleanup_test_db(&db).await;
}
