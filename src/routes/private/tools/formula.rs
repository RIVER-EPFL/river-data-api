//! The formula engine: a calculation whose versions carry formulas rather than an R script.
//!
//! A calculation is one entity with one of two engines (Q43). The script engine runs R in the
//! sandbox; this one evaluates `calculation_formulas` rows attached to the calculation,
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
    /// The curve slot this formula corrects with, if any. Inside the formula the slot's
    /// coefficients are the variables `curve_slope` and `curve_intercept`, so a calculation whose
    /// outputs take different curves declares a slot per formula rather than one per calculation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve_slot: Option<String>,
    /// The variable whose replicate vector this formula evaluates over, one value per index.
    /// `None` is a formula producing one number. The output's replicate identity is the named
    /// input's, never one the calculation assigns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_replicate: Option<String>,
}

/// The coefficients a resolved curve slot binds into a formula.
#[derive(Clone, Copy, Debug)]
pub struct Curve {
    pub slope: f64,
    pub intercept: f64,
}

/// What one formula of a run produced. `value` is `None` for a value computed as not-a-number,
/// which is the portal's NA and clears the stored value; `skipped` says the formula never ran,
/// which names no output at all.
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluated {
    pub code: String,
    pub value: Option<f64>,
    pub curve_slot: Option<String>,
    pub skipped: Option<String>,
}

/// What a calculation produced for one output: one number, or one per replicate index.
///
/// A per-replicate output keeps its gaps: a repeat that was not measured is a `None` at that
/// index, never a shorter list, because the index is the source's column position and closing up
/// would re-label every value after it.
#[derive(Clone, Debug, PartialEq)]
pub enum Produced {
    Scalar(Evaluated),
    PerReplicate {
        code: String,
        values: Vec<Option<f64>>,
        curve_slot: Option<String>,
    },
}

/// The variables a formula reads that are not sources: the coefficients of its curve slot.
pub const CURVE_VARIABLES: [&str; 2] = ["curve_slope", "curve_intercept"];

/// Identifiers a formula may name that are neither a source nor a constant: meval's own
/// functions and constants, and the guard functions [`evaluate_formula`] registers.
pub const FORMULA_BUILTINS: &[&str] = &[
    "sqrt",
    "abs",
    "ln",
    "log",
    "exp",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "sinh",
    "cosh",
    "tanh",
    "floor",
    "ceil",
    "round",
    "signum",
    "min",
    "max",
    "pi",
    "e",
    "if",
    "and",
    "or",
    "not",
    "lt",
    "le",
    "gt",
    "ge",
    "eq",
    "ne",
    "coalesce",
    "is_missing",
    "na",
];

/// Every identifier a formula names that the language does not define itself, in the order they
/// appear. Sources, constants and curve coefficients are all in here; which is which is decided
/// by the caller against what it holds.
pub fn free_identifiers(formula: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in formula.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            tokens.push(&formula[s..i]);
        }
    }
    if let Some(s) = start {
        tokens.push(&formula[s..]);
    }
    let mut seen: Vec<String> = Vec::new();
    for token in tokens {
        if token.chars().next().is_some_and(|c| c.is_ascii_digit())
            || FORMULA_BUILTINS.contains(&token)
            || seen.iter().any(|s| s == token)
        {
            continue;
        }
        seen.push(token.to_string());
    }
    seen
}

/// The constants a formula set reads: every free identifier that is neither one of the formula's
/// own source variables nor a curve coefficient. Sorted, so a manifest is the same document
/// whichever order the definitions were written in.
pub fn constants_of(formulas: &[PinnedFormula]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for formula in formulas {
        for identifier in free_identifiers(&formula.formula) {
            if formula.sources.iter().any(|(v, _)| *v == identifier)
                || CURVE_VARIABLES.contains(&identifier.as_str())
                || names.contains(&identifier)
            {
                continue;
            }
            names.push(identifier);
        }
    }
    names.sort();
    names
}

/// The curve slots a formula set declares, in evaluation order and deduplicated.
pub fn curve_slots(formulas: &[PinnedFormula]) -> Vec<String> {
    let mut slots: Vec<String> = Vec::new();
    for formula in in_order(formulas).unwrap_or_else(|_| formulas.iter().collect()) {
        if let Some(slot) = &formula.curve_slot
            && !slots.contains(slot)
        {
            slots.push(slot.clone());
        }
    }
    slots
}

