//! The audit reports on the set the repair can repair: a calculation whose inputs a site declares
//! applies there and its absent output is a finding, whether or not the site already holds the
//! output slot (Q193, narrowing Q98); one whose inputs the site does not hold does not apply, so
//! its absent output is not a finding.
//!
//! No R runs here: the audit decides applicability before any runner is reached.
//!
//! Run: cargo test --test tools audit_applicability -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const TOOL: &str = "audit_probe";
const OUTPUT_PARAM_ID: &str = "00000000-0000-4000-b000-0000000000a1";
const EVENT_TIME: &str = "2025-06-16T09:00:00Z";

const SCRIPT: &str = r"tool <- function(inputs, constants, curves) list(out = inputs$t * 2)";

fn manifest() -> serde_json::Value {
    json!({
        "label": "Audit probe",
        "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "t", "parameter_code": "DO_Temperature" }],
        "outputs": [{ "key": "out", "label": "Out", "parameter_id": OUTPUT_PARAM_ID }],
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

async fn install_tool(db: &DatabaseConnection) {
    exec(
        db,
        "INSERT INTO parameters (id, code, name, default_units, category)
         VALUES ($1::uuid, 'AuditProbeOut', 'Audit probe out', 'ppb', 'measurement')",
        vec![OUTPUT_PARAM_ID.into()],
    )
    .await;
    for (sql, values) in [
        (
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, 'Audit probe', 'test')",
            vec![TOOL.into()],
        ),
        (
            "INSERT INTO tool_script_versions
                 (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                  content_hash, created_by, validated_at)
             SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
             FROM tool_scripts s WHERE s.name = $1",
            vec![TOOL.into(), SCRIPT.into(), manifest().to_string().into()],
        ),
        (
            "UPDATE tool_scripts s SET active_version_id = v.id
             FROM tool_script_versions v
             WHERE v.tool_script_id = s.id AND s.name = $1",
            vec![TOOL.into()],
        ),
    ] {
        exec(db, sql, values).await;
    }
}

/// A visit carrying the input the probe reads, and no output value.
async fn seed_visit(db: &DatabaseConnection) -> Uuid {
    let event_id = Uuid::new_v4();
    exec(
        db,
        "INSERT INTO collection_events (id, site_id, collected_at, source)
         VALUES ($1, $2::uuid, $3::timestamptz, 'manual')",
        vec![event_id.into(), SITE1_ID.into(), EVENT_TIME.into()],
    )
    .await;
    let stream_id = Uuid::new_v4();
    exec(
        db,
        "INSERT INTO data_streams (id, source_system, source_key, is_active)
         VALUES ($1, 'grab_sample', $2, true)",
        vec![stream_id.into(), format!("{SITE1_ID}:temp").into()],
    )
    .await;
    exec(
        db,
        "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index,
             raw_value, measurement_type, collection_event_id)
         VALUES ($1, $2::uuid, $3::uuid, $4::timestamptz, 0, 11.5, 'spot', $5)",
        vec![
            stream_id.into(),
            SITE1_ID.into(),
            GLOBAL_PARAM_TEMP_ID.into(),
            EVENT_TIME.into(),
            event_id.into(),
        ],
    )
    .await;
    event_id
}

/// The output slot, on the arm `cadence` names: `low` is written at a visit, `high` by the stream
/// engine.
async fn declare_output_slot(db: &DatabaseConnection, cadence: &str) {
    exec(
        db,
        "INSERT INTO site_parameters (site_id, parameter_id, name, cadence)
         VALUES ($1::uuid, $2::uuid, 'Audit probe out', $3)",
        vec![SITE1_ID.into(), OUTPUT_PARAM_ID.into(), cadence.into()],
    )
    .await;
}

async fn findings_on_output(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT count(*)::bigint AS n FROM replicate_audit_holds
          WHERE site_id = $1::uuid AND parameter_id = $2::uuid
            AND group_time = $3::timestamptz AND tool = $4",
        [
            SITE1_ID.into(),
            OUTPUT_PARAM_ID.into(),
            EVENT_TIME.into(),
            TOOL.into(),
        ],
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get::<i64>("", "n")
    .expect("n")
}

async fn audit(state: &river_db::common::AppState, event_id: Uuid) {
    use river_db::routes::private::tools::flows;
    use river_db::routes::private::tools::models::AuditCounts;
    use river_db::routes::private::tools::service;

    let event = flows::load_event(&state.db, event_id).await.expect("event");
    let tools = service::list_active_tools(&state.db)
        .await
        .expect("active tools");
    let catalog = service::load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest))
        .await
        .expect("catalog");
    let order = flows::dependency_order(&tools, &catalog).expect("an order");
    let mut counts = AuditCounts {
        events_audited: 0,
        missing: 0,
        stale: 0,
        skipped: 0,
        superseded: 0,
    };
    flows::audit_event(state, &event, &tools, &catalog, &order, &mut counts)
        .await
        .expect("the audit runs");
}

