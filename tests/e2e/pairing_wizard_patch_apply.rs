//! The pairing wizard's edit-then-apply sequence, and the two non-plan pairing paths beside it.
//!
//! Scenario: a data manager points the streams wizard at a sync source, edits the proposed
//! mappings (the dashboard batches those edits into debounced PATCHes and flushes them before
//! apply, `river-data-ui/src/routes/streams/+page.svelte`), then applies, reverts, or takes one of
//! the two bulk paths, `POST /sync/bulk-pair` and `POST /sync/apply-discovery`.
//!
//! Expected behaviour: the plan the server applies is the accumulation of every PATCH in order, an
//! applied plan is frozen, revert accounts only for the streams that plan paired, and a failing
//! apply-discovery action leaves no entities behind while its siblings still commit.
//!
//! State is provisioned from nothing through HTTP, in the shape of the sensor-flow track: catalog
//! entities over the entity CRUD, streams over `POST /streams/register`, readings over
//! `POST /ingest`. Direct SQL appears only in assertions, over columns no endpoint exposes.
//!
//! The wizard's write routes are Administrator-only for humans
//! (`require_admin_or_token_write_metadata`, `routes/service/mod.rs`), so these run as the `admin`
//! realm user and check that the level below is refused; its read routes need only
//! `read_metadata`, so `intern1` drives those.
//!
//! Run: cargo test --test e2e -- --test-threads=1

use axum::Router;
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::e2e::count;
use crate::common::keycloak as kc;
use crate::common::tracks::BAND_FLOW;

/// The day every fixture reading sits on. Past-dated: the aggregate refresh window is
/// `[since, NOW()]`, so a future-dated fixture is never materialised.
const FIXTURE_DAY: &str = "2025-06-02";

fn find_entry<'a>(plan: &'a serde_json::Value, stream_id: &str) -> &'a serde_json::Value {
    plan["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries array: {plan}"))
        .iter()
        .find(|e| e["stream_id"] == json!(stream_id))
        .unwrap_or_else(|| panic!("entry for stream {stream_id} missing: {plan}"))
}

/// Register an unpaired stream carrying the hierarchy metadata the wizard reads.
async fn register_stream(
    app: &Router,
    jwt: &str,
    source_system: &str,
    source_key: &str,
    metadata: serde_json::Value,
) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": source_system,
            "source_key": source_key,
            "source_name": format!("{source_system} {source_key}"),
            "metadata": metadata,
        }),
        jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register {source_system}/{source_key} ({status}): {stream}"
    );
    assert!(
        stream["site_parameter_id"].is_null(),
        "a freshly registered stream is unpaired: {stream}"
    );
    e2e::id_of(&stream)
}

fn hierarchy_metadata(
    project: &str,
    site: &str,
    parameter: &str,
    units: &str,
) -> serde_json::Value {
    json!({
        "hierarchy": { "project": project, "site": site, "parameter": parameter },
        "units": units,
    })
}

/// Push `n` readings into an unpaired stream, five minutes apart on `FIXTURE_DAY`.
async fn ingest(app: &Router, jwt: &str, stream_id: &str, n: usize) {
    let readings: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            json!({
                "time": format!("{FIXTURE_DAY}T00:{:02}:00Z", i * 5),
                "raw_value": BAND_FLOW.0 + i as f64,
            })
        })
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &json!({ "stream_id": stream_id, "readings": readings }),
        jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "ingest {n} readings into {stream_id} ({status}): {body}"
    );
    assert_eq!(body["inserted"], n, "every reading lands: {body}");
    assert_eq!(
        body["paired"], false,
        "an unpaired stream stores its readings unattributed: {body}"
    );
}

/// A registered stream plus its unattributed history, the state the wizard starts from.
#[allow(clippy::too_many_arguments)]
async fn unpaired_stream(
    app: &Router,
    jwt: &str,
    source_system: &str,
    source_key: &str,
    project: &str,
    site: &str,
    parameter: &str,
    units: &str,
    n_readings: usize,
) -> String {
    let id = register_stream(
        app,
        jwt,
        source_system,
        source_key,
        hierarchy_metadata(project, site, parameter, units),
    )
    .await;
    if n_readings > 0 {
        ingest(app, jwt, &id, n_readings).await;
    }
    id
}

