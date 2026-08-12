//! The CSV import wizard's staging session, conflict modes and timezone step, driven the way
//! `river-data-ui/src/routes/sites/[id]/import/+page.svelte` drives them.
//!
//! Scenario: an operator uploads a file once, previews it, adjusts the mapping, the timezone and the
//! conflict mode, and only then confirms.
//! Expected behaviour: the CSV text travels once; every later request carries `session_id` alone and
//! is re-parsed identically; a session the server cannot resolve is a 400 whose message the client
//! recovers from by re-sending the file; `overwrite` changes exactly the rows the preview listed and
//! nothing else; `skip` reports the same difference and changes nothing; and `tz_offset_hours` is a
//! per-request parameter, not state baked into the session.
//!
//! Everything is provisioned from nothing over HTTP on the CSV onboarding track. Provisioning runs
//! as `admin` (projects and parameters are Administrator-managed); every import runs as `river1`,
//! the lowest level `require_write_data` admits.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::e2e::count;
use crate::common::keycloak as kc;
use crate::common::tracks::{self, Track};

/// A real 150-row viewLinc export: `DOmgL,DOuM,WaterTempdegC` at a 30-minute cadence. The header
/// casing differs from the catalog code the test registers (`WaterTempDegC`), which is what makes
/// the case-insensitive column resolution load-bearing rather than incidental.
const VIEWLINC_NARROW: &str = include_str!("../fixtures/viewlinc_narrow.csv");
const VIEWLINC_ROWS: usize = 150;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    admin: String,
    river: String,
    track: Track,
}

impl Fixture {
    /// The two parameter codes track A registers, in creation order, as CSV column headers.
    fn codes(&self) -> (String, String) {
        assert!(
            self.track.parameters.len() >= 2,
            "the CSV track provisions two parameters, got {:?}",
            self.track.parameters
        );
        (self.track.parameters[0].0.clone(), self.track.parameters[1].0.clone())
    }
}

async fn setup(test_name: &str) -> Option<Fixture> {
    if !kc::require_keycloak_or_skip(test_name).await {
        return None;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let track = tracks::onboard_csv_track(&app, &admin).await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &track.project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    Some(Fixture { db, app, admin, river, track })
}

/// A single numeric column aliased `v`, `None` when no row matches or the column is NULL.
async fn value(db: &DatabaseConnection, sql: &str) -> Option<f64> {
    let row = db
        .query_one(Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.to_string()))
        .await
        .unwrap_or_else(|e| panic!("value query failed: {e}\n{sql}"))?;
    row.try_get::<Option<f64>>("", "v").ok().flatten()
}

async fn staging_rows(db: &DatabaseConnection) -> i64 {
    count(db, "SELECT count(*) AS c FROM csv_import_staging").await
}

fn session_of(body: &serde_json::Value) -> String {
    body["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("every import response carries a session_id: {body}"))
        .to_string()
}

fn error_of(body: &serde_json::Value) -> String {
    body["error"]
        .as_str()
        .unwrap_or_else(|| panic!("an error response carries an `error` string: {body}"))
        .to_string()
}

fn instant(body: &serde_json::Value, field: &str) -> chrono::DateTime<chrono::Utc> {
    let raw = body[field]
        .as_str()
        .unwrap_or_else(|| panic!("`{field}` missing or null: {body}"));
    raw.parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap_or_else(|e| panic!("`{field}` = '{raw}' is not an RFC3339 instant: {e}"))
}

fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
    iso.parse().expect("fixture timestamp")
}

/// A two-parameter file at a naive local timestamp, the delivery shape the wizard accepts.
/// All values sit inside the CSV track's band so a row read by another track's assertions shows up.
fn two_column_csv(depth: &str, turbidity: &str, first_depth: f64) -> String {
    format!(
        "DateTime,{depth},{turbidity}\n\
         2025-09-10 00:00:00,{first_depth},110\n\
         2025-09-10 00:10:00,160,120\n"
    )
}

