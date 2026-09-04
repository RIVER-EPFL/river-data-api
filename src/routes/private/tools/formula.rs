//! The formula engine: a calculation whose versions carry formulas rather than an R script.
//!
//! A calculation is one entity with one of two engines (Q43). The script engine runs R in the
//! sandbox; this one evaluates `derived_parameter_definitions` rows attached to the calculation,
//! in `ordinal` order, over the same resolved inputs. Everything downstream, the manifest, the
//! dependency order, the stored run, the provenance blob, the audit, sees one shape, because a
//! formula calculation is assembled into the same manifest and the same run outcome.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::routes::private::sensors::calibrations::service::evaluate_formula;

/// One formula of a calculation: what it computes, from what, and where the value goes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PinnedFormula {
    /// The definition's `code`; the output key on the run and in the provenance blob.
    pub code: String,
    pub label: String,
    pub units: Option<String>,
    pub formula: String,
    pub ordinal: i32,
    /// The catalog code of the parameter the value is stored under. `None` where the definition
    /// names no output parameter yet, which makes it unsavable but still evaluable.
    pub output_parameter_code: Option<String>,
    /// `(variable_name, parameter_code)`: the formula variable and the catalog parameter read
    /// into it.
    pub sources: Vec<(String, String)>,
}

/// Formulas in the order they evaluate: by `ordinal`, then by `code` so a shared ordinal is still
/// a fixed order rather than the database's.
pub fn in_order(formulas: &[PinnedFormula]) -> Vec<&PinnedFormula> {
    let mut ordered: Vec<&PinnedFormula> = formulas.iter().collect();
    ordered.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then_with(|| a.code.cmp(&b.code)));
    ordered
}

/// Parameter codes a calculation produces before the formula at `index` runs. Those are satisfied
/// from inside the calculation, so they are not declared as event inputs and are not resolved from
/// stored readings.
fn produced_before(ordered: &[&PinnedFormula], index: usize) -> Vec<String> {
    ordered[..index]
        .iter()
        .filter_map(|f| f.output_parameter_code.as_ref())
        .map(|c| c.to_lowercase())
        .collect()
}

/// The manifest a formula calculation presents, in the same JSON shape an authored manifest is
/// written in, so it parses and validates through the one manifest parser.
///
/// Each distinct source becomes a number param and an event input, except a source reading a
/// parameter an earlier formula produces: that value comes from the evaluation, not from the
/// store, so declaring it would make a first run refuse for want of a reading nothing has written
/// yet.
pub fn manifest_json(
    label: &str,
    description: Option<&str>,
    formulas: &[PinnedFormula],
) -> serde_json::Value {
    let ordered = in_order(formulas);
    let mut params = Vec::new();
    let mut event_inputs = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (index, formula) in ordered.iter().enumerate() {
        let internal = produced_before(&ordered, index);
        for (variable, parameter_code) in &formula.sources {
            if internal.contains(&parameter_code.to_lowercase()) || seen.contains(variable) {
                continue;
            }
            seen.push(variable.clone());
            params.push(json!({
                "name": variable,
                "label": variable,
                "kind": "number",
                "required": true,
            }));
            event_inputs.push(json!({ "param": variable, "parameter_code": parameter_code }));
        }
    }
    let outputs: Vec<serde_json::Value> = ordered
        .iter()
        .map(|f| {
            json!({
                "key": f.code,
                "label": f.label,
                "units": f.units,
                "suggested_parameter_code": f.output_parameter_code,
            })
        })
        .collect();

    json!({
        "label": label,
        "description": description,
        "params": params,
        "outputs": outputs,
        "event_inputs": event_inputs,
    })
}

/// The version body of a formula calculation: its formula set in evaluation order, as JSON.
///
/// A `tool_script_versions` row holds this where a script calculation holds R, so a version is
/// self-contained: the run that pins it can be replayed from the version alone, without reading
/// the definitions as they stand today.
pub fn render(formulas: &[PinnedFormula]) -> String {
    let ordered: Vec<&PinnedFormula> = in_order(formulas);
    serde_json::to_string_pretty(&ordered).unwrap_or_else(|_| "[]".to_string())
}

/// The formula set a stored version body holds.
pub fn parse_pinned(body: &str) -> Result<Vec<PinnedFormula>, String> {
    serde_json::from_str(body).map_err(|e| format!("unreadable formula set: {e}"))
}

