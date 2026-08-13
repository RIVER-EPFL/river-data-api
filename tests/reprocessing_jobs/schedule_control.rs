//! Operator control of recurring-job schedules, driven as the person the dashboard puts in front of
//! them (`river-data-ui/src/routes/system/+page.svelte`).
//!
//! Scenario: an operator edits a schedule's overlap and catchup policies, disables and re-enables
//! it, and fires one run off-cadence.
//! Expected behaviour: an unknown policy value is refused without touching the row, an accepted edit
//! applies and is audited with the fields the operator did not touch carried into the snapshot, a
//! disabled schedule never fires however overdue it is, and a manual run reaches the worker and
//! streams its own completion on `/api/events`.
//!
//! Two things have no HTTP surface and are therefore driven directly. Schedule rows are seeded from
//! the job registry at startup, so the probe schedules go through the production
//! `seed_default_schedules` path rather than a hand-written INSERT. And nothing exposes the passage
//! of time, so a slot is made overdue with a `next_run_at` UPDATE, pinned to a fixed instant so the
//! dedupe key the tick derives from it is known. Every operator action itself goes through HTTP as a
//! real Keycloak user, at the lowest role that should be able to perform it.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use chrono::Utc;
use http_body_util::BodyExt;
use river_db::routes::private::reprocessing_jobs::job::{Job, JobRegistry};
use river_db::routes::private::reprocessing_jobs::lifecycle::JobContext;
use river_db::routes::private::reprocessing_jobs::schedule::Schedule;
use river_db::routes::private::reprocessing_jobs::scheduler;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::{Value, json};
use serial_test::serial;
use tower::ServiceExt;
use uuid::Uuid;

use crate::common::keycloak as kc;
use crate::common::tracks;

/// One probe job name per test: the suite shares a database, so counting rows by `trigger_type`
/// only means something when the name belongs to exactly one test.
const REJECT_PROBE: &str = "sched_ctl_reject";
const OVERLAP_PROBE: &str = "sched_ctl_overlap";
const CATCHUP_PROBE: &str = "sched_ctl_catchup";
const ENABLED_PROBE: &str = "sched_ctl_enabled";

const PROBE_INTERVAL: i64 = 300;

/// A fixed past slot, so the dedupe key the scheduler derives from it (`{job}:{unix_seconds}`) is
/// predictable, and so the slot is unambiguously a *missed* one (many intervals back) rather than
/// merely due.
const MISSED_SLOT: &str = "2020-01-01T00:00:00Z";
const MISSED_SLOT_EPOCH: i64 = 1_577_836_800;

/// A job that is registered and schedulable but does nothing, so a tick's decision to enqueue is
/// the only thing under observation.
struct ProbeService {
    name: &'static str,
    interval_secs: i64,
}

#[async_trait]
impl Job for ProbeService {
    fn name(&self) -> &'static str {
        self.name
    }
    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_secs))
    }
    async fn run(&self, _ctx: JobContext) -> Result<i64, sea_orm::DbErr> {
        Ok(0)
    }
}

async fn setup(test_name: &str) -> Option<(DatabaseConnection, Router)> {
    if !kc::require_keycloak_or_skip(test_name).await {
        return None;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    Some((db, app))
}

/// Seed the probe's schedule row through the same path startup uses, and hand back the registry the
/// scheduler ticks against.
async fn seed_probe(db: &DatabaseConnection, name: &'static str) -> Arc<JobRegistry> {
    let mut registry = JobRegistry::new();
    registry.register(Arc::new(ProbeService {
        name,
        interval_secs: PROBE_INTERVAL,
    }));
    let registry = Arc::new(registry);
    scheduler::seed_default_schedules(db, &registry)
        .await
        .expect("seed the probe's schedule row");
    registry
}

async fn manager_jwt() -> String {
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    kc::get_keycloak_jwt("manager1", "manager1").await
}

async fn river_jwt() -> String {
    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::get_keycloak_jwt("river1", "river1").await
}

async fn intern_jwt() -> String {
    kc::ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    kc::get_keycloak_jwt("intern1", "intern1").await
}

async fn patch_schedule(app: &Router, job: &str, body: &Value, jwt: &str) -> (u16, String) {
    crate::common::patch_json_with_token(app, &format!("/api/schedules/{job}"), body, jwt).await
}

async fn audit_entries(app: &Router, job: &str, jwt: &str) -> Vec<Value> {
    let (status, body) =
        crate::common::get_json_with_token(app, &format!("/api/schedules/{job}/audit"), jwt).await;
    assert_eq!(status, 200, "the audit trail is readable: {body}");
    body.as_array()
        .unwrap_or_else(|| panic!("the audit trail is a list: {body}"))
        .clone()
}

/// Simulate elapsed time: no endpoint moves a schedule's next run into the past.
async fn set_slot(db: &DatabaseConnection, job: &str, at: &str) {
    crate::common::exec(
        db,
        &format!("UPDATE schedules SET next_run_at = '{at}' WHERE job_name = '{job}'"),
    )
    .await;
}

async fn next_run_at(db: &DatabaseConnection, job: &str) -> chrono::DateTime<Utc> {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT next_run_at FROM schedules WHERE job_name = $1",
        [job.into()],
    ))
    .await
    .expect("query schedules")
    .expect("the probe's schedule row exists")
    .try_get::<chrono::DateTime<Utc>>("", "next_run_at")
    .expect("next_run_at is set")
}

