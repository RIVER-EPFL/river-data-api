//! `GET /api/reprocessing_jobs/{id}/logs`, the job timeline the dashboard tails while a job runs.
//!
//! Scenario: an operator watches a tracked job's timeline, re-polling with the highest `seq`
//! already on screen.
//! Expected behaviour: the feed comes back ordered by `seq`, `after_seq` returns strictly later
//! lines only, `limit` bounds the page, one job's lines never surface under another job's id, and
//! the feed sits behind `read_data` rather than plain metadata read.
//!
//! Base state is the CSV onboarding track: the project, site and parameters are provisioned over
//! HTTP as a real Keycloak user, and the CSV dump's own `csv_import` job is read alongside the
//! probes. No endpoint writes a timeline line, the job lifecycle does, so the probe timelines go in
//! through `run_tracked_job`, the same writer every registered job uses.

use std::sync::{Arc, Mutex};

use axum::Router;
use river_db::routes::private::reprocessing_jobs::lifecycle::run_tracked_job;
use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::keycloak as kc;
use crate::common::tracks;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

/// Run a tracked job that writes `lines` to its timeline, returning the job's id.
///
/// The id is lifted out of the `JobContext` because `run_tracked_job` hands back the work's count,
/// not the row it created.
async fn write_job_timeline(
    db: &DatabaseConnection,
    trigger_type: &str,
    site_id: Uuid,
    lines: Vec<(&'static str, String, serde_json::Value)>,
) -> Uuid {
    let captured: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    let written = lines.len() as i64;

    run_tracked_job(db, None, trigger_type, None, events(), move |ctx| async move {
        *sink.lock().unwrap() = Some(ctx.job_id());
        ctx.set_site(site_id).await;
        for (level, message, context) in &lines {
            ctx.log(level, message, context.clone()).await;
        }
        Ok(written)
    })
    .await
    .unwrap_or_else(|e| panic!("tracked {trigger_type} job runs: {e}"));

    let id = *captured.lock().unwrap();
    id.expect("the job context exposes the id of the row it created")
}

async fn fetch_logs(app: &Router, token: &str, job_id: Uuid, query: &str) -> (u16, String) {
    crate::common::get_with_token(
        app,
        &format!("/api/reprocessing_jobs/{job_id}/logs{query}"),
        token,
    )
    .await
}

fn parse_lines(body: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(body)
        .unwrap_or_else(|e| panic!("job logs must be a JSON array: {e}\nBody: {body}"))
        .as_array()
        .unwrap_or_else(|| panic!("job logs must be a JSON array, got: {body}"))
        .clone()
}

fn seqs(lines: &[serde_json::Value]) -> Vec<i64> {
    lines
        .iter()
        .map(|l| {
            l["seq"]
                .as_i64()
                .unwrap_or_else(|| panic!("every log line carries a numeric seq: {l}"))
        })
        .collect()
}

fn field(lines: &[serde_json::Value], key: &str) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l[key]
                .as_str()
                .unwrap_or_else(|| panic!("every log line carries a string {key}: {l}"))
                .to_string()
        })
        .collect()
}

const PROBE_PREFIX: &str = "reprocess-probe:";
const NEIGHBOUR_PREFIX: &str = "refresh-neighbour:";

/// Track A, provisioned over HTTP, plus its CSV dump imported. Returns the track and the
/// `csv_import` job the import enqueued.
async fn csv_track_with_import(
    app: &Router,
    jwt: &str,
    db: &DatabaseConnection,
) -> (tracks::Track, Uuid) {
    let track = tracks::onboard_csv_track(app, jwt).await;
    let codes: Vec<&str> = track.parameters.iter().map(|(c, _)| c.as_str()).collect();
    let csv = tracks::csv_body(&codes, 6, "2025-06-01");

    let (status, imported) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/import_csv",
        &json!({ "site": track.site_id, "csv": csv, "dry_run": false }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "import Track A's CSV ({status}): {imported}");

    let raw = imported["derived_job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the import enqueues a tracked csv_import job: {imported}"));
    let job_id = Uuid::parse_str(raw).unwrap_or_else(|e| panic!("csv_import job id is a uuid: {e}"));

    assert!(
        crate::common::e2e::wait_for_jobs_by_trigger(db, "csv_import", 30).await,
        "the csv_import job reaches a terminal state without failing"
    );
    (track, job_id)
}