async fn create_plan(app: &Router, jwt: &str, source_system: &str) -> serde_json::Value {
    let (status, plan) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/pairing-plans",
        &json!({ "source_system": source_system }),
        jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "create plan for {source_system} ({status}): {plan}"
    );
    assert_eq!(plan["status"], "draft", "a new plan is a draft: {plan}");
    plan
}

/// One debounced PATCH batch, returning the server's snapshot of the plan. The dashboard replaces
/// its local plan with exactly this body, so it has to carry the accumulated state.
async fn patch_plan(
    app: &Router,
    jwt: &str,
    plan_id: &str,
    updates: serde_json::Value,
) -> serde_json::Value {
    let (status, body) = crate::common::patch_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({ "updates": updates }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "PATCH plan ({status}): {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("PATCH response is JSON: {e}\nBody: {body}"))
}

/// Post `apply`/`revert` (both run as tracked jobs), wait for completion, return `detail.counts`.
async fn run_plan_action(
    app: &Router,
    jwt: &str,
    plan_id: &str,
    action: &str,
) -> serde_json::Value {
    let (status, res) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/{action}"),
        &json!({}),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "{action} ({status}): {res}");
    let job_id = res["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{action} returns a job_id: {res}"));
    assert_eq!(
        e2e::poll_job(app, jwt, job_id, 30).await,
        "completed",
        "{action} job completes",
    );
    let (_, job) =
        crate::common::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), jwt)
            .await;
    job["detail"]["counts"].clone()
}

