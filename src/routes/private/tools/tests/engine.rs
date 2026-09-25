#[test]
fn an_na_output_is_a_clear_and_an_unnamed_output_is_left_alone() {
    use crate::routes::private::tools::service::partition_cleared;
    let mut results = serde_json::json!({
        "doc_avg": 1.25, "doc_sd": null, "dom": null, "flag": "ok"
    })
    .as_object()
    .unwrap()
    .clone();
    let cleared = partition_cleared(&mut results);
    assert_eq!(cleared, vec!["doc_sd".to_string(), "dom".to_string()]);
    assert_eq!(
        serde_json::Value::Object(results),
        serde_json::json!({ "doc_avg": 1.25, "flag": "ok" }),
        "a cleared key never reaches a consumer reading values"
    );
    // A run that named nothing clears nothing: silence is not a request to blank a column.
    let mut empty = serde_json::Map::new();
    assert!(partition_cleared(&mut empty).is_empty());
}

use crate::routes::private::tools::models::{Manifest, ParamWhen, StructLayout};

/// The request-body contract: what `/tools/{name}/calculate` refuses before it resolves
/// anything. This was reached only through a Keycloak login, a database and the R runner.
mod body_shape {
    use crate::routes::private::tools::models::Manifest;
    use crate::routes::private::tools::service::check_body_shape;

    fn doc_manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "label": "DOC",
            "params": [
                { "name": "DOC", "label": "DOC", "kind": "replicates", "parameter_code": "DOC" },
                { "name": "dilution", "label": "Dilution", "kind": "number" },
                { "name": "operator", "label": "Operator", "kind": "string" },
            ],
            "curves": [{ "name": "std_curve", "label": "Standard curve" }],
            "outputs": [{ "key": "doc_avg", "label": "DOC average" }],
        }))
        .expect("the manifest parses")
    }

    fn check(body: serde_json::Value) -> Result<(), String> {
        let serde_json::Value::Object(map) = body else {
            panic!("the body is an object");
        };
        check_body_shape("doc", &doc_manifest(), &map).map_err(|e| e.to_string())
    }

    #[test]
    fn a_key_the_manifest_declares_nothing_for_is_refused_naming_it() {
        let err = check(serde_json::json!({ "DOC": [1.0], "typo": 1 }))
            .expect_err("an undeclared key is refused");
        assert!(err.contains("typo"), "{err}");
        assert!(err.contains("doc"), "{err}");
    }

    #[test]
    fn a_curve_slot_is_a_declared_key_like_a_param() {
        check(serde_json::json!({ "DOC": [1.0], "std_curve": "c1" }))
            .expect("a declared curve slot is accepted");
    }

    #[test]
    fn a_value_of_the_wrong_kind_is_refused_naming_the_field_and_the_kind() {
        let err = check(serde_json::json!({ "DOC": ["not-a-number"] }))
            .expect_err("a text cell in a numeric replicate list is refused");
        assert!(err.contains("DOC"), "{err}");

        let err = check(serde_json::json!({ "dilution": "two" }))
            .expect_err("text in a number param is refused");
        assert!(err.contains("dilution") && err.contains("number"), "{err}");
    }

    #[test]
    fn an_absent_or_null_field_passes_the_shape_check() {
        // Requiredness runs after the resolvers, which may still fill the gap.
        check(serde_json::json!({})).expect("an empty body has no shape error");
        check(serde_json::json!({ "DOC": serde_json::Value::Null }))
            .expect("an explicit null is an absence, not a wrong kind");
    }

    #[test]
    fn a_replicates_list_may_be_gapped_and_of_any_length() {
        check(serde_json::json!({ "DOC": [1.0, null, 3.0, 4.0, 5.0, 6.0] }))
            .expect("the operator chooses the count and a null is a repeat not measured");
    }
}

fn manifest_with(param: serde_json::Value) -> Result<Manifest, serde_json::Error> {
    serde_json::from_value(serde_json::json!({ "label": "T", "params": [param] }))
}

fn manifest(raw: serde_json::Value) -> Result<Manifest, serde_json::Error> {
    serde_json::from_value(raw)
}

fn doc_replicates(extra: serde_json::Value) -> serde_json::Value {
    let mut param = serde_json::json!({
        "name": "DOC", "label": "DOC", "kind": "replicates", "units": "ppb",
        "parameter_code": "DOC"
    });
    if let Some(map) = extra.as_object() {
        param.as_object_mut().unwrap().extend(map.clone());
    }
    param
}

