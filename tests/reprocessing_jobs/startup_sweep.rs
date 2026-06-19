//! Startup reconciliation: tracked jobs left `pending`/`running`/`retrying` by a process that died
//! mid-flight are swept to the terminal `interrupted` status at boot, while already-terminal jobs
//! are left untouched.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn insert_job(db: &DatabaseConnection, status: &str, owner: &str) -> Uuid {
    let id = Uuid::new_v4();
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, owner) \
             VALUES ('{id}', 'manual_reprocess', '{status}', '{owner}')"
        ),
    ))
    .await
    .unwrap();
    id
}

async fn row_state(db: &DatabaseConnection, id: Uuid) -> (String, Option<String>, bool) {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT status, error_message, completed_at IS NOT NULL AS done \
                 FROM reprocessing_jobs WHERE id = '{id}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.try_get("", "status").unwrap(),
        row.try_get("", "error_message").unwrap(),
        row.try_get("", "done").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn startup_sweep_marks_orphans_interrupted() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let me = river_db::routes::private::reprocessing_jobs::lifecycle::process_owner();

    let pending = insert_job(&db, "pending", me).await;
    let running = insert_job(&db, "running", me).await;
    let retrying = insert_job(&db, "retrying", me).await;
    let completed = insert_job(&db, "completed", me).await;
    let failed = insert_job(&db, "failed", me).await;
    // A peer replica's live inline job must survive this replica's boot sweep.
    let peer = insert_job(&db, "running", "peer-replica").await;

    let swept =
        river_db::routes::private::reprocessing_jobs::lifecycle::reconcile_interrupted_jobs(&db)
            .await
            .unwrap();
    assert_eq!(swept, 3, "exactly this replica's three in-flight jobs are swept");

    for id in [pending, running, retrying] {
        let (status, err, done) = row_state(&db, id).await;
        assert_eq!(status, "interrupted");
        assert_eq!(err.as_deref(), Some("Interrupted by API restart"));
        assert!(done, "swept job should have completed_at set");
    }

    for (id, expected) in [(completed, "completed"), (failed, "failed")] {
        let (status, _, _) = row_state(&db, id).await;
        assert_eq!(status, expected, "terminal jobs must be left untouched");
    }

    let (peer_status, _, _) = row_state(&db, peer).await;
    assert_eq!(peer_status, "running", "a peer replica's live inline job is never stranded");
}

#[tokio::test]
#[serial]
async fn startup_sweep_is_a_noop_when_nothing_in_flight() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let me = river_db::routes::private::reprocessing_jobs::lifecycle::process_owner();
    insert_job(&db, "completed", me).await;
    let swept =
        river_db::routes::private::reprocessing_jobs::lifecycle::reconcile_interrupted_jobs(&db)
            .await
            .unwrap();
    assert_eq!(swept, 0);
}
