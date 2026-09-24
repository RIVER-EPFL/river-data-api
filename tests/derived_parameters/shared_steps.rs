//! A step two calculations compute is authored once and declared by each (Q156), and the step
//! itself answers which calculations read it.
//!
//! Run with: cargo test --test derived_parameters shared_steps

use serde_json::{Value, json};
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn put_json_with_token(
    app: &axum::Router,
    uri: &str,
    body: &Value,
    token: &str,
) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request");
    let response = app.clone().oneshot(req).await.expect("a response");
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post(app: &axum::Router, uri: &str, body: &Value, token: &str) -> Value {
    let (status, parsed) = crate::common::post_json_parse_with_token(app, uri, body, token).await;
    assert!(
        (200..300).contains(&status),
        "POST {uri} refused with {status}: {parsed}"
    );
    parsed
}

fn id_of(created: &Value) -> String {
    created["id"].as_str().expect("an id").to_string()
}

/// A formula calculation, inserted directly: authoring one is an administrator's act behind a
/// Keycloak gate (`/api/tool_scripts`), and what is under test here is the step, not that gate.
async fn calculation(db: &sea_orm::DatabaseConnection, name: &str) -> String {
    let id = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (id, name, label, engine, enabled) \
             VALUES ('{id}', '{name}', '{name}', 'formula', true)"
        ),
    )
    .await;
    id.to_string()
}

/// A step of `first`, then the same step read by `second`: `guard` reads a seeded catalog
/// parameter, and each calculation has a formula of its own that names the step.
#[tokio::test]
#[serial]
async fn a_step_two_calculations_compute_is_authored_once_and_declared_by_each() {
    let (db, app, token) = setup().await;

    let first = calculation(&db, "first_set").await;
    let second = calculation(&db, "second_set").await;

    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "guard", "name": "Guard", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": first,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    let step_id = id_of(&step);

    post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "first_out", "name": "First out", "units": "uM",
            "formula": "guard + 1", "tool_script_id": first, "ordinal": 1,
        }),
        &token,
    )
    .await;

    // The second calculation cannot write the step again: one code is one formula.
    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "guard", "name": "Guard", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": second,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "one code is one formula: {refused}");

    // It declares that it reads the step instead, which takes the step out of the first
    // calculation's ownership and leaves the first reading it the same way.
    post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": step_id }),
        &token,
    )
    .await;

    // With the step declared, a formula of the second calculation resolves its code.
    post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "second_out", "name": "Second out", "units": "uM",
            "formula": "guard * 3", "tool_script_id": second, "ordinal": 1,
        }),
        &token,
    )
    .await;

    let (status, dependents) = crate::common::get_json_with_token(
        &app,
        &format!("/api/derived_parameters/{step_id}/dependents"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "({status}): {dependents}");
    assert_eq!(dependents["code"], "guard");
    assert_eq!(
        dependents["shared"], true,
        "the step belongs to no calculation: {dependents}"
    );
    let calculations = dependents["calculations"].as_array().expect("calculations");
    assert_eq!(
        calculations.len(),
        2,
        "both calculations read it: {dependents}"
    );
    let names: Vec<&str> = calculations
        .iter()
        .map(|c| c["name"].as_str().expect("a name"))
        .collect();
    assert!(
        names.contains(&"first_set") && names.contains(&"second_set"),
        "{dependents}"
    );
    for calculation in calculations {
        let readers: Vec<&str> = calculation["formulas"]
            .as_array()
            .expect("formulas")
            .iter()
            .map(|f| f["code"].as_str().expect("a code"))
            .collect();
        assert_eq!(
            readers.len(),
            1,
            "one formula of each names the step: {calculation}"
        );
        assert!(
            readers[0].ends_with("_out"),
            "the reader is the output, not the step itself: {calculation}"
        );
        assert_eq!(
            calculation["owns"], false,
            "a shared step is owned by nobody: {calculation}"
        );
    }

    // A third calculation cannot write the code either: one code is one formula whether the
    // formula belongs to a calculation or to none.
    let third = calculation(&db, "third_set").await;
    let (status, refused) = crate::common::save_formula_set(
        &app,
        &token,
        &third,
        json!([{ "code": "guard", "units": "uM", "formula": "Dissolved_O2 * 4", "ordinal": 0 }]),
    )
    .await;
    assert_eq!(status, 400, "the save is refused: {refused}");
    assert!(
        refused.contains("guard is a formula of a shared step"),
        "the refusal names what holds the code: {refused}"
    );
}

