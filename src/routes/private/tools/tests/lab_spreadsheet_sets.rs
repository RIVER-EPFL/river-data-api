//! The two sets the lab documents as spreadsheets, authored on the calculation page one formula
//! per spreadsheet column, run over the rows the spreadsheets carry.
//!
//! `tests/fixtures/lab_spreadsheet_sets.json` is that authoring: every column of
//! `DIC.xlsx` row 19 and `CO2_CH4.xlsx` row 13 as one formula, its expression in the page's own
//! language, and the cell's own computed value as the expected number. Nine intermediate columns
//! per set are checked as well as the outputs, so a slip is located at the column it happened in
//! rather than at the last one.
//!
//! Where the spreadsheet's number differs from the constant the baseline seeds, the spreadsheet's
//! is typed as a literal and named here, because the set is a transcription of the lab's document
//! and not a correction of it.

use std::collections::HashMap;

use crate::routes::private::tools::models::*;
use crate::routes::private::tools::service::*;

const FIXTURE: &str = include_str!("../../../../../tests/fixtures/lab_spreadsheet_sets.json");

/// The deviation a set is allowed from its spreadsheet, declared per set in the fixture: nothing
/// but arithmetic reordering where the numbers agree, and the seed's own rounding where they do
/// not.
const EXACT: f64 = 1e-12;

fn fixture() -> serde_json::Value {
    serde_json::from_str(FIXTURE).expect("the fixture parses")
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

fn deviation(got: f64, want: f64) -> f64 {
    if want == 0.0 {
        return got.abs();
    }
    (got - want).abs() / want.abs()
}

fn value_of(produced: &[Produced], code: &str, index: usize) -> Option<f64> {
    produced.iter().find_map(|p| match p {
        Produced::Scalar(e) if e.code == code => Some(e.value),
        Produced::PerReplicate {
            code: c, values, ..
        } if c == code => Some(values.get(index).copied().flatten()),
        _ => None,
    })?
}

#[test]
fn test_each_lab_spreadsheet_column_is_reproduced_by_its_formula() {
    let fixture = fixture();
    let constants = numbers(&fixture["constants"]);
    for calculation in fixture["calculations"].as_array().expect("calculations") {
        let name = calculation["name"].as_str().expect("name");
        let formulas: Vec<PinnedFormula> =
            serde_json::from_value(calculation["formulas"].clone()).expect("a stored formula set");
        for case in calculation["cases"].as_array().expect("cases") {
            let context = format!("{name}, {}", case["name"].as_str().unwrap_or(""));
            let tolerance = calculation["tolerance"].as_f64().unwrap_or(EXACT);
            let mut worst = (0.0_f64, String::new());
            let produced = evaluate_over_replicates(
                &formulas,
                &numbers(&case["inputs"]),
                &replicates(&case["replicates"]),
                &constants,
                &HashMap::new(),
            )
            .unwrap_or_else(|e| panic!("{context}: {e}"));
            for (code, want) in numbers(&case["steps"]).into_iter().chain(
                case["expected"]
                    .as_object()
                    .expect("expected")
                    .iter()
                    .map(|(code, list)| {
                        (
                            code.clone(),
                            list.as_array().expect("a list")[0]
                                .as_f64()
                                .expect("a number"),
                        )
                    }),
            ) {
                let got = value_of(&produced, &code, 0)
                    .unwrap_or_else(|| panic!("{context}: {code} produced nothing"));
                let off = deviation(got, want);
                assert!(
                    off <= tolerance,
                    "{context}: {code} = {got}, the spreadsheet cell holds {want} ({off:e} off, \
                     the set allows {tolerance:e})"
                );
                if off > worst.0 {
                    worst = (off, code.clone());
                }
            }
            println!("{context}: worst {:e} at {}", worst.0, worst.1);
        }
    }
}

#[test]
fn test_the_lab_spreadsheet_sets_name_only_seeded_constants() {
    let fixture = fixture();
    let declared = numbers(&fixture["constants"]);
    for calculation in fixture["calculations"].as_array().expect("calculations") {
        let formulas: Vec<PinnedFormula> =
            serde_json::from_value(calculation["formulas"].clone()).expect("a stored formula set");
        for constant in constants_of(&formulas) {
            assert!(
                declared.contains_key(&constant),
                "{}: {constant} is not one of the twelve constants the baseline seeds",
                calculation["name"]
            );
        }
    }
}

/// The same visit through three accounts of it: the value the portal stored, the set ported from
/// the portal's R, and the set transcribed from the lab's spreadsheet. The ported set is asserted
/// against what the portal stored; the spreadsheet's distance from it is printed and asserted at
/// the recorded ratio, so neither document can move without the test saying so.
#[test]
fn test_the_spreadsheet_and_the_portal_disagree_by_the_recorded_ratio() {
    let sheets = fixture();
    let ported_fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/cnet_formula_sets.json"
    ))
    .expect("the ported sets parse");
    let constants = numbers(&sheets["constants"]);
    for row in sheets["portal_rows"].as_array().expect("portal_rows") {
        let context = row["case"].as_str().unwrap_or("");
        let stored = numbers(&row["portal"]);
        let ported = run(&ported_fixture, &row["ported"], &constants, context);
        let transcribed = run(&sheets, &row["sheet"], &constants, context);
        for (quantity, portal) in &stored {
            let r = ported[quantity];
            let sheet = transcribed[quantity];
            println!(
                "{context}: {quantity} portal {portal}, R {r} (x{:.6}), spreadsheet {sheet} \
                 (x{:.6})",
                r / portal,
                sheet / portal
            );
            let recorded = &row["ratios"][quantity];
            for (side, got, want) in [
                ("the ported set", r / portal, recorded["ported"].as_f64()),
                (
                    "the spreadsheet",
                    sheet / portal,
                    recorded["sheet"].as_f64(),
                ),
            ] {
                let want = want.unwrap_or_else(|| panic!("{context}: no {side} ratio recorded"));
                assert!(
                    deviation(got, want) <= 1e-5,
                    "{context}: {quantity} over {side} is {got} of what the portal stored, the \
                     recorded ratio is {want}"
                );
            }
        }
    }
}

/// One set from one fixture over one row, as a map from the quantity the row names to the value
/// the set's own code for it produced.
fn run(
    fixture: &serde_json::Value,
    spec: &serde_json::Value,
    constants: &HashMap<String, f64>,
    context: &str,
) -> HashMap<String, f64> {
    let name = spec["calculation"].as_str().expect("calculation");
    let calculation = fixture["calculations"]
        .as_array()
        .expect("calculations")
        .iter()
        .find(|c| c["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("no set named {name}"));
    let formulas: Vec<PinnedFormula> =
        serde_json::from_value(calculation["formulas"].clone()).expect("a stored formula set");
    let produced = evaluate_over_replicates(
        &formulas,
        &numbers(&spec["inputs"]),
        &replicates(&spec["replicates"]),
        constants,
        &HashMap::new(),
    )
    .unwrap_or_else(|e| panic!("{context}, {name}: {e}"));
    spec["codes"]
        .as_object()
        .expect("codes")
        .iter()
        .map(|(quantity, code)| {
            let code = code.as_str().expect("a code");
            let value = value_of(&produced, code, 0)
                .unwrap_or_else(|| panic!("{context}, {name}: {code} produced nothing"));
            (quantity.clone(), value)
        })
        .collect()
}
