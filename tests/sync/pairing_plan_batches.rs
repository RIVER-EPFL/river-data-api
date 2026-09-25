//! A plan apply attributes a stream's history in batches that each commit, and a failed apply
//! resumes from what those batches committed.
//!
//! Run: cargo test --test sync pairing_plan_batches -- --test-threads=1

use river_db::routes::private::sync::service::{BACKFILL_BATCH, apply_plan};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

use crate::common::e2e::count;
use crate::common::exec;

/// Readings on the stream: three batches, the last one short.
const READINGS: u64 = 2 * BACKFILL_BATCH + 500;

/// Fails any attribution of a reading at or after the instant named in `batch_fault`, while that
/// table holds a row. A statement that fails rolls back the whole batch it belongs to.
const READING_FAULT: &[&str] = &[
    "CREATE TABLE batch_fault (at timestamptz NOT NULL)",
    "CREATE FUNCTION batch_fault() RETURNS trigger LANGUAGE plpgsql AS $$
     BEGIN
         IF OLD.site_id IS NULL AND NEW.site_id IS NOT NULL
            AND EXISTS (SELECT 1 FROM batch_fault WHERE NEW.time >= at) THEN
             RAISE EXCEPTION 'injected batch fault';
         END IF;
         RETURN NEW;
     END $$",
    "CREATE TRIGGER zz_batch_fault BEFORE UPDATE ON readings
     FOR EACH ROW EXECUTE FUNCTION batch_fault()",
];

/// Fails the plan's move to `applied`, after every batch has committed.
const FINISH_FAULT: &[&str] = &[
    "CREATE FUNCTION finish_fault() RETURNS trigger LANGUAGE plpgsql AS $$
     BEGIN
         IF NEW.status = 'applied' THEN RAISE EXCEPTION 'injected finish fault'; END IF;
         RETURN NEW;
     END $$",
    "CREATE TRIGGER zz_finish_fault BEFORE UPDATE ON pairing_plans
     FOR EACH ROW EXECUTE FUNCTION finish_fault()",
];

/// Records how many advisory locks the writing backend holds each time the ledger takes one.
const LOCK_PROBE: &[&str] = &[
    "CREATE TABLE advisory_lock_probe (held bigint NOT NULL)",
    "CREATE FUNCTION advisory_lock_probe() RETURNS trigger LANGUAGE plpgsql AS $$
     BEGIN
         INSERT INTO advisory_lock_probe
         SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid();
         RETURN NEW;
     END $$",
    "CREATE TRIGGER zz_advisory_lock_probe AFTER INSERT ON reading_decisions
     FOR EACH ROW EXECUTE FUNCTION advisory_lock_probe()",
];

const TEARDOWN: &[&str] = &[
    "DROP TRIGGER IF EXISTS zz_batch_fault ON readings",
    "DROP FUNCTION IF EXISTS batch_fault()",
    "DROP TABLE IF EXISTS batch_fault",
    "DROP TRIGGER IF EXISTS zz_finish_fault ON pairing_plans",
    "DROP FUNCTION IF EXISTS finish_fault()",
    "DROP TRIGGER IF EXISTS zz_advisory_lock_probe ON reading_decisions",
    "DROP FUNCTION IF EXISTS advisory_lock_probe()",
    "DROP TABLE IF EXISTS advisory_lock_probe",
];

async fn run_all(db: &DatabaseConnection, statements: &[&str]) {
    for sql in statements {
        exec(db, sql).await;
    }
}

/// The instant of the stream's `n`-th reading, counting from one.
fn nth_instant(n: u64) -> String {
    format!("TIMESTAMPTZ '2024-01-01' + {n} * INTERVAL '10 minutes'")
}

