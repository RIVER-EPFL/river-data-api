//! M283: the calculation chain previewed against what an operator has typed and not saved.
//!
//! Scenario: a visit holds a replicate family a calculation reads; the operator corrects one
//! repeat, adds another, and empties a third. Expected behaviour: the preview is the same walk the
//! save runs, over the same inputs, and it writes nothing. What the preview reports and what the
//! save then stores are the same numbers from the same consumed provenance.
//!
//! Run: cargo test --test tools staged_preview -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_COND_ID, GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};
use river_db::routes::private::tools::flows::{preview_event, recompute_event};
use river_db::routes::private::tools::staged::StagedCell;

const AT: &str = "2025-06-15T09:00:00Z";

/// Stage one: reads the visit's temperature (the family's served statistic) and doubles it.
const SCRIPT_A: &str = r"tool <- function(inputs, constants, curves) {
  list(out_a = inputs$t * 2)
}";

/// Stage two: reads what stage one published and adds one, so it can only be right if the
/// preview fed stage one's output forward.
const SCRIPT_B: &str = r"tool <- function(inputs, constants, curves) {
  list(out_b = inputs$a + 1)
}";

fn manifest_a() -> serde_json::Value {
    json!({
        "label": "Stage one",
        "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "t", "parameter_code": "DO_Temperature" }],
        "outputs": [{ "key": "out_a", "label": "A", "parameter_id": GLOBAL_PARAM_DO_ID }],
    })
}

fn manifest_b() -> serde_json::Value {
    json!({
        "label": "Stage two",
        "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "a", "parameter_code": "Dissolved_O2" }],
        "outputs": [{ "key": "out_b", "label": "B", "parameter_id": GLOBAL_PARAM_COND_ID }],
    })
}

async fn exec(db: &DatabaseConnection, sql: &str, values: Vec<sea_orm::Value>) {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\n{sql}"));
}

async fn install_script(db: &DatabaseConnection, name: &str, script: &str, manifest: &str) {
    for (sql, values) in [
        (
            "INSERT INTO tool_scripts (name, label, engine, created_by)
             VALUES ($1, $1, 'script', 'test')",
            vec![name.into()],
        ),
        (
            "INSERT INTO tool_script_versions
                 (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                  content_hash, created_by, validated_at)
             SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
             FROM tool_scripts s WHERE s.name = $1",
            vec![name.into(), script.into(), manifest.into()],
        ),
        (
            "UPDATE tool_scripts s SET active_version_id = v.id
             FROM tool_script_versions v
             WHERE v.tool_script_id = s.id AND s.name = $1",
            vec![name.into()],
        ),
    ] {
        exec(db, sql, values).await;
    }
}

/// The two dependent calculations the parity fixture runs.
async fn install_chain(db: &DatabaseConnection) {
    install_script(db, "staged_a", SCRIPT_A, &manifest_a().to_string()).await;
    install_script(db, "staged_b", SCRIPT_B, &manifest_b().to_string()).await;
}

/// A formula calculation and the set of formulas it publishes, saved as the authoring route saves
/// them: the save is what mints the version a run reads and the catalog parameter it writes to.
async fn install_formula_set(
    app: &axum::Router,
    db: &DatabaseConnection,
    token: &str,
    name: &str,
    formulas: serde_json::Value,
) {
    let script_id = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, engine, created_by)
             VALUES ($1, $1, 'formula', 'test') RETURNING id",
            [name.into()],
        ))
        .await
        .expect("query")
        .expect("the calculation")
        .try_get::<Uuid>("", "id")
        .expect("id")
        .to_string();
    let (status, text) = crate::common::save_formula_set(app, token, &script_id, formulas).await;
    assert!(
        (200..300).contains(&status),
        "save the formula set ({status}): {text}"
    );
}

/// The catalog parameter a formula minted for its output code.
async fn minted_parameter(db: &DatabaseConnection, code: &str) -> String {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT id FROM parameters WHERE lower(code) = lower($1)",
        [code.into()],
    ))
    .await
    .expect("query")
    .expect("the formula minted its output parameter")
    .try_get::<Uuid>("", "id")
    .expect("id")
    .to_string()
}