/// A `schedules` column no endpoint exposes as a raw persisted value, read to prove an edit landed
/// in the table rather than only in the response body.
async fn persisted(db: &DatabaseConnection, job: &str, column: &str) -> Option<String> {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT {column} AS v FROM schedules WHERE job_name = $1"),
        [job.into()],
    ))
    .await
    .expect("query schedules")
    .expect("the probe's schedule row exists")
    .try_get::<Option<String>>("", "v")
    .expect("column readable")
}

async fn count(db: &DatabaseConnection, sql: &str, param: &str) -> i64 {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        [param.into()],
    ))
    .await
    .expect("count query")
    .expect("count row")
    .try_get::<i64>("", "n")
    .expect("n")
}

/// Wait until nothing is in flight. Onboarding a sensor enqueues its own jobs (identity
/// calibration, deployment), some of which refresh continuous aggregates; letting them settle
/// before the readings arrive is what makes "the bucket is empty" a fact rather than a race.
async fn drain_job_queue(db: &DatabaseConnection, max_secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let active = crate::common::e2e::count(
            db,
            "SELECT count(*) AS n FROM reprocessing_jobs \
             WHERE status IN ('queued', 'pending', 'running', 'retrying')",
        )
        .await;
        if active == 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the onboarding's jobs did not settle within {max_secs}s ({active} still in flight)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn job_rows(db: &DatabaseConnection, trigger: &str) -> i64 {
    count(
        db,
        "SELECT count(*) AS n FROM reprocessing_jobs WHERE trigger_type = $1",
        trigger,
    )
    .await
}

async fn audit_rows(db: &DatabaseConnection, job: &str) -> i64 {
    count(
        db,
        "SELECT count(*) AS n FROM schedule_audit WHERE job_name = $1",
        job,
    )
    .await
}

/// `dedupe_key` and `params` are job columns the API does not serialise, so the enqueue's identity
/// and its snapshotted inputs are only checkable here.
async fn dedupe_keys(db: &DatabaseConnection, trigger: &str) -> Vec<String> {
    db.query_all(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT dedupe_key FROM reprocessing_jobs WHERE trigger_type = $1",
        [trigger.into()],
    ))
    .await
    .expect("query reprocessing_jobs")
    .iter()
    .filter_map(|r| r.try_get::<Option<String>>("", "dedupe_key").ok().flatten())
    .collect()
}

async fn job_params(db: &DatabaseConnection, job_id: &str) -> Value {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT params FROM reprocessing_jobs WHERE id = $1",
        [Uuid::parse_str(job_id).expect("job id is a uuid").into()],
    ))
    .await
    .expect("query reprocessing_jobs")
    .expect("the enqueued job row exists")
    .try_get::<Value>("", "params")
    .expect("params")
}