/// A draft plan pairing one unpaired stream, holding [`READINGS`] readings and a second replicate
/// at the instant a batch ends on, to Upstream Station's water temperature.
async fn seeded_plan(db: &DatabaseConnection) -> (Uuid, Uuid) {
    crate::common::cleanup_test_db(db).await;
    crate::common::seed_test_data(db).await;
    run_all(db, TEARDOWN).await;

    let stream = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        db,
        &stream.to_string(),
        "batches",
        "batches-1",
        "Test River Project",
        "Upstream Station",
        "Water Temperature",
        "°C",
        None,
        0,
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             SELECT '{stream}', TIMESTAMPTZ '2024-01-01' + g * INTERVAL '10 minutes', g, 0 \
             FROM generate_series(1, {READINGS}) g"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             VALUES ('{stream}', {}, -1, 1)",
            nth_instant(BACKFILL_BATCH)
        ),
    )
    .await;

    let plan_id = Uuid::new_v4();
    let entries = serde_json::json!([{
        "stream_id": stream,
        "source_key": "batches-1",
        "source_name": null,
        "action": "pair",
        "acknowledged": true,
        "is_device": true,
        "project": { "id": crate::common::PROJECT_ID, "name": "Test River Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Upstream Station", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Water Temperature", "create": false, "units": "°C", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "original_parameter_name": null
    }]);
    exec(
        db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'batches', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;
    (plan_id, stream)
}

/// Where the apply stands: plan status, the count it recorded, readings attributed, ledger rows,
/// ledger rows naming a reading twice, and `plan_attribution` jobs queued.
#[derive(Debug, PartialEq, Eq)]
struct Standing {
    status: String,
    recorded: i64,
    attributed: i64,
    decisions: i64,
    duplicated: i64,
    attribution_jobs: i64,
}

async fn standing(db: &DatabaseConnection, plan_id: Uuid, stream: Uuid) -> Standing {
    let status = crate::common::e2e::scalar(
        db,
        &format!("SELECT status FROM pairing_plans WHERE id = '{plan_id}'"),
    )
    .await;
    Standing {
        status,
        recorded: count(
            db,
            &format!(
                "SELECT COALESCE((apply_result ->> 'readings_backfilled')::bigint, -1) \
                 FROM pairing_plans WHERE id = '{plan_id}'"
            ),
        )
        .await,
        attributed: count(
            db,
            &format!(
                "SELECT COUNT(*)::bigint FROM readings \
                 WHERE stream_id = '{stream}' AND site_id IS NOT NULL"
            ),
        )
        .await,
        decisions: count(
            db,
            &format!(
                "SELECT COUNT(*)::bigint FROM reading_decisions \
                 WHERE stream_id = '{stream}' AND kind = 'attribution' AND reason = 'paired'"
            ),
        )
        .await,
        duplicated: count(
            db,
            &format!(
                "SELECT COUNT(*)::bigint FROM ( \
                   SELECT 1 FROM reading_decisions \
                   WHERE stream_id = '{stream}' AND kind = 'attribution' AND reason = 'paired' \
                   GROUP BY time, replicate_index HAVING COUNT(*) > 1) d"
            ),
        )
        .await,
        attribution_jobs: count(
            db,
            &format!(
                "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
                 WHERE trigger_type = 'plan_attribution' AND params ->> 'plan_id' = '{plan_id}'"
            ),
        )
        .await,
    }
}

/// Every reading attributed once, the plan applied with the full count and one attribution queued.
fn applied() -> Standing {
    let all = i64::try_from(READINGS + 1).unwrap();
    Standing {
        status: "applied".to_string(),
        recorded: all,
        attributed: all,
        decisions: all,
        duplicated: 0,
        attribution_jobs: 1,
    }
}

/// Scenario: the plan pairs a stream holding three batches of compressed history.
/// Expected behaviour: every reading is attributed once with its ledger row, while the writing
/// backend never holds more advisory locks than one batch takes.
#[tokio::test]
#[serial]
async fn test_apply_attributes_history_in_bounded_batches() {
    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    let compressed = crate::common::compression::compress_readings_range(
        &db,
        "2024-01-01T00:00:00Z".parse().unwrap(),
        "2024-02-01T00:00:00Z".parse().unwrap(),
    )
    .await;
    assert!(compressed > 0, "the history sits in compressed chunks");
    run_all(&db, LOCK_PROBE).await;

    let result = apply_plan(&db, plan_id, None).await;
    let peak = count(
        &db,
        "SELECT COALESCE(MAX(held), 0)::bigint FROM advisory_lock_probe",
    )
    .await;
    run_all(&db, TEARDOWN).await;

    let result = result.expect("the apply completes");
    assert_eq!(result.readings_backfilled, READINGS + 1);
    assert_eq!(standing(&db, plan_id, stream).await, applied());
    // One batch's instants, plus the one its last instant shares with a second replicate.
    assert!(
        peak <= i64::try_from(BACKFILL_BATCH).unwrap() + 1,
        "a batch holds at most its own readings' locks, saw {peak}"
    );
    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the second batch fails partway through its statement.
/// Expected behaviour: the pairing and the first batch stay committed and counted, nothing of the
/// second batch is kept, and applying again finishes the rest without writing anything twice.
#[tokio::test]
#[serial]
async fn test_apply_resumes_after_a_failed_batch() {
    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    run_all(&db, READING_FAULT).await;
    exec(
        &db,
        &format!(
            "INSERT INTO batch_fault VALUES ({})",
            nth_instant(BACKFILL_BATCH + 100)
        ),
    )
    .await;

    let failed = apply_plan(&db, plan_id, None).await;
    assert!(failed.is_err(), "the injected fault fails the apply");
    let first_batch = i64::try_from(BACKFILL_BATCH).unwrap() + 1;
    assert_eq!(
        standing(&db, plan_id, stream).await,
        Standing {
            status: "applying".to_string(),
            recorded: first_batch,
            attributed: first_batch,
            decisions: first_batch,
            duplicated: 0,
            attribution_jobs: 0,
        },
        "the first batch outlives the failure of the second"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM data_streams \
                 WHERE id = '{stream}' AND pairing_plan_id = '{plan_id}' \
                   AND site_parameter_id IS NOT NULL"
            ),
        )
        .await,
        1,
        "the pairing committed before the history"
    );

    run_all(&db, TEARDOWN).await;
    let resumed = apply_plan(&db, plan_id, None)
        .await
        .expect("the resumed apply completes");
    assert_eq!(resumed.readings_backfilled, READINGS + 1);
    assert_eq!(resumed.streams_paired, 1, "the counts of the pairing survive");
    assert_eq!(standing(&db, plan_id, stream).await, applied());
    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the first batch fails, so only the pairing has committed.
/// Expected behaviour: the plan is left applying with nothing attributed, and a retry does it all.
#[tokio::test]
#[serial]
async fn test_apply_resumes_after_only_the_pairing_committed() {
    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    run_all(&db, READING_FAULT).await;
    exec(&db, "INSERT INTO batch_fault VALUES ('-infinity')").await;

    assert!(apply_plan(&db, plan_id, None).await.is_err());
    let paired_only = standing(&db, plan_id, stream).await;
    assert_eq!(
        (
            paired_only.status.as_str(),
            paired_only.recorded,
            paired_only.attributed
        ),
        ("applying", 0, 0)
    );

    run_all(&db, TEARDOWN).await;
    apply_plan(&db, plan_id, None)
        .await
        .expect("the resumed apply completes");
    assert_eq!(standing(&db, plan_id, stream).await, applied());
    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: every batch commits, then marking the plan applied fails.
/// Expected behaviour: the retry attributes nothing again, and queues the attribution once.
#[tokio::test]
#[serial]
async fn test_apply_resumes_after_the_history_but_before_the_finish() {
    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    run_all(&db, FINISH_FAULT).await;

    assert!(apply_plan(&db, plan_id, None).await.is_err());
    let all = i64::try_from(READINGS + 1).unwrap();
    let unfinished = standing(&db, plan_id, stream).await;
    assert_eq!(
        unfinished,
        Standing {
            status: "applying".to_string(),
            attribution_jobs: 0,
            ..applied()
        },
        "the history committed, the finish did not"
    );
    assert_eq!(unfinished.decisions, all);

    run_all(&db, TEARDOWN).await;
    apply_plan(&db, plan_id, None)
        .await
        .expect("the resumed apply completes");
    assert_eq!(standing(&db, plan_id, stream).await, applied());
    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: two workers resume the same applying plan at once.
/// Expected behaviour: the plan's lock takes their batches one at a time, so the history is
/// attributed once and the attribution queued once, whichever of them finishes it.
#[tokio::test]
#[serial]
async fn test_concurrent_resumes_attribute_the_history_once() {
    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    run_all(&db, READING_FAULT).await;
    exec(&db, "INSERT INTO batch_fault VALUES ('-infinity')").await;
    assert!(apply_plan(&db, plan_id, None).await.is_err());
    run_all(&db, TEARDOWN).await;

    let (one, two) = tokio::join!(
        apply_plan(&db, plan_id, None),
        apply_plan(&db, plan_id, None)
    );
    assert!(
        one.is_ok() || two.is_ok(),
        "one of them finishes: {one:?} / {two:?}"
    );
    assert_eq!(standing(&db, plan_id, stream).await, applied());
    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the source sends new readings while an applying plan resumes its backfill.
/// Expected behaviour: the stream is paired already, so the ingest attributes what it writes and
/// the backfill attributes the history; each reading is attributed and recorded once.
#[tokio::test]
#[serial]
async fn test_ingest_during_a_resumed_backfill_is_attributed_once() {
    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    run_all(&db, READING_FAULT).await;
    exec(&db, "INSERT INTO batch_fault VALUES ('-infinity')").await;
    assert!(apply_plan(&db, plan_id, None).await.is_err());
    run_all(&db, TEARDOWN).await;

    let arriving = [
        ("2025-01-01T00:00:00Z", 1.0),
        ("2025-01-01T00:10:00Z", 2.0),
    ];
    let stream_id = stream.to_string();
    let (resumed, _) = tokio::join!(
        apply_plan(&db, plan_id, None),
        crate::common::e2e::ingest(&app, &token, &stream_id, &arriving)
    );
    resumed.expect("the resumed apply completes");

    let history = i64::try_from(READINGS + 1).unwrap();
    let now = standing(&db, plan_id, stream).await;
    assert_eq!(now.attributed, history + 2, "{now:?}");
    assert_eq!(now.duplicated, 0, "{now:?}");
    assert_eq!(now.recorded, history, "the plan counts only its history");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM readings r \
                 WHERE r.stream_id = '{stream}' AND r.time < '2025-01-01' \
                   AND NOT EXISTS (SELECT 1 FROM reading_decisions d \
                     WHERE d.stream_id = r.stream_id AND d.time = r.time \
                       AND d.replicate_index = r.replicate_index)"
            ),
        )
        .await,
        0,
        "every reading of the history has its attribution on the ledger"
    );
    crate::common::cleanup_test_db(&db).await;
}

/// The apply job's row: status, what its report counts, and its bar.
async fn apply_job(
    db: &DatabaseConnection,
    job_id: Uuid,
) -> (String, serde_json::Value, Option<i32>, Option<i32>) {
    use sea_orm::EntityTrait;
    let row = river_db::routes::private::reprocessing_jobs::models::job::Entity::find_by_id(job_id)
        .one(db)
        .await
        .unwrap()
        .expect("the apply job");
    (
        row.status,
        row.detail["counts"].clone(),
        row.progress,
        row.total,
    )
}

/// Scenario: the apply job fails in its second batch, and its retry finishes the history.
/// Expected behaviour: the failed run's row shows what it committed while later batches remain,
/// the retry starts from that count, and the job completes only once the plan is applied.
#[tokio::test]
#[serial]
async fn test_apply_job_reports_committed_history_across_a_retry() {
    use river_db::routes::private::reprocessing_jobs::service::{self as jobs, RetryPolicy};

    let db = crate::common::setup_test_db().await;
    let (plan_id, stream) = seeded_plan(&db).await;
    run_all(&db, READING_FAULT).await;
    exec(
        &db,
        &format!(
            "INSERT INTO batch_fault VALUES ({})",
            nth_instant(BACKFILL_BATCH + 100)
        ),
    )
    .await;
    let job_id = jobs::enqueue(
        &db,
        "plan_apply",
        None,
        Some(plan_id),
        &serde_json::json!({ "plan_id": plan_id }),
        None,
    )
    .await
    .unwrap()
    .expect("a fresh enqueue inserts a row");
    let registry = jobs::build_registry();
    let events = tokio::sync::broadcast::channel(256).0;
    let policy = RetryPolicy {
        max_retries: 2,
        backoff_base: std::time::Duration::ZERO,
    };
    jobs::run_one_with_policy(&db, &events, &registry, &jobs::worker_id(), policy)
        .await
        .unwrap();

    let first_batch = BACKFILL_BATCH + 1;
    let (status, counts, done, streams) = apply_job(&db, job_id).await;
    assert_eq!(status, "queued", "the failed run is retried, not completed");
    assert_eq!(
        counts,
        serde_json::json!({
            "readings_backfilled": first_batch,
            "streams_backfilled": 0,
            "streams_to_backfill": 1,
        }),
        "the row holds what the first batch committed"
    );
    assert_eq!((done, streams), (Some(0), Some(1)));
    assert_eq!(
        standing(&db, plan_id, stream).await.status,
        "applying",
        "the plan is not applied while history remains"
    );

    run_all(&db, TEARDOWN).await;
    jobs::run_one_with_policy(&db, &events, &registry, &jobs::worker_id(), policy)
        .await
        .unwrap();

    let (status, counts, done, streams) = apply_job(&db, job_id).await;
    assert_eq!(status, "completed");
    assert_eq!(counts["readings_backfilled"], serde_json::json!(READINGS + 1));
    assert_eq!((done, streams), (Some(1), Some(1)));
    assert_eq!(standing(&db, plan_id, stream).await, applied());
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM reprocessing_job_logs WHERE job_id = '{job_id}' \
                 AND message = 'Attributing the history of 1 streams; {first_batch} readings already committed'"
            ),
        )
        .await,
        1,
        "the retry says where it resumed"
    );
    crate::common::cleanup_test_db(&db).await;
}
