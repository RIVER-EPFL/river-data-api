use super::{
    DerivedGraph, OwnedStep, StepRef, owned_chain, shared_cycle, unread_steps,
    validate_dependency_chain,
};
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

/// Depth is not a limit (Q96): a chain that orders is runnable however deep it runs, four stages
/// included.
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

// --- Declaring a shared step (Q156) ---

const FIELD_DATA: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00f1);
const PCO2: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00f2);

#[test]
fn test_declaring_an_owned_step_releases_it_to_its_previous_owner() {
    // `bp` is `field_data`'s step until `pco2` declares it; then it is nobody's and `field_data`
    // reads it through a declaration of its own.
    assert_eq!(
        super::promotion(Some(FIELD_DATA), PCO2),
        Ok(super::Promotion::Release {
            previous_owner: FIELD_DATA
        })
    );
}

#[test]
fn test_declaring_a_step_that_is_already_shared_only_adds_the_reading() {
    assert_eq!(
        super::promotion(None, PCO2),
        Ok(super::Promotion::AlreadyShared)
    );
}

#[test]
fn test_a_calculation_cannot_declare_its_own_step() {
    let refused = super::promotion(Some(PCO2), PCO2).expect_err("its own step");
    assert!(refused.contains("already computes"), "{refused}");
}

// --- Refused instants, one finding per slot ---

use crate::routes::private::sensor_calibrations::service::{DerivedSlot, SlotPass};
use crate::routes::private::sync::models::HoldKind;

fn pass(site: Uuid, parameter: Uuid, pass: SlotPass) -> DerivedSlot {
    DerivedSlot {
        site_id: site,
        parameter_id: parameter,
        calculation_id: Uuid::from_u128(9),
        pass,
    }
}

fn refusal(site: Uuid, parameter: Uuid) -> DerivedSlot {
    pass(site, parameter, SlotPass::Refused)
}

fn stored(site: Uuid, parameter: Uuid) -> DerivedSlot {
    pass(site, parameter, SlotPass::Stored)
}

fn instant(hour: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 6, 1, hour, 0, 0).unwrap()
}

/// Scenario: an input is corrected so a formula divides by zero over a run of instants.
///
/// Expected behaviour: one finding per slot carrying how many instants refused, keyed on the
/// first of them (Q172), never one finding per instant.
#[test]
fn test_a_run_of_refused_instants_is_one_finding_per_slot() {
    let site = Uuid::from_u128(1);
    let one = Uuid::from_u128(2);
    let two = Uuid::from_u128(3);
    let mut refused = super::DerivedPass::default();
    for hour in [9, 10, 11] {
        refused.record(&[refusal(site, one)], instant(hour));
    }
    refused.record(&[refusal(site, two)], instant(10));

    let holds = refused.holds();
    assert_eq!(holds.len(), 2, "two slots, four instants");
    let first = &holds[0];
    assert_eq!(first.computed["instants"], 3);
    assert_eq!(first.computed["from"], serde_json::json!(instant(9)));
    assert_eq!(first.computed["to"], serde_json::json!(instant(11)));
    assert_eq!(
        first.key,
        crate::routes::private::sync::service::HoldKey::Slot {
            site_id: site,
            parameter_id: one,
            group_time: instant(9),
        },
        "the finding names where the formula stopped computing"
    );
    assert_eq!(holds[1].computed["instants"], 1);
}

/// A pass that refused nothing writes nothing.
#[test]
fn test_a_pass_with_no_refusals_raises_no_finding() {
    assert!(super::DerivedPass::default().holds().is_empty());
}

/// Scenario: the input a divide by zero came from is corrected, and the slot computes again.
///
/// Expected behaviour: the finding standing on that slot is the run's to close. A slot that both
/// stored and refused in the same run keeps its finding: some of its instants still have no value.
#[test]
fn test_a_slot_that_computed_again_closes_its_finding_unless_it_also_refused() {
    let site = Uuid::from_u128(1);
    let repaired = Uuid::from_u128(2);
    let partly = Uuid::from_u128(3);
    let mut run = super::DerivedPass::default();
    run.record(&[stored(site, repaired)], instant(9));
    run.record(&[stored(site, partly)], instant(9));
    run.record(&[refusal(site, partly)], instant(10));

    assert_eq!(run.resolved(), vec![(site, repaired)]);
    assert_eq!(
        run.holds().len(),
        1,
        "the partly refused slot still reports"
    );
}