/// Evaluate every formula in order, feeding each result forward under the parameter code it is
/// stored as, so a formula reading an earlier formula's output takes the fresh value.
///
/// `inputs` is keyed by variable name, the form the resolved run holds. The result is keyed by
/// output code, the form a run outcome holds. A formula whose variables do not all resolve is
/// reported by name rather than evaluated to nothing.
pub fn evaluate(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
) -> Result<Vec<(String, f64)>, String> {
    let ordered = in_order(formulas);
    let mut produced: HashMap<String, f64> = HashMap::new();
    let mut results = Vec::with_capacity(ordered.len());
    for formula in &ordered {
        let mut variables: HashMap<String, f64> = HashMap::new();
        for (variable, parameter_code) in &formula.sources {
            let value = produced
                .get(&parameter_code.to_lowercase())
                .or_else(|| inputs.get(variable))
                .copied();
            match value {
                Some(value) => {
                    variables.insert(variable.clone(), value);
                }
                None => {
                    return Err(format!(
                        "formula {} has no value for {variable} ({parameter_code})",
                        formula.code
                    ));
                }
            }
        }
        let value = evaluate_formula(&formula.formula, &variables)
            .map_err(|e| format!("formula {}: {e}", formula.code))?;
        if let Some(code) = &formula.output_parameter_code {
            produced.insert(code.to_lowercase(), value);
        }
        results.push((formula.code.clone(), value));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formula(
        code: &str,
        ordinal: i32,
        expr: &str,
        output: Option<&str>,
        sources: &[(&str, &str)],
    ) -> PinnedFormula {
        PinnedFormula {
            code: code.to_string(),
            label: code.to_uppercase(),
            units: None,
            formula: expr.to_string(),
            ordinal,
            output_parameter_code: output.map(str::to_string),
            sources: sources
                .iter()
                .map(|(v, p)| ((*v).to_string(), (*p).to_string()))
                .collect(),
        }
    }

    fn dom() -> Vec<PinnedFormula> {
        vec![
            formula(
                "c_a",
                2,
                "c / a",
                Some("dom_c_a"),
                &[("c", "peak_c"), ("a", "peak_a")],
            ),
            formula(
                "suva",
                1,
                "a254 / doc * 100",
                Some("suva"),
                &[("a254", "a254"), ("doc", "doc_avg_ppb")],
            ),
        ]
    }

    fn inputs(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn test_in_order_is_by_ordinal_then_code() {
        let formulas = dom();
        let ordered = in_order(&formulas);
        assert_eq!(
            ordered.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
            vec!["suva", "c_a"]
        );
    }

    #[test]
    fn test_evaluate_runs_every_formula_of_one_calculation() {
        let results = evaluate(
            &dom(),
            &inputs(&[("a254", 2.0), ("doc", 400.0), ("c", 3.0), ("a", 6.0)]),
        )
        .expect("both formulas evaluate");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "suva");
        assert!((results[0].1 - 0.5).abs() < 1e-12);
        assert_eq!(results[1].0, "c_a");
        assert!((results[1].1 - 0.5).abs() < 1e-12);
    }

    #[test]
    fn test_a_later_formula_reads_the_earlier_one_s_output() {
        let chained = vec![
            formula(
                "suva",
                1,
                "a254 / doc * 100",
                Some("suva"),
                &[("a254", "a254"), ("doc", "doc_avg_ppb")],
            ),
            formula("suva_x2", 2, "s * 2", Some("suva_x2"), &[("s", "suva")]),
        ];
        let results = evaluate(&chained, &inputs(&[("a254", 2.0), ("doc", 400.0)]))
            .expect("the second reads the first");
        assert!((results[1].1 - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_a_missing_input_names_the_variable_and_the_parameter() {
        let err = evaluate(
            &dom(),
            &inputs(&[("a254", 2.0), ("doc", 400.0), ("c", 3.0)]),
        )
        .expect_err("a is missing");
        assert!(err.contains("c_a"), "{err}");
        assert!(err.contains("peak_a"), "{err}");
    }

    #[test]
    fn test_an_unparseable_formula_names_the_definition() {
        let broken = vec![formula("bad", 1, "a +", Some("bad"), &[("a", "peak_a")])];
        let err = evaluate(&broken, &inputs(&[("a", 1.0)])).expect_err("parse fails");
        assert!(err.contains("bad"), "{err}");
    }

    #[test]
    fn test_the_manifest_declares_every_source_as_an_event_input() {
        let manifest = manifest_json("DOM", None, &dom());
        let event_inputs = manifest["event_inputs"].as_array().unwrap();
        assert_eq!(event_inputs.len(), 4);
        let params = manifest["params"].as_array().unwrap();
        assert_eq!(params.len(), 4);
        assert!(params.iter().all(|p| p["kind"] == "number"));
        let outputs = manifest["outputs"].as_array().unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0]["key"], "suva");
        assert_eq!(outputs[0]["suggested_parameter_code"], "suva");
    }

    #[test]
    fn test_an_internally_produced_source_is_not_an_event_input() {
        let chained = vec![
            formula(
                "suva",
                1,
                "a254 / doc * 100",
                Some("suva"),
                &[("a254", "a254"), ("doc", "doc_avg_ppb")],
            ),
            formula("suva_x2", 2, "s * 2", Some("suva_x2"), &[("s", "suva")]),
        ];
        let manifest = manifest_json("DOM", None, &chained);
        let event_inputs = manifest["event_inputs"].as_array().unwrap();
        assert_eq!(
            event_inputs.len(),
            2,
            "only the two stored reads: {event_inputs:?}"
        );
        assert!(
            !event_inputs.iter().any(|e| e["parameter_code"] == "suva"),
            "the produced value is not read from the store: {event_inputs:?}"
        );
    }

    #[test]
    fn test_a_version_body_round_trips_in_evaluation_order() {
        let body = render(&dom());
        let parsed = parse_pinned(&body).expect("the body reads back");
        assert_eq!(
            parsed.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
            vec!["suva", "c_a"]
        );
        assert_eq!(
            parsed,
            in_order(&dom()).into_iter().cloned().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_an_unreadable_version_body_is_refused() {
        assert!(parse_pinned("suva -> suva: a / b").is_err());
    }
}