/// Formulas in the order they evaluate: producers before consumers, the same relation
/// `chain::dependency_order` and `build_evaluation_order` sort tools and derived parameters by. An
/// edge A→B exists where A's output parameter is one of B's sources. `ordinal` then `code` is the
/// tie-break among formulas nothing orders, never the order itself: it is hand-set, defaults to 0
/// for every formula the authoring form creates, and ordering by it alone made a dependent formula
/// read the store instead of the value just produced.
///
/// A cycle has no runnable order and is returned naming its members, the way the other two engines
/// answer one.
pub fn in_order(formulas: &[PinnedFormula]) -> Result<Vec<&PinnedFormula>, String> {
    let mut candidates: Vec<&PinnedFormula> = formulas.iter().collect();
    candidates.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then_with(|| a.code.cmp(&b.code)));

    let produces: Vec<Option<String>> = candidates
        .iter()
        .map(|f| f.output_parameter_code.as_ref().map(|c| c.to_lowercase()))
        .collect();
    let consumes: Vec<Vec<String>> = candidates
        .iter()
        .map(|f| f.sources.iter().map(|(_, p)| p.to_lowercase()).collect())
        .collect();

    let n = candidates.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in 0..n {
        for (a, produced) in produces.iter().enumerate() {
            if a != b
                && let Some(code) = produced
                && consumes[b].contains(code)
            {
                deps[b].push(a);
            }
        }
    }

    let mut ordered = Vec::with_capacity(n);
    let mut placed = vec![false; n];
    loop {
        let mut progressed = false;
        for i in 0..n {
            if !placed[i] && deps[i].iter().all(|&d| placed[d]) {
                placed[i] = true;
                ordered.push(candidates[i]);
                progressed = true;
            }
        }
        if ordered.len() == n {
            return Ok(ordered);
        }
        if !progressed {
            let cycle: Vec<&str> = (0..n)
                .filter(|&i| !placed[i])
                .map(|i| candidates[i].code.as_str())
                .collect();
            return Err(format!(
                "formulas form a dependency cycle: {}",
                cycle.join(", ")
            ));
        }
    }
}

/// Parameter codes a calculation produces before the formula at `index` runs. Those are satisfied
/// from inside the calculation, so they are not declared as event inputs and are not resolved from
/// stored readings.
///
/// A per-replicate output is one of them only for another per-replicate formula, which reads it at
/// its own index. A scalar consumer reads the family's mean, which the `samples` trigger derives
/// after the repeats are stored and never a formula (Q95, D21), so for that one the output stays an
/// event input and the second stage converges on the pass after the repeats land.
fn produced_before(
    ordered: &[&PinnedFormula],
    index: usize,
    consumer_is_per_replicate: bool,
) -> Vec<String> {
    ordered[..index]
        .iter()
        .filter(|f| f.per_replicate.is_none() || consumer_is_per_replicate)
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
) -> Result<serde_json::Value, String> {
    let ordered = in_order(formulas)?;
    let driven: Vec<String> = ordered
        .iter()
        .filter_map(|f| f.per_replicate.clone())
        .collect();
    let mut params = Vec::new();
    let mut event_inputs = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (index, formula) in ordered.iter().enumerate() {
        let internal = produced_before(&ordered, index, formula.per_replicate.is_some());
        for (variable, parameter_code) in &formula.sources {
            if internal.contains(&parameter_code.to_lowercase()) || seen.contains(variable) {
                continue;
            }
            seen.push(variable.clone());
            if driven.contains(variable) {
                // A variable a formula evaluates over is the family, not one number: the body
                // carries the whole list, and the param names the parameter those readings are
                // of. It is deliberately not an event input as well, because resolving one would
                // put the group's single served value into a field that holds the repeats.
                params.push(json!({
                    "name": variable,
                    "label": variable,
                    "kind": "replicates",
                    "parameter_code": parameter_code,
                    "required": false,
                }));
                continue;
            }
            params.push(json!({
                "name": variable,
                "label": variable,
                "kind": "number",
                "required": false,
            }));
            event_inputs.push(json!({ "param": variable, "parameter_code": parameter_code }));
        }
    }
    let outputs: Vec<serde_json::Value> = ordered
        .iter()
        .map(|f| {
            let mut output = json!({
                "key": f.code,
                "label": f.label,
                "units": f.units,
                "suggested_parameter_code": f.output_parameter_code,
            });
            // Declared only when true: an output that says nothing is one number, which is what
            // every manifest written before this meant.
            if f.per_replicate.is_some() {
                output["per_replicate"] = json!(true);
            }
            output
        })
        .collect();
    let curves: Vec<serde_json::Value> = curve_slots(formulas)
        .into_iter()
        .map(|slot| json!({ "name": slot, "label": slot, "required": false }))
        .collect();

    Ok(json!({
        "label": label,
        "description": description,
        "params": params,
        "outputs": outputs,
        "constants": constants_of(formulas),
        "curves": curves,
        "event_inputs": event_inputs,
    }))
}

/// The version body of a formula calculation: its formula set in evaluation order, as JSON.
///
/// A `tool_script_versions` row holds this where a script calculation holds R, so a version is
/// self-contained: the run that pins it can be replayed from the version alone, without reading
/// the definitions as they stand today.
pub fn render(formulas: &[PinnedFormula]) -> Result<String, String> {
    let ordered = in_order(formulas)?;
    Ok(serde_json::to_string_pretty(&ordered).unwrap_or_else(|_| "[]".to_string()))
}

/// The formula set a stored version body holds.
pub fn parse_pinned(body: &str) -> Result<Vec<PinnedFormula>, String> {
    serde_json::from_str(body).map_err(|e| format!("unreadable formula set: {e}"))
}