#[test]
fn a_replicates_param_takes_a_gapped_list_of_any_length() {
    let m = manifest(serde_json::json!({
        "label": "DOC",
        "params": [doc_replicates(serde_json::json!({ "suggested": 3, "curve": "std_curve" }))],
        "curves": [{ "name": "std_curve", "label": "Curve" }]
    }))
    .unwrap();
    let p = &m.params[0];
    assert_eq!(p.parameter_code.as_deref(), Some("DOC"));
    assert_eq!(p.suggested, Some(3));
    assert_eq!(p.curve.as_deref(), Some("std_curve"));
    for value in [
        serde_json::json!([120.0]),
        serde_json::json!([120.0, null, 118.0]),
        serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        serde_json::json!([]),
    ] {
        assert!(
            crate::routes::private::tools::models::kind_accepts(&p.kind, &value),
            "{value}"
        );
    }
    for value in [
        serde_json::json!(120.0),
        serde_json::json!(["120"]),
        serde_json::json!({ "0": 120.0 }),
    ] {
        assert!(
            !crate::routes::private::tools::models::kind_accepts(&p.kind, &value),
            "{value}"
        );
    }
}

#[test]
fn a_replicates_param_that_is_not_anchored_is_refused() {
    for (raw, expected) in [
        (
            serde_json::json!({
                "label": "T",
                "params": [{ "name": "DOC", "label": "DOC", "kind": "replicates" }]
            }),
            "must name the parameter_code",
        ),
        (
            serde_json::json!({
                "label": "T",
                "params": [doc_replicates(serde_json::json!({ "suggested": 0 }))]
            }),
            "at least 1",
        ),
        (
            serde_json::json!({
                "label": "T",
                "params": [doc_replicates(serde_json::json!({ "curve": "nope" }))]
            }),
            "names no curve slot",
        ),
        (
            serde_json::json!({
                "label": "T",
                "params": [{ "name": "a", "label": "A", "kind": "number", "parameter_code": "DOC" }]
            }),
            "belong to a replicates param",
        ),
    ] {
        let err = manifest(raw).unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn a_section_must_be_declared_once_and_named_by_key() {
    let m = manifest(serde_json::json!({
        "label": "T",
        "sections": [{ "key": "lab", "label": "Lab" }],
        "params": [doc_replicates(serde_json::json!({ "section": "lab" }))]
    }))
    .unwrap();
    assert_eq!(m.params[0].section.as_deref(), Some("lab"));
    assert_eq!(m.sections[0].key, "lab");
    for (raw, expected) in [
        (
            serde_json::json!({
                "label": "T",
                "params": [doc_replicates(serde_json::json!({ "section": "lab" }))]
            }),
            "not declared",
        ),
        (
            serde_json::json!({
                "label": "T",
                "sections": [{ "key": "lab", "label": "Lab" }, { "key": "lab", "label": "Lab 2" }]
            }),
            "declared twice",
        ),
    ] {
        let err = manifest(raw).unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn a_manifest_without_the_new_fields_serializes_as_it_was_read() {
    let raw = serde_json::json!({
        "label": "T",
        "params": [{ "name": "a", "label": "A", "kind": "number", "units": null,
                     "required": false, "default": null, "when": null }]
    });
    let m = manifest(raw.clone()).unwrap();
    assert_eq!(serde_json::to_value(&m.params).unwrap(), raw["params"]);
}

#[test]
fn a_curve_description_is_kept() {
    let m = manifest(serde_json::json!({
        "label": "T",
        "curves": [{ "name": "c", "label": "C", "description": "y = ax + b" }]
    }))
    .unwrap();
    assert_eq!(m.curves[0].description.as_deref(), Some("y = ax + b"));
    let plain = manifest(serde_json::json!({
        "label": "T",
        "curves": [{ "name": "c", "label": "C" }]
    }))
    .unwrap();
    let json = serde_json::to_value(&plain.curves).unwrap();
    assert!(json[0].get("description").is_none());
}

fn grid(structure: serde_json::Value) -> Result<Manifest, serde_json::Error> {
    manifest_with(serde_json::json!({
        "name": "replicates", "label": "Replicates", "kind": "replicate_grid",
        "structure": structure
    }))
}

#[test]
fn a_declaration_fills_the_defaults_its_layout_implies() {
    let manifest = grid(serde_json::json!({
        "fields": [{ "name": "vol_ml", "label": "Vol", "units": "mL" }]
    }))
    .unwrap();
    let structure = manifest.params[0].structure.as_ref().unwrap();
    assert_eq!(structure.layout, StructLayout::Rows);
    assert_eq!(structure.rows, 3);
    assert_eq!(structure.fields[0].values, 1);
    assert!(structure.fields[0].send);
}

#[test]
fn a_stored_row_labels_key_still_loads_and_is_not_served() {
    let manifest = grid(serde_json::json!({
        "row_labels": "letters",
        "fields": [{ "name": "vol_ml", "label": "Vol" }]
    }))
    .unwrap();
    let served = serde_json::to_value(manifest.params[0].structure.as_ref().unwrap()).unwrap();
    assert!(served.get("row_labels").is_none(), "{served}");
}

#[test]
fn a_declaration_that_contradicts_its_param_is_refused() {
    for (param, expected) in [
        (
            serde_json::json!({ "name": "n", "label": "N", "kind": "number",
                "structure": { "fields": [{ "name": "a", "label": "A" }] } }),
            "object or replicate_grid",
        ),
        (
            serde_json::json!({ "name": "o", "label": "O", "kind": "object",
                "structure": { "layout": "rows", "fields": [{ "name": "a", "label": "A" }] } }),
            "does not fit kind",
        ),
    ] {
        let err = manifest_with(param).unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn a_computed_field_must_name_fields_of_its_own_structure() {
    let err = grid(serde_json::json!({
        "fields": [
            { "name": "dried_g", "label": "Dried", "send": false },
            { "name": "afdm_g", "label": "AFDM",
              "computed": { "subtract": ["dried_g", "ashed_g"] } }
        ]
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("ashed_g"), "{err}");
}

#[test]
fn a_value_is_checked_against_the_columns_the_structure_declares() {
    let manifest = grid(serde_json::json!({
        "fields": [
            { "name": "vol_ml", "label": "Vol" },
            { "name": "diameters_cm", "label": "Diameters", "values": 3 },
            { "name": "dried_g", "label": "Dried", "send": false }
        ]
    }))
    .unwrap();
    let structure = manifest.params[0].structure.as_ref().unwrap();
    let check = |rows: serde_json::Value| structure.check_value("replicates", &rows);

    assert!(check(serde_json::json!([{ "vol_ml": 1.0, "diameters_cm": [1.0, null] }])).is_ok());
    // A blank row is what an untouched replicate looks like.
    assert!(check(serde_json::json!([{}])).is_ok());

    for (value, expected) in [
        (serde_json::json!([{ "nope": 1.0 }]), "declares no 'nope'"),
        (serde_json::json!([{ "dried_g": 1.0 }]), "entry-only"),
        (serde_json::json!([{ "vol_ml": [1.0] }]), "must be a number"),
        (
            serde_json::json!([{ "diameters_cm": 1.0 }]),
            "must be a list of numbers",
        ),
        (serde_json::json!([1.0]), "must be an object"),
    ] {
        let err = check(value).unwrap_err();
        assert!(err.contains(expected), "{err}");
        assert!(err.contains("replicates"), "{err}");
    }
}

#[test]
fn an_open_structure_takes_a_column_it_does_not_declare() {
    let manifest = manifest_with(serde_json::json!({
        "name": "species", "label": "Species", "kind": "object",
        "structure": {
            "layout": "lists", "values": 3, "additional_fields": true,
            "fields": [{ "name": "NOx", "label": "NOx" }]
        }
    }))
    .unwrap();
    let structure = manifest.params[0].structure.as_ref().unwrap();
    assert!(
        structure
            .check_value("species", &serde_json::json!({ "TDN": [1.0, null, 3.0] }))
            .is_ok()
    );
    let err = structure
        .check_value("species", &serde_json::json!({ "TDN": 1.0 }))
        .unwrap_err();
    assert!(err.contains("list of numbers"), "{err}");
}

#[test]
fn a_misspelled_manifest_key_is_refused_naming_it() {
    use crate::routes::private::tools::models::parse_manifest;
    let err = parse_manifest(&serde_json::json!({
        "label": "T",
        "site_input": [{ "property": "altitude_m" }]
    }))
    .unwrap_err();
    assert!(err.contains("site_input"), "{err}");

    let err = parse_manifest(&serde_json::json!({
        "label": "T",
        "params": [{ "name": "t", "label": "T", "kind": "number", "requried": true }]
    }))
    .unwrap_err();
    assert!(err.contains("requried"), "{err}");
    assert!(err.contains("params"), "{err}");

    let err = parse_manifest(&serde_json::json!({
        "label": "T",
        "outputs": [{ "key": "doc", "label": "DOC", "agregate": "mean" }]
    }))
    .unwrap_err();
    assert!(err.contains("agregate"), "{err}");

    let err = parse_manifest(&serde_json::json!({
        "label": "T",
        "params": [
            { "name": "mode", "label": "Mode", "kind": "string" },
            { "name": "t", "label": "T", "kind": "number",
              "when": { "param": "mode", "equal": "full" } }
        ]
    }))
    .unwrap_err();
    assert!(err.contains("equal"), "{err}");
}

#[test]
fn the_station_inputs_spelling_of_site_inputs_still_parses() {
    let m = manifest(serde_json::json!({
        "label": "T",
        "params": [{ "name": "altitude_m", "label": "Altitude", "kind": "number" }],
        "station_inputs": [{ "property": "altitude_m" }]
    }))
    .unwrap();
    assert_eq!(m.site_inputs.len(), 1);
    assert_eq!(m.site_inputs[0].target(), "altitude_m");
}

#[test]
fn a_manifest_kind_outside_the_vocabulary_is_refused() {
    let raw = serde_json::json!({
        "label": "T",
        "params": [{ "name": "hue", "label": "Hue", "kind": "colour" }]
    });
    let err = serde_json::from_value::<Manifest>(raw)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown kind 'colour'"), "{err}");
}

#[test]
fn an_aggregate_outside_mean_and_sd_is_refused() {
    let raw = serde_json::json!({
        "label": "T",
        "params": [{ "name": "reps", "label": "Reps", "kind": "replicates",
                     "parameter_code": "X" }],
        "outputs": [{ "key": "med", "label": "Median", "aggregate_of": "reps",
                      "aggregate": "median" }]
    });
    let err = serde_json::from_value::<Manifest>(raw)
        .unwrap_err()
        .to_string();
    assert!(err.contains("'median' is not 'mean' or 'sd'"), "{err}");
}

#[test]
fn an_aggregate_without_a_source_is_refused() {
    let raw = serde_json::json!({
        "label": "T",
        "outputs": [{ "key": "avg", "label": "Avg", "aggregate": "mean" }]
    });
    let err = serde_json::from_value::<Manifest>(raw)
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs aggregate_of"), "{err}");
}

#[test]
fn an_aggregate_over_a_non_replicates_param_is_refused() {
    let raw = serde_json::json!({
        "label": "T",
        "params": [{ "name": "temp", "label": "Temp", "kind": "number" }],
        "outputs": [{ "key": "avg", "label": "Avg", "aggregate_of": "temp",
                      "aggregate": "mean" }]
    });
    let err = serde_json::from_value::<Manifest>(raw)
        .unwrap_err()
        .to_string();
    assert!(err.contains("names no replicates param"), "{err}");
}

#[test]
fn a_display_marker_without_aggregate_stays_free_form() {
    let raw = serde_json::json!({
        "label": "T",
        "params": [{ "name": "x", "label": "X", "kind": "number" }],
        "outputs": [{ "key": "x_avg", "label": "Avg", "aggregate_of": "x_family" }]
    });
    assert!(serde_json::from_value::<Manifest>(raw).is_ok());
}

#[test]
fn a_free_text_when_stays_a_note_and_an_object_becomes_a_condition() {
    let raw = serde_json::json!({
        "label": "T",
        "params": [
            { "name": "mode", "label": "Mode", "kind": "string" },
            { "name": "a", "label": "A", "kind": "number", "required": true,
              "when": "mode=full" },
            { "name": "b", "label": "B", "kind": "number", "required": true,
              "when": { "param": "mode", "equals": "full" } }
        ]
    });
    let manifest: Manifest = serde_json::from_value(raw).unwrap();
    assert!(matches!(manifest.params[1].when, Some(ParamWhen::Note(_))));
    let Some(ParamWhen::Condition(cond)) = &manifest.params[2].when else {
        panic!("the object form parses as a condition");
    };
    let mut body = serde_json::Map::new();
    assert!(!cond.holds(&body));
    body.insert("mode".into(), serde_json::json!("full"));
    assert!(cond.holds(&body));
    body.insert("mode".into(), serde_json::json!("simple"));
    assert!(!cond.holds(&body));
}

/// The entity is a site; `station_inputs` is what every stored manifest was written with.
#[test]
fn both_spellings_of_the_site_inputs_key_parse_the_same() {
    let by_site = manifest(serde_json::json!({
        "label": "T",
        "params": [{ "name": "alt", "label": "Altitude", "kind": "number" }],
        "site_inputs": [{ "property": "altitude_m", "param": "alt" }],
    }))
    .expect("site_inputs parses");
    let by_station = manifest(serde_json::json!({
        "label": "T",
        "params": [{ "name": "alt", "label": "Altitude", "kind": "number" }],
        "station_inputs": [{ "property": "altitude_m", "param": "alt" }],
    }))
    .expect("station_inputs still parses");

    assert_eq!(by_site.site_inputs.len(), 1);
    assert_eq!(by_station.site_inputs.len(), 1);
    assert_eq!(
        by_site.site_inputs[0].property,
        by_station.site_inputs[0].property
    );
    assert_eq!(by_site.site_inputs[0].target(), "alt");
    assert_eq!(by_station.site_inputs[0].target(), "alt");
}

/// The chain executor and the event-input resolver read the same instant, so both build this one
/// expression. The predicates below are the spot serving contract, minus the one curation rule a
/// calculation deliberately opts out of: an unverified entry is an input here, because the run
/// marks its own output pending in turn. `common/served.rs` names that exception; a drift in
/// either direction fails here.
#[test]
fn test_served_spot_value_carries_the_serving_predicates() {
    use crate::routes::private::tools::service::{build, served_spot_value_expr};
    use sea_orm::sea_query::{Alias, Expr, Query};

    for parameter in [Expr::val(uuid::Uuid::nil()), Expr::col(Alias::new("p_id"))] {
        let query = Query::select()
            .expr_as(
                served_spot_value_expr(
                    Expr::val(uuid::Uuid::nil()),
                    parameter,
                    Expr::val(chrono::Utc::now()),
                ),
                Alias::new("value"),
            )
            .to_owned();
        let sql = build(&query).sql;

        assert!(sql.contains(r#"FROM "samples" AS "smp""#), "{sql}");
        assert!(sql.contains(r#""smp"."mean""#), "{sql}");
        assert!(sql.contains(r#""r"."measurement_type" = $"#), "{sql}");
        assert!(sql.contains("r.is_flagged IS NOT TRUE"), "{sql}");
        assert!(sql.contains(r#""r"."withdrawn_at" IS NULL"#), "{sql}");
        assert!(
            !sql.contains("unverified"),
            "a pending entry is still an input to a calculation: {sql}"
        );
        assert!(
            sql.contains(r#"ORDER BY "r"."replicate_index" ASC LIMIT $"#),
            "{sql}"
        );
        assert_eq!(sql.matches("parameter_id").count(), 2, "{sql}");
    }
}

/// The parameter is the only thing that differs between the two callers: one binds an id, the
/// other names the column the statement already resolved.
#[test]
fn test_served_spot_value_takes_the_parameter_as_an_expression() {
    use crate::routes::private::tools::service::{build, served_spot_value_expr};
    use sea_orm::sea_query::{Alias, Expr, Query};

    let named = build(
        &Query::select()
            .expr(served_spot_value_expr(
                Expr::val(uuid::Uuid::nil()),
                Expr::col(Alias::new("p_id")),
                Expr::val(chrono::Utc::now()),
            ))
            .to_owned(),
    )
    .sql;
    assert!(named.contains(r#""parameter_id" = "p_id""#), "{named}");
}

/// Scenario: the coverage query is built rather than written out.
/// Expected behaviour: configuration and observation stay separate LATERALs, so a parameter with
/// no readings still reports zeroes, and a site scope narrows both sides.
#[test]
fn test_coverage_query_keeps_the_two_sides_lateral() {
    use crate::routes::private::tools::service::coverage_query;

    let unscoped = coverage_query(&[uuid::Uuid::nil()], None).sql;
    assert_eq!(
        unscoped.matches("LEFT JOIN LATERAL").count(),
        2,
        "{unscoped}"
    );
    assert!(
        unscoped.contains(r#"FROM "parameters" AS "p""#),
        "{unscoped}"
    );
    assert!(
        unscoped.contains(r#"ARRAY_AGG(DISTINCT "ds"."source_system") FILTER (WHERE "ds"."source_system" IS NOT NULL)"#),
        "{unscoped}"
    );
    assert!(unscoped.contains("ARRAY[]::text[]"), "{unscoped}");
    assert!(
        unscoped.contains(r#"ORDER BY "p"."code" ASC"#),
        "{unscoped}"
    );
    assert!(!unscoped.contains(r#""sp"."site_id""#), "{unscoped}");

    let scoped = coverage_query(&[uuid::Uuid::nil()], Some(&[uuid::Uuid::nil()])).sql;
    assert!(scoped.contains(r#""sp"."site_id" IN ($"#), "{scoped}");
    assert!(scoped.contains(r#""r"."site_id" IN ($"#), "{scoped}");
}

/// The manifest's aggregate outputs, recomputed server-side over the curve-applied replicates.
mod manifest_aggregates {
    use crate::routes::private::tools::models::{CurveSnapshot, Manifest, ResolvedCurve};
    use crate::routes::private::tools::service::apply_manifest_aggregates;

    fn manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "label": "DOC",
            "params": [{ "name": "reps", "label": "Reps", "kind": "replicates",
                         "parameter_code": "DOC", "curve": "std_curve" }],
            "curves": [{ "name": "std_curve", "label": "Standard curve" }],
            "outputs": [
                { "key": "avg", "label": "Avg", "aggregate_of": "reps", "aggregate": "mean" },
                { "key": "sd", "label": "Sd", "aggregate_of": "reps", "aggregate": "sd" },
                { "key": "other", "label": "Other" },
            ],
        }))
        .unwrap()
    }

    fn inputs(reps: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({ "reps": reps })
            .as_object()
            .unwrap()
            .clone()
    }

    fn snapshot(slope: f64, intercept: f64) -> CurveSnapshot {
        CurveSnapshot {
            name: "std_curve".to_string(),
            curve: ResolvedCurve {
                slope,
                intercept,
                standard_curve_id: None,
                label: None,
            },
        }
    }

    #[test]
    fn test_apply_manifest_aggregates_replaces_script_values_over_curved_replicates() {
        let mut results = serde_json::json!({ "avg": 999.0, "sd": 999.0, "other": 7.0 })
            .as_object()
            .unwrap()
            .clone();
        apply_manifest_aggregates(
            &manifest(),
            &inputs(serde_json::json!([1.0, null, 3.0])),
            &[snapshot(2.0, 1.0)],
            &mut results,
        );
        // Curved: 3.0 and 7.0; the null is a repeat not measured.
        assert_eq!(results["avg"], serde_json::json!(5.0));
        // Sample sd of 3 and 7: sqrt(8).
        let sd = results["sd"].as_f64().unwrap();
        assert!((sd - 8.0_f64.sqrt()).abs() < 1e-12, "{sd}");
        assert_eq!(
            results["other"],
            serde_json::json!(7.0),
            "a plain output is left alone"
        );
    }

    #[test]
    fn test_apply_manifest_aggregates_without_a_curve_uses_the_raw_values() {
        let mut results = serde_json::Map::new();
        apply_manifest_aggregates(
            &manifest(),
            &inputs(serde_json::json!([2.0, 4.0])),
            &[],
            &mut results,
        );
        assert_eq!(results["avg"], serde_json::json!(3.0));
    }

    #[test]
    fn test_apply_manifest_aggregates_removes_what_cannot_be_computed() {
        let mut results = serde_json::json!({ "avg": 1.0, "sd": 1.0 })
            .as_object()
            .unwrap()
            .clone();
        // One value has a mean and no sample sd.
        apply_manifest_aggregates(
            &manifest(),
            &inputs(serde_json::json!([2.0])),
            &[],
            &mut results,
        );
        assert_eq!(results.get("avg"), Some(&serde_json::json!(2.0)));
        assert!(!results.contains_key("sd"), "{results:?}");

        let mut results = serde_json::json!({ "avg": 1.0, "sd": 1.0 })
            .as_object()
            .unwrap()
            .clone();
        apply_manifest_aggregates(&manifest(), &serde_json::Map::new(), &[], &mut results);
        assert!(
            results.is_empty(),
            "no replicates sent, no aggregate: {results:?}"
        );
    }
}

/// How a stored golden case is judged against what a run returned.
mod golden_matching {
    use crate::routes::private::tools::service::matches_expected;
    use serde_json::json;

    #[test]
    fn test_matches_expected_numbers_within_relative_tolerance() {
        assert!(matches_expected(&json!(100.05), &json!(100.0), 1e-3));
        assert!(!matches_expected(&json!(100.2), &json!(100.0), 1e-3));
        // Below one the bound is absolute: tol * max(|expected|, 1).
        assert!(matches_expected(&json!(0.0005), &json!(0.0), 1e-3));
        assert!(!matches_expected(&json!(0.002), &json!(0.0), 1e-3));
    }

    #[test]
    fn test_matches_expected_null_and_type_mismatches() {
        assert!(matches_expected(&json!(null), &json!(null), 1e-6));
        assert!(!matches_expected(&json!(1.0), &json!(null), 1e-6));
        assert!(!matches_expected(&json!(null), &json!(1.0), 1e-6));
        assert!(matches_expected(&json!("ok"), &json!("ok"), 1e-6));
        assert!(!matches_expected(&json!("ok"), &json!("no"), 1e-6));
    }

    #[test]
    fn test_matches_expected_objects_allow_extra_keys_and_arrays_do_not() {
        assert!(matches_expected(
            &json!({ "a": 1.0, "extra": 2.0 }),
            &json!({ "a": 1.0 }),
            1e-6
        ));
        assert!(!matches_expected(
            &json!({ "a": 1.0 }),
            &json!({ "a": 1.0, "b": 2.0 }),
            1e-6
        ));
        assert!(matches_expected(
            &json!([1.0, null]),
            &json!([1.0, null]),
            1e-6
        ));
        assert!(!matches_expected(&json!([1.0]), &json!([1.0, 2.0]), 1e-6));
        assert!(!matches_expected(&json!({ "0": 1.0 }), &json!([1.0]), 1e-6));
    }
}

/// What a request body and a stored run are read into before a formula set runs.
mod run_bindings {
    use crate::routes::private::tools::service::{
        context_site, formula_bindings, read_curve, stored_curves, take_context,
    };
    use serde_json::json;

    #[test]
    fn test_take_context_pops_the_reserved_fields_and_leaves_inputs() {
        let site = uuid::Uuid::new_v4();
        let mut body =
            json!({ "site_id": site, "collected_at": "2025-06-01T10:00:00+02:00", "x": 1 })
                .as_object()
                .unwrap()
                .clone();
        let (site_id, at) = take_context(&mut body).unwrap();
        assert_eq!(site_id, Some(site));
        assert_eq!(at.unwrap().to_rfc3339(), "2025-06-01T08:00:00+00:00");
        assert_eq!(body.keys().collect::<Vec<_>>(), vec!["x"]);

        let mut body = json!({ "site_id": null }).as_object().unwrap().clone();
        assert_eq!(take_context(&mut body).unwrap(), (None, None));
    }

    #[test]
    fn test_take_context_refuses_malformed_context() {
        let mut body = json!({ "site_id": "not-a-uuid" })
            .as_object()
            .unwrap()
            .clone();
        assert!(take_context(&mut body).is_err());
        let mut body = json!({ "collected_at": "yesterday" })
            .as_object()
            .unwrap()
            .clone();
        assert!(take_context(&mut body).is_err());
    }

    #[test]
    fn test_context_site_reads_the_named_site_only() {
        let site = uuid::Uuid::new_v4();
        let body = json!({ "site_id": site, "x": 1 }).to_string();
        assert_eq!(context_site(body.as_bytes()).unwrap(), Some(site));
        assert_eq!(context_site(br#"{"x": 1}"#).unwrap(), None);
        assert_eq!(context_site(b"not json").unwrap(), None);
        assert!(context_site(br#"{"site_id": "not-a-uuid"}"#).is_err());
    }

    #[test]
    fn test_formula_bindings_splits_scalars_from_families() {
        let inputs = json!({ "temp": 12.5, "reps": [1.0, null, 3.0], "note": "x" })
            .as_object()
            .unwrap()
            .clone();
        let (scalars, families) = formula_bindings(&inputs);
        assert_eq!(scalars.len(), 1);
        assert_eq!(scalars["temp"], 12.5);
        assert_eq!(families.len(), 1);
        assert_eq!(families["reps"], vec![Some(1.0), None, Some(3.0)]);
    }

    #[test]
    fn test_stored_curves_reads_each_complete_snapshot() {
        let stored = json!([
            { "name": "std_curve", "curve": { "slope": 2.0, "intercept": 0.5 } },
            { "name": "partial", "curve": { "slope": 2.0 } },
            { "curve": { "slope": 1.0, "intercept": 0.0 } },
        ]);
        let curves = stored_curves(&stored);
        assert_eq!(curves.len(), 1);
        assert_eq!(curves["std_curve"].slope, 2.0);
        assert_eq!(curves["std_curve"].intercept, 0.5);
        assert!(stored_curves(&json!(null)).is_empty());
        assert!(read_curve(&json!({ "slope": "2", "intercept": 0.0 })).is_none());
    }
}

/// The catalog codes a stored manifest reads and writes.
mod manifest_catalog_codes {
    use crate::routes::private::tools::service::{codes_of_event_inputs, manifest_codes};
    use serde_json::json;

    #[test]
    fn test_manifest_codes_lowercases_and_merges_event_inputs() {
        let manifest = json!({
            "params": [
                { "name": "a", "parameter_code": "DOC" },
                { "name": "b" },
                { "name": "c", "parameter_code": "doc" },
            ],
            "event_inputs": [{ "name": "t", "parameter_code": "Water_Temp" }],
            "outputs": [{ "key": "o", "suggested_parameter_code": "DOC_ppb" }],
        });
        let (inputs, outputs) = manifest_codes(&manifest);
        assert_eq!(inputs, vec!["doc".to_string(), "water_temp".to_string()]);
        assert_eq!(outputs, vec!["doc_ppb".to_string()]);
        assert_eq!(
            codes_of_event_inputs(&manifest),
            vec!["water_temp".to_string()]
        );
        assert_eq!(manifest_codes(&json!({})), (Vec::new(), Vec::new()));
    }
}

/// The `doc` script the suites carry, judged by its stored manifest rather than a stand-in: a
/// change to that manifest that loosens what `doc` refuses fails here.
mod stored_doc {
    use crate::routes::private::tools::models::{Manifest, parse_manifest};
    use crate::routes::private::tools::service::{account_inputs, check_body_shape};

    /// The manifest literal of the `doc` version in the reference rows the suites load.
    fn doc_manifest() -> Manifest {
        let path = crate::test_crate_root().join("tests/fixtures/reference_rows.sql");
        let sql = std::fs::read_to_string(&path).expect("the reference rows");
        let start = sql
            .find(r#"'{"label": "DOC""#)
            .expect("the doc manifest literal")
            + 1;
        let mut depth = 0usize;
        let mut end = start;
        for (i, c) in sql[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let raw: serde_json::Value =
            serde_json::from_str(&sql[start..end].replace("''", "'")).expect("the manifest JSON");
        parse_manifest(&raw).expect("the stored manifest parses")
    }

    fn check(body: serde_json::Value) -> Result<(), String> {
        let serde_json::Value::Object(map) = body else {
            panic!("the body is an object");
        };
        check_body_shape("doc", &doc_manifest(), &map).map_err(|e| e.to_string())
    }

    #[test]
    fn test_a_well_formed_doc_body_is_accepted() {
        check(serde_json::json!({ "DOC": [101.5, null, 99.0], "std_curve": "c-1" }))
            .expect("replicates with a gap and a curve");
    }

    #[test]
    fn test_a_text_replicate_is_refused_naming_the_field() {
        let err = check(serde_json::json!({ "DOC": [101.5, "high"] })).expect_err("refused");
        assert!(err.contains("'DOC'") && err.contains("replicates"), "{err}");
    }

    #[test]
    fn test_a_scalar_where_the_replicates_go_is_refused() {
        assert!(check(serde_json::json!({ "DOC": 101.5 })).is_err());
    }

    #[test]
    fn test_a_key_doc_does_not_declare_is_refused_by_name() {
        let err = check(serde_json::json!({ "DOC": [1.0], "DOC_avg_ppb": 1.0 }))
            .expect_err("an output is not an input");
        assert!(err.contains("unknown field 'DOC_avg_ppb'"), "{err}");
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_every_key_a_doc_run_was_sent_is_used_or_ignored() {
        let provided = keys(&["DOC", "std_curve"]);
        let (used, ignored) = account_inputs(&provided, &["DOC"], Vec::new(), keys(&["std_curve"]));
        assert_eq!(used, keys(&["DOC", "std_curve"]));
        assert!(ignored.is_empty());
    }

    #[test]
    fn test_a_script_naming_what_it_used_leaves_the_rest_ignored() {
        let provided = keys(&["DOC", "operator"]);
        let (used, ignored) = account_inputs(
            &provided,
            &["DOC", "operator"],
            keys(&["DOC", "absent"]),
            Vec::new(),
        );
        assert_eq!(
            used,
            keys(&["DOC"]),
            "a key the request never sent is not used"
        );
        assert_eq!(ignored, keys(&["operator"]));
    }

    #[test]
    fn test_a_sent_key_no_param_declares_is_ignored_rather_than_dropped() {
        let provided = keys(&["DOC", "site_id"]);
        let (used, ignored) = account_inputs(&provided, &["DOC"], Vec::new(), Vec::new());
        assert_eq!(used, keys(&["DOC"]));
        assert_eq!(ignored, keys(&["site_id"]));
    }
}