#[tokio::test]
#[serial]
async fn job_log_tail_is_ordered_by_seq_and_returns_only_lines_after_it() {
    if !kc::require_keycloak_or_skip("job_log_tail_ordering").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let (track, csv_job_id) = csv_track_with_import(&app, &jwt, &db).await;
    let site_id = Uuid::parse_str(&track.site_id).expect("track site id is a uuid");
    let parameter_id = track.parameter_id("TrkCsvDepth").to_string();

    let probe = write_job_timeline(
        &db,
        "manual_reprocess",
        site_id,
        vec![
            ("info", format!("{PROBE_PREFIX} started"), json!({})),
            (
                "warn",
                format!("{PROBE_PREFIX} slot skipped"),
                json!({ "parameter_id": parameter_id }),
            ),
            ("info", format!("{PROBE_PREFIX} refreshing aggregates"), json!({})),
            (
                "error",
                format!("{PROBE_PREFIX} aggregate refresh failed"),
                json!({ "aggregate": "readings_hourly" }),
            ),
        ],
    )
    .await;

    let neighbour = write_job_timeline(
        &db,
        "refresh_aggregates",
        site_id,
        vec![
            ("info", format!("{NEIGHBOUR_PREFIX} first"), json!({})),
            ("info", format!("{NEIGHBOUR_PREFIX} second"), json!({})),
        ],
    )
    .await;

    let (status, body) = fetch_logs(&app, &jwt, probe, "").await;
    assert_eq!(status, 200, "the full timeline is served ({status}): {body}");
    let all = parse_lines(&body);
    assert_eq!(all.len(), 4, "every written line is returned exactly once: {body}");
    assert_eq!(seqs(&all), vec![0, 1, 2, 3], "lines come back in seq order: {body}");
    assert_eq!(
        field(&all, "level"),
        vec!["info", "warn", "info", "error"],
        "the alternating levels arrive in the order they were written: {body}"
    );
    assert_eq!(
        field(&all, "message"),
        vec![
            format!("{PROBE_PREFIX} started"),
            format!("{PROBE_PREFIX} slot skipped"),
            format!("{PROBE_PREFIX} refreshing aggregates"),
            format!("{PROBE_PREFIX} aggregate refresh failed"),
        ],
        "messages arrive in the order they were written: {body}"
    );
    assert_eq!(
        all[1]["context"]["parameter_id"].as_str(),
        Some(parameter_id.as_str()),
        "structured context travels with its line: {body}"
    );
    assert_eq!(
        all[3]["context"]["aggregate"], "readings_hourly",
        "structured context travels with its line: {body}"
    );
    for line in &all {
        let ts = line["ts"]
            .as_str()
            .unwrap_or_else(|| panic!("every line carries a timestamp: {line}"));
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("ts is RFC 3339: {e} ({ts})"));
    }

    // The tail: after_seq is exclusive, so the line the operator already has is not resent.
    let (status, body) = fetch_logs(&app, &jwt, probe, "?after_seq=0").await;
    assert_eq!(status, 200, "tail from seq 0 ({status}): {body}");
    let tail = parse_lines(&body);
    assert_eq!(
        seqs(&tail),
        vec![1, 2, 3],
        "after_seq=0 returns strictly later lines, never seq 0 itself: {body}"
    );

    let (status, body) = fetch_logs(&app, &jwt, probe, "?after_seq=1").await;
    assert_eq!(status, 200, "tail from seq 1 ({status}): {body}");
    let tail = parse_lines(&body);
    assert_eq!(seqs(&tail), vec![2, 3], "after_seq=1 skips seq 0 and 1: {body}");
    assert_eq!(
        field(&tail, "level"),
        vec!["info", "error"],
        "the tail keeps each line's own level rather than re-deriving it: {body}"
    );

    let (status, body) = fetch_logs(&app, &jwt, probe, "?after_seq=3").await;
    assert_eq!(status, 200, "tail from the last seq ({status}): {body}");
    assert!(
        parse_lines(&body).is_empty(),
        "a caught-up tail returns nothing rather than replaying the timeline: {body}"
    );

    let (status, body) = fetch_logs(&app, &jwt, probe, "?limit=2").await;
    assert_eq!(status, 200, "limited page ({status}): {body}");
    assert_eq!(
        seqs(&parse_lines(&body)),
        vec![0, 1],
        "limit takes the earliest lines, so paging forward by seq covers the timeline: {body}"
    );

    let (status, body) = fetch_logs(&app, &jwt, probe, "?after_seq=0&limit=1").await;
    assert_eq!(status, 200, "limited tail ({status}): {body}");
    assert_eq!(
        seqs(&parse_lines(&body)),
        vec![1],
        "after_seq and limit compose into one page of the tail: {body}"
    );

    // Scoping: a timeline belongs to one job. Were the job_id predicate lost, each of these would
    // carry the other job's lines too.
    let (status, body) = fetch_logs(&app, &jwt, neighbour, "").await;
    assert_eq!(status, 200, "the neighbouring job's timeline ({status}): {body}");
    let neighbour_lines = parse_lines(&body);
    assert_eq!(
        neighbour_lines.len(),
        2,
        "the neighbouring job returns only its own two lines: {body}"
    );
    assert_eq!(
        seqs(&neighbour_lines),
        vec![0, 1],
        "seq restarts per job rather than being global: {body}"
    );
    assert!(
        !body.contains(PROBE_PREFIX),
        "no line from another job leaks into this timeline: {body}"
    );

    let (status, body) = fetch_logs(&app, &jwt, csv_job_id, "").await;
    assert_eq!(status, 200, "the CSV import job's timeline ({status}): {body}");
    assert!(
        !body.contains(PROBE_PREFIX) && !body.contains(NEIGHBOUR_PREFIX),
        "the import job's timeline holds no other job's lines: {body}"
    );
}