#[tokio::test]
#[serial]
async fn final_patch_state_is_what_applies_and_late_patch_is_rejected() {
    if !kc::require_keycloak_or_skip("final_patch_state_is_what_applies").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let s_rename = unpaired_stream(
        &app,
        &admin,
        "wizpatch",
        "rename",
        "WIZARD",
        "WZ_A",
        "Raw Param A",
        "mg/L",
        3,
    )
    .await;
    let s_map = unpaired_stream(
        &app,
        &admin,
        "wizpatch",
        "map",
        "WIZARD",
        "WZ_A",
        "Mystery B",
        "FNU",
        2,
    )
    .await;
    let s_skip = unpaired_stream(
        &app, &admin, "wizpatch", "skip", "WIZARD", "WZ_A", "Junk C", "x", 4,
    )
    .await;

    let turb_fnu = e2e::create_parameter(&app, &admin, "Turb_FNU", "Turbidity FNU", "FNU").await;

    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;
    let (status, refused) = crate::common::post_json_with_token(
        &app,
        "/api/sync/pairing-plans",
        &json!({ "source_system": "wizpatch" }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "driving the pairing wizard is an Administrator action; a manager is refused: {refused}"
    );

    let plan = create_plan(&app, &admin, "wizpatch").await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    assert_eq!(
        plan["summary"]["total_streams"], 3,
        "all three streams discovered: {plan}"
    );
    assert_eq!(
        plan["summary"]["will_pair"], 3,
        "every entry starts as pair: {plan}"
    );
    assert_eq!(
        plan["summary"]["will_skip"], 0,
        "nothing is skipped yet: {plan}"
    );

    patch_plan(
        &app,
        &admin,
        &plan_id,
        json!([{ "stream_id": s_rename, "parameter_name": "Interim Name" }]),
    )
    .await;
    patch_plan(
        &app,
        &admin,
        &plan_id,
        json!([{ "stream_id": s_rename, "parameter_name": "Final Name", "parameter_units": "ug/L" }]),
    )
    .await;
    patch_plan(
        &app,
        &admin,
        &plan_id,
        json!([{ "stream_id": s_map, "parameter_name": "turbidity fnu" }]),
    )
    .await;
    let latest = patch_plan(
        &app,
        &admin,
        &plan_id,
        json!([{ "stream_id": s_skip, "action": "skip" }]),
    )
    .await;

    let renamed = find_entry(&latest, &s_rename);
    assert_eq!(
        renamed["parameter"]["name"], "Final Name",
        "the last rename wins and later batches do not resurrect the earlier one: {latest}"
    );
    assert_eq!(
        renamed["parameter"]["units"], "ug/L",
        "the units edit sent with the rename survives the batches after it: {latest}"
    );
    assert_eq!(
        renamed["parameter"]["create"], true,
        "'Final Name' is new: {latest}"
    );
    assert!(
        renamed["parameter"]["id"].is_null(),
        "a to-be-created parameter carries no id: {latest}"
    );

    let mapped = find_entry(&latest, &s_map);
    assert_eq!(
        mapped["parameter"]["id"],
        json!(turb_fnu),
        "renaming onto an existing catalog name resolves it by id: {latest}"
    );
    assert_eq!(
        mapped["parameter"]["create"], false,
        "a resolved parameter is reused, not created: {latest}"
    );

    assert_eq!(
        find_entry(&latest, &s_skip)["action"],
        "skip",
        "the skip toggle persists: {latest}"
    );

    let summary = &latest["summary"];
    assert_eq!(
        summary["will_pair"], 2,
        "two entries left to pair: {latest}"
    );
    assert_eq!(summary["will_skip"], 1, "one entry skipped: {latest}");
    assert_eq!(
        summary["parameters_to_create"], 1,
        "only 'Final Name' is new; 'turbidity fnu' resolved: {latest}"
    );
    assert_eq!(summary["sites_to_create"], 1, "WZ_A is new: {latest}");
    assert_eq!(summary["projects_to_create"], 1, "WIZARD is new: {latest}");
    assert_eq!(
        summary["unique_parameters"], 2,
        "two distinct parameters across the pairing entries: {latest}"
    );

    let (status, fetched) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "read back the plan ({status}): {fetched}");
    for stream_id in [&s_rename, &s_map, &s_skip] {
        let live = find_entry(&fetched, stream_id);
        let patched = find_entry(&latest, stream_id);
        assert_eq!(
            live["action"], patched["action"],
            "the PATCH response is the persisted snapshot, not a transient projection: {fetched}"
        );
        assert_eq!(
            live["parameter"]["name"], patched["parameter"]["name"],
            "persisted parameter name matches the last PATCH: {fetched}"
        );
        assert_eq!(
            live["parameter"]["id"], patched["parameter"]["id"],
            "persisted parameter resolution matches the last PATCH: {fetched}"
        );
    }

    let counts = run_plan_action(&app, &admin, &plan_id, "apply").await;
    assert_eq!(
        counts["streams_paired"], 2,
        "the skipped stream is not paired: {counts}"
    );
    assert_eq!(
        counts["streams_skipped"], 0,
        "a user-chosen skip is filtered before the skip counter, which counts unusable entries: {counts}"
    );
    assert_eq!(counts["projects_created"], 1, "WIZARD created: {counts}");
    assert_eq!(counts["sites_created"], 1, "WZ_A created: {counts}");
    assert_eq!(
        counts["parameters_created"], 1,
        "only the renamed parameter is created; the mapped one is reused: {counts}"
    );
    assert_eq!(
        counts["site_parameters_created"], 2,
        "one slot per paired stream: {counts}"
    );
    assert_eq!(
        counts["readings_backfilled"], 5,
        "3 from the renamed stream + 2 from the mapped one; the skipped stream's 4 stay out: {counts}"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "pairing_backfill", 30).await,
        "apply enqueues a window re-derivation per paired slot and each succeeds"
    );

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM parameters WHERE LOWER(code) = 'interim name'"
        )
        .await,
        0,
        "the superseded rename left no catalog artifact"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM parameters WHERE code = 'Final Name'"
        )
        .await,
        1,
        "the parameter apply created carries the final name"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams ds \
                 JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
                 WHERE ds.id = '{s_map}' AND sp.parameter_id = '{turb_fnu}'"
            )
        )
        .await,
        1,
        "the mapped stream landed on the pre-existing catalog parameter"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings r \
                 JOIN data_streams ds ON r.stream_id = ds.id \
                 WHERE ds.pairing_plan_id = '{plan_id}' AND r.site_id IS NOT NULL"
            )
        )
        .await,
        5,
        "the backfilled readings stay attributed after the slot re-derivation"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams \
                 WHERE id = '{s_skip}' AND site_parameter_id IS NULL AND pairing_plan_id IS NULL"
            )
        )
        .await,
        1,
        "a skipped stream is neither paired nor linked to the plan"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{s_skip}'")
        )
        .await,
        4,
        "the skipped stream keeps its history"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{s_skip}' AND site_id IS NOT NULL"
            )
        )
        .await,
        0,
        "and none of it was attributed"
    );

    let (status, late) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({ "updates": [{ "stream_id": s_rename, "parameter_name": "Too Late" }] }),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "an applied plan is frozen: {late}");

    let (status, final_plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "read back the applied plan ({status}): {final_plan}"
    );
    assert_eq!(final_plan["status"], "applied", "plan status: {final_plan}");
    assert_eq!(
        find_entry(&final_plan, &s_rename)["parameter"]["name"],
        "Final Name",
        "the rejected PATCH changed nothing: {final_plan}"
    );
}