struct Visit {
    db: DatabaseConnection,
    app: axum::Router,
    state: river_db::common::AppState,
    token: String,
    event_id: Uuid,
}

/// A visit at site 1 holding a two-repeat temperature family, entered through the save path so the
/// `samples` trigger derives its statistics.
async fn visit_with_family(values: &[(i16, f64)]) -> Visit {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    // The chain's outputs are visit calculations: a high-cadence slot is computed on its stream.
    for parameter_id in [GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_COND_ID] {
        exec(
            &db,
            "UPDATE site_parameters SET cadence = 'low' \
             WHERE site_id = $1::uuid AND parameter_id = $2::uuid",
            vec![SITE1_ID.into(), parameter_id.into()],
        )
        .await;
    }

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": SITE1_ID, "collected_at": AT }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "stage the visit ({status}): {text}");
    let staged: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let event_id: Uuid = staged["id"].as_str().expect("id").parse().expect("uuid");

    save_family(&app, &token, GLOBAL_PARAM_TEMP_ID, values).await;
    Visit {
        db,
        app,
        state,
        token,
        event_id,
    }
}

/// Write one parameter's family at the visit through the grab path, replacing whatever stands.
async fn save_family(app: &axum::Router, token: &str, parameter_id: &str, values: &[(i16, f64)]) {
    let readings: Vec<serde_json::Value> = values
        .iter()
        .map(|(index, value)| {
            json!({
                "parameter_id": parameter_id, "value": value, "time": AT,
                "replicate_index": index,
            })
        })
        .collect();
    let (status, text) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "mode": "replace", "readings": readings }),
        token,
    )
    .await;
    assert_eq!(status, 200, "save the family ({status}): {text}");
}

fn cell(parameter_id: &str, replicate_index: i16, value: Option<f64>) -> StagedCell {
    StagedCell {
        parameter_id: parameter_id.parse().expect("uuid"),
        replicate_index,
        value,
        standard_curve_id: None,
    }
}

/// The served value of one output slot at the visit.
async fn stored_value(db: &DatabaseConnection, parameter_id: &str) -> Option<f64> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT COALESCE(r.calibrated_value, r.raw_value) AS v FROM readings r
          WHERE r.site_id = $1::uuid AND r.parameter_id = $2::uuid AND r.time = $3::timestamptz
            AND r.withdrawn_at IS NULL
          ORDER BY r.replicate_index LIMIT 1",
        [SITE1_ID.into(), parameter_id.into(), AT.into()],
    ))
    .await
    .expect("query")
    .and_then(|row| row.try_get::<Option<f64>>("", "v").expect("v"))
}

fn output(
    preview: &river_db::routes::private::tools::models::EventPreview,
    key: &str,
) -> Option<f64> {
    preview
        .outputs
        .iter()
        .find(|o| o.output == key)
        .and_then(|o| o.value)
}

