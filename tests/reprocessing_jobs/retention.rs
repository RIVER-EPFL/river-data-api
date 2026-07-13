//! Tiered job-row retention: maintenance rows age out fast, operator/metadata slowly, and a count
//! cap trims maintenance overflow. Logs cascade away with their job.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use river_db::routes::private::parameters::derived::janitor::prune_tracked_jobs;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn insert_backdated(
    db: &DatabaseConnection,
    category: &str,
    interval: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, created_at) \
             VALUES ('{id}', 'manual_reprocess', 'completed', '{category}', NOW() - INTERVAL '{interval}')"
        ),
    ))
    .await
    .unwrap();
    id
}

async fn exists(db: &DatabaseConnection, id: Uuid) -> bool {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT 1 AS v FROM reprocessing_jobs WHERE id = '{id}'"),
    ))
    .await
    .unwrap()
    .is_some()
}

#[tokio::test]
#[serial]
async fn retention_prunes_by_category_tier_and_cascades_logs() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let old_maint = insert_backdated(&db, "maintenance", "30 days").await; // > 14d -> pruned
    let recent_maint = insert_backdated(&db, "maintenance", "2 days").await; // kept
    let recent_op = insert_backdated(&db, "operator", "30 days").await; // < 180d -> kept
    let ancient_op = insert_backdated(&db, "operator", "200 days").await; // > 180d -> pruned

    // A log line on the to-be-pruned maintenance job should cascade-delete with it.
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO reprocessing_job_logs (job_id, seq, level, message) \
             VALUES ('{old_maint}', 0, 'info', 'x')"
        ),
    ))
    .await
    .unwrap();

    let deleted = prune_tracked_jobs(&db, 14, 180, 0).await;
    assert_eq!(deleted, 2, "one old maintenance + one ancient operator row");

    assert!(!exists(&db, old_maint).await);
    assert!(exists(&db, recent_maint).await);
    assert!(exists(&db, recent_op).await);
    assert!(!exists(&db, ancient_op).await);

    let log_count = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) AS v FROM reprocessing_job_logs WHERE job_id = '{old_maint}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "v")
        .unwrap();
    assert_eq!(log_count, 0, "logs cascade-deleted with the pruned job");
}

#[tokio::test]
#[serial]
async fn retention_count_cap_trims_maintenance_overflow() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // Five recent maintenance rows with distinct ages; cap keeps the two newest.
    for i in 0..5 {
        insert_backdated(&db, "maintenance", &format!("{i} minutes")).await;
    }

    let deleted = prune_tracked_jobs(&db, 0, 0, 2).await;
    assert_eq!(deleted, 3);

    let remaining = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS v FROM reprocessing_jobs WHERE category = 'maintenance'".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "v")
        .unwrap();
    assert_eq!(remaining, 2);
}
