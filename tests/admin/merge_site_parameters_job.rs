//! `POST /actions/merge_site_parameters` now runs as a tracked `merge_site_parameters` job: the
//! endpoint returns a `job_id` and the multi-table move runs in the job. This pins the end state,
//! the source site_parameter is absorbed and deleted, so the conversion can't change behavior.
//!
//! Run: cargo test --test admin -- --test-threads=1

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
        if !matches!(status.as_str(), "queued" | "pending" | "running" | "retrying") {
            return status;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "merge job did not settle");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn site_parameter_exists(db: &sea_orm::DatabaseConnection, id: &str) -> bool {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT 1 AS v FROM site_parameters WHERE id = '{id}'"),
    ))
    .await
    .unwrap()
    .is_some()
}

#[tokio::test]
#[serial]
async fn merge_site_parameters_runs_as_job_and_deletes_source() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    assert!(site_parameter_exists(&db, crate::common::PARAM_S1_DO_ID).await);

    let body = serde_json::json!({
        "source_site_parameter_id": crate::common::PARAM_S1_DO_ID,
        "target_site_parameter_id": crate::common::PARAM_S1_TEMP_ID,
    });
    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/actions/merge_site_parameters",
        &body,
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "merge should be 2xx, got {status}: {text}");

    let job_id = serde_json::from_str::<serde_json::Value>(&text).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(wait_terminal(&db, &job_id).await, "completed");

    assert!(
        !site_parameter_exists(&db, crate::common::PARAM_S1_DO_ID).await,
        "the source site_parameter should be absorbed and deleted"
    );
    assert!(
        site_parameter_exists(&db, crate::common::PARAM_S1_TEMP_ID).await,
        "the target survives"
    );

    crate::common::cleanup_test_db(&db).await;
}
