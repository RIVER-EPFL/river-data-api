//! Pairing-plan apply/revert now run as tracked background jobs (`plan_apply`/`plan_revert`): the
//! endpoint returns a `job_id` immediately and the heavy backfill runs in the job. This test pins
//! the end state, applying pairs the stream and backfills its readings; reverting unpairs them,
//! so the conversion can't silently change behavior.
//!
//! Run: cargo test --test sync -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub async fn wait_terminal(db: &sea_orm::DatabaseConnection, job_id: &str) -> String {
    let id = Uuid::parse_str(job_id).unwrap();
    let start = Instant::now();
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT status FROM reprocessing_jobs WHERE id = $1",
                [id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        if !matches!(
            status.as_str(),
            "queued" | "pending" | "running" | "retrying"
        ) {
            return status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "job did not settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn job_id_of(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn scalar_opt_uuid(db: &sea_orm::DatabaseConnection, sql: &str) -> Option<Uuid> {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<Uuid>>("", "v")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn apply_then_revert_pairing_plan_via_jobs() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'loc-apply-1', 'Loc Apply 1', true)"
        ),
    )
    .await;
    // Two unpaired readings on the stream.
    for ts in ["2025-02-01T00:00:00Z", "2025-02-01T00:10:00Z"] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
                 VALUES ('{stream_id}', '{ts}', 1.0, 0)"
            ),
        )
        .await;
    }

    let entries = serde_json::json!([{
        "stream_id": stream_id,
        "source_key": "loc-apply-1",
        "source_name": "Loc Apply 1",
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Site 1", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Temperature", "create": false, "units": "C", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "original_parameter_name": null
    }]);
    let plan_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;

    // Apply, returns a job id immediately; the backfill runs in the job.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/apply"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "apply should be 2xx, got {status}: {text}"
    );
    assert_eq!(wait_terminal(&db, &job_id_of(&text)).await, "completed");

    // Plan applied, stream paired, readings backfilled with a site_id.
    let plan_status = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status AS v FROM pairing_plans WHERE id = '{plan_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "v")
        .unwrap();
    assert_eq!(plan_status, "applied");
    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{stream_id}'")
        )
        .await
        .is_some(),
        "stream should be paired"
    );
    assert!(
        scalar_opt_uuid(&db, &format!("SELECT site_id AS v FROM readings WHERE stream_id = '{stream_id}' ORDER BY time LIMIT 1")).await.is_some(),
        "readings should be backfilled with a site_id"
    );

    // Revert, also a job; unpairs the stream and clears the readings' site_id.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/revert"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "revert should be 2xx, got {status}: {text}"
    );
    assert_eq!(wait_terminal(&db, &job_id_of(&text)).await, "completed");

    let plan_status = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status AS v FROM pairing_plans WHERE id = '{plan_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "v")
        .unwrap();
    assert_eq!(plan_status, "reverted");
    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{stream_id}'")
        )
        .await
        .is_none(),
        "stream should be unpaired"
    );
    assert!(
        scalar_opt_uuid(&db, &format!("SELECT site_id AS v FROM readings WHERE stream_id = '{stream_id}' ORDER BY time LIMIT 1")).await.is_none(),
        "readings site_id should be cleared"
    );

    crate::common::cleanup_test_db(&db).await;
}
