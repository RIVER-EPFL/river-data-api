//! The CNET calculators as formula sets, each run over a golden visit from the portal's own
//! database with the portal's stored outputs as the expected values.
//!
//! `tests/fixtures/cnet_formula_sets.json` is the port: one formula set per calculation, in the
//! stored form a version body holds, plus the constants the portal carries and the visits that
//! check it. Every value expected here was written by the portal's R, not derived from the
//! formulas, so a transcription slip fails against the number the lab has already published.

use std::collections::HashMap;

use crate::routes::private::tools::models::*;
use crate::routes::private::tools::service::*;

const FIXTURE: &str = include_str!("../../../../../tests/fixtures/cnet_formula_sets.json");

/// The portal stores six significant figures.
const DEFAULT_TOLERANCE: f64 = 2e-5;

fn fixture() -> serde_json::Value {
    serde_json::from_str(FIXTURE).expect("the fixture parses")
}

fn constants(fixture: &serde_json::Value) -> HashMap<String, f64> {
    fixture["constants"]
        .as_object()
        .expect("constants")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().expect("a number")))
        .collect()
}

fn numbers(value: &serde_json::Value) -> HashMap<String, f64> {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_f64().expect("a number")))
                .collect()
        })
        .unwrap_or_default()
}

fn replicates(value: &serde_json::Value) -> HashMap<String, Vec<Option<f64>>> {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let list = v.as_array().expect("a list");
                    (
                        k.clone(),
                        list.iter().map(serde_json::Value::as_f64).collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn curves(value: &serde_json::Value) -> HashMap<String, Curve> {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let pair = v.as_array().expect("[slope, intercept]");
                    (
                        k.clone(),
                        Curve {
                            slope: pair[0].as_f64().expect("slope"),
                            intercept: pair[1].as_f64().expect("intercept"),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn close(got: f64, want: f64, tolerance: f64) -> bool {
    (got - want).abs() <= tolerance * want.abs().max(1.0)
}

fn check(context: &str, produced: &[Produced], expected: &serde_json::Value, tolerance: f64) {
    for (code, want) in expected.as_object().expect("expected outputs") {
        let entry = produced
            .iter()
            .find(|p| match p {
                Produced::Scalar(e) => &e.code == code,
                Produced::PerReplicate { code: c, .. } => c == code,
            })
            .unwrap_or_else(|| panic!("{context}: nothing produced {code}"));
        match (entry, want) {
            (Produced::Scalar(evaluated), serde_json::Value::Number(n)) => {
                let want = n.as_f64().unwrap();
                let got = evaluated.value.unwrap_or_else(|| {
                    panic!("{context}: {code} skipped: {:?}", evaluated.skipped)
                });
                assert!(
                    close(got, want, tolerance),
                    "{context}: {code} = {got}, portal stored {want}"
                );
            }
            (Produced::PerReplicate { values, .. }, serde_json::Value::Array(list)) => {
                assert_eq!(values.len(), list.len(), "{context}: {code} width");
                for (index, (got, want)) in values.iter().zip(list).enumerate() {
                    let want = want.as_f64().unwrap();
                    let got =
                        got.unwrap_or_else(|| panic!("{context}: {code}[{index}] has no value"));
                    assert!(
                        close(got, want, tolerance),
                        "{context}: {code}[{index}] = {got}, portal stored {want}"
                    );
                }
            }
            (Produced::Scalar(_), _) => {
                panic!("{context}: {code} is one number, the case expects a list")
            }
            (Produced::PerReplicate { .. }, _) => {
                panic!("{context}: {code} is per replicate, the case expects one number")
            }
        }
    }
}

#[test]
fn test_every_cnet_formula_set_reproduces_its_golden_visit() {
    let fixture = fixture();
    let constants = constants(&fixture);
    for calculation in fixture["calculations"].as_array().expect("calculations") {
        let name = calculation["name"].as_str().expect("name");
        let formulas: Vec<PinnedFormula> =
            serde_json::from_value(calculation["formulas"].clone()).expect("a stored formula set");
        manifest_json(name, None, &formulas).unwrap_or_else(|e| panic!("{name}: {e}"));
        for case in calculation["cases"].as_array().expect("cases") {
            let context = format!("{name}, {}", case["name"].as_str().unwrap_or(""));
            let tolerance = case["tolerance"].as_f64().unwrap_or(DEFAULT_TOLERANCE);
            let produced = evaluate_over_replicates(
                &formulas,
                &numbers(&case["inputs"]),
                &replicates(&case["replicates"]),
                &constants,
                &curves(&case["curves"]),
            )
            .unwrap_or_else(|e| panic!("{context}: {e}"));
            check(&context, &produced, &case["expected"], tolerance);
            for code in case["skipped"].as_array().into_iter().flatten() {
                let code = code.as_str().unwrap();
                let skipped = produced.iter().any(
                    |p| matches!(p, Produced::Scalar(e) if e.code == code && e.skipped.is_some()),
                );
                assert!(skipped, "{context}: {code} was expected to skip");
            }
        }
    }
}

#[test]
fn test_every_cnet_formula_set_names_only_declared_constants() {
    let fixture = fixture();
    let declared = constants(&fixture);
    for calculation in fixture["calculations"].as_array().expect("calculations") {
        let formulas: Vec<PinnedFormula> =
            serde_json::from_value(calculation["formulas"].clone()).expect("a stored formula set");
        for constant in constants_of(&formulas) {
            assert!(
                declared.contains_key(&constant),
                "{}: {constant} is not a constant the portal carries",
                calculation["name"]
            );
        }
    }
}