/// Scenario: a calculation's set cannot be evaluated at all, so every output it publishes stays on
/// the value it last stored.
///
/// Expected behaviour: each output slot raises a finding naming the evaluator's error, as a divide
/// by zero does, and the run does not count the slot as computed again.
#[test]
fn test_an_unevaluable_set_is_a_finding_per_slot_naming_the_error() {
    let site = Uuid::from_u128(1);
    let output = Uuid::from_u128(2);
    let mut run = super::DerivedPass::default();
    for hour in [9, 10] {
        run.record(
            &[pass(
                site,
                output,
                SlotPass::Unevaluable("unknown function 'lg'".to_string()),
            )],
            instant(hour),
        );
    }
    run.record(&[stored(site, output)], instant(11));

    let holds = run.holds();
    assert_eq!(
        holds.len(),
        1,
        "one finding for the slot, not one per instant"
    );
    assert_eq!(holds[0].kind, HoldKind::SkippedOutput);
    assert_eq!(holds[0].computed["instants"], 2);
    let reason = holds[0].expected["reason"].as_str().expect("a reason");
    assert!(reason.contains("unknown function 'lg'"), "{reason}");
    assert!(
        run.resolved().is_empty(),
        "the slot still has instants with no value"
    );
}

/// Scenario: an author renames a calculation's code after the catalog parameter it publishes has
/// been used.
///
/// Expected behaviour: the rename is refused with what is published, and the refusal names the
/// readings count in preference to the project, because that is the larger claim on the code.
mod rename {
    use super::super::rename_refusal;

    #[test]
    fn test_rename_refusal_names_the_stored_readings() {
        let reason = rename_refusal(412, None).expect("stored readings hold the code");
        assert!(reason.contains("412"), "{reason}");
    }

    #[test]
    fn test_rename_refusal_names_the_publishing_project() {
        let reason = rename_refusal(0, Some("BREATHE")).expect("a public slot holds the code");
        assert!(reason.contains("BREATHE"), "{reason}");
    }

    #[test]
    fn test_rename_refusal_names_the_readings_when_both_hold_it() {
        let reason = rename_refusal(7, Some("BREATHE")).expect("both hold the code");
        assert!(
            reason.contains('7') && !reason.contains("BREATHE"),
            "{reason}"
        );
    }

    #[test]
    fn test_rename_refusal_passes_an_unpublished_code() {
        assert!(rename_refusal(0, None).is_none());
    }
}

/// A formula belongs to a calculation or is a step of one (Q231).
mod ownership {
    use super::super::require_owner_or_step;
    use uuid::Uuid;

    #[test]
    fn test_a_formula_owned_by_a_calculation_is_accepted() {
        assert!(require_owner_or_step(Some(Uuid::new_v4()), false).is_ok());
    }

    #[test]
    fn test_a_step_owned_by_no_calculation_is_accepted() {
        assert!(require_owner_or_step(None, true).is_ok());
    }

    #[test]
    fn test_an_output_owned_by_no_calculation_is_refused() {
        let refusal = require_owner_or_step(None, false).expect_err("a kind that is gone");
        assert!(
            format!("{refusal:?}").contains("calculation"),
            "{refusal:?}"
        );
    }
}

mod formula_syntax {
    use super::super::validate_formula;

    #[test]
    fn test_an_expression_that_parses_is_accepted() {
        for formula in ["a + b", "exp(lab_temp / 100) * 2", "(DOC - blank) / 1.05"] {
            assert!(validate_formula(formula).is_ok(), "{formula}");
        }
    }

    #[test]
    fn test_an_expression_that_does_not_parse_is_refused_as_invalid() {
        for formula in ["a +", "(a + b", "a b", ""] {
            let err = validate_formula(formula).expect_err(formula);
            assert!(
                err.to_string().contains("Invalid formula"),
                "{formula}: {err}"
            );
        }
    }
}

fn step(code: &str, owner: Option<Uuid>) -> StepRef {
    StepRef {
        code: code.to_string(),
        owner,
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| (*n).to_string()).collect()
}