/// Evaluate every formula in order, feeding each result forward under the parameter code it is
/// stored as, so a formula reading an earlier formula's output takes the fresh value.
///
/// `inputs` is keyed by variable name, the form the resolved run holds; `constants` and `curves`
/// are what the manifest declared, resolved by the caller. The result is one entry per formula,
/// in evaluation order, keyed by output code, the form a run outcome holds.
///
/// A formula whose sources do not all resolve is skipped and the rest still evaluate: the portal
/// warns and moves to the next calculation rather than losing the row. A formula reading a
/// skipped formula's output skips in turn. Only an unevaluable expression is fatal, because that
/// is the definition being wrong rather than the visit being incomplete.
pub fn evaluate(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
) -> Result<Vec<Evaluated>, String> {
    evaluate_set(formulas, inputs, constants, curves, false)
}

/// One pass over the formula set. `chain_replicates` is the per-index pass of
/// [`evaluate_over_replicates`]: a per-replicate result is handed to a later per-replicate formula
/// at the same index, and to nothing else.
fn evaluate_set(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
    chain_replicates: bool,
) -> Result<Vec<Evaluated>, String> {
    let ordered = in_order(formulas)?;
    let mut produced: HashMap<String, f64> = HashMap::new();
    let mut at_index: HashMap<String, f64> = HashMap::new();
    let mut results = Vec::with_capacity(ordered.len());
    for formula in &ordered {
        let mut variables: HashMap<String, f64> = constants.clone();
        let mut skipped = None;
        for (variable, parameter_code) in &formula.sources {
            let code = parameter_code.to_lowercase();
            let chained = formula
                .per_replicate
                .is_some()
                .then(|| at_index.get(&code))
                .flatten();
            let value = chained
                .or_else(|| produced.get(&code))
                .or_else(|| inputs.get(variable))
                .copied();
            match value {
                Some(value) => {
                    variables.insert(variable.clone(), value);
                }
                None => {
                    skipped = Some(format!("no value for {variable} ({parameter_code})"));
                    break;
                }
            }
        }
        if let Some(slot) = &formula.curve_slot
            && skipped.is_none()
        {
            match curves.get(slot) {
                Some(curve) => {
                    variables.insert(CURVE_VARIABLES[0].to_string(), curve.slope);
                    variables.insert(CURVE_VARIABLES[1].to_string(), curve.intercept);
                }
                None => skipped = Some(format!("curve '{slot}' was not supplied")),
            }
        }
        if let Some(reason) = skipped {
            results.push(Evaluated {
                code: formula.code.clone(),
                value: None,
                curve_slot: formula.curve_slot.clone(),
                skipped: Some(reason),
            });
            continue;
        }
        let value = evaluate_formula(&formula.formula, &variables)
            .map_err(|e| format!("formula {}: {e}", formula.code))?;
        // NaN is the portal's NA: computed, and not a number. It clears the stored value rather
        // than feeding the next formula, which would turn one NA into a whole calculation of them.
        // A per-replicate value is one repeat, so it travels only to a later per-replicate formula
        // at this index; a scalar formula reading that parameter takes the stored mean instead.
        if !value.is_nan()
            && let Some(code) = &formula.output_parameter_code
        {
            if formula.per_replicate.is_none() {
                produced.insert(code.to_lowercase(), value);
            } else if chain_replicates {
                at_index.insert(code.to_lowercase(), value);
            }
        }
        results.push(Evaluated {
            code: formula.code.clone(),
            value: (!value.is_nan()).then_some(value),
            curve_slot: formula.curve_slot.clone(),
            skipped: None,
        });
    }
    Ok(results)
}

