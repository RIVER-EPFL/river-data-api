use super::{DerivedGraph, validate_dependency_chain};
use std::collections::HashMap;
use uuid::Uuid;

/// A definition producing `output`, reading `sources`. Its own code never enters the graph:
/// what it produces is `output_parameter_id`, and the two are routinely spelled differently.
fn graph(definitions: &[(Uuid, Uuid, Vec<Uuid>)]) -> DerivedGraph {
    let mut definition_of = HashMap::new();
    let mut sources_of = HashMap::new();
    for (id, output, sources) in definitions {
        definition_of.insert(*output, *id);
        sources_of.insert(*id, sources.clone());
    }
    DerivedGraph {
        definition_of,
        sources_of,
    }
}

/// A definition is found by what it produces, not by its own code: the walk has to reach a
/// definition whose code and output parameter are spelled differently, which is what B158 was.
#[test]
fn a_definition_is_found_by_what_it_produces() {
    let (definition, output, input) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let g = graph(&[(definition, output, vec![input])]);
    validate_dependency_chain(&g, Some(input), &[("x".to_string(), output)])
        .expect_err("input feeds output, so producing input from output closes the loop");
    validate_dependency_chain(&g, Some(Uuid::new_v4()), &[("x".to_string(), output)])
        .expect("reading a derived parameter is not a cycle");
}

#[test]
fn a_two_definition_cycle_is_refused() {
    let (a, a_out, b, b_out) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    // A reads B's output; the formula under test produces B's output and reads A's.
    let g = graph(&[(a, a_out, vec![b_out]), (b, b_out, vec![a_out])]);
    let err = validate_dependency_chain(&g, Some(b_out), &[("a".to_string(), a_out)]).unwrap_err();
    assert!(err.contains("Circular dependency"), "{err}");
}

#[test]
fn a_formula_reading_its_own_output_is_refused() {
    let output = Uuid::new_v4();
    let err = validate_dependency_chain(&graph(&[]), Some(output), &[("self".to_string(), output)])
        .unwrap_err();
    assert!(err.contains("its own output parameter"), "{err}");
}

/// Depth is not a limit (Q96): a chain that orders is runnable however deep it runs. Four
/// stages is past the cap this used to enforce.
#[test]
fn a_deep_chain_is_allowed() {
    let outputs: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
    let base = Uuid::new_v4();
    let mut definitions = Vec::new();
    let mut below = base;
    for output in &outputs {
        definitions.push((Uuid::new_v4(), *output, vec![below]));
        below = *output;
    }
    let g = graph(&definitions);
    let deepest = *outputs.last().unwrap();
    validate_dependency_chain(&g, Some(Uuid::new_v4()), &[("x".to_string(), deepest)])
        .expect("a five-deep chain orders, so nothing refuses it");
}

#[test]
fn a_definition_with_no_output_parameter_yet_closes_no_cycle() {
    let input = Uuid::new_v4();
    validate_dependency_chain(&graph(&[]), None, &[("x".to_string(), input)]).unwrap();
}

/// A curve slot binds `curve_slope` and `curve_intercept` at evaluation, so neither is a variable
/// to resolve; with no slot to bind them the formula could never be evaluated.
#[test]
fn test_variables_of_holds_back_the_coefficients_a_curve_slot_binds() {
    let formula = "(Vaisala_CO2_min * curve_slope + curve_intercept) * bp";
    let names = super::variables_of(formula, Some("vaisala")).expect("a slot binds them");
    assert_eq!(names, vec!["Vaisala_CO2_min".to_string(), "bp".to_string()]);
}

#[test]
fn test_variables_of_refuses_a_coefficient_with_no_slot() {
    let refused = super::variables_of("raw * curve_slope", None).expect_err("no slot");
    let message = format!("{refused:?}");
    assert!(message.contains("curve_slope"), "{message}");
    assert!(message.contains("curve slot"), "{message}");
}

#[test]
fn test_variables_of_reads_an_empty_slot_as_no_slot() {
    assert!(super::variables_of("raw * curve_intercept", Some("  ")).is_err());
}