/// One import request, as the river-level member who owns the flow.
async fn import(fx: &Fixture, body: &serde_json::Value) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(&fx.app, "/api/readings/import_csv", body, &fx.river)
        .await
}

#[tokio::test]
#[serial]
async fn dry_run_stages_session_and_import_reuses_it_without_csv() {
    let Some(fx) = setup("dry_run_stages_session").await else {
        return;
    };

    // The catalog entries the real export's columns resolve to. `DOmgL` deliberately gets none: it
    // is a derived quantity in this lineage and must never be ingested as a raw column.
    let do_id = crate::common::e2e::create_parameter(&fx.app, &fx.admin, "DOuM", "Dissolved Oxygen", "uM").await;
    let temp_id =
        crate::common::e2e::create_parameter(&fx.app, &fx.admin, "WaterTempDegC", "Water Temperature", "degC").await;
    for pid in [&do_id, &temp_id] {
        crate::common::e2e::assign_site_parameter_minimal(&fx.app, &fx.admin, &fx.track.site_id, pid).await;
    }

    let (status, preview) = import(
        &fx,
        &json!({ "site": fx.track.site_id, "csv": VIEWLINC_NARROW, "dry_run": true, "tz_offset_hours": 0 }),
    )
    .await;
    assert_eq!(status, 200, "a river-level member previews an import: {preview}");
    assert_eq!(preview["dry_run"], true, "the preview reports itself as one: {preview}");
    assert_eq!(preview["row_count"], VIEWLINC_ROWS, "every data row parses: {preview}");
    assert_eq!(preview["inserted_total"], 0, "a preview writes nothing: {preview}");
    assert_eq!(
        preview["mapped_columns"]["DOuM"], "Dissolved Oxygen",
        "a column resolves to its site parameter's name: {preview}"
    );
    assert_eq!(
        preview["mapped_columns"]["WaterTempdegC"], "Water Temperature",
        "the export's casing differs from the catalog code, resolution is case-insensitive: {preview}"
    );
    assert_eq!(
        preview["unmapped_columns"],
        json!(["DOmgL"]),
        "the column with no catalog entry is reported, not silently ingested: {preview}"
    );
    let session = session_of(&preview);

    // The operator skips a column. Only the session travels: the file is not re-uploaded.
    let (status, remapped) = import(
        &fx,
        &json!({
            "site": fx.track.site_id,
            "session_id": session,
            "mapping": { "DOmgL": null },
            "dry_run": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "the staged session is re-previewable without the CSV: {remapped}");
    assert_eq!(
        session_of(&remapped),
        session,
        "the server echoes the caller's session rather than minting a new one: {remapped}"
    );
    assert_eq!(
        remapped["row_count"], VIEWLINC_ROWS,
        "the cached CSV text is re-parsed identically: {remapped}"
    );
    assert_eq!(
        remapped["mapped_columns"]["DOuM"], "Dissolved Oxygen",
        "resolution of the other columns is unchanged: {remapped}"
    );
    assert_eq!(
        remapped["skipped_columns"],
        json!(["DOmgL"]),
        "a mapping edit applies to the cached CSV, so it needs no re-upload: {remapped}"
    );
    assert!(
        remapped["mapped_columns"].get("DOmgL").is_none(),
        "an explicitly skipped column is not also mapped: {remapped}"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{}'", fx.track.site_id)
        )
        .await,
        0,
        "a session-driven preview still writes nothing"
    );

    // Confirm. Still no CSV in the body, and the earlier mapping is not carried over.
    let (status, imported) = import(
        &fx,
        &json!({
            "site": fx.track.site_id,
            "session_id": session,
            "conflict": "skip",
            "measurement_type": "spot",
        }),
    )
    .await;
    assert_eq!(status, 200, "the import runs off the staged session: {imported}");
    assert_eq!(imported["dry_run"], false, "{imported}");
    assert_eq!(session_of(&imported), session, "the same session serves the write: {imported}");
    assert_eq!(
        imported["inserted_total"],
        VIEWLINC_ROWS * 2,
        "150 rows across the two mapped columns: {imported}"
    );
    assert_eq!(imported["duplicates"], 0, "nothing was already stored: {imported}");
    assert_eq!(
        imported["skipped_columns"],
        json!([]),
        "mapping travels per request, only the CSV text is cached: {imported}"
    );
    assert_eq!(
        imported["unmapped_columns"],
        json!(["DOmgL"]),
        "without the mapping the column falls back to unresolved: {imported}"
    );
    let job_id = imported["derived_job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an import with work to do enqueues a job: {imported}"))
        .to_string();

    assert_eq!(
        crate::common::e2e::poll_job(&fx.app, &fx.admin, &job_id, 60).await,
        "completed",
        "the csv_import job runs to completion"
    );

    let site = fx.track.site_id.clone();
    assert_eq!(
        count(&fx.db, &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{site}'")).await,
        (VIEWLINC_ROWS * 2) as i64,
        "every mapped cell became a reading"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT count(DISTINCT parameter_id) AS c FROM readings WHERE site_id = '{site}'")
        )
        .await,
        2,
        "the unmapped column produced no readings under any parameter"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE site_id = '{site}' AND measurement_type = 'spot'"
            )
        )
        .await,
        (VIEWLINC_ROWS * 2) as i64,
        "the request-level measurement_type survives the staging round trip into the worker"
    );
    assert_eq!(staging_rows(&fx.db).await, 0, "the worker drops its staged rows on completion");

    // The wizard polls the job as the user who started the import (`pollJob` in
    // sites/[id]/import/+page.svelte), so the importing member must be able to read it. Expectation
    // inferred from that call sequence.
    let (status, job) =
        crate::common::get_json_with_token(&fx.app, &format!("/api/reprocessing_jobs/{job_id}"), &fx.river).await;
    assert_eq!(status, 200, "the member who ran the import can watch its job: {job}");
}