fn parse_sse_frames(text: &str) -> Vec<(String, Value)> {
    let mut frames = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(ev) = line.strip_prefix("event:") {
            current = Some(ev.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data:")
            && let Some(ev) = current.take()
            && let Ok(json) = serde_json::from_str::<Value>(data.trim())
        {
            frames.push((ev, json));
        }
    }
    frames
}

/// Read SSE frames off an open streaming body until `wanted` matches, then return everything
/// received. Panics with the raw stream contents rather than returning early, so a frame that never
/// arrives fails the test instead of skipping its assertions.
async fn read_frames_until(
    body: &mut Body,
    max_secs: u64,
    wanted: impl Fn(&str, &Value) -> bool,
) -> Vec<(String, Value)> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    let mut accumulated = String::new();
    loop {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    accumulated.push_str(&String::from_utf8_lossy(data));
                    let frames = parse_sse_frames(&accumulated);
                    if frames.iter().any(|(ev, d)| wanted(ev, d)) {
                        return frames;
                    }
                }
            }
            Ok(Some(Err(e))) => panic!("SSE frame error: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    panic!("the awaited SSE frame never arrived. stream so far:\n{accumulated}");
}

#[tokio::test]
#[serial]
async fn unknown_policy_values_are_rejected_and_leave_the_schedule_unchanged() {
    let Some((db, app)) = setup("unknown_policy_values_are_rejected").await else {
        return;
    };
    let manager = manager_jwt().await;
    let river = river_jwt().await;
    let intern = intern_jwt().await;
    seed_probe(&db, REJECT_PROBE).await;

    let (status, body) = patch_schedule(
        &app,
        REJECT_PROBE,
        &json!({ "overlap_policy": "sometimes" }),
        &manager,
    )
    .await;
    assert_eq!(status, 400, "an unknown overlap_policy is refused: {body}");
    assert!(
        body.contains("allow_concurrent"),
        "the refusal names the values the operator may choose: {body}"
    );

    let (status, body) = patch_schedule(
        &app,
        REJECT_PROBE,
        &json!({ "catchup_policy": "eventually" }),
        &manager,
    )
    .await;
    assert_eq!(status, 400, "an unknown catchup_policy is refused: {body}");
    assert!(
        body.contains("run_once"),
        "the refusal names the values the operator may choose: {body}"
    );

    // River sits one level below the manager that `require_manage_sensors` asks for.
    let (status, body) =
        patch_schedule(&app, REJECT_PROBE, &json!({ "enabled": false }), &river).await;
    assert_eq!(
        status, 403,
        "editing a schedule is a manager action, not a river one: {body}"
    );

    // Reading is open to any member, and shows the row untouched by all three refusals.
    let (status, view) = crate::common::get_json_with_token(
        &app,
        &format!("/api/schedules/{REJECT_PROBE}"),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "inspecting a schedule needs only read_metadata: {view}"
    );
    assert_eq!(
        view["overlap_policy"], "skip_if_running",
        "the refused overlap edit did not land: {view}"
    );
    assert_eq!(
        view["catchup_policy"], "run_once",
        "the refused catchup edit did not land: {view}"
    );
    assert_eq!(
        view["enabled"], true,
        "the forbidden disable did not land: {view}"
    );
    assert_eq!(
        view["interval_seconds"], PROBE_INTERVAL,
        "the seeded cadence is intact: {view}"
    );

    assert_eq!(
        persisted(&db, REJECT_PROBE, "overlap_policy")
            .await
            .as_deref(),
        Some("skip_if_running"),
        "and the table agrees with the view"
    );
    assert_eq!(
        audit_rows(&db, REJECT_PROBE).await,
        0,
        "a refused edit writes no audit row"
    );
    assert!(
        audit_entries(&app, REJECT_PROBE, &intern).await.is_empty(),
        "and the audit endpoint reports none"
    );
}

