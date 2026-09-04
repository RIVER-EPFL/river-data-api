//! A tracked job records its `category` (from the trigger_type registry), a structured `detail`
//! summary, a `site_id` scope, and an ordered timeline in `reprocessing_job_logs`. Enqueues the job
//! and runs one worker cycle over it so the assertions are deterministic.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use river_db::routes::private::reprocessing_jobs::job::Job;
use river_db::routes::private::reprocessing_jobs::lifecycle::JobReport;
use river_db::routes::private::reprocessing_jobs::worker;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::jobs::{ClosureJob, registry_of};

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

async fn scalar_string(db: &DatabaseConnection, sql: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<String>("", "v")
    .unwrap()
}

/// Enqueue one job of `job`'s kind and run it through a worker cycle, returning its row id.
async fn run_job(db: &DatabaseConnection, job: ClosureJob) -> Uuid {
    let trigger_type = job.name();
    let registry = registry_of(job);
    let job_id = worker::enqueue(db, trigger_type, None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");
    assert!(
        worker::run_one(db, &events(), &registry, &worker::worker_id())
            .await
            .unwrap(),
        "the worker claims the enqueued job"
    );
    job_id
}

#[tokio::test]
#[serial]
async fn tracked_job_records_category_detail_and_timeline() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let site = Uuid::new_v4();
    run_job(
        &db,
        ClosureJob::new("manual_reprocess", move |ctx| async move {
            ctx.info("starting reprocess").await;
            ctx.set_site(site).await;
            ctx.report(
                JobReport::new()
                    .scope("sensor", "x")
                    .count("readings_updated", 7i64),
            )
            .await;
            ctx.log("warn", "one slot skipped", serde_json::json!({ "slot": 3 })).await;
            Ok(7)
        }),
    )
    .await;

    // Job row: operator category (manual_reprocess), completed, detail + site_id persisted.
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT category, status, readings_updated, site_id, detail \
             FROM reprocessing_jobs WHERE trigger_type = 'manual_reprocess'"
                .to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "category").unwrap(), "operator");
    assert_eq!(row.try_get::<String>("", "status").unwrap(), "completed");
    assert_eq!(
        row.try_get::<Option<i32>>("", "readings_updated").unwrap(),
        Some(7)
    );
    assert_eq!(
        row.try_get::<Option<Uuid>>("", "site_id").unwrap(),
        Some(site)
    );
    let detail: serde_json::Value = row.try_get("", "detail").unwrap();
    assert_eq!(detail["counts"]["readings_updated"], serde_json::json!(7));

    // Timeline: two ordered lines (info seq 0, warn seq 1) with structured context.
    let lines = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT seq, level, message, context FROM reprocessing_job_logs ORDER BY seq"
                .to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].try_get::<i64>("", "seq").unwrap(), 0);
    assert_eq!(lines[0].try_get::<String>("", "level").unwrap(), "info");
    assert_eq!(lines[1].try_get::<i64>("", "seq").unwrap(), 1);
    assert_eq!(lines[1].try_get::<String>("", "level").unwrap(), "warn");
    let ctx1: serde_json::Value = lines[1].try_get("", "context").unwrap();
    assert_eq!(ctx1["slot"], serde_json::json!(3));
}

#[tokio::test]
#[serial]
async fn maintenance_trigger_types_are_categorised_maintenance() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    run_job(
        &db,
        ClosureJob::new("janitor_service", |_ctx| async { Ok(0) }),
    )
    .await;

    let category = scalar_string(
        &db,
        "SELECT category AS v FROM reprocessing_jobs WHERE trigger_type = 'janitor_service'",
    )
    .await;
    assert_eq!(category, "maintenance");
}

/// Every count a run reports is a number, so an aggregate over `detail->'counts'` meets no boolean
/// or string. A merge reports whether it deleted its source, which is a fact about the scope.
#[tokio::test]
#[serial]
async fn a_merge_reports_numeric_counts_and_carries_source_deleted_in_scope() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let source = Uuid::parse_str(crate::common::GLOBAL_PARAM_DEPTH_ID).unwrap();
    let target = Uuid::parse_str(crate::common::GLOBAL_PARAM_TEMP_ID).unwrap();
    let job_id = worker::enqueue(
        &db,
        "merge_parameters",
        None,
        None,
        &serde_json::json!({ "source_parameter_id": source, "target_parameter_id": target }),
        None,
    )
    .await
    .unwrap()
    .expect("the merge is enqueued");

    let registry = river_db::routes::private::reprocessing_jobs::job::build_registry();
    assert!(
        worker::run_one(&db, &events(), &registry, &worker::worker_id())
            .await
            .unwrap(),
        "the worker claims the merge"
    );

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status, detail FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "status").unwrap(), "completed");
    let detail: serde_json::Value = row.try_get("", "detail").unwrap();
    let counts = detail["counts"].as_object().expect("counts is an object");
    assert!(!counts.is_empty(), "the merge reports its counts: {detail}");
    for (key, value) in counts {
        assert!(value.is_number(), "{key} is not a number: {value}");
    }
    assert!(
        detail["scope"]["source_deleted"].is_boolean(),
        "source_deleted is scope, not a count: {detail}"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Rerun replays what the row stores, so the row has to say what that is.
#[tokio::test]
#[serial]
async fn a_job_row_carries_the_params_it_was_enqueued_with() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let params = serde_json::json!({ "target": "spot", "retag_existing": true });
    let job_id = worker::enqueue(&db, "measurement_retag", None, None, &params, None)
        .await
        .unwrap()
        .expect("the retag is enqueued");

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["params"], params, "the row states its inputs: {body}");

    crate::common::cleanup_test_db(&db).await;
}