#[tokio::test]
#[serial]
async fn revert_after_partial_apply_leaves_skipped_stream_and_replan_sees_created_catalog() {
    if !kc::require_keycloak_or_skip("revert_after_partial_apply").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let s_one = unpaired_stream(
        &app,
        &admin,
        "wizrevert",
        "one",
        "WIZREVERT",
        "WR_A",
        "Alpha",
        "mg/L",
        2,
    )
    .await;
    let s_two = unpaired_stream(
        &app,
        &admin,
        "wizrevert",
        "two",
        "WIZREVERT",
        "WR_A",
        "Beta",
        "mg/L",
        2,
    )
    .await;
    let s_skip = unpaired_stream(
        &app,
        &admin,
        "wizrevert",
        "skip",
        "WIZREVERT",
        "WR_A",
        "Gamma",
        "mg/L",
        2,
    )
    .await;

    let plan = create_plan(&app, &admin, "wizrevert").await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    patch_plan(
        &app,
        &admin,
        &plan_id,
        json!([{ "stream_id": s_skip, "action": "skip" }]),
    )
    .await;

    let counts = run_plan_action(&app, &admin, &plan_id, "apply").await;
    assert_eq!(counts["streams_paired"], 2, "Alpha and Beta pair: {counts}");
    assert_eq!(
        counts["parameters_created"], 2,
        "Alpha and Beta are created; Gamma never is: {counts}"
    );

    // The apply job enqueues a window re-derivation per paired slot post-commit. Revert has to
    // race nothing, so let those settle before unpairing.
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "pairing_backfill", 30).await,
        "the post-apply slot re-derivations run and succeed"
    );

    let counts = run_plan_action(&app, &admin, &plan_id, "revert").await;
    assert_eq!(
        counts["reverted"], 2,
        "revert accounts only for the streams this plan paired: {counts}"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams \
                 WHERE id = '{s_skip}' AND pairing_plan_id IS NULL AND site_parameter_id IS NULL"
            )
        )
        .await,
        1,
        "the skipped stream was never touched by the plan"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams \
                 WHERE pairing_plan_id = '{plan_id}' AND site_parameter_id IS NULL"
            )
        )
        .await,
        2,
        "the plan link is kept as an audit trail while the pairing is removed"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM readings r JOIN data_streams ds ON r.stream_id = ds.id \
             WHERE ds.source_system = 'wizrevert' AND r.site_id IS NOT NULL"
        )
        .await,
        0,
        "revert un-attributes every reading the apply backfilled"
    );

    let (status, site) =
        crate::common::get_json_with_token(&app, "/api/sites/WR_A/detail", &admin).await;
    assert_eq!(
        status, 200,
        "the site the apply created survives the revert ({status}): {site}"
    );
    let wr_a = site["id"].as_str().expect("site id").to_string();

    let second = create_plan(&app, &admin, "wizrevert").await;
    let second_id = second["id"].as_str().expect("plan id").to_string();
    assert_ne!(second_id, plan_id, "a re-plan is a new plan");
    assert_eq!(
        second["summary"]["total_streams"], 3,
        "all three streams are unpaired again and re-discovered: {second}"
    );
    assert_eq!(
        second["summary"]["will_pair"], 3,
        "every entry starts as pair again: {second}"
    );
    assert_eq!(
        second["summary"]["will_skip"], 0,
        "the skip decision lived in the first plan only: {second}"
    );
    assert_eq!(
        find_entry(&second, &s_skip)["action"],
        "pair",
        "a skip is not persisted onto the stream: {second}"
    );

    let one = find_entry(&second, &s_one);
    assert_eq!(one["site"]["create"], false, "WR_A now exists: {second}");
    assert_eq!(
        one["site"]["id"],
        json!(wr_a),
        "and the re-plan resolves it to the site the first apply created: {second}"
    );
    assert_eq!(
        one["parameter"]["create"], false,
        "Alpha resolves to the catalog row the first apply created: {second}"
    );
    assert_eq!(
        find_entry(&second, &s_two)["parameter"]["create"],
        false,
        "Beta likewise: {second}"
    );
    assert_eq!(
        find_entry(&second, &s_skip)["parameter"]["create"],
        true,
        "Gamma was never created, so it is still new: {second}"
    );
    assert_eq!(
        second["summary"]["sites_to_create"], 0,
        "no new sites: {second}"
    );
    assert_eq!(
        second["summary"]["parameters_to_create"], 1,
        "only Gamma remains to create: {second}"
    );
}

