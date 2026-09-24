use uuid::Uuid;

use crate::routes::private::tools::models::*;
use crate::routes::private::tools::service::*;

fn formula(code: &str, expr: &str, slot: Option<&str>, intermediate: bool) -> PinnedFormula {
    PinnedFormula {
        code: code.to_string(),
        label: code.to_uppercase(),
        units: None,
        formula: expr.to_string(),
        ordinal: 0,
        output_parameter_code: (!intermediate).then(|| code.to_uppercase()),
        sources: vec![("raw".to_string(), "RAW".to_string())],
        held: Vec::new(),
        site_sources: Vec::new(),
        curve_slot: slot.map(str::to_string),
        per_replicate: None,
        intermediate,
    }
}

fn snapshot(name: &str, id: Option<Uuid>, slope: f64) -> CurveSnapshot {
    CurveSnapshot {
        name: name.to_string(),
        curve: ResolvedCurve {
            slope,
            intercept: 0.5,
            standard_curve_id: id,
            label: None,
        },
    }
}

fn manifest(value: serde_json::Value) -> Manifest {
    parse_manifest(&value).expect("manifest")
}

fn formula_manifest() -> Manifest {
    manifest(serde_json::json!({ "label": "set", "params": [], "outputs": [] }))
}

#[test]
fn test_computed_curves_names_only_the_output_that_read_the_slot() {
    let curve = Uuid::new_v4();
    let formulas = [
        formula(
            "x",
            "raw * curve_slope + curve_intercept",
            Some("corr"),
            false,
        ),
        formula("y", "raw * 2", None, false),
    ];
    let snapshots = [snapshot("corr", Some(curve), 1.5)];
    let x = computed_curves("x", &formulas, &formula_manifest(), &snapshots);
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].id, curve);
    assert!((x[0].slope - 1.5).abs() < f64::EPSILON);
    assert!(
        computed_curves("y", &formulas, &formula_manifest(), &snapshots).is_empty(),
        "y read no curve, so it names none"
    );
}

#[test]
fn test_computed_curves_follows_the_steps_an_output_reads() {
    let curve = Uuid::new_v4();
    let formulas = [
        formula(
            "k",
            "raw * curve_slope + curve_intercept",
            Some("corr"),
            true,
        ),
        formula("z", "k * 2", None, false),
    ];
    let snapshots = [snapshot("corr", Some(curve), 1.5)];
    let z = computed_curves("z", &formulas, &formula_manifest(), &snapshots);
    assert_eq!(z.iter().map(|c| c.id).collect::<Vec<_>>(), vec![curve]);
}

#[test]
fn test_computed_curves_names_a_curve_once_however_many_paths_reach_it() {
    let curve = Uuid::new_v4();
    let formulas = [
        formula(
            "k",
            "raw * curve_slope + curve_intercept",
            Some("corr"),
            true,
        ),
        formula("m", "k + 1", Some("corr"), true),
        formula("z", "k * m", None, false),
    ];
    let snapshots = [snapshot("corr", Some(curve), 1.5)];
    let z = computed_curves("z", &formulas, &formula_manifest(), &snapshots);
    assert_eq!(z.len(), 1);
}

#[test]
fn test_computed_curves_names_nothing_for_an_ad_hoc_slope_or_an_unfilled_slot() {
    let formulas = [
        formula(
            "x",
            "raw * curve_slope + curve_intercept",
            Some("corr"),
            false,
        ),
        formula(
            "w",
            "raw * curve_slope + curve_intercept",
            Some("other"),
            false,
        ),
    ];
    let snapshots = [snapshot("corr", None, 1.5)];
    assert!(computed_curves("x", &formulas, &formula_manifest(), &snapshots).is_empty());
    assert!(computed_curves("w", &formulas, &formula_manifest(), &snapshots).is_empty());
}

#[test]
fn test_computed_curves_reads_a_scripts_aggregate_through_its_replicates_curve() {
    let curve = Uuid::new_v4();
    let tool = manifest(serde_json::json!({
        "label": "doc",
        "params": [{
            "name": "doc_reps", "label": "DOC", "kind": "replicates", "required": true,
            "parameter_code": "DOC_raw", "curve": "doc_curve"
        }],
        "curves": [{ "name": "doc_curve", "label": "DOC curve" }],
        "outputs": [
            { "key": "doc", "label": "DOC", "aggregate": "mean", "aggregate_of": "doc_reps" },
            { "key": "other", "label": "Other" }
        ],
    }));
    let snapshots = [snapshot("doc_curve", Some(curve), 2.0)];
    let doc = computed_curves("doc", &[], &tool, &snapshots);
    assert_eq!(doc.iter().map(|c| c.id).collect::<Vec<_>>(), vec![curve]);
    assert!(computed_curves("other", &[], &tool, &snapshots).is_empty());
}

#[test]
fn test_output_key_of_finds_the_formula_writing_the_code_or_the_bound_output() {
    let formulas = [
        formula("x", "raw", None, false),
        formula("k", "raw", None, true),
    ];
    let parameter = Uuid::new_v4();
    assert_eq!(
        output_key_of(parameter, "X", &formulas, &formula_manifest()).as_deref(),
        Some("x")
    );
    assert_eq!(
        output_key_of(parameter, "K", &formulas, &formula_manifest()),
        None
    );
    let tool = manifest(serde_json::json!({
        "label": "doc",
        "params": [],
        "outputs": [
            { "key": "by_code", "label": "A", "suggested_parameter_code": "DOC" },
            { "key": "by_id", "label": "B", "parameter_id": parameter }
        ],
    }));
    assert_eq!(
        output_key_of(parameter, "DOC", &[], &tool).as_deref(),
        Some("by_id")
    );
    assert_eq!(
        output_key_of(Uuid::new_v4(), "DOC", &[], &tool).as_deref(),
        Some("by_code")
    );
}