#[tokio::test]
#[serial]
async fn a_calculation_cannot_declare_a_step_it_already_computes() {
    let (db, app, token) = setup().await;
    let only = calculation(&db, "only_set").await;
    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "own_step", "name": "Own step", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": only,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": only, "formula_id": id_of(&step) }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "({status}): {refused}");
    assert!(
        refused.to_string().contains("already computes"),
        "{refused}"
    );
}

#[tokio::test]
#[serial]
async fn an_output_is_not_a_step_and_cannot_be_declared() {
    let (db, app, token) = setup().await;
    let first = calculation(&db, "output_set").await;
    let second = calculation(&db, "reader_set").await;
    let output = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "an_output", "name": "An output", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": first, "ordinal": 0,
        }),
        &token,
    )
    .await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": id_of(&output) }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "({status}): {refused}");
    assert!(refused.to_string().contains("an output"), "{refused}");
}

/// A formula that was an output and is then ticked as a step gives up the parameter it published:
/// nothing stores a step's value, so a link left standing would keep the gap scan computing it
/// and keep the group calling the catalog parameter an Output.
#[tokio::test]
#[serial]
async fn a_formula_turned_into_a_step_gives_up_its_output_parameter() {
    let (db, app, token) = setup().await;
    let set = calculation(&db, "flip_set").await;

    let formula = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "was_an_output", "name": "Was an output", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": set, "ordinal": 0,
        }),
        &token,
    )
    .await;
    assert!(
        formula["output_parameter_id"].is_string(),
        "an output publishes a catalog parameter: {formula}"
    );

    let (status, raw) = put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{}", id_of(&formula)),
        &json!({ "intermediate": true }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "({status}): {raw}");
    let flipped: Value = serde_json::from_str(&raw).expect("JSON");
    assert!(
        flipped["output_parameter_id"].is_null(),
        "the response gives up the link: {flipped}"
    );

    let stored = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT COALESCE(output_parameter_id::text, 'none') \
               FROM calculation_formulas WHERE id = '{}'",
            id_of(&formula)
        ),
    )
    .await;
    assert_eq!(stored, "none", "and so does the stored row");
}