#[tokio::test]
#[serial]
async fn plan_site_metadata_returns_one_typed_row_per_plan_site() {
    if !kc::require_keycloak_or_skip("plan_site_metadata").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    // `channel_id` and `sample_interval_sec` sit at the top level of metadata; glacier, location,
    // station and device are nested one level (src/routes/private/sync/views.rs).
    let rich = |parameter: &str| {
        json!({
            "hierarchy": { "project": "WIZMETA", "site": "GL_META_A", "parameter": parameter },
            "units": "uS/cm",
            "coordinates": { "latitude": 46.51, "longitude": 7.98, "altitude_m": 2410.5 },
            "glacier": { "name": "Otemma", "rgi_v6": "RGI60-11.02704" },
            "location": { "type": "proglacial" },
            "station": { "catchment": "Rhone", "full_name": "Otemma Downstream", "elevation": 2410.5 },
            "device": { "logger_serial": "LOG-778" },
            "channel_id": "3",
            "sample_interval_sec": 600,
        })
    };
    // The endpoint takes one arbitrary row per site name, so the two GL_META_A streams differ only
    // in the parameter they name, which the endpoint does not read.
    register_stream(&app, &admin, "wizmeta", "a-cond", rich("Conductivity")).await;
    register_stream(&app, &admin, "wizmeta", "a-temp", rich("Temperature")).await;
    register_stream(
        &app,
        &admin,
        "wizmeta",
        "b-cond",
        hierarchy_metadata("WIZMETA", "GL_META_B", "Conductivity", "uS/cm"),
    )
    .await;
    register_stream(
        &app,
        &admin,
        "wizmeta_other",
        "other",
        json!({
            "hierarchy": { "project": "OTHER", "site": "GL_OTHER", "parameter": "Conductivity" },
            "units": "uS/cm",
            "coordinates": { "latitude": 45.0, "longitude": 6.0, "altitude_m": 1000.0 },
        }),
    )
    .await;

    let plan = create_plan(&app, &admin, "wizmeta").await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    assert_eq!(
        plan["summary"]["total_streams"], 3,
        "the other source system's stream is not in this plan: {plan}"
    );

    // Site metadata is a read: the lowest level that may read it drives it.
    let intern = kc::get_keycloak_jwt("intern1", "intern1").await;
    let (status, rows) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/site-metadata"),
        &intern,
    )
    .await;
    assert_eq!(status, 200, "site-metadata ({status}): {rows}");
    let rows = rows
        .as_array()
        .expect("site-metadata returns an array")
        .clone();
    assert_eq!(
        rows.len(),
        2,
        "one row per distinct site, so the two GL_META_A streams collapse: {rows:?}"
    );
    assert_eq!(
        rows[0]["site_name"], "GL_META_A",
        "rows are ordered by site name: {rows:?}"
    );
    assert_eq!(
        rows[1]["site_name"], "GL_META_B",
        "rows are ordered by site name: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r["site_name"] == "GL_OTHER"),
        "the endpoint is scoped to this plan's streams: {rows:?}"
    );

    let a = &rows[0];
    assert_eq!(
        a["latitude"].as_f64(),
        Some(46.51),
        "latitude parsed as a number: {a}"
    );
    assert_eq!(
        a["longitude"].as_f64(),
        Some(7.98),
        "longitude parsed as a number: {a}"
    );
    assert_eq!(
        a["altitude_m"].as_f64(),
        Some(2410.5),
        "altitude parsed as a number: {a}"
    );
    assert_eq!(
        a["elevation"].as_f64(),
        Some(2410.5),
        "station elevation parsed as a number: {a}"
    );
    assert_eq!(
        a["sample_interval_sec"].as_i64(),
        Some(600),
        "sample interval parsed as an integer: {a}"
    );
    assert_eq!(
        a["channel_id"],
        json!("3"),
        "channel_id stays a string, which is what the dashboard types it as: {a}"
    );
    assert_eq!(a["glacier_name"], "Otemma", "glacier name: {a}");
    assert_eq!(a["glacier_rgi"], "RGI60-11.02704", "glacier RGI id: {a}");
    assert_eq!(a["location_type"], "proglacial", "location type: {a}");
    assert_eq!(a["catchment"], "Rhone", "station catchment: {a}");
    assert_eq!(
        a["full_name"], "Otemma Downstream",
        "station full name: {a}"
    );
    assert_eq!(a["device_serial"], "LOG-778", "logger serial: {a}");

    let b = &rows[1];
    for field in [
        "latitude",
        "longitude",
        "altitude_m",
        "glacier_name",
        "glacier_rgi",
        "location_type",
        "catchment",
        "full_name",
        "elevation",
        "device_serial",
        "channel_id",
        "sample_interval_sec",
    ] {
        assert!(
            b[field].is_null(),
            "a stream carrying no enrichment reports {field} as null rather than an empty string: {b}"
        );
    }

    let (status, missing) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sync/pairing-plans/{}/site-metadata",
            uuid::Uuid::new_v4()
        ),
        &intern,
    )
    .await;
    assert_eq!(
        status, 404,
        "site metadata for an unknown plan is a 404: {missing}"
    );
}

