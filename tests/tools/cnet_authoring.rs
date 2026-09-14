//! Scenario: an administrator authors the CNET calculators by hand, which Q149 made the only path
//! a deployment has to them, and the routes the calculation page posts to are all they have.
//!
//! Expected behaviour: every set of `tests/fixtures/cnet_formula_sets.json` can be created through
//! those routes, and the set as created reproduces the portal's own stored outputs at its golden
//! visit. A refusal here is a gap in the authoring surface, not in the engine: the in-process
//! suite (`src/routes/private/tools/tests/cnet_formula_sets.rs`) already shows the arithmetic.
//!
//! Run: cargo test --test tools cnet_authoring -- --test-threads=1

use std::collections::HashSet;

use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use serial_test::serial;

use crate::common::SITE1_ID;
use crate::common::keycloak as kc;

const FIXTURE: &str = include_str!("../fixtures/cnet_formula_sets.json");

/// The portal stores six significant figures.
const DEFAULT_TOLERANCE: f64 = 2e-5;

const AT: &str = "2025-07-03T09:00:00Z";

/// What the authoring surface cannot yet carry, and the refusal it must still produce. An entry
/// whose set stops being refused fails this test, which is how it leaves: Q156 takes `pco2` out,
/// and Q155 `nutrients` and `dic`.
const BLOCKED: &[(&str, Stage, &str)] = &[
    ("pco2", Stage::Save, "already exists"),
    ("nutrients", Stage::Run, "must be number"),
    ("dic", Stage::Run, "must be number"),
];

/// Where a blocked set is refused: saving a formula, or running the set at the visit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Save,
    Run,
}

fn blocked(name: &str, stage: Stage) -> Option<&'static str> {
    BLOCKED
        .iter()
        .find(|(set, at, _)| *set == name && *at == stage)
        .map(|(_, _, fragment)| *fragment)
}

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::member_jwt("scriptadmin", "scriptadmin", "riverdata-admin").await;
    (db, app, admin)
}

async fn post(app: &axum::Router, uri: &str, body: &Value, admin: &str) -> Value {
    let (status, parsed) = crate::common::post_json_parse_with_token(app, uri, body, admin).await;
    assert!(
        (200..300).contains(&status),
        "POST {uri} refused with {status}: {parsed}\nbody: {body}"
    );
    parsed
}

/// A catalog row the reference rows already hold is not a refusal of the authoring surface: the
/// author would have found it on the page and used it.
async fn post_or_find(app: &axum::Router, uri: &str, body: &Value, admin: &str) -> Option<Value> {
    let (status, parsed) = crate::common::post_json_parse_with_token(app, uri, body, admin).await;
    if status == 409 {
        return None;
    }
    assert!(
        (200..300).contains(&status),
        "POST {uri} refused with {status}: {parsed}\nbody: {body}"
    );
    Some(parsed)
}

/// Whether the catalog already holds a parameter of this code.
async fn in_catalog(app: &axum::Router, code: &str, admin: &str) -> bool {
    let filter = crate::common::e2e::percent_encode(&json!({ "code": code }).to_string());
    let (status, body) =
        crate::common::get_json_with_token(app, &format!("/api/parameters?filter={filter}"), admin)
            .await;
    assert_eq!(status, 200, "GET /api/parameters: {body}");
    // The filter matches a code that contains the one asked for, so `bp` comes back holding
    // `Field_BP`: the row wanted is the one whose code is the code.
    body.as_array().expect("a list of rows").iter().any(|row| {
        row["code"]
            .as_str()
            .is_some_and(|c| c.eq_ignore_ascii_case(code))
    })
}

fn id_of(created: &Value) -> String {
    created["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in {created}"))
        .to_string()
}