async fn setup() -> (DatabaseConnection, river_db::common::AppState, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    install_tool(&db).await;
    let event_id = seed_visit(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    (db, state, event_id)
}

#[tokio::test]
#[serial]
async fn a_calculation_whose_inputs_the_site_holds_reports_its_absent_output() {
    let (db, state, event_id) = setup().await;
    audit(&state, event_id).await;
    assert_eq!(
        findings_on_output(&db).await,
        1,
        "the site declares what the calculation reads, so the recompute can repair this and the \
         audit says so; the output slot is the run's to mint"
    );
}

#[tokio::test]
#[serial]
async fn a_calculation_whose_inputs_the_site_lacks_raises_no_finding() {
    let (db, state, event_id) = setup().await;
    // The probe now reads a parameter no site declares, so it belongs to no site here.
    exec(
        &db,
        "INSERT INTO parameters (id, code, name, default_units, category)
         VALUES (gen_random_uuid(), 'AuditProbeUnheld', 'Audit probe unheld', 'ppb', 'measurement')",
        vec![],
    )
    .await;
    exec(
        &db,
        "UPDATE tool_script_versions
            SET manifest = jsonb_set(manifest, '{event_inputs}',
                '[{\"param\": \"t\", \"parameter_code\": \"AuditProbeUnheld\"}]'::jsonb)
          WHERE tool_script_id = (SELECT id FROM tool_scripts WHERE name = $1)",
        vec![TOOL.into()],
    )
    .await;
    audit(&state, event_id).await;
    assert_eq!(
        findings_on_output(&db).await,
        0,
        "the site declares neither what the calculation reads nor what it writes"
    );
}

#[tokio::test]
#[serial]
async fn the_same_calculation_reports_its_absent_output_where_the_site_declared_the_slot() {
    let (db, state, event_id) = setup().await;
    declare_output_slot(&db, "low").await;
    audit(&state, event_id).await;
    assert_eq!(
        findings_on_output(&db).await,
        1,
        "the input is there and the output is not, which is the finding"
    );
}

/// Scenario: the site declares the output slot high cadence, so its stream fills it and the chain
/// never writes it at a visit.
///
/// Expected behaviour: the audit reports nothing a recompute would refuse to act on.
#[tokio::test]
#[serial]
async fn an_output_on_a_high_cadence_slot_raises_no_finding() {
    let (db, state, event_id) = setup().await;
    declare_output_slot(&db, "high").await;
    audit(&state, event_id).await;
    assert_eq!(findings_on_output(&db).await, 0);
}

/// Scenario: an admin detached the output slot at the visit, so the value there is the
/// operator's.
///
/// Expected behaviour: the audit reports nothing on it.
#[tokio::test]
#[serial]
async fn an_output_on_a_detached_slot_raises_no_finding() {
    let (db, state, event_id) = setup().await;
    declare_output_slot(&db, "low").await;
    let stream_id = Uuid::new_v4();
    exec(
        &db,
        "INSERT INTO data_streams (id, source_system, source_key, is_active)
         VALUES ($1, 'grab_sample', $2, true)",
        vec![stream_id.into(), format!("{SITE1_ID}:out").into()],
    )
    .await;
    exec(
        &db,
        "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index,
             raw_value, measurement_type)
         VALUES ($1, $2::uuid, $3::uuid, $4::timestamptz, 0, 7.0, 'derived')",
        vec![
            stream_id.into(),
            SITE1_ID.into(),
            OUTPUT_PARAM_ID.into(),
            EVENT_TIME.into(),
        ],
    )
    .await;
    exec(
        &db,
        "INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, actor, origin)
         VALUES ($1, $2::timestamptz, 0, 'detach', 'test', 'manual')",
        vec![stream_id.into(), EVENT_TIME.into()],
    )
    .await;
    audit(&state, event_id).await;
    assert_eq!(findings_on_output(&db).await, 0);
}