/// Scenario: one step is owned by a calculation the lab runs at a visit and declared by a second
/// calculation a site computes on its stream. The site declares a high-cadence slot for the second
/// calculation's output, and a logger reading lands.
///
/// Expected behaviour: the stream pass evaluates the declared step as part of its own set and
/// stores the output. A step shared between the arms is authored once and computes in both, which
/// is the whole point of sharing it.
#[tokio::test]
#[serial]
async fn a_step_shared_with_a_visit_calculation_computes_on_the_stream_too() {
    use sea_orm::{ConnectionTrait, Statement};

    let (db, app, token) = setup().await;
    let site_id = uuid::Uuid::parse_str(crate::common::SITE1_ID).expect("a uuid");

    let visit_side = calculation(&db, "visit_side_set").await;
    let stream_side = calculation(&db, "stream_side_set").await;

    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "o2_doubled", "name": "Oxygen doubled", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": visit_side,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": stream_side, "formula_id": id_of(&step) }),
        &token,
    )
    .await;
    let output = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "o2_doubled_mgl", "name": "Oxygen doubled mg/L", "units": "mg/L",
            "formula": "o2_doubled * 0.032", "tool_script_id": stream_side, "ordinal": 1,
        }),
        &token,
    )
    .await;
    let output_parameter_id = output["output_parameter_id"]
        .as_str()
        .expect("the formula minted its output")
        .to_string();
    crate::common::commit_calculation(&db, stream_side.parse().expect("a uuid")).await;

    post(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output_parameter_id,
            "name": "o2_doubled_mgl",
            "sensor_type": "derived",
            "entry_mode": "tool",
            "cadence": "high",
        }),
        &token,
    )
    .await;

    let at = chrono::Utc::now() - chrono::Duration::hours(4);
    let at = at - chrono::Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, written) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": at.to_rfc3339(),
                "raw_value": 100.0,
            }]
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "batch ({status}): {written}");

    let output_uuid = uuid::Uuid::parse_str(&output_parameter_id).expect("a uuid");
    river_db::routes::private::sensor_calibrations::service::recalculate_derived_at_timestamp(
        &db, site_id, at,
    )
    .await
    .expect("the stream pass runs");

    let stored: Option<f64> = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT raw_value FROM readings WHERE site_id = $1 AND parameter_id = $2 AND time = $3",
            [site_id.into(), output_uuid.into(), at.into()],
        ))
        .await
        .expect("a query")
        .map(|row| row.try_get::<f64>("", "raw_value").expect("a value"));
    // (100 * 2) * 0.032
    assert_eq!(
        stored,
        Some(6.4),
        "the declared step evaluated inside the stream set"
    );
}

/// Correcting a shared step changes what every calculation that declares it computes, so each of
/// them is re-pinned to a version holding the correction.
#[tokio::test]
#[serial]
async fn correcting_a_shared_step_mints_a_version_for_each_declaring_calculation() {
    let (db, app, token) = setup().await;
    let first = calculation(&db, "corrected_first").await;
    let second = calculation(&db, "corrected_second").await;

    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "typo_step", "name": "Typo step", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": first,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": id_of(&step) }),
        &token,
    )
    .await;

    let active = |calculation: String| {
        let db = db.clone();
        async move {
            crate::common::e2e::scalar(
                &db,
                &format!(
                    "SELECT COALESCE(active_version_id::text, 'none') \
                       FROM tool_scripts WHERE id = '{calculation}'"
                ),
            )
            .await
        }
    };
    let before = (active(first.clone()).await, active(second.clone()).await);

    let (status, raw) = put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{}", id_of(&step)),
        &json!({ "formula": "Dissolved_O2 * 3" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "({status}): {raw}");

    for (calculation, was) in [(first, before.0), (second, before.1)] {
        let now = active(calculation.clone()).await;
        assert_ne!(now, was, "{calculation} is re-pinned");
        let body = crate::common::e2e::scalar(
            &db,
            &format!("SELECT script FROM tool_script_versions WHERE id::text = '{now}'"),
        )
        .await;
        assert!(
            body.contains("Dissolved_O2 * 3"),
            "the version {calculation} runs holds the correction: {body}"
        );
        let author = crate::common::e2e::scalar(
            &db,
            &format!(
                "SELECT COALESCE(created_by, 'none') FROM tool_script_versions \
                  WHERE id::text = '{now}'"
            ),
        )
        .await;
        assert_ne!(author, "none", "the correction names who made it");
        for key in [
            format!("event_recompute:version:{was}"),
            format!("derived_recompute:version:{was}"),
        ] {
            let queued = crate::common::e2e::scalar(
                &db,
                &format!("SELECT count(*)::text FROM reprocessing_jobs WHERE dedupe_key = '{key}'"),
            )
            .await;
            assert_eq!(
                queued, "1",
                "the values {calculation}'s replaced version made are queued: {key}"
            );
        }
    }
}

