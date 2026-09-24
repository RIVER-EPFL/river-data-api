use super::{check_engine, normalise_name};

#[test]
fn test_a_tool_name_is_lower_cased_and_path_safe() {
    assert_eq!(normalise_name("  DOC  ").expect("trimmed"), "doc");
    assert_eq!(normalise_name("tss_afdm").expect("plain"), "tss_afdm");
    assert!(normalise_name("").is_err());
    assert!(normalise_name("chl a").is_err(), "a space is not a segment");
    assert!(
        normalise_name("co2/air").is_err(),
        "a slash is not a segment"
    );
}

#[test]
fn test_only_the_two_engines_are_accepted() {
    assert!(check_engine("script").is_ok());
    assert!(check_engine("formula").is_ok());
    assert!(check_engine("r").is_err());
}

mod activation_migration {
    use crate::routes::private::tools::service::migration_jobs;
    use uuid::Uuid;

    /// Scenario: an author activates a corrected version of a calculation that has already
    /// produced values at visits.
    #[test]
    fn an_activation_scopes_the_repair_to_the_version_it_replaced() {
        let superseded = Uuid::new_v4();
        let jobs = migration_jobs("pco2", Uuid::new_v4(), Some(superseded));
        let visit = jobs
            .iter()
            .find(|j| j.kind == "event_recompute")
            .expect("the visit arm");
        assert_eq!(visit.params["version"], serde_json::json!(superseded));
        assert_eq!(visit.params["calculation"], "pco2");
        assert_eq!(
            visit.dedupe_key,
            format!("event_recompute:version:{superseded}")
        );
    }

    /// A calculation computes on both arms, and a stream pass names no visit: its values are found
    /// through the calculation, so the stream arm is scoped to it rather than to the version.
    #[test]
    fn the_stream_arm_is_repaired_too_and_is_scoped_to_the_calculation() {
        let superseded = Uuid::new_v4();
        let script_id = Uuid::new_v4();
        let jobs = migration_jobs("pco2", script_id, Some(superseded));
        let stream = jobs
            .iter()
            .find(|j| j.kind == "derived_recompute")
            .expect("the stream arm");
        assert_eq!(
            stream.params["calculation_id"],
            serde_json::json!(script_id)
        );
        assert_eq!(
            stream.dedupe_key,
            format!("derived_recompute:version:{superseded}"),
            "one enqueue per arm per save"
        );
        assert_eq!(jobs.len(), 2, "one job per arm, and no more");
    }

    /// Scenario: an admin edits constant k from 2.0 to 3.0, and one formula calculation reads it.
    #[test]
    fn a_constant_edit_repairs_the_visits_and_the_stream_values_computed_with_it() {
        use crate::routes::private::tools::service::constant_edit_jobs;

        let constant = Uuid::new_v4();
        let calculation = Uuid::new_v4();
        let jobs = constant_edit_jobs(constant, "k", 2.0, 3.0, &[calculation]);
        let visit = jobs
            .iter()
            .find(|j| j.kind == "event_recompute")
            .expect("the visit arm");
        assert_eq!(visit.trigger_id, Some(constant));
        assert_eq!(
            visit.params,
            serde_json::json!({ "constant": "k", "previous_value": 2.0, "value": 3.0 })
        );
        assert_eq!(visit.dedupe_key, "event_recompute:constant:k");
        let stream = jobs
            .iter()
            .find(|j| j.kind == "derived_recompute")
            .expect("the stream arm");
        assert_eq!(stream.trigger_id, Some(calculation));
        assert_eq!(
            stream.params["calculation_id"],
            serde_json::json!(calculation)
        );
        assert_eq!(
            stream.dedupe_key,
            format!("derived_recompute:constant:k:{calculation}")
        );
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn a_constant_no_formula_reads_repairs_the_visits_only() {
        use crate::routes::private::tools::service::constant_edit_jobs;

        let jobs = constant_edit_jobs(Uuid::new_v4(), "k", 2.0, 3.0, &[]);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "event_recompute");
    }