#[tokio::test]
#[serial]
async fn unresolvable_session_is_rejected_by_message_and_full_csv_retry_succeeds() {
    let Some(fx) = setup("unresolvable_session_retry").await else {
        return;
    };
    let (depth, turbidity) = fx.codes();
    let csv = two_column_csv(&depth, &turbidity, 150.0);

    let (status, preview) =
        import(&fx, &json!({ "site": fx.track.site_id, "csv": csv, "dry_run": true })).await;
    assert_eq!(status, 200, "preview ({status}): {preview}");
    let session = session_of(&preview);

    // Staging is per-instance and in memory, so a session does not survive a restart or reach a
    // second replica. The client sees the same failure it sees on expiry.
    let other_replica = kc::build_test_app_with_keycloak(fx.db.clone()).await;
    let (status, elsewhere) = crate::common::post_json_parse_with_token(
        &other_replica,
        "/api/readings/import_csv",
        &json!({ "site": fx.track.site_id, "session_id": session, "dry_run": true }),
        &fx.river,
    )
    .await;
    assert_eq!(status, 400, "a session another instance minted is not honoured: {elsewhere}");
    assert!(
        error_of(&elsewhere).contains("expired"),
        "the client branches on this substring to drop the session and retry: {elsewhere}"
    );

    let (status, unknown) = import(
        &fx,
        &json!({
            "site": fx.track.site_id,
            "session_id": uuid::Uuid::new_v4().to_string(),
            "dry_run": true,
        }),
    )
    .await;
    assert_eq!(status, 400, "an unknown session id is refused: {unknown}");
    assert_eq!(
        error_of(&unknown),
        error_of(&elsewhere),
        "an unknown and an unreachable session are indistinguishable, so one recovery branch covers both"
    );

    let (status, neither) = import(&fx, &json!({ "site": fx.track.site_id })).await;
    assert_eq!(status, 400, "a request with neither CSV nor session is refused: {neither}");
    assert!(
        error_of(&neither).contains("Provide either csv or session_id"),
        "the no-input case is a distinct message, not the expiry one: {neither}"
    );

    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{}'", fx.track.site_id)
        )
        .await,
        0,
        "no refused request wrote anything"
    );

    // The client's recovery: drop the session, send the whole file again.
    let (status, retried) =
        import(&fx, &json!({ "site": fx.track.site_id, "csv": csv, "conflict": "skip" })).await;
    assert_eq!(status, 200, "retry with the full CSV ({status}): {retried}");
    assert_ne!(
        session_of(&retried),
        session,
        "the retry mints a fresh session rather than resurrecting the lost one: {retried}"
    );
    assert_eq!(retried["inserted_total"], 4, "two rows across two columns: {retried}");
    let job_id = retried["derived_job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the retry enqueues the import job: {retried}"))
        .to_string();

    assert_eq!(
        crate::common::e2e::poll_job(&fx.app, &fx.admin, &job_id, 60).await,
        "completed",
        "the retried import completes"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{}'", fx.track.site_id)
        )
        .await,
        4,
        "the retry loses no rows"
    );
    assert_eq!(staging_rows(&fx.db).await, 0, "staging is drained");
}