#[tokio::test]
#[serial]
async fn allow_concurrent_lets_a_due_slot_enqueue_while_a_run_is_in_flight() {
    let Some((db, app)) = setup("allow_concurrent_enqueues").await else {
        return;
    };
    let manager = manager_jwt().await;
    let registry = seed_probe(&db, OVERLAP_PROBE).await;

    // A previous run of this job is still in flight. Nothing creates a job of an arbitrary
    // trigger_type over HTTP, and this row is the precondition the overlap policy is about. Its
    // NULL lease keeps the test worker's reaper away from it.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category) \
             VALUES ('{}', '{OVERLAP_PROBE}', 'running', 'maintenance')",
            Uuid::new_v4()
        ),
    )
    .await;

    set_slot(&db, OVERLAP_PROBE, MISSED_SLOT).await;
    assert_eq!(
        scheduler::tick(&db, &registry).await.expect("tick"),
        0,
        "the seeded skip_if_running policy suppresses the enqueue while a run is in flight"
    );
    assert_eq!(
        job_rows(&db, OVERLAP_PROBE).await,
        1,
        "only the in-flight run exists before the policy is changed"
    );

    let (status, body) = patch_schedule(
        &app,
        OVERLAP_PROBE,
        &json!({ "overlap_policy": "allow_concurrent" }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "a manager may relax the overlap policy: {body}"
    );
    let view: Value = serde_json::from_str(&body).expect("the PATCH returns the updated schedule");
    assert_eq!(
        view["overlap_policy"], "allow_concurrent",
        "the response carries the new policy: {view}"
    );
    assert_eq!(
        persisted(&db, OVERLAP_PROBE, "overlap_policy")
            .await
            .as_deref(),
        Some("allow_concurrent"),
        "and it is persisted, not merely echoed"
    );
    assert!(
        view["updated_by"]
            .as_str()
            .is_some_and(|s| s.contains("manager1")),
        "the edit is stamped with the person who made it: {view}"
    );

    set_slot(&db, OVERLAP_PROBE, MISSED_SLOT).await;
    assert_eq!(
        scheduler::tick(&db, &registry).await.expect("tick"),
        1,
        "allow_concurrent enqueues the due slot even though the earlier run is still active"
    );
    assert_eq!(
        job_rows(&db, OVERLAP_PROBE).await,
        2,
        "the in-flight run plus the newly enqueued one"
    );
    assert!(
        dedupe_keys(&db, OVERLAP_PROBE)
            .await
            .contains(&format!("{OVERLAP_PROBE}:{MISSED_SLOT_EPOCH}")),
        "the enqueue is keyed on the scheduled slot, so two replicas racing it collapse to one job"
    );

    let entries = audit_entries(&app, OVERLAP_PROBE, &manager).await;
    assert_eq!(
        entries.len(),
        1,
        "one accepted edit leaves one audit row: {entries:?}"
    );
    assert_eq!(
        entries[0]["old_value"]["overlap_policy"], "skip_if_running",
        "the audit row holds the pre-image: {entries:?}"
    );
    assert_eq!(
        entries[0]["new_value"]["overlap_policy"], "allow_concurrent",
        "and the post-image: {entries:?}"
    );
    assert_eq!(
        entries[0]["new_value"]["interval_seconds"], PROBE_INTERVAL,
        "fields the operator did not touch are carried into the snapshot: {entries:?}"
    );
    assert_eq!(
        entries[0]["new_value"]["catchup_policy"], "run_once",
        "including the policy that was left alone: {entries:?}"
    );
    assert!(
        entries[0]["changed_by"]
            .as_str()
            .is_some_and(|s| s.contains("manager1")),
        "the audit row names the person, not a token: {entries:?}"
    );
}