    #[test]
    fn a_formula_reads_a_constant_only_where_no_source_variable_binds_the_name() {
        use crate::routes::private::tools::models::PinnedFormula;
        use crate::routes::private::tools::service::formulas_read_constant;

        let formula = |expr: &str, sources: &[&str]| PinnedFormula {
            code: "out".to_string(),
            label: "Out".to_string(),
            units: None,
            formula: expr.to_string(),
            ordinal: 0,
            output_parameter_code: Some("Out".to_string()),
            sources: sources
                .iter()
                .map(|v| ((*v).to_string(), "Source".to_string()))
                .collect(),
            held: Vec::new(),
            site_sources: Vec::new(),
            curve_slot: None,
            per_replicate: None,
            intermediate: false,
        };
        assert!(formulas_read_constant(&[formula("x * k", &["x"])], "k"));
        assert!(!formulas_read_constant(&[formula("x * 2", &["x"])], "k"));
        assert!(
            !formulas_read_constant(&[formula("x * k", &["x", "k"])], "k"),
            "a variable named k is a source, not the constant"
        );
    }

    /// A first activation replaces no version, so there is nothing to migrate.
    #[test]
    fn a_first_activation_supersedes_nothing() {
        assert!(migration_jobs("pco2", Uuid::new_v4(), None).is_empty());
    }
}

#[test]
fn a_switched_off_calculation_is_not_run_by_name() {
    use super::admit_run;
    use crate::error::AppError;

    assert!(admit_run("pco2", true).is_ok());
    match admit_run("pco2", false) {
        Err(AppError::Conflict(message)) => {
            assert_eq!(message, "Calculation 'pco2' is switched off");
        }
        other => panic!("a switched-off calculation is refused by name: {other:?}"),
    }
}

#[test]
fn a_decommission_records_a_trimmed_reason_and_refuses_a_blank_one() {
    use super::decommission_reason;

    assert_eq!(
        decommission_reason("  replaced by pco2_v2 ").expect("a reason"),
        "replaced by pco2_v2"
    );
    assert!(decommission_reason("").is_err());
    assert!(decommission_reason("   ").is_err(), "whitespace is blank");
}

#[test]
fn a_decommissioned_calculation_is_refused_naming_the_decommission() {
    use super::refuse_decommissioned;
    use crate::error::AppError;

    assert!(refuse_decommissioned("pco2", None).is_ok());
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-23T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    match refuse_decommissioned("pco2", Some(at)) {
        Err(AppError::Conflict(message)) => assert_eq!(
            message,
            "Calculation 'pco2' was decommissioned at 2026-09-23T10:00:00+00:00"
        ),
        other => panic!("a decommissioned calculation is refused: {other:?}"),
    }
}

#[test]
fn test_decommissioned_name_frees_the_name_and_counts_on_when_taken() {
    use super::decommissioned_name;
    let on = chrono::NaiveDate::from_ymd_opt(2026, 9, 24).unwrap();
    let none = std::collections::HashSet::new();
    assert_eq!(
        decommissioned_name("pco2", on, &none),
        "pco2_decommissioned_20260924"
    );
    let taken: std::collections::HashSet<String> = [
        "pco2_decommissioned_20260924".to_string(),
        "pco2_decommissioned_20260924_2".to_string(),
    ]
    .into();
    assert_eq!(
        decommissioned_name("pco2", on, &taken),
        "pco2_decommissioned_20260924_3"
    );
}

use super::{FormulaWrite, codes_held_elsewhere, plan_formula_set, steps_taken_back};

fn id(n: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(n)
}

fn row(n: u128, code: &str) -> (uuid::Uuid, String) {
    (id(n), code.to_string())
}

fn kept(n: u128, code: &str) -> (Option<uuid::Uuid>, String) {
    (Some(id(n)), code.to_string())
}

