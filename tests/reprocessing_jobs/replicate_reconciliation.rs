//! The `replicate_reconciliation` job (migrate a legacy avg stream's slot onto its replicate
//! family stream, behind two verifications) and its destructive counterpart
//! `replicate_reconciliation_delete` (remove the obsolete avg stream only after the family
//! re-verifies). Families are paired by source_key suffix: `<old_key>:reps` with a
//! `metadata.replicates` spec supersedes `<old_key>` of the same source_system.
//!
//! Run: cargo test --test reprocessing_jobs replicate_reconciliation -- --test-threads=1

use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::{job, worker};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const SOURCE: &str = "cnet";
const OLD_KEY: &str = "STA:DOC_avg_ppb";
const NEW_KEY: &str = "STA:DOC_avg_ppb:reps";
const T1: &str = "2025-03-01T08:00:00Z";
const T2: &str = "2025-03-01T09:00:00Z";
const T3: &str = "2025-03-01T10:00:00Z";

/// Per instant: the three replicate values and the mean the portal served for them.
const GROUPS: [(&str, [f64; 3], f64); 3] = [
    (T1, [10.0, 20.0, 30.0], 20.0),
    (T2, [40.0, 50.0, 60.0], 50.0),
    (T3, [7.0, 8.0, 9.0], 8.0),
];

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<AppEvent>(16).0
}

struct Family {
    old_id: Uuid,
    new_id: Uuid,
}

async fn seed_family(db: &DatabaseConnection) -> Family {
    let old_id = Uuid::new_v4();
    let new_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, \
                                       site_parameter_id, paired_at, is_active, measurement_type) \
             VALUES ('{old_id}', '{SOURCE}', '{OLD_KEY}', 'DOC avg', '{sp}', NOW(), true, 'spot')",
            sp = crate::common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, \
                                       measurement_type, metadata) \
             VALUES ('{new_id}', '{SOURCE}', '{NEW_KEY}', 'DOC replicates', true, 'spot', \
                     '{{\"replicates\": {{\"source_columns\": \
                       [\"DOC_1_ppb\", \"DOC_2_ppb\", \"DOC_3_ppb\"], \
                       \"portal_mean_column\": \"DOC_avg_ppb\"}}}}'::jsonb)"
        ),
    )
    .await;

    for (time, reps, avg) in GROUPS {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                                       raw_value, measurement_type) \
                 VALUES ('{old_id}', '{site}', '{param}', '{time}', 0, {avg}, 'spot')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
        for (idx, value) in reps.iter().enumerate() {
            crate::common::exec(
                db,
                &format!(
                    "INSERT INTO readings (stream_id, time, replicate_index, raw_value, \
                                           measurement_type) \
                     VALUES ('{new_id}', '{time}', {idx}, {value}, 'spot')"
                ),
            )
            .await;
        }
    }
    Family { old_id, new_id }
}

async fn setup() -> (DatabaseConnection, Family) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::sensor_lifecycle::seed_base_entities(&db).await;
    let family = seed_family(&db).await;
    (db, family)
}

async fn run_job(db: &DatabaseConnection, trigger: &str, dry_run: bool) -> Uuid {
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();
    let id = worker::enqueue(
        db,
        trigger,
        None,
        None,
        &serde_json::json!({"source_system": SOURCE, "dry_run": dry_run}),
        None,
    )
    .await
    .unwrap()
    .expect("enqueue inserts a row");
    worker::drain(db, &ev, &registry, &wid).await.unwrap();
    id
}

async fn job_outcome(db: &DatabaseConnection, id: Uuid) -> (String, serde_json::Value) {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT status, detail FROM reprocessing_jobs WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.try_get::<String>("", "status").unwrap(),
        row.try_get::<serde_json::Value>("", "detail").unwrap(),
    )
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

async fn family_is_paired(db: &DatabaseConnection, family: &Family) -> bool {
    count(
        db,
        &format!(
            "SELECT COUNT(*) FROM data_streams WHERE id = '{}' AND site_parameter_id = '{}'",
            family.new_id,
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await
        == 1
}

async fn slot_samples(db: &DatabaseConnection) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*) FROM samples WHERE site_id = '{}' AND parameter_id = '{}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await
}

fn family_status(detail: &serde_json::Value) -> &str {
    detail["families"][0]["status"].as_str().unwrap_or("")
}

#[tokio::test]
#[serial]
async fn migrate_verify_happy_path() {
    let (db, family) = setup().await;

    let id = run_job(&db, "replicate_reconciliation", false).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(detail["counts"]["migrated"], 1, "detail: {detail}");
    assert_eq!(family_status(&detail), "migrated");

    assert!(
        family_is_paired(&db, &family).await,
        "family took the old slot"
    );
    assert_eq!(slot_samples(&db).await, 3, "one sample per replicate group");
    for (time, _reps, avg) in GROUPS {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT mean, n::bigint AS n FROM samples \
                     WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{time}'",
                    crate::common::SITE1_ID,
                    crate::common::GLOBAL_PARAM_TEMP_ID
                ),
            ))
            .await
            .unwrap()
            .expect("sample exists");
        let mean: f64 = row.try_get("", "mean").unwrap();
        let n: i64 = row.try_get("", "n").unwrap();
        assert_eq!(n, 3);
        assert!(
            (mean - avg).abs() < 1e-9,
            "trigger-computed mean at {time} matches the old served value: {mean} vs {avg}"
        );
    }
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{}' AND site_id = '{}'",
                family.new_id,
                crate::common::SITE1_ID
            ),
        )
        .await,
        9,
        "family readings gained attribution"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{}'",
                family.old_id
            ),
        )
        .await,
        3,
        "the migrate job deletes nothing"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{}'",
                family.old_id
            ),
        )
        .await,
        1,
        "the old stream stands until the delete job"
    );
}