/// Expected behaviour is taken from the `CatchupPolicy::Skip` contract in
/// `reprocessing_jobs/schedule.rs`: "Skip entirely; wait for the next scheduled slot." A slot years
/// behind a five-minute cadence is a backlog left by downtime, the case that policy names.
#[tokio::test]
#[serial]
async fn catchup_skip_is_persisted_audited_and_suppresses_a_missed_slot() {
    let Some((db, app)) = setup("catchup_skip_suppresses_a_missed_slot").await else {
        return;
    };
    let manager = manager_jwt().await;
    let registry = seed_probe(&db, CATCHUP_PROBE).await;

    let (status, body) = patch_schedule(
        &app,
        CATCHUP_PROBE,
        &json!({ "catchup_policy": "skip" }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "a manager may choose the catchup policy: {body}"
    );
    let view: Value = serde_json::from_str(&body).expect("the PATCH returns the updated schedule");
    assert_eq!(
        view["catchup_policy"], "skip",
        "the response carries the new policy: {view}"
    );
    assert_eq!(
        view["overlap_policy"], "skip_if_running",
        "the policy the operator did not touch is unchanged: {view}"
    );
    assert_eq!(
        persisted(&db, CATCHUP_PROBE, "catchup_policy")
            .await
            .as_deref(),
        Some("skip"),
        "the edit is persisted, not merely echoed"
    );

    let entries = audit_entries(&app, CATCHUP_PROBE, &manager).await;
    assert_eq!(entries.len(), 1, "one edit, one audit row: {entries:?}");
    assert_eq!(
        entries[0]["old_value"]["catchup_policy"], "run_once",
        "the seeded default is the pre-image: {entries:?}"
    );
    assert_eq!(
        entries[0]["new_value"]["catchup_policy"], "skip",
        "the post-image is the operator's choice: {entries:?}"
    );
    assert_eq!(
        entries[0]["new_value"]["overlap_policy"], "skip_if_running",
        "the untouched policy is carried into the snapshot: {entries:?}"
    );

    set_slot(&db, CATCHUP_PROBE, MISSED_SLOT).await;
    assert_eq!(
        scheduler::tick(&db, &registry).await.expect("tick"),
        0,
        "catchup_policy 'skip' waits for the next scheduled slot instead of firing the missed one"
    );
    assert_eq!(
        job_rows(&db, CATCHUP_PROBE).await,
        0,
        "and no job is created for the missed slot"
    );
    assert!(
        next_run_at(&db, CATCHUP_PROBE).await > Utc::now(),
        "the cadence grid still resyncs forward, so the schedule is not stuck in the past"
    );
}

#[tokio::test]
#[serial]
async fn disabling_a_schedule_halts_the_tick_and_re_enabling_resets_the_grid() {
    let Some((db, app)) = setup("disable_halts_the_tick").await else {
        return;
    };
    let manager = manager_jwt().await;
    let registry = seed_probe(&db, ENABLED_PROBE).await;

    let seeded_slot = next_run_at(&db, ENABLED_PROBE).await;

    let (status, body) =
        patch_schedule(&app, ENABLED_PROBE, &json!({ "enabled": false }), &manager).await;
    assert_eq!(status, 200, "a manager may disable a schedule: {body}");
    let view: Value = serde_json::from_str(&body).expect("the PATCH returns the updated schedule");
    assert_eq!(
        view["enabled"], false,
        "the schedule reports disabled: {view}"
    );
    assert_eq!(
        next_run_at(&db, ENABLED_PROBE).await,
        seeded_slot,
        "disabling alone must not move the cadence grid"
    );

    set_slot(&db, ENABLED_PROBE, MISSED_SLOT).await;
    assert_eq!(
        scheduler::tick(&db, &registry).await.expect("tick"),
        0,
        "a disabled schedule is never claimed, however overdue its slot is"
    );
    assert_eq!(
        job_rows(&db, ENABLED_PROBE).await,
        0,
        "and nothing was enqueued while it was off"
    );

    let (status, body) =
        patch_schedule(&app, ENABLED_PROBE, &json!({ "enabled": true }), &manager).await;
    assert_eq!(status, 200, "a manager may re-enable a schedule: {body}");
    let view: Value = serde_json::from_str(&body).expect("the PATCH returns the updated schedule");
    assert_eq!(
        view["enabled"], true,
        "the schedule reports enabled: {view}"
    );

    let resumed = next_run_at(&db, ENABLED_PROBE).await;
    let expected = Utc::now() + chrono::Duration::seconds(PROBE_INTERVAL);
    assert!(
        resumed > Utc::now(),
        "re-enabling snaps the grid forward instead of honouring the stale past slot (got {resumed})"
    );
    assert!(
        (resumed - expected).num_seconds().abs() < 10,
        "the resumed slot is one interval out from now (got {resumed}, expected about {expected})"
    );
    assert_eq!(
        scheduler::tick(&db, &registry).await.expect("tick"),
        0,
        "so nothing is due in the instant after the re-enable"
    );
    assert_eq!(
        job_rows(&db, ENABLED_PROBE).await,
        0,
        "and the backlog the disable covered is not replayed"
    );

    let entries = audit_entries(&app, ENABLED_PROBE, &manager).await;
    assert_eq!(entries.len(), 2, "both toggles are audited: {entries:?}");
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["old_value"]["enabled"] == true && e["new_value"]["enabled"] == false)
            .count(),
        1,
        "exactly one row records the disable: {entries:?}"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["old_value"]["enabled"] == false && e["new_value"]["enabled"] == true)
            .count(),
        1,
        "exactly one row records the re-enable: {entries:?}"
    );
    assert!(
        entries.iter().all(|e| e["changed_by"]
            .as_str()
            .is_some_and(|s| s.contains("manager1"))),
        "both rows name the person who toggled it: {entries:?}"
    );
}

