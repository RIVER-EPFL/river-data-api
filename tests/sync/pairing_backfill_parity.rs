//! Every pairing path leaves the slot in the same state. A stream paired through
//! `POST /sync/apply-discovery` carries the same samples, collection events and instrument
//! attribution as the same stream paired through `POST /streams/{id}/pair`.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const REPLICATE_TIME: &str = "2025-03-04T09:00:00Z";

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

async fn seed_spot_replicates(db: &DatabaseConnection) -> Uuid {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'metalp', '{}', 'Portal lab column', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    for (idx, value) in [(0, 5.0), (1, 6.0), (2, 7.0)] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{REPLICATE_TIME}', {idx}, {value}, 'spot')"
            ),
        )
        .await;
    }
    stream_id
}

#[tokio::test]
#[serial]
async fn the_sync_pairing_path_materialises_samples_and_events() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = seed_spot_replicates(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/apply-discovery",
        &serde_json::json!({ "actions": [{
            "stream_id": stream_id,
            "use_project_id": crate::common::PROJECT_ID,
            "use_site_id": crate::common::SITE1_ID,
            "use_parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "pair_to": crate::common::PARAM_S1_TEMP_ID
        }]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "apply-discovery ({status}): {body}");

    let sample_n = scalar_i64(
        &db,
        &format!(
            "SELECT COALESCE(MAX(n), 0)::bigint AS n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{REPLICATE_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(sample_n, 3, "the replicate group forms a sample: {body}");

    let attached = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM readings \
             WHERE stream_id = '{stream_id}' AND collection_event_id IS NOT NULL"
        ),
    )
    .await;
    assert_eq!(attached, 3, "the instant is addressable as a visit: {body}");

    let unattributed = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM readings \
             WHERE stream_id = '{stream_id}' AND sensor_id IS NULL"
        ),
    )
    .await;
    assert_eq!(unattributed, 0, "every reading names what measured it");
}