#[tokio::test]
#[serial]
async fn dry_run_previews_overlap_diff_then_overwrite_applies_exactly_those_rows() {
    let Some(fx) = setup("overlap_preview_then_overwrite").await else {
        return;
    };
    let (depth, turbidity) = fx.codes();
    let depth_id = fx.track.parameter_id(&depth).to_string();
    let turbidity_id = fx.track.parameter_id(&turbidity).to_string();
    let site = fx.track.site_id.clone();

    let (status, first) = import(
        &fx,
        &json!({ "site": site, "csv": two_column_csv(&depth, &turbidity, 150.0) }),
    )
    .await;
    assert_eq!(status, 200, "first import ({status}): {first}");
    assert_eq!(first["inserted_total"], 4, "{first}");
    assert_eq!(first["overlaps_identical"], 0, "an empty slot has no overlap: {first}");
    assert_eq!(first["overlaps_differing"], 0, "{first}");
    let job_id = first["derived_job_id"].as_str().expect("first import enqueues a job").to_string();
    assert_eq!(
        crate::common::e2e::poll_job(&fx.app, &fx.admin, &job_id, 60).await,
        "completed",
        "first import completes"
    );

    let stored_at = |param: &str, time: &str| {
        format!(
            "SELECT raw_value AS v FROM readings \
             WHERE site_id = '{site}' AND parameter_id = '{param}' AND time = '{time}'"
        )
    };
    assert_eq!(
        value(&fx.db, &stored_at(&depth_id, "2025-09-10T00:00:00Z")).await,
        Some(150.0),
        "the row the corrected file will differ on"
    );

    // The operator re-exports the file with one cell corrected and previews it.
    let (status, diff) = import(
        &fx,
        &json!({
            "site": site,
            "csv": two_column_csv(&depth, &turbidity, 155.0),
            "dry_run": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "overlap preview ({status}): {diff}");
    assert_eq!(diff["overlaps_identical"], 3, "three cells are unchanged: {diff}");
    assert_eq!(diff["overlaps_differing"], 1, "one cell changed: {diff}");
    assert_eq!(diff["inserted_total"], 0, "a preview inserts nothing: {diff}");
    assert_eq!(diff["overwritten"], 0, "and overwrites nothing: {diff}");
    assert_eq!(diff["duplicates"], 0, "{diff}");
    let sample = diff["overlap_sample"]
        .as_array()
        .unwrap_or_else(|| panic!("overlap_sample is an array: {diff}"));
    assert_eq!(sample.len(), 1, "the preview lists the one differing row: {diff}");
    assert_eq!(sample[0]["existing"], 150.0, "what is stored today: {diff}");
    assert_eq!(sample[0]["incoming"], 155.0, "what the file would write: {diff}");
    assert_eq!(sample[0]["parameter_id"], depth_id, "attributed to the right parameter: {diff}");
    assert_eq!(
        instant(&sample[0], "time"),
        at("2025-09-10T00:00:00Z"),
        "and to the right instant: {diff}"
    );
    let session = session_of(&diff);

    let (status, applied) = import(
        &fx,
        &json!({ "site": site, "session_id": session, "conflict": "overwrite" }),
    )
    .await;
    assert_eq!(status, 200, "confirming the overwrite ({status}): {applied}");
    assert_eq!(applied["inserted_total"], 0, "nothing is new: {applied}");
    assert_eq!(applied["duplicates"], 3, "the identical cells are left alone: {applied}");
    assert_eq!(applied["overwritten"], 1, "exactly the previewed row is replaced: {applied}");
    let job_id = applied["derived_job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an all-overlap file still has work in overwrite mode: {applied}"))
        .to_string();
    assert_eq!(
        crate::common::e2e::poll_job(&fx.app, &fx.admin, &job_id, 60).await,
        "completed",
        "the overwrite job completes"
    );

    assert_eq!(
        value(&fx.db, &stored_at(&depth_id, "2025-09-10T00:00:00Z")).await,
        Some(155.0),
        "the corrected value replaced the stored one"
    );
    assert_eq!(
        value(
            &fx.db,
            &format!(
                "SELECT calibrated_value AS v FROM readings \
                 WHERE site_id = '{site}' AND parameter_id = '{depth_id}' AND time = '2025-09-10T00:00:00Z'"
            )
        )
        .await,
        Some(155.0),
        "and the served value with it, not only the raw column"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE site_id = '{site}' AND parameter_id = '{depth_id}' AND time = '2025-09-10T00:00:00Z'"
            )
        )
        .await,
        1,
        "overwrite replaces in place, it does not append a replicate"
    );
    assert_eq!(
        value(&fx.db, &stored_at(&turbidity_id, "2025-09-10T00:00:00Z")).await,
        Some(110.0),
        "the other parameter at the same instant is untouched"
    );
    assert_eq!(
        value(&fx.db, &stored_at(&depth_id, "2025-09-10T00:10:00Z")).await,
        Some(160.0),
        "and so is the same parameter at the other instant"
    );
    assert_eq!(
        count(&fx.db, &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{site}'")).await,
        4,
        "the site still holds four readings"
    );
    assert_eq!(staging_rows(&fx.db).await, 0, "staging is drained");
}