/// `backfill_attribution` is on-demand only, so it has no `schedules` row: that is what makes the
/// empty tunables snapshot and the empty audit trail meaningful here. It is also in the worker's
/// registry, so the manual run genuinely reaches a handler instead of terminal-failing.
#[tokio::test]
#[serial]
async fn run_now_dedupes_within_the_second_snapshots_empty_tunables_and_writes_no_audit_row() {
    const JOB: &str = "backfill_attribution";

    let Some((db, app)) = setup("run_now_dedupes_within_the_second").await else {
        return;
    };
    let manager = manager_jwt().await;
    let river = river_jwt().await;
    // The job queue is read as an administrator: `inject_read_scope` confines a granted member's
    // `/reprocessing_jobs` reads to their own sensors, and these rows carry no sensor.
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/schedules/{JOB}/run_now"),
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "firing a job by hand is a manager action, not a river one: {body}"
    );
    assert_eq!(
        job_rows(&db, JOB).await,
        0,
        "and the refused call enqueued nothing"
    );

    // Both calls must land in the same wall-clock second for the per-second dedupe key to collide,
    // so start just past a second boundary. This waits on a clock edge, not on work finishing.
    let into_second = u64::from(Utc::now().timestamp_subsec_millis());
    tokio::time::sleep(std::time::Duration::from_millis(1_050 - into_second)).await;
    let second = Utc::now().timestamp();

    let (status, first) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/schedules/{JOB}/run_now"),
        &json!({}),
        &manager,
    )
    .await;
    let (status_repeat, repeat) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/schedules/{JOB}/run_now"),
        &json!({}),
        &manager,
    )
    .await;
    assert_eq!(
        Utc::now().timestamp(),
        second,
        "both run_now calls have to fall inside one second for the dedupe key to be exercised"
    );

    assert_eq!(status, 200, "a manager may fire a known job now: {first}");
    assert_eq!(first["enqueued"], true, "the first call enqueues: {first}");
    let job_id = first["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an enqueued run returns its job id: {first}"))
        .to_string();

    assert_eq!(
        status_repeat, 200,
        "the repeat call is not an error: {repeat}"
    );
    assert_eq!(
        repeat["enqueued"], false,
        "a double click in the same second collapses to one run: {repeat}"
    );
    assert!(
        repeat["job_id"].is_null(),
        "and reports no new job id: {repeat}"
    );
    assert_eq!(
        job_rows(&db, JOB).await,
        1,
        "exactly one job row exists for the two calls"
    );
    assert_eq!(
        dedupe_keys(&db, JOB).await,
        vec![format!("{JOB}:run_now:{second}")],
        "the run is keyed on the job and the second it was asked for"
    );

    let params = job_params(&db, &job_id).await;
    assert_eq!(
        params["trigger"], "run_now",
        "the row records that a person asked for this run: {params}"
    );
    assert_eq!(
        params["tunables"],
        json!({}),
        "a job with no schedule row snapshots empty tunables rather than failing: {params}"
    );

    assert!(
        audit_entries(&app, JOB, &manager).await.is_empty(),
        "the schedule audit trail records configuration edits, and a manual run is not one"
    );
    assert_eq!(
        crate::common::e2e::count(&db, "SELECT count(*) AS n FROM schedule_audit").await,
        0,
        "and no audit row was written against any schedule"
    );

    assert_eq!(
        crate::common::e2e::poll_job(&app, &admin, &job_id, 30).await,
        "completed",
        "the worker picks the manual run up and finishes it"
    );
    let (status, job) = crate::common::get_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the finished job is readable: {job}");
    assert_eq!(
        job["readings_updated"], 0,
        "with no slots to backfill the run touches no readings: {job}"
    );
    assert!(
        job["error_message"].is_null(),
        "and reports no error: {job}"
    );
}

