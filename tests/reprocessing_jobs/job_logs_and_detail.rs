//! A tracked job records its `category` (from the trigger_type registry), a structured `detail`
//! summary, a `site_id` scope, and an ordered timeline in `reprocessing_job_logs`. Drives the
//! synchronous `run_tracked_job` so the assertions are deterministic.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use river_db::routes::private::reprocessing_jobs::lifecycle::run_tracked_job;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

async fn scalar_string(db: &DatabaseConnection, sql: &str) -> String {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<String>("", "v")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn tracked_job_records_category_detail_and_timeline() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let site = Uuid::new_v4();
    let count = run_tracked_job(&db, None, "manual_reprocess", None, events(), move |ctx| async move {
        ctx.info("starting reprocess").await;
        ctx.set_site(site).await;
        ctx.set_detail(serde_json::json!({ "scope": { "sensor": "x" }, "counts": { "readings_updated": 7 } }))
            .await;
        ctx.log("warn", "one slot skipped", serde_json::json!({ "slot": 3 })).await;
        Ok(7)
    })
    .await
    .unwrap();
    assert_eq!(count, 7);

    // Job row: operator category (manual_reprocess), completed, detail + site_id persisted.
    let row = db
        .query_one(Statement::from_string(
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
        .query_all(Statement::from_string(
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

    run_tracked_job(
        &db,
        None,
        "janitor_run",
        None,
        events(),
        |_ctx| async move { Ok(0) },
    )
    .await
    .unwrap();

    let category = scalar_string(
        &db,
        "SELECT category AS v FROM reprocessing_jobs WHERE trigger_type = 'janitor_run'",
    )
    .await;
    assert_eq!(category, "maintenance");
}
