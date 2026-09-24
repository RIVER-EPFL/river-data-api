//! A step the chain could not run leaves a `skipped_output` hold on every slot it would have
//! filled, so the absence of a computed value is explained rather than silent.
//!
//! The skip is decided before the runner is reached: an input the visit has no value for fails
//! the manifest's requiredness check, which is database-only. No R runs here. A visit holding
//! none of what the step measures is not the step's to compute, and raises nothing.
//!
//! Run: cargo test --test tools skipped_output -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_COND_ID, GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_DO_ID, SITE1_ID};

const TOOL: &str = "skip_probe";
const EVENT_TIME: &str = "2025-06-15T09:00:00Z";

const SCRIPT: &str = r"tool <- function(inputs, constants, curves) list(out = inputs$t * 2)";

fn manifest() -> serde_json::Value {
    json!({
        "label": "Skip probe",
        "params": [
            { "name": "t", "label": "T", "kind": "number", "required": true },
            { "name": "c", "label": "C", "kind": "number", "required": false }
        ],
        "event_inputs": [
            { "param": "t", "parameter_code": "DO_Temperature" },
            { "param": "c", "parameter_code": "Conductivity" }
        ],
        "outputs": [{ "key": "out", "label": "Out", "parameter_id": GLOBAL_PARAM_DO_ID }],
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

async fn install_tool(db: &DatabaseConnection, name: &str, manifest: &serde_json::Value) {
    for (sql, values) in [
        (
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, $1, 'test')",
            vec![name.into()],
        ),
        (
            "INSERT INTO tool_script_versions
                 (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                  content_hash, created_by, validated_at)
             SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
             FROM tool_scripts s WHERE s.name = $1",
            vec![name.into(), SCRIPT.into(), manifest.to_string().into()],
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

/// A visit holding `readings` as `(parameter_id, value)` pairs, and nothing else.
async fn seed_visit(db: &DatabaseConnection, readings: &[(&str, f64)]) -> Uuid {
    let event_id = Uuid::new_v4();
    exec(
        db,
        "INSERT INTO collection_events (id, site_id, collected_at, source)
         VALUES ($1, $2::uuid, $3::timestamptz, 'manual')",
        vec![event_id.into(), SITE1_ID.into(), EVENT_TIME.into()],
    )
    .await;
    for (parameter_id, value) in readings {
        let stream_id = Uuid::new_v4();
        exec(
            db,
            "INSERT INTO data_streams (id, source_system, source_key, is_active)
             VALUES ($1, 'grab_sample', $2, true)",
            vec![
                stream_id.into(),
                format!("{SITE1_ID}:{parameter_id}").into(),
            ],
        )
        .await;
        exec(
            db,
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index,
                 raw_value, measurement_type, collection_event_id)
             VALUES ($1, $2::uuid, $3::uuid, $4::timestamptz, 0, $5, 'spot', $6)",
            vec![
                stream_id.into(),
                SITE1_ID.into(),
                (*parameter_id).into(),
                EVENT_TIME.into(),
                (*value).into(),
                event_id.into(),
            ],
        )
        .await;
    }
    event_id
}

/// A pending hold of `kind` on this visit's output slot, as an earlier pass left it.
async fn pending_hold(db: &DatabaseConnection, kind: &str) {
    exec(
        db,
        "INSERT INTO replicate_audit_holds
             (id, kind, site_id, parameter_id, group_time, expected, computed, delta, status, tool)
         VALUES (gen_random_uuid(), $4, $1::uuid, $2::uuid, $3::timestamptz,
                 '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, 'pending', $5)",
        vec![
            SITE1_ID.into(),
            GLOBAL_PARAM_DO_ID.into(),
            EVENT_TIME.into(),
            kind.into(),
            TOOL.into(),
        ],
    )
    .await;
}

struct Hold {
    kind: String,
    tool: Option<String>,
    status: String,
    reason: Option<String>,
    output: Option<String>,
}

/// Every hold standing on the output slot of this visit.
async fn holds_on_output(db: &DatabaseConnection) -> Vec<Hold> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT h.kind, h.tool, h.status,
                h.expected->>'reason' AS reason, h.expected->>'output' AS output
           FROM replicate_audit_holds h
          WHERE h.site_id = $1::uuid AND h.parameter_id = $2::uuid
            AND h.group_time = $3::timestamptz
          ORDER BY h.kind",
        [
            SITE1_ID.into(),
            GLOBAL_PARAM_DO_ID.into(),
            EVENT_TIME.into(),
        ],
    ))
    .await
    .expect("query")
    .into_iter()
    .map(|r| Hold {
        kind: r.try_get("", "kind").expect("kind"),
        tool: r.try_get("", "tool").expect("tool"),
        status: r.try_get("", "status").expect("status"),
        reason: r.try_get("", "reason").expect("reason"),
        output: r.try_get("", "output").expect("output"),
    })
    .collect()
}

async fn setup(readings: &[(&str, f64)]) -> (DatabaseConnection, river_db::common::AppState, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    install_tool(&db, TOOL, &manifest()).await;
    let event_id = seed_visit(&db, readings).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    (db, state, event_id)
}

#[tokio::test]
#[serial]
async fn a_step_whose_input_the_visit_lacks_is_reported_as_a_skipped_output() {
    let (db, state, event_id) = setup(&[(GLOBAL_PARAM_COND_ID, 5.0)]).await;

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("a step that cannot run is not a failed recompute");
    assert_eq!(outcome.tools_run, 0, "the runner was never reached");
    assert_eq!(outcome.findings_raised, 1, "one slot went unfilled");
    assert_eq!(
        outcome.skipped.len(),
        1,
        "the step is named in the outcome: {:?}",
        outcome.skipped
    );
    assert_eq!(outcome.skipped[0].0, TOOL);

    let holds = holds_on_output(&db).await;
    assert_eq!(holds.len(), 1, "one hold on the slot");
    assert_eq!(holds[0].kind, "skipped_output");
    assert_eq!(holds[0].tool.as_deref(), Some(TOOL));
    assert_eq!(holds[0].status, "pending");
    assert_eq!(holds[0].output.as_deref(), Some("out"));
    assert!(
        holds[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("missing required field 't'")),
        "the hold says why the step did not run: {:?}",
        holds[0].reason
    );
}

/// A slot some other path already filled is not reported: the value is there, so its absence is
/// not what has to be explained.
#[tokio::test]
#[serial]
async fn a_skipped_step_raises_nothing_for_an_output_that_already_has_a_value() {
    let (db, state, event_id) =
        setup(&[(GLOBAL_PARAM_DO_ID, 7.0), (GLOBAL_PARAM_COND_ID, 5.0)]).await;

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the recompute runs");
    assert_eq!(outcome.skipped.len(), 1, "the step still did not run");
    assert_eq!(outcome.findings_raised, 0);
    assert!(holds_on_output(&db).await.is_empty());
}

/// The skip is the executor's own account of the slot, and supersedes the audit's.
#[tokio::test]
#[serial]
async fn a_skip_supersedes_a_pending_missing_output_finding_on_the_same_slot() {
    let (db, state, event_id) = setup(&[(GLOBAL_PARAM_COND_ID, 5.0)]).await;
    pending_hold(&db, "missing_output").await;

    river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");

    let holds = holds_on_output(&db).await;
    let missing = holds
        .iter()
        .find(|h| h.kind == "missing_output")
        .expect("the audit's finding is still on the record");
    assert_ne!(
        missing.status, "pending",
        "it gave way to the executor's account"
    );
    assert!(
        holds
            .iter()
            .any(|h| h.kind == "skipped_output" && h.status == "pending"),
        "the skip is what stands"
    );
}

/// Scenario: the step once filed a skip at a visit that holds none of what it measures.
///
/// Expected behaviour: the recompute reports the step as not applicable there, files nothing, and
/// withdraws the skip it filed before, so the visit is no longer stale.
#[tokio::test]
#[serial]
async fn a_visit_holding_none_of_the_inputs_raises_nothing_and_clears_the_old_skip() {
    let (db, state, event_id) = setup(&[]).await;
    pending_hold(&db, "skipped_output").await;

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the recompute runs");
    assert!(
        outcome.not_applicable.iter().any(|t| t == TOOL),
        "the step is not the visit's: {:?}",
        outcome.not_applicable
    );
    assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.findings_raised, 0);
    assert_eq!(outcome.findings_closed, 1, "the old skip is withdrawn");
    assert!(
        holds_on_output(&db)
            .await
            .iter()
            .all(|h| h.status != "pending"),
        "no hold is left pending on the slot"
    );
}

/// Scenario: the probe skips at the visit, and a second step reads only the probe's output.
///
/// Expected behaviour: the second step's input is missing because the probe failed, so it files
/// its own finding on its own output rather than being not applicable there (Q327).
#[tokio::test]
#[serial]
async fn a_step_reading_only_a_failed_step_output_files_its_own_skip() {
    let (db, state, event_id) = setup(&[(GLOBAL_PARAM_COND_ID, 5.0)]).await;
    let downstream = json!({
        "label": "Skip downstream",
        "params": [{ "name": "d", "label": "D", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "d", "parameter_code": "Dissolved_O2" }],
        "outputs": [{ "key": "depth", "label": "Depth", "parameter_id": GLOBAL_PARAM_DEPTH_ID }],
    });
    install_tool(&db, "skip_downstream", &downstream).await;

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the recompute runs");
    assert!(
        outcome.not_applicable.is_empty(),
        "{:?}",
        outcome.not_applicable
    );
    let skipped: Vec<&str> = outcome.skipped.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(skipped, [TOOL, "skip_downstream"]);
    assert_eq!(outcome.findings_raised, 2, "one finding per step");
}
