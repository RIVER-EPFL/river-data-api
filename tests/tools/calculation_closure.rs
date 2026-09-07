//! What an edit will recompute, and where a calculation's data lives, known before the write.
//!
//! The dependency graph lived only inside the chain executor, so an operator editing a 2023 DOC
//! replicate could not see that a DOM output at that visit is computed from it, and an admin
//! activating a calculation could not see that one of its inputs is configured at no site. Both
//! read the same closure the chain runs on.
//!
//! Run: cargo test --test tools calculation_closure -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-02-10T08:00:00Z";

/// One calculation reading the seeded temperature parameter and writing the seeded DO one, plus a
/// second reading that output, so the closure has a chain to walk rather than one edge.
async fn install_chain(db: &DatabaseConnection, temp_code: &str, do_code: &str) {
    let tools = [
        (
            "closure_a",
            json!({
                "label": "Closure A",
                "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
                "event_inputs": [{ "param": "t", "parameter_code": temp_code }],
                "outputs": [{ "key": "out", "label": "O", "suggested_parameter_code": do_code }],
            }),
        ),
        (
            "closure_b",
            json!({
                "label": "Closure B",
                "params": [{ "name": "d", "label": "D", "kind": "number", "required": true }],
                "event_inputs": [{ "param": "d", "parameter_code": do_code }],
                "outputs": [{ "key": "out", "label": "O2", "suggested_parameter_code": "ClosureO2" }],
            }),
        ),
    ];
    // The seeded portal tools are reference data and survive cleanup, so these fixtures do too:
    // remove the prior run's before installing this one's.
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'closure\\_%'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name LIKE 'closure\\_%'",
        "DELETE FROM tool_scripts WHERE name LIKE 'closure\\_%'",
    ] {
        db.execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql.to_string(),
        ))
        .await
        .expect("prior fixtures removed");
    }
    for (name, manifest) in tools {
        for statement in [
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, $1, 'test')",
                [name.into()],
            ),
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO tool_script_versions
                      (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                       content_hash, created_by, validated_at)
                  SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2 || $1), 'test', now()
                  FROM tool_scripts s WHERE s.name = $1",
                [
                    name.into(),
                    "tool <- function(inputs, constants, curves) list(out = 1)".into(),
                    manifest.to_string().into(),
                ],
            ),
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE tool_scripts s SET active_version_id = v.id
                  FROM tool_script_versions v
                  WHERE v.tool_script_id = s.id AND s.name = $1",
                [name.into()],
            ),
        ] {
            db.execute_raw(statement)
                .await
                .expect("calculation installed");
        }
    }
}

async fn code_of(db: &DatabaseConnection, parameter_id: &str) -> String {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT code FROM parameters WHERE id = $1::uuid",
        [parameter_id.into()],
    ))
    .await
    .expect("query")
    .expect("the seeded parameter")
    .try_get::<String>("", "code")
    .expect("code")
}

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let temp = code_of(&db, GLOBAL_PARAM_TEMP_ID).await;
    let do_code = code_of(&db, GLOBAL_PARAM_DO_ID).await;
    install_chain(&db, &temp, &do_code).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Expected behaviour: touching the temperature parameter reaches both calculations, the second
/// through the first's output, in the order the chain would run them.
#[tokio::test]
#[serial]
async fn the_closure_names_every_calculation_the_edit_reaches() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/calculations/closure?parameter_ids={GLOBAL_PARAM_TEMP_ID}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let names: Vec<&str> = body["calculations"]
        .as_array()
        .expect("calculations")
        .iter()
        .filter_map(|c| c["tool"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["closure_a", "closure_b"],
        "producer first, then the calculation reading its output: {body}"
    );
    assert_eq!(
        body["calculations"][1]["reads"][0]["parameter_id"], GLOBAL_PARAM_TEMP_ID,
        "the downstream calculation is traced back to the edited value: {body}"
    );
}

/// Expected behaviour: a parameter nothing reads reaches nothing, so a logger series does not
/// warn about consequences it has none of.
#[tokio::test]
#[serial]
async fn a_parameter_no_calculation_reads_reaches_nothing() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/calculations/closure?parameter_ids=00000000-0000-4000-8000-0000000000ff",
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["calculations"].as_array().expect("array").is_empty(),
        "{body}"
    );
}

/// Expected behaviour: an admin can see whether a calculation's slots are configured anywhere and
/// what the stored values under them came from.
#[tokio::test]
#[serial]
async fn coverage_says_where_a_calculation_s_data_lives() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/calculations/closure?include_coverage=true",
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let coverage = body["coverage"].as_array().expect("coverage");
    let unconfigured = coverage
        .iter()
        .find(|c| c["parameter_code"] == "ClosureO2");
    assert!(
        unconfigured.is_none() || unconfigured.unwrap()["sites_configured"] == 0,
        "an output no site configures reports zero rather than being omitted: {body}"
    );

    let temp = coverage
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_TEMP_ID)
        .expect("the calculation's input is covered");
    assert!(
        temp["sites_configured"].as_i64().unwrap_or(0) >= 1,
        "the seeded slot is configured: {temp}"
    );
    assert!(
        temp["reading_count"].as_i64().unwrap_or(0) > 0,
        "and holds readings: {temp}"
    );
    assert!(
        temp["source_systems"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "the stored values name where they came from: {temp}"
    );
}