fn fresh(code: &str) -> (Option<uuid::Uuid>, String) {
    (None, code.to_string())
}

#[test]
fn test_a_set_save_updates_what_it_names_and_deletes_what_it_leaves_out() {
    let stored = [row(1, "a"), row(2, "b"), row(3, "c")];
    let writes =
        plan_formula_set(&stored, &[kept(2, "b"), fresh("d"), kept(1, "a")]).expect("planned");
    assert_eq!(
        writes,
        vec![
            FormulaWrite::Delete(id(3)),
            FormulaWrite::Update(id(2), 0),
            FormulaWrite::Create(1),
            FormulaWrite::Update(id(1), 2),
        ],
        "the row nobody named is deleted, and the deletes come before the writes that may reuse \
         its code"
    );
}

#[test]
fn test_a_replaced_formula_keeps_its_row_when_the_code_is_the_same() {
    // Scenario: the author rewrites one formula in the editor and saves the set without its id.
    // Expected behaviour: the output the code names keeps its row, so the catalog parameter it
    // mints is never released and re-claimed.
    let stored = [row(1, "temp_ratio_out")];
    assert_eq!(
        plan_formula_set(&stored, &[fresh("temp_ratio_out")]).expect("planned"),
        vec![FormulaWrite::Update(id(1), 0)]
    );
}

#[test]
fn test_a_dropped_code_taken_by_a_new_formula_pairs_one_to_one() {
    let stored = [row(1, "x"), row(2, "x"), row(3, "y")];
    assert_eq!(
        plan_formula_set(&stored, &[fresh("x"), fresh("z")]).expect("planned"),
        vec![
            FormulaWrite::Delete(id(2)),
            FormulaWrite::Delete(id(3)),
            FormulaWrite::Update(id(1), 0),
            FormulaWrite::Create(1),
        ],
        "one dropped row takes one new row of its code; the second keeps its delete"
    );
}

#[test]
fn test_a_code_moved_onto_a_named_formula_still_deletes_the_row_it_left() {
    let stored = [row(1, "x"), row(2, "y")];
    assert_eq!(
        plan_formula_set(&stored, &[kept(2, "x")]).expect("planned"),
        vec![FormulaWrite::Delete(id(1)), FormulaWrite::Update(id(2), 0)],
        "the code is free by the time the update needs it"
    );
}

#[test]
fn test_an_empty_set_deletes_every_formula() {
    let stored = [row(1, "a"), row(2, "b")];
    assert_eq!(
        plan_formula_set(&stored, &[]).expect("planned"),
        vec![FormulaWrite::Delete(id(1)), FormulaWrite::Delete(id(2))]
    );
}

#[test]
fn test_a_first_save_creates_every_formula() {
    assert_eq!(
        plan_formula_set(&[], &[fresh("a"), fresh("b")]).expect("planned"),
        vec![FormulaWrite::Create(0), FormulaWrite::Create(1)]
    );
}

#[test]
fn test_a_set_save_refuses_a_formula_of_another_calculation() {
    let err = plan_formula_set(&[row(1, "a")], &[kept(9, "a")]).expect_err("refused");
    assert!(
        err.contains(&id(9).to_string()),
        "the error names it: {err}"
    );
}

#[test]
fn test_a_set_save_refuses_the_same_formula_twice() {
    let err = plan_formula_set(&[row(1, "a"), row(2, "b")], &[kept(1, "a"), kept(1, "a")])
        .expect_err("refused");
    assert!(err.contains("twice"), "{err}");
}

#[test]
fn test_codes_held_elsewhere_names_each_taken_code_and_its_calculation() {
    let held = [
        (
            "co2sheet_lab_temp_k".to_string(),
            "co2_ch4_sheet".to_string(),
        ),
        ("ch4sheet_ppm".to_string(), "co2_ch4_sheet".to_string()),
    ];
    assert_eq!(
        codes_held_elsewhere(&held),
        Err(
            "co2sheet_lab_temp_k is a formula of co2_ch4_sheet; ch4sheet_ppm is a formula of \
             co2_ch4_sheet; a formula code is unique across calculations"
                .to_string()
        )
    );
}