/// Every other `/api/events` test injects a synthetic `AppEvent`. This one watches the frames a
/// genuine worker transition emits, for a run an operator asked for over HTTP, on data onboarded
/// through the sensor-flow track.
///
/// `refresh_aggregates_full` is the job under test because its effect is checkable: the incremental
/// variant only covers the last 24 hours, and the track's readings are past-dated.
#[tokio::test]
#[serial]
async fn sse_streams_the_completion_of_an_operator_run_that_did_real_work() {
    let Some((db, app)) = setup("sse_streams_a_real_completion").await else {
        return;
    };
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let manager = manager_jwt().await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let stream_id = track.stream_ids[0].clone();
    let site_parameter_id = track.site_parameter_ids[0].clone();
    let parameter_id = track.parameter_id("TrkFlowDO").to_string();

    // Pairing before any reading arrives means ingest attributes at write time and no backfill job
    // is enqueued, so the aggregate stays unmaterialised until the operator's run.
    let (status, paired) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": site_parameter_id }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair the stream to the slot ({status}): {paired}"
    );
    drain_job_queue(&db, 60).await;

    let (status, ingested) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({ "stream_id": stream_id, "readings": tracks::flow_cycle_readings(0) }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "ingest one cycle ({status}): {ingested}");
    assert_eq!(
        ingested["inserted"], 5,
        "the cycle's five readings land: {ingested}"
    );
    assert_eq!(
        ingested["paired"], true,
        "and land already attributed to the slot: {ingested}"
    );

    let bucket_at: chrono::DateTime<Utc> = format!("{}T00:00:00Z", tracks::FLOW_BASE_DAY)
        .parse()
        .expect("the track's base day parses");
    assert!(
        crate::common::e2e::hourly_bucket(&db, &track.site_id, &parameter_id, bucket_at)
            .await
            .is_none(),
        "the hourly bucket holds nothing until something refreshes it"
    );

    // The feed is opened as an administrator: job events carry no project, so `event_stream` only
    // forwards them to an unrestricted principal.
    let stream_request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {admin}"))
        .header("Accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(stream_request).await.unwrap();
    assert_eq!(
        response.status().as_u16(),
        200,
        "an operator can open the event stream"
    );
    let mut stream_body = response.into_body();

    let (status, run) = crate::common::post_json_parse_with_token(
        &app,
        "/api/schedules/refresh_aggregates_full/run_now",
        &json!({}),
        &manager,
    )
    .await;
    assert_eq!(status, 200, "run_now ({status}): {run}");
    assert_eq!(run["enqueued"], true, "the run is enqueued: {run}");
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an enqueued run returns its job id: {run}"))
        .to_string();

    let frames = read_frames_until(&mut stream_body, 60, |event, data| {
        event == "job_completed" && data["job_id"] == job_id.as_str()
    })
    .await;

    let completed = &frames
        .iter()
        .find(|(event, data)| event == "job_completed" && data["job_id"] == job_id.as_str())
        .expect("the awaited frame is in the returned set")
        .1;
    assert_eq!(
        completed["type"], "job_completed",
        "the frame's payload names its own type: {completed}"
    );
    assert_eq!(
        completed["status"], "completed",
        "the run finished rather than failing: {completed}"
    );
    assert_eq!(
        completed["readings_updated"], 0,
        "an aggregate refresh rewrites no readings: {completed}"
    );
    assert!(
        completed["error_message"].is_null(),
        "and carries no error: {completed}"
    );
    assert!(
        !frames
            .iter()
            .any(|(event, data)| event == "job_created" && data["job_id"] == job_id.as_str()),
        "a worker-claimed job emits only its terminal event: {frames:?}"
    );

    let (status, job) = crate::common::get_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the finished job is readable: {job}");
    assert_eq!(
        job["status"], "completed",
        "the stored row and the streamed frame agree: {job}"
    );
    assert_eq!(
        job["trigger_type"], "refresh_aggregates_full",
        "the row is the run the operator asked for: {job}"
    );

    // A completed status is not evidence a refresh happened: aggregate refresh failures are only
    // warn-logged (common/sync_state.rs). The bucket's values are.
    let bucket =
        crate::common::e2e::hourly_bucket(&db, &track.site_id, &parameter_id, bucket_at).await;
    assert!(
        bucket.is_some(),
        "the operator's run materialised the hourly bucket for the ingested cycle"
    );
    let (mean, readings_in_bucket) = bucket.expect("bucket present");
    assert_eq!(
        readings_in_bucket, 5,
        "every reading of the cycle is counted"
    );
    assert!(
        (mean - 202.0).abs() < 1e-9,
        "the cycle's values 200..204 average 202, got {mean}"
    );
}