#[tokio::test]
#[serial]
async fn bulk_pair_creates_only_what_is_missing_and_skips_unlisted_sites() {
    if !kc::require_keycloak_or_skip("bulk_pair_creates_only_what_is_missing").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    // Deliberately not named BULKWIZ: bulk-pair resolves its project by case-insensitive name, and
    // a pre-existing match would make `project_created` false for reasons unrelated to the test.
    let project =
        e2e::create_project(&app, &admin, "Bulk Wizard Host", "bulkwiz-host", false).await;
    let bw_b = e2e::create_site(&app, &admin, &project, "BW_B", "bw-b").await;
    let conductivity =
        e2e::create_parameter(&app, &admin, "Conductivity", "Conductivity", "uS/cm").await;

    let s_a_cond = unpaired_stream(
        &app,
        &admin,
        "bulkwiz",
        "a-cond",
        "BULKWIZ",
        "BW_A",
        "Conductivity",
        "uS/cm",
        2,
    )
    .await;
    let s_a_temp = unpaired_stream(
        &app,
        &admin,
        "bulkwiz",
        "a-temp",
        "BULKWIZ",
        "BW_A",
        "Bulk Temp",
        "degC",
        3,
    )
    .await;
    let s_b_cond = unpaired_stream(
        &app,
        &admin,
        "bulkwiz",
        "b-cond",
        "BULKWIZ",
        "BW_B",
        "Conductivity",
        "uS/cm",
        1,
    )
    .await;
    let s_unlisted = unpaired_stream(
        &app,
        &admin,
        "bulkwiz",
        "unlisted",
        "BULKWIZ",
        "BW_UNLISTED",
        "Conductivity",
        "uS/cm",
        2,
    )
    .await;

    let request = json!({
        "source_system": "bulkwiz",
        "project_name": "BULKWIZ",
        "sites": [
            { "name": "BW_A", "existing_id": null, "latitude": 46.1, "longitude": 7.2, "altitude_m": 1900.0 },
            { "name": "BW_B", "existing_id": bw_b, "latitude": null, "longitude": null, "altitude_m": null }
        ],
        "parameters": [
            { "code": "Conductivity", "name": "Conductivity", "units": "uS/cm", "existing_id": conductivity },
            { "code": "Bulk Temp", "name": "Bulk Temp", "units": "degC", "existing_id": null }
        ]
    });

    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;
    let (status, refused) =
        crate::common::post_json_with_token(&app, "/api/sync/bulk-pair", &request, &manager).await;
    assert_eq!(
        status, 403,
        "bulk pairing is an Administrator action; a manager is refused: {refused}"
    );

    let (status, resp) =
        crate::common::post_json_parse_with_token(&app, "/api/sync/bulk-pair", &request, &admin)
            .await;
    assert_eq!(status, 200, "bulk-pair ({status}): {resp}");
    assert_eq!(resp["project_created"], true, "BULKWIZ is new: {resp}");
    assert_eq!(
        resp["sites_created"], 1,
        "only BW_A is created; BW_B was supplied by id: {resp}"
    );
    assert_eq!(
        resp["parameters_created"], 1,
        "only Bulk Temp is created; Conductivity was supplied by id: {resp}"
    );
    assert_eq!(
        resp["site_parameters_created"], 3,
        "one slot per (site, parameter) pair the streams name: {resp}"
    );
    assert_eq!(
        resp["streams_paired"], 3,
        "every stream on a listed site pairs: {resp}"
    );
    let skipped = resp["streams_skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("streams_skipped is an array: {resp}"));
    assert_eq!(
        skipped.len(),
        1,
        "one stream names a site the request omits: {resp}"
    );
    assert_eq!(
        skipped[0],
        json!(s_unlisted),
        "and it is the stream on the unlisted site: {resp}"
    );

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites \
             WHERE name = 'BW_A' AND latitude = 46.1 AND longitude = 7.2 AND altitude_m = 1900.0"
        )
        .await,
        1,
        "the request's coordinates land on the site it creates"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM sites WHERE id = '{bw_b}' AND latitude = 46.0")
        )
        .await,
        1,
        "a site supplied by id is used as-is; the request's null coordinates do not blank it"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{s_a_cond}' AND parameter_id = '{conductivity}' \
                   AND site_id = (SELECT id FROM sites WHERE name = 'BW_A')"
            )
        )
        .await,
        2,
        "the paired stream's history is attributed to the created site and the reused parameter"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{s_a_temp}' AND site_id IS NOT NULL"
            )
        )
        .await,
        3,
        "and so is the history of the stream whose parameter bulk-pair created"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{s_b_cond}' AND site_id = '{bw_b}'"
            )
        )
        .await,
        1,
        "the stream on the pre-existing site is attributed to it"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{s_unlisted}' AND site_id IS NOT NULL"
            )
        )
        .await,
        0,
        "the skipped stream's history stays unattributed"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams WHERE id = '{s_unlisted}' AND site_parameter_id IS NULL"
            )
        )
        .await,
        1,
        "and the skipped stream stays unpaired"
    );

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM data_streams WHERE source_system = 'bulkwiz' AND sensor_id IS NOT NULL"
        )
        .await,
        3,
        "pairing creates a sensor per paired stream"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM data_streams WHERE source_system = 'bulkwiz' AND pairing_plan_id IS NOT NULL"
        )
        .await,
        0,
        "bulk-pair writes no plan audit link, so it has no revert path"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM site_parameters sp JOIN sites s ON sp.site_id = s.id WHERE s.name = 'BW_A'"
        )
        .await,
        2,
        "BW_A holds both parameters its streams name"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM site_parameters WHERE site_id = '{bw_b}'")
        )
        .await,
        1,
        "BW_B holds the one parameter its stream names"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM parameters WHERE LOWER(code) = 'conductivity'"
        )
        .await,
        1,
        "the reused parameter is not duplicated"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "refresh_aggregates_full", 30).await,
        "bulk-pair enqueues an aggregate refresh for the readings it attributed, and it succeeds"
    );
}