#[test]
fn test_codes_held_elsewhere_passes_a_set_no_other_calculation_holds() {
    assert_eq!(codes_held_elsewhere(&[]), Ok(()));
}

#[test]
fn test_a_step_declared_here_and_named_by_the_set_is_taken_back() {
    let declared = [(id(5), "this_set".to_string())];
    assert_eq!(
        steps_taken_back("this_set", &[id(1), id(5)], &declared).expect("taken back"),
        vec![id(5)]
    );
}

#[test]
fn test_a_set_naming_no_declared_step_takes_nothing_back() {
    let declared = [(id(5), "this_set".to_string())];
    assert!(
        steps_taken_back("this_set", &[id(1)], &declared)
            .expect("nothing")
            .is_empty()
    );
}

#[test]
fn test_a_step_another_calculation_still_reads_is_not_taken_back() {
    let declared = [
        (id(5), "this_set".to_string()),
        (id(5), "other_set".to_string()),
    ];
    let err = steps_taken_back("this_set", &[id(5)], &declared).expect_err("refused");
    assert!(
        err.contains("other_set"),
        "the error names the reader: {err}"
    );
}

use super::{carry_renames, reads_of_former, renames_of};
use uuid::Uuid;

#[test]
fn test_renames_of_names_each_formula_whose_code_moved() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let stored = [(a, "CO2_HS_Um".to_string()), (b, "pCO2".to_string())];
    let named = [
        (Some(a), "CO2_HS_Um2".to_string()),
        (Some(b), "pCO2".to_string()),
        (None, "fresh".to_string()),
    ];
    assert_eq!(
        renames_of(&stored, &named),
        [("CO2_HS_Um".to_string(), "CO2_HS_Um2".to_string())]
    );
}

#[test]
fn test_carry_renames_rewrites_whole_identifiers_only() {
    let renames = [("CO2_HS_Um".to_string(), "CO2_HS_Um2".to_string())];
    assert_eq!(
        carry_renames("CO2_HS_Um / kh + CO2_HS_Um_sd + xCO2_HS_Um", &renames),
        "CO2_HS_Um2 / kh + CO2_HS_Um_sd + xCO2_HS_Um"
    );
    assert_eq!(
        carry_renames("mean(CO2_HS_Um)*2", &renames),
        "mean(CO2_HS_Um2)*2"
    );
}

#[test]
fn test_carry_renames_swaps_two_codes_in_one_pass() {
    let renames = [
        ("a".to_string(), "b".to_string()),
        ("b".to_string(), "a".to_string()),
    ];
    // a - b, each carried once
    assert_eq!(carry_renames("a - b", &renames), "b - a");
}

#[test]
fn test_carry_renames_leaves_a_number_alone() {
    let renames = [("e5".to_string(), "e6".to_string())];
    assert_eq!(carry_renames("2e5 * e5", &renames), "2e5 * e6");
}

#[test]
fn test_reads_of_former_names_a_reader_of_a_formula_the_set_dropped() {
    let former = ["CO2_HS_Um".to_string()];
    let readers = [
        ("pCO2", "CO2_HS_Um / kh", None),
        ("other", "x * 2", Some("CO2_HS_Um")),
        ("clean", "kh * 2", None),
    ];
    assert_eq!(
        reads_of_former(&readers, &former),
        [
            ("pCO2".to_string(), "CO2_HS_Um".to_string()),
            ("other".to_string(), "CO2_HS_Um".to_string()),
        ]
    );
}

#[test]
fn test_reads_of_former_is_empty_when_nothing_was_dropped() {
    let readers = [("pCO2", "CO2_HS_Um / kh", None)];
    assert!(reads_of_former(&readers, &[]).is_empty());
}