/// The number of replicate indexes a calculation runs over: the longest replicate vector any
/// per-replicate formula names. Nothing declared per-replicate means a width of one, which is the
/// scalar case running once.
fn replicate_width(
    formulas: &[&PinnedFormula],
    replicates: &HashMap<String, Vec<Option<f64>>>,
) -> usize {
    formulas
        .iter()
        .filter_map(|f| family_width(f, formulas, replicates))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// How many indexes one per-replicate formula runs over: the length of the entered family it names,
/// or, where it names an earlier formula's per-replicate output, that producer's width.
fn family_width(
    formula: &PinnedFormula,
    formulas: &[&PinnedFormula],
    replicates: &HashMap<String, Vec<Option<f64>>>,
) -> Option<usize> {
    let variable = formula.per_replicate.as_ref()?;
    if let Some(values) = replicates.get(variable) {
        return Some(values.len());
    }
    let code = formula
        .sources
        .iter()
        .find(|(name, _)| name == variable)
        .map(|(_, code)| code.to_lowercase())?;
    // The set is topologically ordered by `in_order`, so a producer is always earlier and the walk
    // terminates.
    let producer = formulas.iter().find(|f| {
        f.output_parameter_code
            .as_ref()
            .is_some_and(|produced| produced.to_lowercase() == code)
    })?;
    family_width(producer, formulas, replicates)
}

/// Evaluate a calculation whose formulas may be per-replicate, running the whole set once per
/// index and assembling one entry per output.
///
/// A scalar formula is evaluated at index 0 and reported once: it reads the replicate group's
/// statistics or a scalar input, not one repeat. A per-replicate formula is reported as a vector
/// the width of the family, holding `None` where that index had no value, so a gap at index 1
/// stays at index 1.
///
/// `inputs` are the scalars, `replicates` the vectors keyed by the same variable names. A variable
/// present in both takes its indexed value, because a formula that declared itself per-replicate
/// asked for the repeat rather than the summary.
pub fn evaluate_over_replicates(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    replicates: &HashMap<String, Vec<Option<f64>>>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
) -> Result<Vec<Produced>, String> {
    let ordered = in_order(formulas)?;
    let width = replicate_width(&ordered, replicates);
    let mut per_index: Vec<Vec<Evaluated>> = Vec::with_capacity(width);
    for index in 0..width {
        let mut at_index = inputs.clone();
        for (variable, values) in replicates {
            match values.get(index).copied().flatten() {
                Some(value) => {
                    at_index.insert(variable.clone(), value);
                }
                // A repeat that was not measured is absent, which skips the formulas reading it
                // and leaves this index a gap rather than falling back to the group's summary.
                None => {
                    at_index.remove(variable);
                }
            }
        }
        per_index.push(evaluate_set(formulas, &at_index, constants, curves, true)?);
    }

    let mut produced = Vec::with_capacity(ordered.len());
    for (position, formula) in ordered.iter().enumerate() {
        if formula.per_replicate.is_none() {
            produced.push(Produced::Scalar(per_index[0][position].clone()));
            continue;
        }
        produced.push(Produced::PerReplicate {
            code: formula.code.clone(),
            values: per_index
                .iter()
                .map(|results| results[position].value)
                .collect(),
            curve_slot: formula.curve_slot.clone(),
        });
    }
    Ok(produced)
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
            curve_slot: None,
            per_replicate: None,
        }
    }

    fn per_replicate(
        code: &str,
        ordinal: i32,
        expr: &str,
        output: Option<&str>,
        sources: &[(&str, &str)],
        over: &str,
    ) -> PinnedFormula {
        PinnedFormula {
            per_replicate: Some(over.to_string()),
            ..formula(code, ordinal, expr, output, sources)
        }
    }

    fn replicates(pairs: &[(&str, &[Option<f64>])]) -> HashMap<String, Vec<Option<f64>>> {
        pairs
            .iter()
            .map(|(name, values)| ((*name).to_string(), values.to_vec()))
            .collect()
    }

    fn with_curve(mut f: PinnedFormula, slot: &str) -> PinnedFormula {
        f.curve_slot = Some(slot.to_string());
        f
    }

    fn constants(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    fn curves(pairs: &[(&str, f64, f64)]) -> HashMap<String, Curve> {
        pairs
            .iter()
            .map(|(name, slope, intercept)| {
                (
                    (*name).to_string(),
                    Curve {
                        slope: *slope,
                        intercept: *intercept,
                    },
                )
            })
            .collect()
    }

    fn run(formulas: &[PinnedFormula], inputs: &HashMap<String, f64>) -> Vec<Evaluated> {
        evaluate(formulas, inputs, &HashMap::new(), &HashMap::new()).expect("evaluates")
    }

    fn value_of(results: &[Evaluated], code: &str) -> Option<f64> {
        results
            .iter()
            .find(|e| e.code == code)
            .expect("the output is named")
            .value
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

    fn codes(formulas: &[PinnedFormula]) -> Vec<String> {
        in_order(formulas)
            .expect("the set has an order")
            .iter()
            .map(|f| f.code.clone())
            .collect()
    }

    #[test]
    fn test_in_order_falls_back_to_ordinal_then_code_where_nothing_depends() {
        assert_eq!(codes(&dom()), vec!["suva", "c_a"]);
    }

    /// The authoring form sends no ordinal, so every formula of a calculation is created at 0 and
    /// the codes decide the order. `co2` reads `k_h`, and sorts before it.
    fn shared_ordinal_chain() -> Vec<PinnedFormula> {
        vec![
            formula("co2", 0, "k * 2", Some("co2"), &[("k", "k_h")]),
            formula("k_h", 0, "t + 1", Some("k_h"), &[("t", "water_temp")]),
        ]
    }

    #[test]
    fn test_a_dependency_orders_ahead_of_its_ordinal_and_its_code() {
        assert_eq!(codes(&shared_ordinal_chain()), vec!["k_h", "co2"]);
    }

    #[test]
    fn test_a_produced_value_is_not_declared_an_event_input_whatever_the_codes_sort_to() {
        let manifest = manifest_json("Carbonate", None, &shared_ordinal_chain())
            .expect("the set has an order");
        let event_inputs = manifest["event_inputs"].as_array().unwrap();
        assert_eq!(
            event_inputs.len(),
            1,
            "only water_temp is read from the store: {event_inputs:?}"
        );
        assert!(!event_inputs.iter().any(|e| e["parameter_code"] == "k_h"));
    }

    #[test]
    fn test_a_dependent_formula_takes_the_produced_value_not_the_stored_one() {
        // With no order computed, `pco2` would run first and read whatever `k_h` is stored as.
        let results = evaluate(
            &shared_ordinal_chain(),
            &inputs(&[("t", 3.0), ("k", 99.0)]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("both formulas evaluate");
        assert_eq!(results[0].code, "k_h");
        assert_eq!(results[0].value, Some(4.0));
        assert_eq!(results[1].code, "co2");
        assert_eq!(results[1].value, Some(8.0));
    }

    #[test]
    fn test_a_cycle_is_refused_naming_both_formulas() {
        let cycle = vec![
            formula("a", 0, "b + 1", Some("out_a"), &[("b", "out_b")]),
            formula("b", 0, "a + 1", Some("out_b"), &[("a", "out_a")]),
        ];
        let err = in_order(&cycle).expect_err("a cycle has no order");
        assert!(err.contains("a") && err.contains("b"), "{err}");
        assert!(evaluate(&cycle, &inputs(&[]), &HashMap::new(), &HashMap::new()).is_err());
    }

    #[test]
    fn test_evaluate_runs_every_formula_of_one_calculation() {
        let results = run(
            &dom(),
            &inputs(&[("a254", 2.0), ("doc", 400.0), ("c", 3.0), ("a", 6.0)]),
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].code, "suva");
        assert!((results[0].value.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(results[1].code, "c_a");
        assert!((results[1].value.unwrap() - 0.5).abs() < 1e-12);
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
        let results = run(&chained, &inputs(&[("a254", 2.0), ("doc", 400.0)]));
        assert!((results[1].value.unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_a_missing_input_skips_its_own_formula_and_keeps_the_rest() {
        let results = run(
            &dom(),
            &inputs(&[("a254", 2.0), ("doc", 400.0), ("c", 3.0)]),
        );
        assert!((value_of(&results, "suva").unwrap() - 0.5).abs() < 1e-12);
        let skipped = results.iter().find(|e| e.code == "c_a").unwrap();
        assert_eq!(skipped.value, None);
        let reason = skipped.skipped.as_deref().expect("c_a is recorded skipped");
        assert!(reason.contains('a'), "{reason}");
        assert!(reason.contains("peak_a"), "{reason}");
    }

    #[test]
    fn test_a_formula_reading_a_skipped_output_skips_in_turn() {
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
        let results = run(&chained, &inputs(&[("a254", 2.0)]));
        assert!(results.iter().all(|e| e.skipped.is_some()));
    }

    #[test]
    fn test_an_unparseable_formula_names_the_definition() {
        let broken = vec![formula("bad", 1, "a +", Some("bad"), &[("a", "peak_a")])];
        let err = evaluate(
            &broken,
            &inputs(&[("a", 1.0)]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect_err("parse fails");
        assert!(err.contains("bad"), "{err}");
    }

    #[test]
    fn test_the_manifest_declares_every_source_as_an_event_input() {
        let manifest = manifest_json("DOM", None, &dom()).expect("the set has an order");
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
        let manifest = manifest_json("DOM", None, &chained).expect("the set has an order");
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
        let body = render(&dom()).expect("the set has an order");
        let parsed = parse_pinned(&body).expect("the body reads back");
        assert_eq!(
            parsed.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(),
            vec!["suva", "c_a"]
        );
        assert_eq!(
            parsed,
            in_order(&dom())
                .expect("the set has an order")
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_an_unreadable_version_body_is_refused() {
        assert!(parse_pinned("suva -> suva: a / b").is_err());
    }

    // --- M105, constants ---

    fn pco2() -> Vec<PinnedFormula> {
        vec![formula(
            "ch4",
            1,
            "raw * gas_const_r_atm / lab_temp_avg_degC",
            Some("ch4_um"),
            &[("raw", "ch4_raw")],
        )]
    }

    #[test]
    fn test_a_formula_naming_a_constant_evaluates_with_the_resolved_value() {
        let results = evaluate(
            &pco2(),
            &inputs(&[("raw", 4.0)]),
            &constants(&[("gas_const_r_atm", 0.082_057), ("lab_temp_avg_degC", 20.0)]),
            &HashMap::new(),
        )
        .expect("the constant resolves");
        let expected = 4.0 * 0.082_057 / 20.0;
        assert!((results[0].value.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn test_the_manifest_declares_the_constants_the_formulas_read() {
        let manifest = manifest_json("pCO2", None, &pco2()).expect("the set has an order");
        let declared = manifest["constants"].as_array().unwrap();
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0], "gas_const_r_atm");
        assert_eq!(declared[1], "lab_temp_avg_degC");
    }

    #[test]
    fn test_a_source_variable_is_not_declared_as_a_constant() {
        let manifest = manifest_json("DOM", None, &dom()).expect("the set has an order");
        assert!(manifest["constants"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_a_constant_the_table_does_not_hold_is_refused_naming_it() {
        let err = evaluate(
            &pco2(),
            &inputs(&[("raw", 4.0)]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect_err("nothing binds the constants");
        assert!(err.contains("gas_const_r_atm"), "{err}");
    }

    // --- M106, curves ---

    fn chla() -> Vec<PinnedFormula> {
        vec![
            with_curve(
                formula(
                    "acid",
                    1,
                    "raw * curve_slope + curve_intercept",
                    Some("chla_acid"),
                    &[("raw", "chla_raw")],
                ),
                "acid_curve",
            ),
            with_curve(
                formula(
                    "no_acid",
                    2,
                    "raw * curve_slope + curve_intercept",
                    Some("chla_no_acid"),
                    &[("raw", "chla_raw")],
                ),
                "no_acid_curve",
            ),
        ]
    }

    #[test]
    fn test_two_formulas_apply_two_curves_and_each_records_its_own() {
        let results = evaluate(
            &chla(),
            &inputs(&[("raw", 2.0)]),
            &HashMap::new(),
            &curves(&[("acid_curve", 3.0, 1.0), ("no_acid_curve", 5.0, 2.0)]),
        )
        .expect("both curves resolve");
        assert!((results[0].value.unwrap() - 7.0).abs() < 1e-12);
        assert_eq!(results[0].curve_slot.as_deref(), Some("acid_curve"));
        assert!((results[1].value.unwrap() - 12.0).abs() < 1e-12);
        assert_eq!(results[1].curve_slot.as_deref(), Some("no_acid_curve"));
    }

    #[test]
    fn test_the_manifest_declares_a_slot_per_curve_a_formula_names() {
        let manifest = manifest_json("Chl a", None, &chla()).expect("the set has an order");
        let declared = manifest["curves"].as_array().unwrap();
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0]["name"], "acid_curve");
        assert_eq!(declared[1]["name"], "no_acid_curve");
        assert!(
            manifest["constants"].as_array().unwrap().is_empty(),
            "the curve coefficients are not constants: {manifest}"
        );
    }

    #[test]
    fn test_a_formula_whose_curve_is_not_supplied_skips_rather_than_correcting_nothing() {
        let results = evaluate(
            &chla(),
            &inputs(&[("raw", 2.0)]),
            &HashMap::new(),
            &curves(&[("acid_curve", 3.0, 1.0)]),
        )
        .expect("the supplied curve still evaluates");
        assert!((results[0].value.unwrap() - 7.0).abs() < 1e-12);
        let reason = results[1].skipped.as_deref().expect("no_acid is skipped");
        assert!(reason.contains("no_acid_curve"), "{reason}");
    }

    // --- M108, the guards ---

    /// `calcCO2corr` takes the field pressure when it is present and within 700 to 1050 hPa, and
    /// the pressure computed from the station's altitude otherwise.
    fn pressure_selection() -> Vec<PinnedFormula> {
        vec![formula(
            "bp",
            1,
            "if(and(ge(field_bp, 700), le(field_bp, 1050)), field_bp, alt_bp)",
            Some("bp"),
            &[("field_bp", "field_bp"), ("alt_bp", "field_bp_altitude")],
        )]
    }

    #[test]
    fn test_the_pressure_selection_takes_the_field_value_inside_the_band() {
        let results = run(
            &pressure_selection(),
            &inputs(&[("field_bp", 950.0), ("alt_bp", 880.0)]),
        );
        assert!((results[0].value.unwrap() - 950.0).abs() < 1e-12);
    }

    #[test]
    fn test_the_pressure_selection_falls_to_the_altitude_outside_the_band() {
        let results = run(
            &pressure_selection(),
            &inputs(&[("field_bp", 400.0), ("alt_bp", 880.0)]),
        );
        assert!((results[0].value.unwrap() - 880.0).abs() < 1e-12);
    }

    #[test]
    fn test_a_missing_field_pressure_falls_to_the_altitude_rather_than_selecting_a_gap() {
        let results = run(
            &pressure_selection(),
            &inputs(&[("field_bp", f64::NAN), ("alt_bp", 880.0)]),
        );
        assert!((results[0].value.unwrap() - 880.0).abs() < 1e-12);
    }

    /// Alkalinity's `calcEquals`: the measured pH, or the initial one where nothing was measured.
    #[test]
    fn test_coalesce_takes_the_second_value_only_when_the_first_is_missing() {
        let alk = vec![formula(
            "ph",
            1,
            "coalesce(wtw_ph, init_ph)",
            Some("alk_ph"),
            &[("wtw_ph", "wtw_ph_1"), ("init_ph", "alk_init_ph")],
        )];
        let measured = run(&alk, &inputs(&[("wtw_ph", 7.4), ("init_ph", 8.1)]));
        assert!((measured[0].value.unwrap() - 7.4).abs() < 1e-12);
        let unmeasured = run(&alk, &inputs(&[("wtw_ph", f64::NAN), ("init_ph", 8.1)]));
        assert!((unmeasured[0].value.unwrap() - 8.1).abs() < 1e-12);
    }

    /// The portal's two divide-by-zero guards differ and the difference is deliberate: `calcSUVA`
    /// guards only `is.na`, so a zero DOC yields Inf; `calcRatio` also guards the divisor, so a
    /// zero denominator yields NA.
    #[test]
    fn test_the_two_zero_divisor_guards_produce_different_answers() {
        let suva = vec![formula(
            "suva",
            1,
            "a254 / doc * 100",
            Some("suva"),
            &[("a254", "a254"), ("doc", "doc_avg_ppb")],
        )];
        let suva_result = run(&suva, &inputs(&[("a254", 2.0), ("doc", 0.0)]));
        assert_eq!(suva_result[0].value, Some(f64::INFINITY));

        let ratio = vec![formula(
            "c_a",
            1,
            "if(eq(a, 0), na, c / a)",
            Some("dom_c_a"),
            &[("c", "peak_c"), ("a", "peak_a")],
        )];
        let ratio_result = run(&ratio, &inputs(&[("c", 3.0), ("a", 0.0)]));
        assert_eq!(ratio_result[0].value, None, "a zero divisor is NA, not Inf");
    }

    #[test]
    fn test_a_guard_function_is_not_mistaken_for_a_constant() {
        let manifest =
            manifest_json("Alkalinity", None, &pressure_selection()).expect("the set has an order");
        assert!(
            manifest["constants"].as_array().unwrap().is_empty(),
            "if/and/ge/le are the language: {manifest}"
        );
    }

    // --- M109, one unresolved input costs one output ---

    /// DOM's five outputs are one calculation. SUVA reads DOC, which is a separate analysis with
    /// its own turnaround; the four ratios read only the peaks entered in the same row.
    #[test]
    fn test_a_visit_without_doc_still_produces_the_four_ratios() {
        let mut dom_five = vec![formula(
            "suva",
            1,
            "a254 / doc * 100",
            Some("suva"),
            &[("a254", "a254"), ("doc", "doc_avg_ppb")],
        )];
        for (index, (code, output)) in [
            ("c_a", "dom_c_a"),
            ("c_m", "dom_c_m"),
            ("c_t", "dom_c_t"),
            ("t_a", "dom_t_a"),
        ]
        .into_iter()
        .enumerate()
        {
            let (num, den) = code.split_at(1);
            dom_five.push(formula(
                code,
                2 + i32::try_from(index).unwrap(),
                &format!("{num} / {}", den.trim_start_matches('_')),
                Some(output),
                &[
                    (num, &format!("peak_{num}")),
                    (
                        den.trim_start_matches('_'),
                        &format!("peak_{}", den.trim_start_matches('_')),
                    ),
                ],
            ));
        }
        let results = run(
            &dom_five,
            &inputs(&[
                ("a254", 2.0),
                ("a", 4.0),
                ("c", 2.0),
                ("m", 1.0),
                ("t", 8.0),
            ]),
        );
        assert_eq!(results.len(), 5);
        assert_eq!(results.iter().filter(|e| e.value.is_some()).count(), 4);
        let skipped: Vec<&str> = results
            .iter()
            .filter(|e| e.skipped.is_some())
            .map(|e| e.code.as_str())
            .collect();
        assert_eq!(skipped, vec!["suva"]);
    }

    #[test]
    fn test_no_formula_param_is_required_so_one_gap_does_not_refuse_the_calculation() {
        let manifest = manifest_json("DOM", None, &dom()).expect("the set has an order");
        assert!(
            manifest["params"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["required"] == false),
            "a required param refuses the run before any formula is skipped"
        );
    }

    /// Scenario: a stage-1 formula over a three-member replicate family, the shape pCO2, DIC,
    /// Nutrients and Chl a all have.
    #[test]
    fn test_per_replicate_formula_yields_one_value_per_index() {
        let formulas = [per_replicate(
            "co2_hs",
            1,
            "peak * 2",
            Some("CO2_HS"),
            &[("peak", "Peak")],
            "peak",
        )];
        let produced = evaluate_over_replicates(
            &formulas,
            &HashMap::new(),
            &replicates(&[("peak", &[Some(1.0), Some(2.0), Some(3.0)])]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("evaluates");
        assert_eq!(
            produced,
            vec![Produced::PerReplicate {
                code: "co2_hs".to_string(),
                values: vec![Some(2.0), Some(4.0), Some(6.0)],
                curve_slot: None,
            }]
        );
    }

    /// A repeat that was not measured leaves its index empty. Closing the list up would re-label
    /// the third repeat as the second, and the index is the source's column position.
    #[test]
    fn test_a_gap_stays_at_its_own_index() {
        let formulas = [per_replicate(
            "co2_hs",
            1,
            "peak * 2",
            Some("CO2_HS"),
            &[("peak", "Peak")],
            "peak",
        )];
        let produced = evaluate_over_replicates(
            &formulas,
            &HashMap::new(),
            &replicates(&[("peak", &[Some(1.0), None, Some(3.0)])]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("evaluates");
        assert_eq!(
            produced,
            vec![Produced::PerReplicate {
                code: "co2_hs".to_string(),
                values: vec![Some(2.0), None, Some(6.0)],
                curve_slot: None,
            }]
        );
    }

    /// A calculation holding both kinds: the per-replicate formula runs three times, the scalar
    /// one once. A scalar formula reads the group's summary, not one repeat, so reporting it three
    /// times would claim three measurements where there is one.
    #[test]
    fn test_a_scalar_formula_beside_a_per_replicate_one_is_reported_once() {
        let formulas = [
            per_replicate("stage1", 1, "peak * 2", Some("S1"), &[("peak", "Peak")], "peak"),
            formula("stage2", 2, "mean + 1", Some("S2"), &[("mean", "Mean")]),
        ];
        let produced = evaluate_over_replicates(
            &formulas,
            &HashMap::from([("mean".to_string(), 10.0)]),
            &replicates(&[("peak", &[Some(1.0), Some(2.0)])]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("evaluates");
        assert_eq!(produced.len(), 2);
        assert_eq!(
            produced[0],
            Produced::PerReplicate {
                code: "stage1".to_string(),
                values: vec![Some(2.0), Some(4.0)],
                curve_slot: None,
            }
        );
        match &produced[1] {
            Produced::Scalar(evaluated) => {
                assert_eq!(evaluated.code, "stage2");
                assert_eq!(evaluated.value, Some(11.0));
            }
            other => panic!("stage2 is one number: {other:?}"),
        }
    }

    /// Nothing declared per-replicate runs the set once, which is what every calculation written
    /// before this did.
    #[test]
    fn test_a_calculation_with_no_per_replicate_formula_runs_once() {
        let formulas = [formula("only", 1, "a + 1", Some("Only"), &[("a", "A")])];
        let produced = evaluate_over_replicates(
            &formulas,
            &HashMap::from([("a".to_string(), 1.0)]),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("evaluates");
        assert_eq!(produced.len(), 1);
        assert!(matches!(produced[0], Produced::Scalar(_)));
    }

    /// The manifest says which outputs are vectors, so the save path and the grid know the shape
    /// before a run happens.
    #[test]
    fn test_the_manifest_declares_a_per_replicate_output() {
        let formulas = [
            per_replicate("stage1", 1, "peak * 2", Some("S1"), &[("peak", "Peak")], "peak"),
            formula("stage2", 2, "mean + 1", Some("S2"), &[("mean", "Mean")]),
        ];
        let manifest = manifest_json("Two stage", None, &formulas).expect("manifest");
        let outputs = manifest["outputs"].as_array().expect("outputs");
        assert_eq!(outputs[0]["per_replicate"], serde_json::json!(true));
        assert!(outputs[1].get("per_replicate").is_none());

        // The driving variable is the family, declared as the readings it is of, and it is not
        // also an event input: resolving one would put the group's served value into the field
        // that holds the repeats.
        let params = manifest["params"].as_array().expect("params");
        let peak = params.iter().find(|p| p["name"] == "peak").expect("peak");
        assert_eq!(peak["kind"], "replicates");
        assert_eq!(peak["parameter_code"], "Peak");
        let event_inputs = manifest["event_inputs"].as_array().expect("event_inputs");
        assert!(
            event_inputs.iter().all(|e| e["param"] != "peak"),
            "the family is not resolved as one number: {event_inputs:?}"
        );
    }

    /// A scalar formula reading a per-replicate output takes the family's stored mean, not one of
    /// its repeats: the mean is the `samples` trigger's, so the value arrives as an event input
    /// resolved from the store rather than as a hand-off inside the run.
    #[test]
    fn test_a_scalar_formula_reads_a_per_replicate_output_from_the_store() {
        let formulas = [
            per_replicate("stage1", 1, "peak * 2", Some("S1"), &[("peak", "Peak")], "peak"),
            formula("stage2", 2, "s1 + 1", Some("S2"), &[("s1", "S1")]),
        ];
        let manifest = manifest_json("Two stage", None, &formulas).expect("manifest");
        let event_inputs = manifest["event_inputs"].as_array().expect("event_inputs");
        assert!(
            event_inputs
                .iter()
                .any(|e| e["param"] == "s1" && e["parameter_code"] == "S1"),
            "the second stage resolves its input from the store: {event_inputs:?}"
        );

        // The stored mean of [2, 4, 6] is 4, so the second stage is 5 whatever the repeats do.
        let produced = evaluate_over_replicates(
            &formulas,
            &HashMap::from([("s1".to_string(), 4.0)]),
            &replicates(&[("peak", &[Some(1.0), Some(2.0), Some(3.0)])]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("evaluates");
        match &produced[1] {
            Produced::Scalar(evaluated) => assert_eq!(evaluated.value, Some(5.0)),
            other => panic!("the second stage is one number: {other:?}"),
        }
    }

    /// A per-replicate formula reading another per-replicate output takes it at its own index,
    /// which is Chl a's two-stage chain: the second stage runs over a family nobody entered, so its
    /// width is the producer's and an index the first stage skipped stays a gap.
    #[test]
    fn test_a_per_replicate_formula_reads_an_earlier_one_at_its_own_index() {
        let formulas = [
            per_replicate("stage1", 1, "peak * 2", Some("S1"), &[("peak", "Peak")], "peak"),
            per_replicate("stage2", 2, "s1 + 1", Some("S2"), &[("s1", "S1")], "s1"),
        ];
        let manifest = manifest_json("Two stage", None, &formulas).expect("manifest");
        assert!(
            !manifest["event_inputs"]
                .as_array()
                .expect("event_inputs")
                .iter()
                .any(|e| e["param"] == "s1"),
            "the chained stage resolves inside the run, not from the store: {manifest:?}"
        );

        let produced = evaluate_over_replicates(
            &formulas,
            &HashMap::new(),
            &replicates(&[("peak", &[Some(1.0), None, Some(3.0)])]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("evaluates");
        match &produced[1] {
            // 1*2+1, the unmeasured repeat, 3*2+1
            Produced::PerReplicate { values, .. } => {
                assert_eq!(values, &[Some(3.0), None, Some(7.0)]);
            }
            other => panic!("the second stage is one value per index: {other:?}"),
        }
    }
}