/// What a run consumed, as `(variable, kind, value)` with the member rows behind it. The identity
/// both sides of the parity comparison are read in.
fn consumed_shape(consumed: &serde_json::Value) -> Vec<(String, String, String, Vec<String>)> {
    consumed
        .as_array()
        .expect("consumed is an array")
        .iter()
        .map(|c| {
            (
                c["variable"].as_str().unwrap_or_default().to_string(),
                c["kind"].as_str().unwrap_or_default().to_string(),
                c["value"].to_string(),
                c["members"]
                    .as_array()
                    .map(|members| {
                        members
                            .iter()
                            .map(|m| {
                                format!(
                                    "{}:{}:{}",
                                    m["stream_id"], m["replicate_index"], m["value"]
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// The stored run of one calculation at the visit, with the provenance it recorded.
async fn stored_run(db: &DatabaseConnection, tool: &str) -> serde_json::Value {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT to_jsonb(t) AS run FROM tool_runs t
          WHERE t.tool_name = $1 AND t.site_id = $2::uuid AND t.collected_at = $3::timestamptz
          ORDER BY t.created_at DESC LIMIT 1",
        [tool.into(), SITE1_ID.into(), AT.into()],
    ))
    .await
    .expect("query")
    .map(|row| row.try_get::<serde_json::Value>("", "run").expect("run"))
    .unwrap_or_else(|| panic!("{tool} stored a run at the visit"))
}

/// A count of everything a preview is forbidden to create.
async fn write_counts(db: &DatabaseConnection) -> (i64, i64, i64, i64, i64, String) {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT (SELECT count(*) FROM tool_runs) AS runs,
                    (SELECT count(*) FROM reading_decisions) AS decisions,
                    (SELECT count(*) FROM replicate_audit_holds) AS findings,
                    (SELECT count(*) FROM site_parameters) AS slots,
                    (SELECT count(*) FROM reprocessing_jobs) AS jobs,
                    (SELECT COALESCE(md5(string_agg(x, '|' ORDER BY x)), '') FROM (
                        SELECT r.stream_id::text || r.time::text || r.replicate_index::text ||
                               COALESCE(r.raw_value::text, '') ||
                               COALESCE(r.calibrated_value::text, '') ||
                               COALESCE(r.withdrawn_at::text, '') AS x
                        FROM readings r) s) AS readings"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("a row");
    (
        row.try_get("", "runs").expect("runs"),
        row.try_get("", "decisions").expect("decisions"),
        row.try_get("", "findings").expect("findings"),
        row.try_get("", "slots").expect("slots"),
        row.try_get("", "jobs").expect("jobs"),
        row.try_get("", "readings").expect("readings"),
    )
}

/// An `AppState` on a connection that cannot write: every statement runs in a read-only
/// transaction, so a write the preview attempts fails there rather than being rolled back
/// unnoticed.
async fn read_only_state() -> river_db::common::AppState {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let separator = if url.contains('?') { '&' } else { '?' };
    let mut opts = sea_orm::ConnectOptions::new(format!(
        "{url}{separator}options=-c%20default_transaction_read_only%3Don"
    ));
    opts.max_connections(4).sqlx_logging(false);
    let db = sea_orm::Database::connect(opts)
        .await
        .expect("a read-only connection");
    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT 1".to_string(),
    ))
    .await
    .expect("the read-only connection answers");
    assert!(
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE TEMP TABLE preview_boundary_probe (x int)".to_string(),
        ))
        .await
        .is_err(),
        "the connection must refuse writes, or the boundary proves nothing"
    );
    river_db::common::AppState::new(db, crate::common::test_config(), None)
}

/// The correction, the new repeat and the emptied cell, previewed and then saved: the same two
/// numbers come out of both, from the same consumed provenance.
#[tokio::test]
#[serial]
async fn a_staged_visit_previews_the_values_its_save_stores() {
    if !crate::common::profile::Service::ToolsRunner
        .require("a_staged_visit_previews_the_values_its_save_stores")
        .await
    {
        return;
    }
    let v = visit_with_family(&[(0, 10.0), (1, 20.0)]).await;
    // The visit already holds the value the last run published, which is the normal case and what
    // gives the output slot the row a consumed member names.
    save_family(&v.app, &v.token, GLOBAL_PARAM_DO_ID, &[(0, 30.0)]).await;
    install_chain(&v.db).await;

    // The operator corrects repeat 0, empties repeat 1 and measures a third: 4 and 8, mean 6.
    let staged = vec![
        cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0)),
        cell(GLOBAL_PARAM_TEMP_ID, 1, None),
        cell(GLOBAL_PARAM_TEMP_ID, 2, Some(8.0)),
    ];
    let preview = preview_event(&v.state, v.event_id, &staged)
        .await
        .expect("the preview runs");

    assert_eq!(output(&preview, "out_a"), Some(12.0), "2 * mean(4, 8)");
    assert_eq!(
        output(&preview, "out_b"),
        Some(13.0),
        "stage two read stage one's output through the overlay, not the store"
    );
    assert!(
        preview.skipped.is_empty(),
        "nothing was skipped: {:?}",
        preview.skipped
    );

    // Saving exactly those cells and running the chain for keeps.
    save_family(
        &v.app,
        &v.token,
        GLOBAL_PARAM_TEMP_ID,
        &[(0, 4.0), (2, 8.0)],
    )
    .await;
    let outcome = recompute_event(&v.state, v.event_id, "test")
        .await
        .expect("the recompute runs");
    assert_eq!(outcome.tools_run, 2, "both stages ran for keeps");

    assert_eq!(stored_value(&v.db, GLOBAL_PARAM_DO_ID).await, Some(12.0));
    assert_eq!(stored_value(&v.db, GLOBAL_PARAM_COND_ID).await, Some(13.0));

    // The provenance each stage consumed, previewed and stored.
    for (tool, index) in [("staged_a", 0), ("staged_b", 1)] {
        let run = stored_run(&v.db, tool).await;
        let stored = consumed_shape(&run["context"]["consumed"]);
        let previewed = consumed_shape(
            &serde_json::to_value(&preview.calculations[index].consumed).expect("JSON"),
        );
        assert_eq!(
            previewed, stored,
            "{tool} consumed the same inputs on both paths"
        );
        assert_eq!(
            preview.calculations[index].tool_version.script_version_id,
            run["tool_version"]["script_version_id"]
                .as_str()
                .map(|s| s.parse::<Uuid>().expect("uuid")),
            "{tool} ran under the version the save pinned"
        );
    }
}

/// The preview attempts no write at all, rolled-back ones included: it runs on a connection that
/// cannot write, and leaves every count and every stored row where it found them.
#[tokio::test]
#[serial]
async fn a_preview_attempts_no_write() {
    if !crate::common::profile::Service::ToolsRunner
        .require("a_preview_attempts_no_write")
        .await
    {
        return;
    }
    let v = visit_with_family(&[(0, 10.0), (1, 20.0)]).await;
    install_chain(&v.db).await;

    let before = write_counts(&v.db).await;
    let read_only = read_only_state().await;
    let preview = preview_event(
        &read_only,
        v.event_id,
        &[
            cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0)),
            cell(GLOBAL_PARAM_TEMP_ID, 1, None),
            cell(GLOBAL_PARAM_TEMP_ID, 2, Some(8.0)),
        ],
    )
    .await
    .expect("the preview runs with no write available to it");
    assert_eq!(output(&preview, "out_a"), Some(12.0));
    assert_eq!(output(&preview, "out_b"), Some(13.0));

    assert_eq!(
        write_counts(&v.db).await,
        before,
        "no run, no decision, no finding, no slot, no job, and every stored row as it was"
    );
}