#[test]
fn test_a_shared_step_reads_every_other_shared_step() {
    let steps = [step("water_k", None), step("kh", None)];
    let unread = unread_steps(
        &step("kh", None),
        names(&["water_k", "Dissolved_O2"]),
        &steps,
        &[],
    );
    assert_eq!(unread, Ok(names(&["Dissolved_O2"])));
}

#[test]
fn test_a_shared_step_naming_an_owned_step_is_refused_with_it() {
    let pco2real = Uuid::new_v4();
    let steps = [step("water_k", Some(pco2real))];
    let refused = unread_steps(&step("kh", None), names(&["water_k"]), &steps, &[]);
    assert_eq!(refused, Err(step("water_k", Some(pco2real))));
}

#[test]
fn test_an_owned_formula_reads_its_own_and_declared_steps() {
    let (own, other) = (Uuid::new_v4(), Uuid::new_v4());
    let steps = [
        step("mine", Some(own)),
        step("declared", None),
        step("undeclared", None),
        step("theirs", Some(other)),
    ];
    let unread = unread_steps(
        &step("out", Some(own)),
        names(&["mine", "declared", "undeclared", "theirs"]),
        &steps,
        &names(&["declared"]),
    );
    // A step the calculation neither owns nor declares resolves as any other name.
    assert_eq!(unread, Ok(names(&["undeclared", "theirs"])));
}

#[test]
fn test_a_formula_is_no_step_of_its_own() {
    let steps = [step("kh", None)];
    let unread = unread_steps(&step("kh", None), names(&["kh"]), &steps, &[]);
    assert_eq!(unread, Ok(names(&["kh"])));
}

#[test]
fn test_a_chain_of_shared_steps_orders() {
    let steps = [
        ("water_k".to_string(), "Dissolved_O2 + 273.15".to_string()),
        ("kh".to_string(), "0.034 / water_k".to_string()),
    ];
    assert_eq!(shared_cycle(&steps), Ok(()));
}

#[test]
fn test_a_shared_step_cycle_names_its_members() {
    let steps = [
        ("loop_a".to_string(), "loop_b + 1".to_string()),
        ("loop_b".to_string(), "loop_a * 2".to_string()),
        ("apart".to_string(), "Dissolved_O2".to_string()),
    ];
    let err = shared_cycle(&steps).unwrap_err();
    assert!(
        err.contains("loop_a") && err.contains("loop_b") && !err.contains("apart"),
        "{err}"
    );
}

fn owned(code: &str, formula: &str) -> OwnedStep {
    OwnedStep {
        id: Uuid::new_v4(),
        code: code.to_string(),
        formula: formula.to_string(),
    }
}

#[test]
fn test_owned_chain_brings_the_owned_step_a_step_reads() {
    let water_k = owned("water_k", "WTW_Temp_degC_1 + 273.15");
    let steps = [water_k.clone(), owned("unread", "1")];
    let chain = owned_chain("0.034 * exp(c_const * (1 / water_k - 1 / 298.15))", &steps);
    assert_eq!(chain, [water_k.id]);
}

#[test]
fn test_owned_chain_puts_each_step_after_the_steps_it_reads() {
    let a = owned("a", "Dissolved_O2 + 1");
    let b = owned("b", "a * 2");
    let c = owned("c", "b + a");
    let steps = [c.clone(), b.clone(), a.clone()];
    assert_eq!(owned_chain("c / 2", &steps), [a.id, b.id, c.id]);
}

#[test]
fn test_owned_chain_is_empty_for_a_step_reading_no_owned_step() {
    let steps = [owned("water_k", "Dissolved_O2 + 273.15")];
    assert!(owned_chain("shared_k * 2 + Dissolved_O2", &steps).is_empty());
}

#[test]
fn test_owned_chain_names_a_step_reached_twice_once() {
    let base = owned("base", "Dissolved_O2");
    let left = owned("left", "base + 1");
    let right = owned("right", "base - 1");
    let steps = [base.clone(), left.clone(), right.clone()];
    assert_eq!(
        owned_chain("left * right", &steps),
        [base.id, left.id, right.id]
    );
}