/// The portals store aggregate cells rounded to 2 decimals, so an avg reading off the true mean
/// by less than half that quantum is the portal's own storage, not a disagreement. The verifier
/// shares the audit's quantum floor and migrates cleanly.
#[tokio::test]
#[serial]
async fn a_sub_quantum_avg_delta_verifies_clean() {
    let (db, family) = setup().await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET raw_value = 8.0033 \
             WHERE stream_id = '{}' AND time = '{T3}'",
            family.old_id
        ),
    )
    .await;

    let id = run_job(&db, "replicate_reconciliation", false).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "{detail}");
    assert_eq!(
        family_status(&detail),
        "migrated",
        "a 2dp-quantised portal cell is within tolerance: {detail}"
    );
    assert!(family_is_paired(&db, &family).await);
}

#[tokio::test]
#[serial]
async fn preverify_mismatch_aborts_untouched() {
    let (db, family) = setup().await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET raw_value = raw_value + 3.0 \
             WHERE stream_id = '{}' AND time = '{T2}' AND replicate_index = 0",
            family.new_id
        ),
    )
    .await;

    let id = run_job(&db, "replicate_reconciliation", false).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(detail["counts"]["preverify_failed"], 1, "detail: {detail}");
    assert_eq!(detail["counts"]["migrated"], 0);
    assert_eq!(family_status(&detail), "preverify_failed");
    assert!(
        detail["mismatches"][0]["time"].is_string(),
        "the disagreeing instant is reported for review: {detail}"
    );

    assert!(
        !family_is_paired(&db, &family).await,
        "family left unpaired"
    );
    assert_eq!(slot_samples(&db).await, 0, "no samples materialised");
}

#[tokio::test]
#[serial]
async fn awaiting_backfill_skips() {
    let (db, family) = setup().await;
    crate::common::exec(
        &db,
        &format!(
            "DELETE FROM readings WHERE stream_id = '{}' AND time = '{T3}'",
            family.new_id
        ),
    )
    .await;

    let id = run_job(&db, "replicate_reconciliation", false).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(detail["counts"]["awaiting_backfill"], 1, "detail: {detail}");
    assert_eq!(family_status(&detail), "awaiting_backfill");
    assert_eq!(detail["families"][0]["missing_instants"], 1);

    assert!(!family_is_paired(&db, &family).await);
    assert_eq!(slot_samples(&db).await, 0);
}

#[tokio::test]
#[serial]
async fn dry_run_reports_without_mutating() {
    let (db, family) = setup().await;

    let id = run_job(&db, "replicate_reconciliation", true).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(family_status(&detail), "ready");
    assert_eq!(detail["counts"]["migrated"], 0);

    assert!(
        !family_is_paired(&db, &family).await,
        "dry run pairs nothing"
    );
    assert_eq!(slot_samples(&db).await, 0, "dry run materialises nothing");
}

#[tokio::test]
#[serial]
async fn delete_job_removes_old_after_verify() {
    let (db, family) = setup().await;
    run_job(&db, "replicate_reconciliation", false).await;

    let id = run_job(&db, "replicate_reconciliation_delete", false).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(detail["counts"]["streams_deleted"], 1, "detail: {detail}");
    assert_eq!(detail["counts"]["readings_deleted"], 3);
    assert_eq!(family_status(&detail), "deleted");

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{}'",
                family.old_id
            ),
        )
        .await,
        0,
        "the obsolete avg stream is gone"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{}'",
                family.old_id
            ),
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{}'",
                family.new_id
            ),
        )
        .await,
        9,
        "family readings intact"
    );
    assert_eq!(slot_samples(&db).await, 3, "samples intact");
}

#[tokio::test]
#[serial]
async fn delete_job_skips_unmigrated() {
    let (db, family) = setup().await;

    let id = run_job(&db, "replicate_reconciliation_delete", false).await;
    let (status, detail) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "detail: {detail}");
    assert_eq!(
        detail["counts"]["skipped_unmigrated"], 1,
        "detail: {detail}"
    );
    assert_eq!(detail["counts"]["streams_deleted"], 0);
    assert_eq!(family_status(&detail), "not_migrated");

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{}'",
                family.old_id
            ),
        )
        .await,
        1,
        "an unmigrated family's old stream is never touched"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{}'",
                family.old_id
            ),
        )
        .await,
        3
    );
}