/// A per-replicate output feeds the scalar that reads it: stage one publishes a family, and the
/// statistic over it is what stage two consumes, within one pass.
#[tokio::test]
#[serial]
async fn a_per_replicate_output_is_read_as_a_statistic_by_the_next_stage() {
    if !crate::common::profile::Service::ToolsRunner
        .require("a_per_replicate_output_is_read_as_a_statistic_by_the_next_stage")
        .await
    {
        return;
    }
    let v = visit_with_family(&[(0, 10.0), (1, 20.0)]).await;
    // Stage one doubles each repeat of the family and publishes the repeats themselves.
    install_script(
        &v.db,
        "staged_a",
        r"tool <- function(inputs, constants, curves) {
  list(out_a = lapply(inputs$reps, function(x) if (is.null(x)) NULL else x * 2))
}",
        &json!({
            "label": "Stage one",
            "params": [{
                "name": "reps", "label": "Repeats", "kind": "replicates",
                "required": true, "parameter_code": "DO_Temperature",
            }],
            "outputs": [{ "key": "out_a", "label": "A", "parameter_id": GLOBAL_PARAM_DO_ID }],
        })
        .to_string(),
    )
    .await;
    install_script(&v.db, "staged_b", SCRIPT_B, &manifest_b().to_string()).await;

    let preview = preview_event(
        &v.state,
        v.event_id,
        &[
            cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0)),
            cell(GLOBAL_PARAM_TEMP_ID, 1, Some(8.0)),
        ],
    )
    .await
    .expect("the preview runs");

    let published: Vec<Option<f64>> = preview
        .outputs
        .iter()
        .filter(|o| o.output == "out_a")
        .map(|o| o.value)
        .collect();
    assert_eq!(
        published,
        vec![Some(8.0), Some(16.0)],
        "each repeat doubled"
    );
    assert_eq!(
        output(&preview, "out_b"),
        Some(13.0),
        "the second stage read the mean of 8 and 16, recomputed over the overlay, not a repeat"
    );
}