#[tokio::test]
#[serial]
async fn job_log_tail_requires_read_data() {
    if !kc::require_keycloak_or_skip("job_log_tail_gate").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin_jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_csv_track(&app, &admin_jwt).await;
    let site_id = Uuid::parse_str(&track.site_id).expect("track site id is a uuid");
    let probe = write_job_timeline(
        &db,
        "manual_reprocess",
        site_id,
        vec![
            ("info", format!("{PROBE_PREFIX} started"), json!({})),
            ("warn", format!("{PROBE_PREFIX} slot skipped"), json!({})),
        ],
    )
    .await;

    // Intern is the lowest level holding ReadData (Capability::min_role), so an intern reads the
    // timeline and actually gets its content. The job names a site, and a job timeline is confined
    // to the projects the caller is granted, so the grant is what leaves the capability as the
    // thing under test here.
    kc::ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("intern1").await, &track.project_id).await;
    let intern_jwt = kc::get_keycloak_jwt("intern1", "intern1").await;
    let (status, body) = fetch_logs(&app, &intern_jwt, probe, "").await;
    assert_eq!(status, 200, "an intern reads a job timeline ({status}): {body}");
    let lines = parse_lines(&body);
    assert_eq!(
        field(&lines, "message"),
        vec![
            format!("{PROBE_PREFIX} started"),
            format!("{PROBE_PREFIX} slot skipped"),
        ],
        "the intern's 200 carries the real timeline, not an empty feed: {body}"
    );

    // The level below any riverdata role: authenticated is not membership.
    kc::ensure_realm_user("norole", "norole", &[]).await;
    let norole_jwt = kc::get_keycloak_jwt("norole", "norole").await;
    let (status, body) = fetch_logs(&app, &norole_jwt, probe, "").await;
    assert_eq!(status, 403, "a role-less login is refused ({status}): {body}");
    assert!(
        !body.contains(PROBE_PREFIX),
        "a refusal carries no timeline content: {body}"
    );

    let (status, body) = crate::common::get(
        &app,
        &format!("/api/reprocessing_jobs/{probe}/logs"),
    )
    .await;
    assert_eq!(status, 401, "an unauthenticated read is 401, not 403 ({status}): {body}");
    assert!(
        !body.contains(PROBE_PREFIX),
        "a refusal carries no timeline content: {body}"
    );

    // The token axis: the feed is read_data, so the metadata bit alone does not open it.
    let metadata_token = crate::common::seed_token_read_metadata_only(&db).await;
    let (status, body) = fetch_logs(&app, &metadata_token, probe, "").await;
    assert_eq!(
        status, 403,
        "a read_metadata-only token cannot tail job logs ({status}): {body}"
    );
    assert!(
        !body.contains(PROBE_PREFIX),
        "a refusal carries no timeline content: {body}"
    );

    let data_token = crate::common::seed_token_read_data_only(&db).await;
    let (status, body) = fetch_logs(&app, &data_token, probe, "").await;
    assert_eq!(
        status, 200,
        "a read_data token is the token-side equivalent and passes ({status}): {body}"
    );
    assert_eq!(
        parse_lines(&body).len(),
        2,
        "and it receives the same timeline the intern saw: {body}"
    );
}
