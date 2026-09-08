//! `POST /actions/merge_site_parameters` now runs as a tracked `merge_site_parameters` job: the
//! endpoint returns a `job_id` and the multi-table move runs in the job. This pins the end state,
//! the source site_parameter is absorbed and deleted, so the conversion can't change behavior.
//!
//! Run: cargo test --test admin -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

async fn site_parameter_exists(db: &sea_orm::DatabaseConnection, id: &str) -> bool {
    db.query_one_raw(Statement::from_string(
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
    assert!(
        (200..300).contains(&status),
        "merge should be 2xx, got {status}: {text}"
    );

    let job_id = serde_json::from_str::<serde_json::Value>(&text).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id).await,
        "completed"
    );

    assert!(
        !site_parameter_exists(&db, crate::common::PARAM_S1_DO_ID).await,
        "the source site_parameter should be absorbed and deleted"
    );
    assert!(
        site_parameter_exists(&db, crate::common::PARAM_S1_TEMP_ID).await,
        "the target survives"
    );

    // The slot that was absorbed is deleted, so the entry under the survivor is the only place the
    // merge can be read back from (M124).
    let entry = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT subject, old_value FROM change_audit WHERE change = 'site_parameter_merge' \
             ORDER BY changed_at DESC LIMIT 1"
                .to_string(),
        ))
        .await
        .expect("the trail is readable")
        .expect("the merge left an entry");
    let subject: String = entry.try_get("", "subject").expect("subject");
    let old_value: serde_json::Value = entry.try_get("", "old_value").expect("old_value");
    assert_eq!(
        subject,
        format!("site_parameter:{}", crate::common::PARAM_S1_TEMP_ID)
    );
    assert_eq!(
        old_value["source"]["id"],
        crate::common::PARAM_S1_DO_ID,
        "the entry keeps the slot that was absorbed: {old_value}"
    );

    crate::common::cleanup_test_db(&db).await;
}
