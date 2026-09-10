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
/// expression. The predicates below are the spot serving contract.
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
        assert!(sql.contains(r#""r"."is_flagged" IS NOT TRUE"#), "{sql}");
        assert!(sql.contains(r#""r"."withdrawn_at" IS NULL"#), "{sql}");
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

    let scoped = coverage_query(&[uuid::Uuid::nil()], Some(uuid::Uuid::nil())).sql;
    assert!(scoped.contains(r#""sp"."site_id" = $"#), "{scoped}");
    assert!(scoped.contains(r#""r"."site_id" = $"#), "{scoped}");
}