/// Scenario: a step two calculations read is corrected from the second calculation's page, and the
/// author saves.
///
/// Expected behaviour: the correction and the set save are one act. The calculation gets one
/// version, authored and holding the correction, and the values the version it replaces produced
/// are queued for recompute.
#[tokio::test]
#[serial]
async fn correcting_a_shared_step_in_a_set_save_mints_one_version_and_migrates() {
    let (db, app, token) = setup().await;
    let first = calculation(&db, "bp_owner").await;
    let second = calculation(&db, "bp_reader").await;

    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "bp_step", "name": "BP step", "units": "hPa",
            "formula": "Dissolved_O2 * 2", "tool_script_id": first,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    let step_id = id_of(&step);
    post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": step_id }),
        &token,
    )
    .await;
    let (status, saved) = crate::common::save_formula_set(
        &app,
        &token,
        &second,
        json!([{ "code": "bp_out", "units": "hPa", "formula": "bp_step + 1", "ordinal": 1 }]),
    )
    .await;
    assert_eq!(status, 200, "{saved}");

    let versions = || {
        let db = db.clone();
        let second = second.clone();
        async move {
            crate::common::e2e::scalar(
                &db,
                &format!(
                    "SELECT count(*)::text FROM tool_script_versions \
                      WHERE tool_script_id = '{second}'"
                ),
            )
            .await
        }
    };
    let minted_before: i64 = versions().await.parse().expect("a count");
    let superseded = crate::common::e2e::scalar(
        &db,
        &format!("SELECT active_version_id::text FROM tool_scripts WHERE id = '{second}'"),
    )
    .await;
    let out_id = crate::common::e2e::scalar(
        &db,
        "SELECT id::text FROM calculation_formulas WHERE code = 'bp_out'",
    )
    .await;

    let (status, raw) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tool_scripts/{second}/formulas"),
        &json!({
            "formulas": [{ "id": out_id, "code": "bp_out", "units": "hPa",
                           "formula": "bp_step + 1", "ordinal": 1 }],
            "shared_steps": [{ "id": step_id, "code": "bp_step", "name": "BP step",
                               "units": "hPa", "formula": "Dissolved_O2 * 3" }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{raw}");
    let saved: Value = serde_json::from_str(&raw).expect("JSON");
    assert_eq!(
        saved["migrated"], true,
        "the superseded version's values are queued: {saved}"
    );

    let minted_after: i64 = versions().await.parse().expect("a count");
    assert_eq!(minted_after, minted_before + 1, "one save is one version");
    let (body, author) = {
        let active = crate::common::e2e::scalar(
            &db,
            &format!(
                "SELECT v.script || '|' || COALESCE(v.created_by, 'none') \
                   FROM tool_scripts s JOIN tool_script_versions v ON v.id = s.active_version_id \
                  WHERE s.id = '{second}'"
            ),
        )
        .await;
        let (body, author) = active.rsplit_once('|').expect("script and author");
        (body.to_string(), author.to_string())
    };
    assert!(
        body.contains("Dissolved_O2 * 3"),
        "the version holds the correction: {body}"
    );
    assert_ne!(author, "none", "and names who saved it");

    let queued = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT count(*)::text FROM reprocessing_jobs \
              WHERE dedupe_key = 'event_recompute:version:{superseded}'"
        ),
    )
    .await;
    assert_eq!(queued, "1", "the recompute names the version it replaces");
}