/// Every identifier a set's formulas read from the catalog, in the order the fixture lists them.
fn sources_of(calculation: &Value) -> Vec<String> {
    let mut seen = Vec::new();
    for formula in calculation["formulas"].as_array().expect("formulas") {
        for pair in formula["sources"].as_array().into_iter().flatten() {
            let name = pair[0].as_str().expect("a variable name").to_string();
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }
    seen
}

fn close(got: f64, want: f64, tolerance: f64) -> bool {
    (got - want).abs() <= tolerance * want.abs().max(1.0)
}

/// Author one set exactly as the calculation page does: the group, the catalog parameters it
/// reads, the intermediates it computes, the calculation itself, then a formula at a time.
async fn author(
    app: &axum::Router,
    admin: &str,
    calculation: &Value,
    known: &mut HashSet<String>,
) -> Result<String, String> {
    let name = calculation["name"].as_str().expect("name");
    let group = post(
        app,
        "/api/parameter_groups",
        &json!({ "code": name, "label": name, "ordinal": 1 }),
        admin,
    )
    .await;
    let group_id = id_of(&group);

    let produced: HashSet<String> = calculation["formulas"]
        .as_array()
        .expect("formulas")
        .iter()
        .map(|f| f["code"].as_str().expect("code").to_string())
        .collect();

    for (ordinal, code) in sources_of(calculation).into_iter().enumerate() {
        if produced.contains(&code) || known.contains(&code) {
            continue;
        }
        // A parameter the catalog already holds belongs to whichever group entered it: a
        // calculation reads it from there rather than claiming it, which the exclusive membership
        // (`parameter_group_members_parameter_id_key`) is what makes that the only reading.
        if let Some(created) = post_or_find(
            app,
            "/api/parameters",
            &json!({ "code": code, "name": code, "category": "measurement", "aliases": [] }),
            admin,
        )
        .await
        {
            post(
                app,
                "/api/parameter_group_members",
                &json!({
                    "group_id": group_id,
                    "parameter_id": id_of(&created),
                    "role": "measured",
                    "ordinal": ordinal as i32,
                }),
                admin,
            )
            .await;
        }
        known.insert(code);
    }

    let mut intermediates: Vec<Value> = Vec::new();
    for formula in calculation["formulas"].as_array().expect("formulas") {
        if formula["intermediate"] != json!(true) {
            continue;
        }
        let code = formula["code"].as_str().expect("code");
        // A step another calculation has already declared is the same catalog row, and one group
        // holds it.
        if in_catalog(app, code, admin).await {
            continue;
        }
        intermediates
            .push(json!({ "code": code, "name": formula["label"].as_str().unwrap_or(code) }));
    }
    if !intermediates.is_empty() {
        post(
            app,
            &format!("/api/parameter_groups/{group_id}/intermediates"),
            &json!({ "intermediates": intermediates }),
            admin,
        )
        .await;
    }

    let script = post(
        app,
        "/api/tool_scripts",
        &json!({
            "name": name,
            "label": name,
            "engine": "formula",
            "parameter_group_id": group_id,
        }),
        admin,
    )
    .await;
    let script_id = id_of(&script);

    for formula in calculation["formulas"].as_array().expect("formulas") {
        let code = formula["code"].as_str().expect("code");
        let (status, body) = crate::common::post_json_parse_with_token(
            app,
            "/api/derived_parameters",
            &json!({
                "code": code,
                "name": formula["label"].as_str().unwrap_or(code),
                "units": formula["units"],
                "formula": formula["formula"],
                "tool_script_id": script_id,
                "ordinal": formula["ordinal"],
                "per_replicate": formula["per_replicate"],
                "curve_slot": formula["curve_slot"],
                "intermediate": formula["intermediate"],
            }),
            admin,
        )
        .await;
        if !(200..300).contains(&status) {
            return Err(format!("{code}: {body}"));
        }
        known.insert(code.to_string());
    }
    Ok(script_id)
}

/// The drafts the calculation page sends to a draft run: the set as it stands on the page.
fn drafts(calculation: &Value) -> Vec<Value> {
    calculation["formulas"]
        .as_array()
        .expect("formulas")
        .iter()
        .map(|f| {
            json!({
                "code": f["code"],
                "formula": f["formula"],
                "ordinal": f["ordinal"],
                "per_replicate": f["per_replicate"],
                "curve_slot": f["curve_slot"],
                "intermediate": f["intermediate"],
            })
        })
        .collect()
}

/// The site columns a set reads, which the run resolves from the site row rather than the request.
fn site_sources(calculation: &Value) -> HashSet<String> {
    let mut columns = HashSet::new();
    for formula in calculation["formulas"].as_array().expect("formulas") {
        for pair in formula["site_sources"].as_array().into_iter().flatten() {
            columns.insert(pair[1].as_str().expect("a column").to_string());
        }
    }
    columns
}

#[tokio::test]
#[serial]
async fn every_cnet_set_is_authorable_through_the_api_and_reproduces_its_golden_visit() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("the fixture parses");
    let (_db, app, admin) = setup().await;

    for (name, value) in fixture["constants"].as_object().expect("constants") {
        post_or_find(
            &app,
            "/api/constants",
            &json!({ "name": name, "value": value, "description": "CNET portal" }),
            &admin,
        )
        .await;
    }

    let mut known: HashSet<String> = HashSet::new();
    for calculation in fixture["calculations"].as_array().expect("calculations") {
        let name = calculation["name"].as_str().expect("name");
        let script_id = match author(&app, &admin, calculation, &mut known).await {
            Ok(script_id) => {
                assert!(
                    blocked(name, Stage::Save).is_none(),
                    "{name} now saves: take its BLOCKED entry out and let the set run"
                );
                script_id
            }
            Err(refusal) => {
                let fragment = blocked(name, Stage::Save)
                    .unwrap_or_else(|| panic!("{name} cannot be authored: {refusal}"));
                assert!(
                    refusal.contains(fragment),
                    "{name} is refused for a reason BLOCKED does not record: {refusal}"
                );
                continue;
            }
        };
        let columns = site_sources(calculation);
        let mut refused = false;

        for case in calculation["cases"].as_array().expect("cases") {
            let context = format!("{name}, {}", case["name"].as_str().unwrap_or(""));
            let tolerance = case["tolerance"].as_f64().unwrap_or(DEFAULT_TOLERANCE);

            let mut inputs = serde_json::Map::new();
            inputs.insert("site_id".into(), json!(SITE1_ID));
            inputs.insert("collected_at".into(), json!(AT));
            let mut site_columns = serde_json::Map::new();
            for (key, value) in case["inputs"].as_object().into_iter().flatten() {
                if columns.contains(key) {
                    site_columns.insert(key.clone(), value.clone());
                } else {
                    inputs.insert(key.clone(), value.clone());
                }
            }
            for (key, value) in case["replicates"].as_object().into_iter().flatten() {
                inputs.insert(key.clone(), value.clone());
            }
            for (key, value) in case["curves"].as_object().into_iter().flatten() {
                inputs.insert(
                    key.clone(),
                    json!({ "slope": value[0], "intercept": value[1] }),
                );
            }
            if !site_columns.is_empty() {
                let (status, body) = crate::common::put_json_with_token(
                    &app,
                    &format!("/api/sites/{SITE1_ID}"),
                    &Value::Object(site_columns),
                    &admin,
                )
                .await;
                assert_eq!(
                    status, 200,
                    "{context}: the site will not take its column: {body}"
                );
            }

            let (status, body) = crate::common::post_json_parse_with_token(
                &app,
                &format!("/api/tool_scripts/{script_id}/formulas/draft_run"),
                &json!({ "formulas": drafts(calculation), "inputs": Value::Object(inputs) }),
                &admin,
            )
            .await;
            assert_eq!(status, 200, "{context}: {body}");
            // A set the run cannot carry is refused outright or comes back with the outputs it
            // could not reach; either way the recorded reason is in the response. A case the gap
            // does not touch is compared like any other.
            if let Some(fragment) = blocked(name, Stage::Run)
                && body.to_string().contains(fragment)
            {
                refused = true;
                continue;
            }
            assert_eq!(body["ran"], json!(true), "{context}: {body}");

            for (code, want) in case["expected"].as_object().expect("expected") {
                let got = &body["results"][code];
                match want {
                    Value::Number(n) => {
                        let want = n.as_f64().expect("a number");
                        let got = got
                            .as_f64()
                            .unwrap_or_else(|| panic!("{context}: {code} is {got}: {body}"));
                        assert!(
                            close(got, want, tolerance),
                            "{context}: {code} = {got}, the portal stored {want}"
                        );
                    }
                    Value::Array(list) => {
                        let got = got
                            .as_array()
                            .unwrap_or_else(|| panic!("{context}: {code} is {got}: {body}"));
                        assert_eq!(got.len(), list.len(), "{context}: {code} width: {body}");
                        for (index, (got, want)) in got.iter().zip(list).enumerate() {
                            let want = want.as_f64().expect("a number");
                            let got = got.as_f64().unwrap_or_else(|| {
                                panic!("{context}: {code}[{index}] has no value: {body}")
                            });
                            assert!(
                                close(got, want, tolerance),
                                "{context}: {code}[{index}] = {got}, the portal stored {want}"
                            );
                        }
                    }
                    other => panic!("{context}: {code} expects {other}"),
                }
            }
        }
        if let Some(fragment) = blocked(name, Stage::Run) {
            assert!(
                refused,
                "{name} no longer reports '{fragment}': take its BLOCKED entry out"
            );
        }
    }
}