#[tokio::test]
#[serial]
async fn apply_discovery_rolls_back_only_the_failing_action() {
    if !kc::require_keycloak_or_skip("apply_discovery_rolls_back_only_the_failing_action").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &admin, "Disc Host", "disc-host", false).await;
    let target_site =
        e2e::create_site(&app, &admin, &project, "Disc Target Site", "disc-target").await;
    let taken_site =
        e2e::create_site(&app, &admin, &project, "Disc Taken Site", "disc-taken").await;
    let parameter = e2e::create_parameter(&app, &admin, "DiscExist", "Disc Existing", "mg/L").await;
    let sp_target =
        e2e::assign_site_parameter_minimal(&app, &admin, &target_site, &parameter).await;
    let sp_taken = e2e::assign_site_parameter_minimal(&app, &admin, &taken_site, &parameter).await;

    let s_new = register_stream(
        &app,
        &admin,
        "discwiz",
        "new",
        json!({
            "hierarchy": { "project": "Disc New Project", "site": "Disc New Site", "parameter": "Disc New Param" },
            "units": "mg/L",
            "coordinates": { "latitude": 45.1, "longitude": 6.2, "altitude_m": 1500.0 },
        }),
    )
    .await;
    ingest(&app, &admin, &s_new, 3).await;
    let s_existing = unpaired_stream(
        &app,
        &admin,
        "discwiz",
        "existing",
        "Disc Host",
        "Disc Target Site",
        "Disc Existing",
        "mg/L",
        2,
    )
    .await;
    let s_taken = unpaired_stream(
        &app,
        &admin,
        "discwiz",
        "taken",
        "Disc Host",
        "Disc Taken Site",
        "Disc Existing",
        "mg/L",
        2,
    )
    .await;

    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{s_taken}/pair"),
        &json!({ "site_parameter_id": sp_taken }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair the third stream first ({status}): {paired}"
    );

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/apply-discovery",
        &json!({ "actions": [
            {
                "stream_id": s_new,
                "create_project": { "name": "Disc New Project" },
                "create_site": { "name": "Disc New Site" },
                "create_parameter": {
                    "code": "DiscNewParam", "name": "Disc New Param",
                    "default_units": "mg/L", "category": "measurement"
                },
                "create_site_parameter": { "display_units": "mg/L", "sample_interval_sec": 600 },
                "pair_to": "new"
            },
            {
                "stream_id": s_existing,
                "use_project_id": project,
                "use_site_id": target_site,
                "use_parameter_id": parameter,
                "pair_to": sp_target
            },
            {
                "stream_id": s_taken,
                "create_project": { "name": "Orphan Project" },
                "create_site": { "name": "Orphan Site" },
                "create_parameter": {
                    "code": "OrphanParam", "name": "Orphan Param",
                    "default_units": "x", "category": "measurement"
                },
                "pair_to": "new"
            }
        ]}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "apply-discovery ({status}): {resp}");
    assert_eq!(
        resp["projects_created"], 1,
        "only the new action's project: {resp}"
    );
    assert_eq!(
        resp["sites_created"], 1,
        "only the new action's site: {resp}"
    );
    assert_eq!(
        resp["parameters_created"], 1,
        "only the new action's parameter: {resp}"
    );
    assert_eq!(
        resp["site_parameters_created"], 1,
        "the second action pairs to an existing slot, so it creates none: {resp}"
    );
    assert_eq!(
        resp["sensors_created"], 2,
        "one sensor per stream that pairs: {resp}"
    );
    assert_eq!(
        resp["streams_paired"], 2,
        "the already-paired stream does not pair again: {resp}"
    );
    assert_eq!(
        resp["total_backfilled"], 5,
        "3 readings from the new-entity stream + 2 from the existing-slot stream: {resp}"
    );

    let errors = resp["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("errors is an array: {resp}"));
    assert_eq!(errors.len(), 1, "exactly one action fails: {resp}");
    let error = errors[0].as_str().unwrap_or_default();
    assert!(
        error.contains(&s_taken),
        "the error names the failing stream: {resp}"
    );
    assert!(
        error.contains("already paired"),
        "and says why it failed: {resp}"
    );

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM projects WHERE name = 'Orphan Project'"
        )
        .await,
        0,
        "the failing action's savepoint rolled back the project it created"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites WHERE LOWER(name) = 'orphan site'"
        )
        .await,
        0,
        "and the site"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM parameters WHERE code = 'OrphanParam'"
        )
        .await,
        0,
        "and the parameter"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams WHERE id = '{s_taken}' AND site_parameter_id = '{sp_taken}'"
            )
        )
        .await,
        1,
        "the already-paired stream keeps the pairing it had"
    );

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites WHERE name = 'Disc New Site'"
        )
        .await,
        1,
        "the succeeding action's site was created and its sibling's failure did not undo it"
    );
    // apply-discovery's `create_site` action carries a name only (`CreateSiteAction`), so a site it
    // creates has no coordinates even when the stream metadata holds them. Plan apply, whose entries
    // carry the coordinates, is the path that sets them.
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites WHERE name = 'Disc New Site' AND latitude IS NULL"
        )
        .await,
        1,
        "and it carries no coordinates, the request having supplied none"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{s_existing}' \
                 AND site_id = '{target_site}' AND parameter_id = '{parameter}'"
            )
        )
        .await,
        2,
        "the existing-slot action attributed its stream's history"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{s_new}' AND site_id IS NOT NULL"
            )
        )
        .await,
        3,
        "and the new-entity action attributed its own"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams WHERE id IN ('{s_new}', '{s_existing}') \
                 AND sensor_id IS NOT NULL"
            )
        )
        .await,
        2,
        "both paired streams carry the sensor pairing created for them"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "refresh_aggregates_full", 30).await,
        "apply-discovery enqueues an aggregate refresh for the readings it attributed, and it succeeds"
    );
}