/// A shared step the other calculation stopped reading is saved back into the set of the one
/// still reading it, as a step of its own; while another calculation reads it, it stays shared.
#[tokio::test]
#[serial]
async fn a_shared_step_read_by_one_calculation_is_taken_back_into_its_set() {
    let (db, app, token) = setup().await;
    let first = calculation(&db, "keeping_set").await;
    let second = calculation(&db, "leaving_set").await;

    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "water_k", "name": "Water K", "units": "K",
            "formula": "Dissolved_O2 + 273.15", "tool_script_id": first,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    let step_id = id_of(&step);
    let declaration = post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": step_id }),
        &token,
    )
    .await;

    let set = json!([
        { "id": step_id, "code": "water_k", "units": "K",
          "formula": "Dissolved_O2 + 273.15", "ordinal": 0, "intermediate": true },
        { "code": "kept_out", "units": "K", "formula": "water_k * 2", "ordinal": 1 },
    ]);
    let (status, refused) =
        crate::common::save_formula_set(&app, &token, &first, set.clone()).await;
    assert_eq!(status, 400, "another calculation still reads it: {refused}");
    assert!(refused.contains("leaving_set"), "{refused}");

    let (status, raw) = crate::common::delete_with_token(
        &app,
        &format!("/api/calculation_shared_steps/{}", id_of(&declaration)),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "({status}): {raw}");

    let (status, saved) = crate::common::save_formula_set(&app, &token, &first, set).await;
    assert_eq!(status, 200, "{saved}");

    let owner = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT COALESCE(tool_script_id::text, 'none') FROM calculation_formulas \
              WHERE id = '{step_id}'"
        ),
    )
    .await;
    assert_eq!(
        owner, first,
        "the step is the calculation's own again, under its id"
    );
    let declared = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT count(*)::text FROM calculation_shared_steps WHERE formula_id = '{step_id}'"
        ),
    )
    .await;
    assert_eq!(declared, "0", "and nothing declares it");
}

/// A step shared by no calculation, created directly.
async fn shared_step(app: &axum::Router, token: &str, code: &str, formula: &str) -> (u16, Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/derived_parameters",
        &json!({
            "code": code, "name": code, "units": "",
            "formula": formula, "intermediate": true,
        }),
        token,
    )
    .await
}

/// A step of `owner`, created directly.
async fn owned_step(
    app: &axum::Router,
    token: &str,
    owner: &str,
    code: &str,
    formula: &str,
) -> Value {
    post(
        app,
        "/api/derived_parameters",
        &json!({
            "code": code, "name": code, "units": "",
            "formula": formula, "tool_script_id": owner,
            "ordinal": 0, "intermediate": true,
        }),
        token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_shared_step_reads_another_shared_step() {
    let (_db, app, token) = setup().await;
    let (status, water_k) = shared_step(&app, &token, "water_k", "Dissolved_O2 + 273.15").await;
    assert!((200..300).contains(&status), "({status}): {water_k}");

    let (status, kh) = shared_step(&app, &token, "kh", "0.034 / water_k").await;
    assert!((200..300).contains(&status), "({status}): {kh}");
    let sources = kh["sources"].as_array().expect("sources");
    assert!(sources.is_empty(), "a step read is no source: {kh}");
}

#[tokio::test]
#[serial]
async fn a_shared_step_reading_a_step_a_calculation_owns_is_refused_by_name_and_owner() {
    let (db, app, token) = setup().await;
    let pco2real = calculation(&db, "pco2real").await;
    let other = calculation(&db, "other_set").await;
    owned_step(&app, &token, &pco2real, "water_k", "Dissolved_O2 + 273.15").await;
    let kh = owned_step(&app, &token, &pco2real, "kh", "0.034 / water_k").await;

    let (status, refused) = shared_step(&app, &token, "kh_shared", "0.034 / water_k").await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("reads water_k, a step of pco2real that is not shared"),
        "{refused}"
    );

    // Declaring kh releases it, and a released kh would read pco2real's own water_k.
    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": other, "formula_id": id_of(&kh) }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("reads water_k, a step of pco2real that is not shared"),
        "{refused}"
    );
    let owner = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT COALESCE(tool_script_id::text, 'none') FROM calculation_formulas \
              WHERE id = '{}'",
            id_of(&kh)
        ),
    )
    .await;
    assert_eq!(owner, pco2real, "kh stays pco2real's own");
}