/// Expected behaviour: the grid marks each cell with the calculations that read it and the one
/// that writes it, so the role is visible while a value is being typed.
#[tokio::test]
#[serial]
async fn the_visit_detail_marks_each_cell_s_role() {
    let (_db, app, token) = setup().await;

    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.0, "time": AT },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": AT },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{saved}");

    let (status, visits) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{visits}");
    let event_id = visits["visits"][0]["id"].as_str().expect("visit id");

    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{detail}");

    let cells = detail["cells"].as_array().expect("cells");
    let input = cells
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_TEMP_ID)
        .expect("the input cell");
    assert_eq!(
        input["read_by"][0], "closure_a",
        "the cell names the script it feeds: {input}"
    );

    let output = cells
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the output cell");
    assert_eq!(
        output["written_by"], "closure_a",
        "a computed value says which calculation writes it: {output}"
    );
    assert_eq!(
        output["read_by"][0], "closure_b",
        "and which reads it next: {output}"
    );
}

/// Expected behaviour: a flag dry run reports the same closure, because flagging changes the
/// served value and so changes what every downstream calculation reads.
#[tokio::test]
#[serial]
async fn a_flag_dry_run_reports_what_it_would_recompute() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::patch_json_parse_with_token(
        &app,
        "/api/readings/flag_range",
        &json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "start_time": "2025-02-01T00:00:00Z",
            "end_time": "2025-02-28T00:00:00Z",
            "dry_run": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let names: Vec<&str> = body["calculations"]
        .as_array()
        .expect("calculations")
        .iter()
        .filter_map(|c| c["tool"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["closure_a", "closure_b"],
        "the same closure the grid shows: {body}"
    );
}

/// Expected behaviour: the same relation, asked from four ends. A calibration of the instrument
/// measuring temperature, the temperature slot, a stream serving it, and the calculation whose
/// output feeds the next one all reach the same calculations the parameter does (M126).
#[tokio::test]
#[serial]
async fn every_subject_naming_the_same_parameter_answers_the_same_set() {
    let (db, app, token) = setup().await;

    let names = async |query: &str| -> Vec<String> {
        let (status, body) = crate::common::get_json_with_token(
            &app,
            &format!("/api/calculations/closure?{query}"),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{query}: {body}");
        body["calculations"]
            .as_array()
            .expect("calculations")
            .iter()
            .filter_map(|c| c["tool"].as_str().map(str::to_string))
            .collect()
    };

    let by_parameter = names(&format!("parameter_ids={GLOBAL_PARAM_TEMP_ID}")).await;
    assert_eq!(by_parameter, vec!["closure_a", "closure_b"], "the baseline");

    // The temperature slot at the seeded site, and a stream paired to it.
    let slot: String = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT id::text AS v FROM site_parameters \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' LIMIT 1"
        ),
    )
    .await;
    assert_eq!(names(&format!("site_parameter_id={slot}")).await, by_parameter);

    let stream: String = crate::common::e2e::scalar(
        &db,
        &format!("SELECT id::text AS v FROM data_streams WHERE site_parameter_id = '{slot}' LIMIT 1"),
    )
    .await;
    assert_eq!(names(&format!("stream_id={stream}")).await, by_parameter);

    // A calibration declared for the temperature parameter reaches what that parameter feeds.
    let sensor: String = crate::common::e2e::scalar(
        &db,
        &format!("SELECT sensor_id::text AS v FROM data_streams WHERE id = '{stream}'"),
    )
    .await;
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, parameter_id, slope, intercept, valid_from) \
             VALUES ('11111111-1111-1111-1111-111111111111', '{sensor}', '{GLOBAL_PARAM_TEMP_ID}', \
                     1.0, 0.0, '2020-01-01T00:00:00Z') ON CONFLICT (id) DO NOTHING"
        ),
    ))
    .await
    .expect("a calibration for the temperature channel");
    assert_eq!(
        names("calibration_id=11111111-1111-1111-1111-111111111111").await,
        by_parameter
    );

    // A calculation is asked about by what its own outputs feed: closure_a writes what closure_b
    // reads, so asking about closure_a reaches closure_b and not itself.
    assert_eq!(names("calculation=closure_a").await, vec!["closure_b"]);
}

/// Expected behaviour: naming two subjects at once is refused rather than ranked.
#[tokio::test]
#[serial]
async fn two_subjects_at_once_are_refused() {
    let (_db, app, token) = setup().await;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/calculations/closure?calibration_id=11111111-1111-1111-1111-111111111111\
         &stream_id=22222222-2222-2222-2222-222222222222",
        &token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}