/// A slot somebody detached at the visit is a manual value: the preview leaves it alone and says
/// so, exactly as the save does.
#[tokio::test]
#[serial]
async fn a_detached_output_is_reported_and_left_alone() {
    if !crate::common::profile::Service::ToolsRunner
        .require("a_detached_output_is_reported_and_left_alone")
        .await
    {
        return;
    }
    let v = visit_with_family(&[(0, 10.0), (1, 20.0)]).await;
    install_script(&v.db, "staged_a", SCRIPT_A, &manifest_a().to_string()).await;
    // The chain takes the slot, then it is detached and a person's number put there. The detach
    // route is Administrator-only, so the decision it appends is written here directly.
    recompute_event(&v.state, v.event_id, "test")
        .await
        .expect("the recompute runs");
    save_family(&v.app, &v.token, GLOBAL_PARAM_DO_ID, &[(0, 99.0)]).await;
    exec(
        &v.db,
        "INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, old, new, actor,
             origin)
         SELECT r.stream_id, r.time, r.replicate_index, 'detach', '{}'::jsonb, '{}'::jsonb,
                'tester', 'manual'
         FROM readings r
         WHERE r.site_id = $1::uuid AND r.parameter_id = $2::uuid AND r.time = $3::timestamptz",
        vec![SITE1_ID.into(), GLOBAL_PARAM_DO_ID.into(), AT.into()],
    )
    .await;

    let preview = preview_event(
        &v.state,
        v.event_id,
        &[cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0))],
    )
    .await
    .expect("the preview runs");
    assert!(
        preview
            .skipped
            .iter()
            .any(|(_, reason)| reason.contains("detached")),
        "the detached slot is reported: {:?}",
        preview.skipped
    );
    assert!(
        output(&preview, "out_a").is_none(),
        "a detached slot takes no previewed value either"
    );
    assert_eq!(
        stored_value(&v.db, GLOBAL_PARAM_DO_ID).await,
        Some(99.0),
        "the manual value stands"
    );
}

/// An output the arithmetic refused is reported as refused, and the preview files no finding for
/// it (Q172).
#[tokio::test]
#[serial]
async fn a_refused_output_is_reported_and_files_nothing() {
    let v = visit_with_family(&[(0, 10.0), (1, 20.0)]).await;
    install_formula_set(
        &v.app,
        &v.db,
        &v.token,
        "staged_refuses",
        json!([{
            "code": "StagedRefused", "name": "Staged refused", "units": "NTU",
            "formula": "DO_Temperature / 0", "ordinal": 1,
        }]),
    )
    .await;

    let findings_before = write_counts(&v.db).await.2;
    let preview = preview_event(
        &v.state,
        v.event_id,
        &[cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0))],
    )
    .await
    .expect("the preview runs");
    assert!(
        preview.calculations[0]
            .refused
            .iter()
            .any(|r| r == "StagedRefused"),
        "the division by zero is refused: {:?}",
        preview.calculations[0]
    );
    assert!(
        output(&preview, "StagedRefused").is_none(),
        "a refused output has no previewed value"
    );
    assert_eq!(
        write_counts(&v.db).await.2,
        findings_before,
        "a previewed refusal files no finding"
    );
}