#[tokio::test]
#[serial]
async fn a_cycle_among_shared_steps_is_refused_naming_its_members() {
    let (_db, app, token) = setup().await;
    let (status, first) = shared_step(&app, &token, "loop_a", "Dissolved_O2 + 1").await;
    assert!((200..300).contains(&status), "({status}): {first}");
    let (status, second) = shared_step(&app, &token, "loop_b", "loop_a * 2").await;
    assert!((200..300).contains(&status), "({status}): {second}");

    let (status, refused) = put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{}", id_of(&first)),
        &json!({ "formula": "loop_b + 1" }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused.contains("loop_a") && refused.contains("loop_b") && refused.contains("cycle"),
        "{refused}"
    );
}

/// A calculation declaring only `kh` receives `water_k`, which `kh` reads, and the step reports
/// the calculation among those it feeds.
#[tokio::test]
#[serial]
async fn a_calculation_receives_the_shared_steps_its_declared_steps_read() {
    use sea_orm::{ConnectionTrait, Statement};

    let (db, app, token) = setup().await;
    let site_id = uuid::Uuid::parse_str(crate::common::SITE1_ID).expect("a uuid");
    let pco2real = calculation(&db, "pco2real").await;

    let (status, water_k) = shared_step(&app, &token, "water_k", "Dissolved_O2 * 2").await;
    assert!((200..300).contains(&status), "({status}): {water_k}");
    let (status, kh) = shared_step(&app, &token, "kh", "water_k + 1").await;
    assert!((200..300).contains(&status), "({status}): {kh}");
    post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": pco2real, "formula_id": id_of(&kh) }),
        &token,
    )
    .await;
    let output = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "kh_out", "name": "kh out", "units": "",
            "formula": "kh * 10", "tool_script_id": pco2real, "ordinal": 1,
        }),
        &token,
    )
    .await;
    crate::common::commit_calculation(&db, pco2real.parse().expect("a uuid")).await;
    let output_parameter_id = output["output_parameter_id"]
        .as_str()
        .expect("the formula minted its output")
        .to_string();
    post(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output_parameter_id,
            "name": "kh_out",
            "sensor_type": "derived",
            "entry_mode": "tool",
            "cadence": "high",
        }),
        &token,
    )
    .await;

    let at = chrono::Utc::now() - chrono::Duration::hours(4);
    let at = at - chrono::Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, written) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": at.to_rfc3339(),
                "raw_value": 100.0,
            }]
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "batch ({status}): {written}");
    river_db::routes::private::sensor_calibrations::service::recalculate_derived_at_timestamp(
        &db, site_id, at,
    )
    .await
    .expect("the stream pass runs");

    let output_uuid = uuid::Uuid::parse_str(&output_parameter_id).expect("a uuid");
    let stored: Option<f64> = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT raw_value FROM readings WHERE site_id = $1 AND parameter_id = $2 AND time = $3",
            [site_id.into(), output_uuid.into(), at.into()],
        ))
        .await
        .expect("a query")
        .map(|row| row.try_get::<f64>("", "raw_value").expect("a value"));
    // (100 * 2 + 1) * 10
    assert_eq!(stored, Some(2010.0), "kh computed from water_k in the run");

    let (status, dependents) = crate::common::get_json_with_token(
        &app,
        &format!("/api/derived_parameters/{}/dependents", id_of(&water_k)),
        &token,
    )
    .await;
    assert_eq!(status, 200, "({status}): {dependents}");
    let calculations = dependents["calculations"].as_array().expect("calculations");
    let feeds = calculations
        .iter()
        .find(|c| c["name"] == "pco2real")
        .unwrap_or_else(|| panic!("water_k feeds pco2real through kh: {dependents}"));
    let readers: Vec<&str> = feeds["formulas"]
        .as_array()
        .expect("formulas")
        .iter()
        .map(|f| f["code"].as_str().expect("a code"))
        .collect();
    assert_eq!(readers, ["kh"], "kh is what reads it there: {feeds}");
}