#[tokio::test]
#[serial]
async fn conflict_skip_reports_the_differing_row_and_leaves_it_stored() {
    let Some(fx) = setup("conflict_skip_leaves_stored").await else {
        return;
    };
    let (depth, turbidity) = fx.codes();
    let depth_id = fx.track.parameter_id(&depth).to_string();
    let site = fx.track.site_id.clone();

    let (status, first) = import(
        &fx,
        &json!({ "site": site, "csv": two_column_csv(&depth, &turbidity, 150.0) }),
    )
    .await;
    assert_eq!(status, 200, "first import ({status}): {first}");
    let job_id = first["derived_job_id"].as_str().expect("first import enqueues a job").to_string();
    assert_eq!(
        crate::common::e2e::poll_job(&fx.app, &fx.admin, &job_id, 60).await,
        "completed",
        "first import completes"
    );

    let (status, skipped) = import(
        &fx,
        &json!({
            "site": site,
            "csv": two_column_csv(&depth, &turbidity, 155.0),
            "conflict": "skip",
        }),
    )
    .await;
    assert_eq!(status, 200, "re-import in skip mode ({status}): {skipped}");
    assert_eq!(skipped["inserted_total"], 0, "nothing is new: {skipped}");
    assert_eq!(skipped["duplicates"], 4, "every incoming cell already exists: {skipped}");
    assert_eq!(
        skipped["overlaps_differing"], 1,
        "the difference is still reported to the operator: {skipped}"
    );
    assert_eq!(skipped["overlaps_identical"], 3, "{skipped}");
    assert_eq!(skipped["overwritten"], 0, "skip mode replaces nothing: {skipped}");
    assert!(
        skipped["derived_job_id"].is_null(),
        "with nothing to write there is no work to enqueue: {skipped}"
    );

    assert_eq!(
        value(
            &fx.db,
            &format!(
                "SELECT raw_value AS v FROM readings \
                 WHERE site_id = '{site}' AND parameter_id = '{depth_id}' AND time = '2025-09-10T00:00:00Z'"
            )
        )
        .await,
        Some(150.0),
        "the stored value stands: skip reports the difference without applying it"
    );
    assert_eq!(
        count(&fx.db, &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{site}'")).await,
        4,
        "and no replicate was appended alongside it"
    );
    assert_eq!(staging_rows(&fx.db).await, 0, "a no-work import stages nothing");
}

#[tokio::test]
#[serial]
async fn tz_offset_hours_converts_to_utc_and_is_not_baked_into_the_session() {
    let Some(fx) = setup("tz_offset_per_request").await else {
        return;
    };
    let (depth, _turbidity) = fx.codes();
    let depth_id = fx.track.parameter_id(&depth).to_string();
    let site = fx.track.site_id.clone();
    let csv = format!(
        "DateTime,{depth}\n\
         2025-09-20 10:00:00,150\n\
         2025-09-20 11:30:00,160\n"
    );

    // The wizard reads UTC+02:00 off a Vaisala export header and sends it with the preview.
    let (status, shifted) = import(
        &fx,
        &json!({ "site": site, "csv": csv, "dry_run": true, "tz_offset_hours": 2.0 }),
    )
    .await;
    assert_eq!(status, 200, "preview at UTC+02:00 ({status}): {shifted}");
    assert_eq!(shifted["row_count"], 2, "{shifted}");
    assert_eq!(
        instant(&shifted, "earliest"),
        at("2025-09-20T08:00:00Z"),
        "local 10:00 at UTC+02:00 is 08:00 UTC: {shifted}"
    );
    assert_eq!(
        instant(&shifted, "latest"),
        at("2025-09-20T09:30:00Z"),
        "and local 11:30 is 09:30 UTC: {shifted}"
    );
    let session = session_of(&shifted);

    // The operator corrects the detection to UTC. Same staged file, no re-upload.
    let (status, corrected) =
        import(&fx, &json!({ "site": site, "session_id": session, "dry_run": true })).await;
    assert_eq!(status, 200, "preview at UTC ({status}): {corrected}");
    assert_eq!(session_of(&corrected), session, "still the same session: {corrected}");
    assert_eq!(
        instant(&corrected, "earliest"),
        at("2025-09-20T10:00:00Z"),
        "the offset is a per-request parameter, not state stored with the session: {corrected}"
    );
    assert_eq!(
        instant(&corrected, "latest"),
        at("2025-09-20T11:30:00Z"),
        "{corrected}"
    );

    let (status, imported) = import(
        &fx,
        &json!({
            "site": site,
            "session_id": session,
            "tz_offset_hours": 2.0,
            "conflict": "skip",
        }),
    )
    .await;
    assert_eq!(status, 200, "import at UTC+02:00 ({status}): {imported}");
    assert_eq!(imported["inserted_total"], 2, "{imported}");
    let job_id = imported["derived_job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the import enqueues its job: {imported}"))
        .to_string();
    assert_eq!(
        crate::common::e2e::poll_job(&fx.app, &fx.admin, &job_id, 60).await,
        "completed",
        "the import completes"
    );

    assert_eq!(
        value(
            &fx.db,
            &format!(
                "SELECT raw_value AS v FROM readings \
                 WHERE site_id = '{site}' AND parameter_id = '{depth_id}' AND time = '2025-09-20T08:00:00Z'"
            )
        )
        .await,
        Some(150.0),
        "the reading is stored at the converted instant"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE site_id = '{site}' AND parameter_id = '{depth_id}' AND time = '2025-09-20T10:00:00Z'"
            )
        )
        .await,
        0,
        "the naive local timestamp is never stored as if it were UTC"
    );
    assert_eq!(
        count(&fx.db, &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{site}'")).await,
        2,
        "both rows landed once, not once per preview"
    );
    assert_eq!(staging_rows(&fx.db).await, 0, "staging is drained");
}