/// A staged number is corrected by the slot's instrument and by the curve the operator picked,
/// in the one order the save applies them: the preview and the save agree because they agree on
/// the arithmetic, not because the numbers happened to match.
#[tokio::test]
#[serial]
async fn a_staged_cell_is_corrected_by_the_instrument_and_the_chosen_curve() {
    let v = visit_with_family(&[(0, 10.0)]).await;
    let sensor =
        crate::common::sensor_lifecycle::create_sensor_without_curve(&v.db, "analyser").await;
    crate::common::exec(
        &v.db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from) \
             VALUES (gen_random_uuid(), '{sensor}', 2.0, 1.0, '2025-01-01T00:00:00Z')"
        ),
    )
    .await;
    let curve = Uuid::new_v4();
    crate::common::exec(
        &v.db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 3.0, 0.0, 'Plate A')"
        ),
    )
    .await;
    crate::common::exec(
        &v.db,
        &format!(
            "UPDATE site_parameters SET instrument_sensor_id = '{sensor}' \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}'"
        ),
    )
    .await;
    install_formula_set(
        &v.app,
        &v.db,
        &v.token,
        "staged_curved",
        json!([{
            "code": "StagedCurved", "name": "Staged curved", "units": "NTU",
            "formula": "DO_Temperature * 10", "ordinal": 1,
        }]),
    )
    .await;

    // 4 corrected by the instrument (2x + 1) is 9, then by the chosen curve (3x) is 27.
    let mut staged = cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0));
    staged.standard_curve_id = Some(curve);
    let preview = preview_event(&v.state, v.event_id, &[staged])
        .await
        .expect("the preview runs");
    assert_eq!(
        output(&preview, "StagedCurved"),
        Some(270.0),
        "10 * apply_curves(4): {:?}",
        preview.skipped
    );

    // The same cell saved, corrected by the same two curves on the way in.
    let (status, text) = crate::common::post_json_with_token(
        &v.app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID, "mode": "replace",
            "readings": [{
                "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.0, "time": AT,
                "replicate_index": 0, "standard_curve_id": curve,
            }],
        }),
        &v.token,
    )
    .await;
    assert_eq!(status, 200, "save the corrected cell ({status}): {text}");
    assert_eq!(
        stored_value(&v.db, GLOBAL_PARAM_TEMP_ID).await,
        Some(27.0),
        "the save applies the instrument, then the curve"
    );
    recompute_event(&v.state, v.event_id, "test")
        .await
        .expect("the recompute runs");
    let minted = minted_parameter(&v.db, "StagedCurved").await;
    assert_eq!(stored_value(&v.db, &minted).await, Some(270.0));
}

/// The formula engine runs the same walk: a formula calculation previews the value its save
/// stores, so the shared path is not a script-only claim.
#[tokio::test]
#[serial]
async fn a_formula_calculation_previews_the_value_its_save_stores() {
    let v = visit_with_family(&[(0, 10.0), (1, 20.0)]).await;
    install_formula_set(
        &v.app,
        &v.db,
        &v.token,
        "staged_formula",
        json!([{
            "code": "StagedCalc", "name": "Staged calc", "units": "NTU",
            "formula": "DO_Temperature * 2", "ordinal": 1,
        }]),
    )
    .await;

    let preview = preview_event(
        &v.state,
        v.event_id,
        &[
            cell(GLOBAL_PARAM_TEMP_ID, 0, Some(4.0)),
            cell(GLOBAL_PARAM_TEMP_ID, 1, Some(8.0)),
        ],
    )
    .await
    .expect("the preview runs");
    assert_eq!(
        output(&preview, "StagedCalc"),
        Some(12.0),
        "2 * mean(4, 8): {:?}",
        preview.skipped
    );

    save_family(
        &v.app,
        &v.token,
        GLOBAL_PARAM_TEMP_ID,
        &[(0, 4.0), (1, 8.0)],
    )
    .await;
    recompute_event(&v.state, v.event_id, "test")
        .await
        .expect("the recompute runs");
    let minted = minted_parameter(&v.db, "StagedCalc").await;
    assert_eq!(
        stored_value(&v.db, &minted).await,
        Some(12.0),
        "the save stored what the preview showed"
    );
}
