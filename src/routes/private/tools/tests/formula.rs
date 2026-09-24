use crate::routes::private::tools::models::*;
use crate::routes::private::tools::service::*;

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
        held: Vec::new(),
        site_sources: Vec::new(),
        curve_slot: None,
        per_replicate: None,
        intermediate: false,
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

/// A step reaches the formulas after it under its own code, never through an output parameter, so
/// the only thing that can order it is the identifier the reading formula names. Both are at the
/// ordinal the authoring form creates, and the step's code sorts last.
fn step_named_after_its_reader() -> Vec<PinnedFormula> {
    let mut zz = formula("zz", 0, "field_bp * 1.0", None, &[("field_bp", "Field_BP")]);
    zz.intermediate = true;
    let aa = formula("aa", 0, "zz * 2", Some("aa"), &[]);
    vec![zz, aa]
}

#[test]
fn test_a_step_orders_ahead_of_the_formula_that_names_it() {
    assert_eq!(codes(&step_named_after_its_reader()), vec!["zz", "aa"]);
}

/// The step edge is on the identifier, so the case it is written in does not decide the order.
#[test]
fn test_a_step_is_ordered_whatever_case_its_reader_names_it_in() {
    let mut step = formula("Zz", 0, "field_bp * 1.0", None, &[("field_bp", "Field_BP")]);
    step.intermediate = true;
    let reader = formula("aa", 0, "ZZ * 2", Some("aa"), &[]);
    assert_eq!(codes(&[step, reader]), vec!["Zz", "aa"]);
}

/// A step nothing reads is still a formula of the set, ordered by the tie-break like any other.
#[test]
fn test_an_unread_step_keeps_the_ordinal_tie_break() {
    let mut step = formula("zz", 0, "field_bp * 1.0", None, &[("field_bp", "Field_BP")]);
    step.intermediate = true;
    let other = formula(
        "aa",
        0,
        "field_bp * 3",
        Some("aa"),
        &[("field_bp", "Field_BP")],
    );
    assert_eq!(codes(&[step, other]), vec!["aa", "zz"]);
}

/// Two steps reading each other have no runnable order, and the cycle is named rather than
/// silently dropped, the same way a parameter cycle is.
#[test]
fn test_two_steps_reading_each_other_are_a_cycle() {
    let mut first = formula("aa", 0, "bb + 1", None, &[]);
    first.intermediate = true;
    let mut second = formula("bb", 0, "aa + 1", None, &[]);
    second.intermediate = true;
    let err = in_order(&[first, second]).expect_err("a cycle has no order");
    assert!(err.contains("aa") && err.contains("bb"), "{err}");
}

/// Scenario: nutrients, `NUT_NO3_avg = NUT_NOx_avg - NUT_NO2_avg`, walked at the same letter.
/// Expected behaviour: the group's declaration is what makes the second source the family. Without
/// it the source is a number, which resolves to the mean and averages the family away.
#[test]
fn test_a_second_family_is_the_family_only_where_the_group_declares_it() {
    let nutrients = vec![per_replicate(
        "NUT_NO3_avg",
        0,
        "NUT_NOx_avg - NUT_NO2_avg",
        Some("NUT_NO3_avg"),
        &[
            ("NUT_NOx_avg", "NUT_NOx_avg"),
            ("NUT_NO2_avg", "NUT_NO2_avg"),
        ],
        "NUT_NOx_avg",
    )];
    let kind_of = |manifest: &serde_json::Value, name: &str| {
        manifest["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("{name} is declared"))["kind"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let declared = manifest_json(
        "Nutrients",
        None,
        &nutrients,
        &["NUT_NOx_avg".to_string(), "NUT_NO2_avg".to_string()],
    )
    .expect("the set has an order");
    assert_eq!(kind_of(&declared, "NUT_NOx_avg"), "replicates");
    assert_eq!(kind_of(&declared, "NUT_NO2_avg"), "replicates");
    assert!(
        declared["event_inputs"].as_array().unwrap().is_empty(),
        "a family is not an event input as well: {declared:?}"
    );

    let undeclared = manifest_json("Nutrients", None, &nutrients, &["NUT_NOx_avg".to_string()])
        .expect("the set has an order");
    assert_eq!(kind_of(&undeclared, "NUT_NO2_avg"), "number");
}

#[test]
fn test_a_produced_value_is_not_declared_an_event_input_whatever_the_codes_sort_to() {
    let manifest = manifest_json("Carbonate", None, &shared_ordinal_chain(), &[])
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
    let manifest = manifest_json("DOM", None, &dom(), &[]).expect("the set has an order");
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

/// Scenario: pCO2's shape, where the pressure choice is a step of the arithmetic and only the
/// value after it is a measurement of anything.
///
/// Expected behaviour: the intermediate is evaluated and reported under its own code, the
/// formula after it reads it as a variable of that name, and the manifest offers only the
/// output that saves.
#[test]
fn test_an_intermediate_feeds_the_next_formula_and_is_no_output() {
    let mut bp = formula("bp", 1, "field_bp * 1.0", None, &[("field_bp", "Field_BP")]);
    bp.intermediate = true;
    let pco2 = formula("pco2", 2, "bp * 2", Some("pCO2_HS_uatm"), &[]);
    let set = vec![bp, pco2];

    let manifest = manifest_json("pCO2", None, &set, &[]).expect("the set has an order");
    let outputs = manifest["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 1, "only what saves is an output: {manifest}");
    assert_eq!(outputs[0]["key"], "pco2");

    let inputs = HashMap::from([("field_bp".to_string(), 950.0)]);
    let results = evaluate(&set, &inputs, &HashMap::new(), &HashMap::new()).expect("evaluates");
    assert_eq!(results.len(), 2, "the run reports the step too");
    assert_eq!(results[0].code, "bp");
    assert_eq!(results[0].value, Some(950.0));
    assert_eq!(
        results[1].value,
        Some(1900.0),
        "the formula after it read the step by its own code"
    );
}

/// Scenario: a formula reads the station's elevation, which is a column of the site row and
/// not a parameter anything measured.
/// Expected behaviour: the manifest declares it a site input, so the existing resolution fills
/// it from the `sites` row at calculate time, and it is not asked for at the event.
#[test]
fn test_a_site_source_is_a_site_input_and_not_an_event_input() {
    let mut bp = formula(
        "field_bp_altitude",
        1,
        "1013.25 * exp(-elevation / 8434.5)",
        Some("field_bp_altitude"),
        &[],
    );
    bp.site_sources = vec![("elevation".to_string(), "elevation".to_string())];
    let manifest = manifest_json("Field Data", None, &[bp], &[]).expect("the set has an order");

    let site_inputs = manifest["site_inputs"].as_array().unwrap();
    assert_eq!(site_inputs.len(), 1, "{site_inputs:?}");
    assert_eq!(site_inputs[0]["property"], "elevation");
    assert_eq!(site_inputs[0]["param"], "elevation");
    assert_eq!(
        site_inputs[0]["required"], true,
        "a formula cannot evaluate without it, and a blank one would silently produce nothing"
    );

    assert!(
        manifest["event_inputs"]
            .as_array()
            .is_none_or(|e| e.is_empty()),
        "the site row holds it, so nothing reads it at the event: {:?}",
        manifest["event_inputs"]
    );

    let params = manifest["params"].as_array().unwrap();
    assert_eq!(params.len(), 1, "{params:?}");
    assert_eq!(params[0]["name"], "elevation");
    assert_eq!(params[0]["kind"], "number");
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
    let manifest = manifest_json("DOM", None, &chained, &[]).expect("the set has an order");
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
    let manifest = manifest_json("pCO2", None, &pco2(), &[]).expect("the set has an order");
    let declared = manifest["constants"].as_array().unwrap();
    assert_eq!(declared.len(), 2);
    assert_eq!(declared[0], "gas_const_r_atm");
    assert_eq!(declared[1], "lab_temp_avg_degC");
}

#[test]
fn test_a_source_variable_is_not_declared_as_a_constant() {
    let manifest = manifest_json("DOM", None, &dom(), &[]).expect("the set has an order");
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
    let manifest = manifest_json("Chl a", None, &chla(), &[]).expect("the set has an order");
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

/// Scenario: the visit holds no field pressure at all, rather than a null cell bound as NaN.
/// Expected behaviour: the variable is read only through guards, so it binds as NaN and the
/// selection falls to the altitude instead of the formula skipping.
#[test]
fn test_an_absent_field_pressure_falls_to_the_altitude() {
    let results = run(&pressure_selection(), &inputs(&[("alt_bp", 880.0)]));
    assert_eq!(results[0].skipped, None);
    assert!((results[0].value.unwrap() - 880.0).abs() < 1e-12);
}

/// pCO2's `labTemp` default: the visit's lab temperature, the constant where the visit holds none.
#[test]
fn test_an_absent_coalesce_argument_takes_the_second() {
    let lab_temp = vec![formula(
        "lab_temp_k",
        1,
        "coalesce(lab_co2_lab_temp, lab_temp_avg_degC) + 273.15",
        Some("lab_temp_k"),
        &[("lab_co2_lab_temp", "lab_co2_lab_temp")],
    )];
    let results = evaluate(
        &lab_temp,
        &inputs(&[]),
        &constants(&[("lab_temp_avg_degC", 22.5)]),
        &curves(&[]),
    )
    .expect("the set evaluates");
    assert_eq!(results[0].skipped, None);
    assert!((results[0].value.unwrap() - 295.65).abs() < 1e-12);
}

/// A source read outside every guard still skips: the formula has no arithmetic to do without it.
#[test]
fn test_an_absent_unguarded_source_still_skips() {
    let suva = vec![formula(
        "suva",
        1,
        "a254 * 1000 / doc",
        Some("suva"),
        &[("a254", "a254"), ("doc", "doc_avg_ppb")],
    )];
    let results = run(&suva, &inputs(&[("a254", 2.0)]));
    assert_eq!(results[0].value, None);
    assert!(results[0].skipped.as_deref().unwrap().contains("doc"));
}

#[test]
fn test_a_variable_read_both_inside_and_outside_a_guard_is_not_guarded() {
    assert!(read_only_through_guards("coalesce(a / 1013.25, b)", "a"));
    assert!(read_only_through_guards("if(is_missing(a), b, a)", "a"));
    assert!(!read_only_through_guards("coalesce(a, b) + a", "a"));
    assert!(!read_only_through_guards("a * coalesce(b, 1)", "a"));
    assert!(!read_only_through_guards("round(a)", "a"));
    assert!(!read_only_through_guards("coalesce(b, 1)", "a"));
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
    assert_eq!(suva_result[0].value, None);
    assert!(suva_result[0].refused, "Inf is refused, not stored");
    assert!(
        suva_result[0]
            .skipped
            .as_deref()
            .unwrap()
            .contains("not a finite number")
    );

    let ratio = vec![formula(
        "c_a",
        1,
        "if(eq(a, 0), na, c / a)",
        Some("dom_c_a"),
        &[("c", "peak_c"), ("a", "peak_a")],
    )];
    let ratio_result = run(&ratio, &inputs(&[("c", 3.0), ("a", 0.0)]));
    assert_eq!(ratio_result[0].value, None, "a zero divisor is NA, not Inf");
    assert!(!ratio_result[0].refused, "NA is a clear, not a refusal");
}

/// Scenario: DOC entered as 0 at a visit, the SUVA shape the portal's R computes as Inf.
/// Expected behaviour: the output is refused (Q172). It reaches neither the result map, where a
/// value would overwrite the stored reading, nor the cleared list, which withdraws it; the reason
/// travels as a skip. `serde_json` maps a non-finite float to null, so the whole distinction is
/// lost unless the engine settles it before the JSON boundary.
#[test]
fn test_a_non_finite_result_is_neither_stored_nor_cleared() {
    let suva = vec![formula(
        "suva",
        1,
        "a254 / doc * 100",
        Some("suva"),
        &[("a254", "a254"), ("doc", "doc_avg_ppb")],
    )];
    let (produced, _) = evaluate_with_trace(
        &suva,
        &inputs(&[("a254", 2.0), ("doc", 0.0)]),
        &replicates(&[]),
        &constants(&[]),
        &curves(&[]),
    )
    .expect("the set evaluates");
    let (mut results, skipped, refused) = collect_produced(produced);
    assert_eq!(refused, vec!["suva".to_string()]);
    assert_eq!(skipped[0]["output"], "suva");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("not a finite number")
    );
    assert!(
        !results.contains_key("suva"),
        "nothing to store: {results:?}"
    );
    assert!(
        partition_cleared(&mut results).is_empty(),
        "a refusal withdraws nothing"
    );
}

/// The NA the same boundary must keep clearing: an explicit null is the portal's blanked column.
#[test]
fn test_an_na_result_still_clears_the_stored_value() {
    let ratio = vec![formula(
        "c_a",
        1,
        "if(eq(a, 0), na, c / a)",
        Some("dom_c_a"),
        &[("c", "peak_c"), ("a", "peak_a")],
    )];
    let (produced, _) = evaluate_with_trace(
        &ratio,
        &inputs(&[("c", 3.0), ("a", 0.0)]),
        &replicates(&[]),
        &constants(&[]),
        &curves(&[]),
    )
    .expect("the set evaluates");
    let (mut results, skipped, refused) = collect_produced(produced);
    assert!(refused.is_empty());
    assert!(skipped.is_empty());
    assert_eq!(partition_cleared(&mut results), vec!["c_a".to_string()]);
}

/// A formula reading a refused output has no value to read, so it skips with it rather than
/// evaluating against a stale number, and the outputs beside it are still produced.
#[test]
fn test_a_formula_reading_a_refused_output_skips_with_it() {
    let mut set = vec![formula(
        "suva",
        1,
        "a254 / doc * 100",
        Some("suva"),
        &[("a254", "a254"), ("doc", "doc_avg_ppb")],
    )];
    set.push(formula(
        "suva_scaled",
        2,
        "suva * 2",
        Some("suva_scaled"),
        &[("suva", "suva")],
    ));
    set.push(formula(
        "c_a",
        3,
        "c / a",
        Some("dom_c_a"),
        &[("c", "peak_c"), ("a", "peak_a")],
    ));
    let results = run(
        &set,
        &inputs(&[("a254", 2.0), ("doc", 0.0), ("c", 3.0), ("a", 1.5)]),
    );
    let scaled = results
        .iter()
        .find(|e| e.code == "suva_scaled")
        .expect("the output is named");
    assert_eq!(scaled.value, None);
    assert!(!scaled.refused, "it never evaluated: it skipped");
    assert!(scaled.skipped.as_deref().unwrap().contains("suva"));
    assert_eq!(value_of(&results, "c_a"), Some(2.0));
}

#[test]
fn test_a_guard_function_is_not_mistaken_for_a_constant() {
    let manifest = manifest_json("Alkalinity", None, &pressure_selection(), &[])
        .expect("the set has an order");
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
    let manifest = manifest_json("DOM", None, &dom(), &[]).expect("the set has an order");
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
    let (produced, _) = evaluate_with_trace(
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
    let (produced, _) = evaluate_with_trace(
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
        per_replicate(
            "stage1",
            1,
            "peak * 2",
            Some("S1"),
            &[("peak", "Peak")],
            "peak",
        ),
        formula("stage2", 2, "mean + 1", Some("S2"), &[("mean", "Mean")]),
    ];
    let (produced, _) = evaluate_with_trace(
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
    let (produced, _) = evaluate_with_trace(
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
        per_replicate(
            "stage1",
            1,
            "peak * 2",
            Some("S1"),
            &[("peak", "Peak")],
            "peak",
        ),
        formula("stage2", 2, "mean + 1", Some("S2"), &[("mean", "Mean")]),
    ];
    let manifest = manifest_json("Two stage", None, &formulas, &[]).expect("manifest");
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
        per_replicate(
            "stage1",
            1,
            "peak * 2",
            Some("S1"),
            &[("peak", "Peak")],
            "peak",
        ),
        formula("stage2", 2, "s1 + 1", Some("S2"), &[("s1", "S1")]),
    ];
    let manifest = manifest_json("Two stage", None, &formulas, &[]).expect("manifest");
    let event_inputs = manifest["event_inputs"].as_array().expect("event_inputs");
    assert!(
        event_inputs
            .iter()
            .any(|e| e["param"] == "s1" && e["parameter_code"] == "S1"),
        "the second stage resolves its input from the store: {event_inputs:?}"
    );

    // The stored mean of [2, 4, 6] is 4, so the second stage is 5 whatever the repeats do.
    let (produced, _) = evaluate_with_trace(
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
        per_replicate(
            "stage1",
            1,
            "peak * 2",
            Some("S1"),
            &[("peak", "Peak")],
            "peak",
        ),
        per_replicate("stage2", 2, "s1 + 1", Some("S2"), &[("s1", "S1")], "s1"),
    ];
    let manifest = manifest_json("Two stage", None, &formulas, &[]).expect("manifest");
    assert!(
        !manifest["event_inputs"]
            .as_array()
            .expect("event_inputs")
            .iter()
            .any(|e| e["param"] == "s1"),
        "the chained stage resolves inside the run, not from the store: {manifest:?}"
    );

    let (produced, _) = evaluate_with_trace(
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

/// Scenario: pCO2's shape, a scalar step feeding a per-replicate stage that a second
/// per-replicate stage reads at its own index.
/// Expected behaviour: the trace names, per cell, the formula and the values it read, so the
/// stage-2 cell at index 1 shows the stage-1 value at index 1 and the scalar, never index 0.
#[test]
fn test_the_trace_records_what_each_cell_read() {
    let mut bp = formula("bp", 1, "field_bp * 1.0", None, &[("field_bp", "Field_BP")]);
    bp.intermediate = true;
    let formulas = [
        bp,
        per_replicate(
            "stage1",
            2,
            "peak * bp",
            Some("S1"),
            &[("peak", "Peak")],
            "peak",
        ),
        per_replicate("stage2", 3, "s1 + k", Some("S2"), &[("s1", "S1")], "s1"),
    ];
    let (produced, trace) = evaluate_with_trace(
        &formulas,
        &HashMap::from([("field_bp".to_string(), 2.0)]),
        &replicates(&[("peak", &[Some(1.0), Some(3.0)])]),
        &constants(&[("k", 10.0)]),
        &HashMap::new(),
    )
    .expect("evaluates");
    assert_eq!(produced.len(), 3);
    assert_eq!(trace.len(), 3, "one step per formula, in order");

    let step = &trace[0];
    assert!(step.intermediate);
    assert!(!step.per_replicate);
    assert_eq!(step.formula, "field_bp * 1.0");
    assert_eq!(step.cells.len(), 1, "a scalar formula is one cell");
    assert_eq!(step.cells[0].index, None);
    assert_eq!(step.cells[0].value, Some(2.0));
    assert_eq!(step.cells[0].bindings.get("field_bp"), Some(&2.0));

    let stage2 = &trace[2];
    assert!(stage2.per_replicate);
    assert_eq!(stage2.cells.len(), 2, "one cell per index");
    let at_b = &stage2.cells[1];
    assert_eq!(at_b.index, Some(1));
    // 3 * 2 + 10
    assert_eq!(at_b.value, Some(16.0));
    assert_eq!(
        at_b.bindings.get("s1"),
        Some(&6.0),
        "the stage-1 value at the same index"
    );
    assert_eq!(at_b.bindings.get("k"), Some(&10.0));
    assert_eq!(
        at_b.bindings.len(),
        2,
        "only what the formula names: {:?}",
        at_b.bindings
    );
}

/// A skipped cell carries its reason and no bindings.
#[test]
fn test_a_skipped_cell_traces_its_reason() {
    let formulas = [per_replicate(
        "stage1",
        1,
        "peak * 2",
        Some("S1"),
        &[("peak", "Peak")],
        "peak",
    )];
    let (_, trace) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("peak", &[Some(1.0), None])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let gap = &trace[0].cells[1];
    assert_eq!(gap.value, None);
    assert!(
        gap.skipped.as_deref().is_some_and(|r| r.contains("peak")),
        "{gap:?}"
    );
    assert!(gap.bindings.is_empty());
}

/// Scenario: the alternating shape a sheet writes by hand, a mean over the entered family, a
/// per-replicate stage dividing by it, and an sd over that stage's own results.
/// Expected behaviour: each reducer sees a finished vector, so the set evaluates in one pass and
/// the sd is the sample sd of the three ratios.
#[test]
fn test_a_reducer_reads_a_vector_the_same_run_produced() {
    let mut m = formula("m", 1, "mean(x)", None, &[("x", "X")]);
    m.intermediate = true;
    let formulas = [
        m,
        per_replicate("y", 2, "x / m", Some("Y"), &[("x", "X")], "x"),
        formula("spread", 3, "sd(y)", Some("Spread"), &[("y", "Y")]),
    ];
    let (produced, _) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("x", &[Some(2.0), Some(4.0), Some(6.0)])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    match &produced[1] {
        // 2/4, 4/4, 6/4
        Produced::PerReplicate { values, .. } => {
            assert_eq!(values, &[Some(0.5), Some(1.0), Some(1.5)]);
        }
        other => panic!("the stage is one value per index: {other:?}"),
    }
    let Produced::Scalar(spread) = &produced[2] else {
        panic!("the sd is one number: {:?}", produced[2]);
    };
    // sd(0.5, 1.0, 1.5) over n-1
    assert!(
        (spread.value.expect("a value") - 0.5).abs() < 1e-12,
        "{spread:?}"
    );
}

/// A repeat the visit did not measure is no member: the statistics are over what was entered.
#[test]
fn test_a_gap_is_not_a_member_of_the_reduction() {
    let formulas = [formula("avg", 1, "mean(x)", Some("Avg"), &[("x", "X")])];
    let (_, trace) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("x", &[Some(2.0), None, Some(4.0)])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let cell = &trace[0].cells[0];
    assert_eq!(cell.value, Some(3.0));
    let reduction = &cell.reductions[0];
    assert_eq!(reduction.call, "mean(x)");
    assert_eq!(
        reduction.members,
        vec![0, 2],
        "the gap at index 1 is left out"
    );
}

/// One eligible value is a mean and no sd: there is no second value to vary from.
#[test]
fn test_one_member_gives_a_mean_and_no_sd() {
    let formulas = [
        formula("avg", 1, "mean(x)", Some("Avg"), &[("x", "X")]),
        formula("spread", 2, "sd(x)", Some("Spread"), &[("x", "X")]),
    ];
    let (produced, _) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("x", &[Some(7.0), None])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let Produced::Scalar(avg) = &produced[0] else {
        panic!("one number");
    };
    let Produced::Scalar(spread) = &produced[1] else {
        panic!("one number");
    };
    assert_eq!(avg.value, Some(7.0));
    assert_eq!(spread.value, None, "an sd of one value is NA, not zero");
    assert!(!spread.refused, "NA is computed, not refused: {spread:?}");
}

/// No eligible member at all is a computed NA, which clears the stored value (Q229).
#[test]
fn test_a_reduction_over_nothing_is_na() {
    let formulas = [formula("avg", 1, "mean(x)", Some("Avg"), &[("x", "X")])];
    let (produced, _) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("x", &[None, None])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let Produced::Scalar(avg) = &produced[0] else {
        panic!("one number");
    };
    assert_eq!(avg.value, None);
    assert!(avg.skipped.is_none(), "it ran: {avg:?}");
}

/// A guard over an NA reduction stands in for it, the way it does for any other missing value.
#[test]
fn test_a_guard_supplies_a_fallback_for_an_na_reduction() {
    let formulas = [formula(
        "spread",
        1,
        "coalesce(sd(x), 0)",
        Some("Spread"),
        &[("x", "X")],
    )];
    let (produced, _) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("x", &[Some(7.0)])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let Produced::Scalar(spread) = &produced[0] else {
        panic!("one number");
    };
    assert_eq!(spread.value, Some(0.0));
}

/// A family a formula only reduces is declared as the family, not as the group's served mean.
#[test]
fn test_a_reduced_family_is_declared_as_replicates() {
    let formulas = [formula("avg", 1, "mean(x)", Some("Avg"), &[("x", "X")])];
    let manifest = manifest_json("Average", None, &formulas, &["X".to_string()]).expect("manifest");
    let params = manifest["params"].as_array().expect("params");
    assert_eq!(params[0]["kind"], serde_json::json!("replicates"));
    assert!(
        manifest["event_inputs"]
            .as_array()
            .expect("event_inputs")
            .is_empty(),
        "the repeats are the input, not one served number: {manifest:?}"
    );
}

/// A variable read both as a family and as one value is bound both ways: the repeat at this cell's
/// index, and the statistic over the whole family.
#[test]
fn test_a_variable_read_inside_and_outside_a_reducer_is_bound_both_ways() {
    let formulas = [formula(
        "ratio",
        1,
        "x / mean(x)",
        Some("Ratio"),
        &[("x", "X")],
    )];
    let (produced, _) = evaluate_with_trace(
        &formulas,
        &HashMap::new(),
        &replicates(&[("x", &[Some(2.0), Some(4.0), Some(6.0)])]),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let Produced::Scalar(ratio) = &produced[0] else {
        panic!("one number");
    };
    // The first repeat over the family's mean, 2/4.
    assert_eq!(ratio.value, Some(0.5));
}

/// A reducer naming something that is not a family skips the formula rather than reducing the one
/// number it found.
#[test]
fn test_a_reducer_over_a_scalar_skips_the_formula() {
    let formulas = [formula(
        "ratio",
        1,
        "x / mean(x)",
        Some("Ratio"),
        &[("x", "X")],
    )];
    let (produced, _) = evaluate_with_trace(
        &formulas,
        &inputs(&[("x", 6.0)]),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("evaluates");
    let Produced::Scalar(ratio) = &produced[0] else {
        panic!("one number");
    };
    assert_eq!(
        ratio.value, None,
        "x is a number here and names no family, so the reduction has nothing to read"
    );
    assert!(
        ratio
            .skipped
            .as_deref()
            .is_some_and(|r| r.contains("mean(x)")),
        "{ratio:?}"
    );
}

/// `mean` over an expression is not a reduction: the engine defines it over a family named by the
/// author, and anything else is refused as a call nothing defines.
#[test]
fn test_a_reducer_over_an_expression_is_refused() {
    assert!(reducer_calls("mean(x + 1)").is_empty());
    let formulas = [formula("avg", 1, "mean(x + 1)", Some("Avg"), &[("x", "X")])];
    let err = evaluate_with_trace(
        &formulas,
        &inputs(&[("x", 1.0)]),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect_err("the expression names a function nothing defines");
    assert!(err.contains("avg"), "{err}");
}

// --- M300, the portal's uncorrected path ---

fn co2_corr(expr: &str) -> Vec<PinnedFormula> {
    vec![with_curve(
        formula("co2_corr", 1, expr, Some("co2_corr"), &[("ppm", "co2_ppm")]),
        "vaisala",
    )]
}

#[test]
fn test_a_guarded_curve_reads_the_raw_value_when_no_curve_is_on_file() {
    let results = evaluate(
        &co2_corr("coalesce(ppm * curve_slope + curve_intercept, ppm)"),
        &inputs(&[("ppm", 400.0)]),
        &HashMap::new(),
        &curves(&[]),
    )
    .expect("the guarded form evaluates with no curve");
    assert_eq!(results[0].skipped, None);
    assert!((results[0].value.unwrap() - 400.0).abs() < 1e-12);
}

#[test]
fn test_a_guarded_curve_still_corrects_when_one_is_chosen() {
    let results = evaluate(
        &co2_corr("coalesce(ppm * curve_slope + curve_intercept, ppm)"),
        &inputs(&[("ppm", 400.0)]),
        &HashMap::new(),
        &curves(&[("vaisala", 2.0, 5.0)]),
    )
    .expect("the chosen curve resolves");
    // 400 * 2 + 5
    assert!((results[0].value.unwrap() - 805.0).abs() < 1e-12);
    assert_eq!(results[0].curve_slot.as_deref(), Some("vaisala"));
}

#[test]
fn test_an_unguarded_curve_still_skips_when_none_is_supplied() {
    let results = evaluate(
        &co2_corr("ppm * curve_slope + curve_intercept"),
        &inputs(&[("ppm", 400.0)]),
        &HashMap::new(),
        &curves(&[]),
    )
    .expect("the set evaluates");
    let reason = results[0].skipped.as_deref().expect("co2_corr is skipped");
    assert!(reason.contains("vaisala"), "{reason}");
}

#[test]
fn test_one_guarded_coefficient_is_not_enough_to_evaluate_without_a_curve() {
    let results = evaluate(
        &co2_corr("coalesce(ppm * curve_slope, ppm) + curve_intercept"),
        &inputs(&[("ppm", 400.0)]),
        &HashMap::new(),
        &curves(&[]),
    )
    .expect("the set evaluates");
    let reason = results[0].skipped.as_deref().expect("co2_corr is skipped");
    assert!(reason.contains("vaisala"), "{reason}");
}

/// A step whose own input is missing produces nothing, and the formulas naming it are skipped
/// like any formula whose input does not resolve (M109); the rest of the set still runs.
#[test]
fn test_a_step_that_produced_nothing_skips_its_readers_and_leaves_the_set_running() {
    let mut step = formula("k_h", 0, "t + 1", None, &[("t", "water_temp")]);
    step.intermediate = true;
    let reader = formula("co2", 1, "k_h * 2", Some("co2"), &[]);
    let other = formula("ph", 2, "a * 3", Some("ph"), &[("a", "alkalinity")]);

    let results = run(&[step, reader, other], &inputs(&[("a", 4.0)]));
    assert_eq!(value_of(&results, "k_h"), None);
    assert_eq!(value_of(&results, "co2"), None);
    let skipped = results
        .iter()
        .find(|e| e.code == "co2")
        .expect("the reader is reported")
        .skipped
        .clone()
        .expect("the reader says why");
    assert!(skipped.contains("k_h"), "{skipped}");
    assert_eq!(value_of(&results, "ph"), Some(12.0), "12.0 = 4.0 * 3");
}

/// A step read only through a guard is NA rather than a skip, the arm the portal's fallbacks take.
#[test]
fn test_a_step_read_only_through_a_guard_is_na_rather_than_a_skip() {
    let mut step = formula("k_h", 0, "t + 1", None, &[("t", "water_temp")]);
    step.intermediate = true;
    let reader = formula("co2", 1, "coalesce(k_h, 7)", Some("co2"), &[]);

    let results = run(&[step, reader], &inputs(&[]));
    assert_eq!(value_of(&results, "co2"), Some(7.0));
}

// Scenario: a calculation on a high-frequency stream reads one input from the stream and one the
// lab measures at a visit, declared held (Q230).
//
// Expected behaviour: the pinned manifest says which event input is held, and says nothing about
// the one read at the instant, so a manifest written before the rule existed still reads as
// exact.
#[test]
fn test_a_held_source_says_so_on_the_event_input_it_fills() {
    let mut pco2 = formula(
        "pco2_corr",
        1,
        "co2 * alkalinity",
        Some("pco2_corr"),
        &[("co2", "Vaisala_CO2_avg"), ("alkalinity", "Alkalinity")],
    );
    pco2.held = vec!["alkalinity".to_string()];
    let manifest = manifest_json("pCO2", None, &[pco2], &[]).expect("the set has an order");

    let inputs = manifest["event_inputs"].as_array().unwrap();
    let held = inputs.iter().find(|i| i["param"] == "alkalinity").unwrap();
    assert_eq!(held["alignment"], "hold");
    let exact = inputs.iter().find(|i| i["param"] == "co2").unwrap();
    assert!(
        exact.get("alignment").is_none(),
        "an input read at the instant says nothing, as every manifest before the rule did: {exact:?}"
    );
}

// Expected behaviour: a manifest with no alignment on an event input parses, and reads as exact.
#[test]
fn test_an_event_input_with_no_alignment_reads_as_exact() {
    let parsed: crate::routes::private::tools::models::ManifestEventInput =
        serde_json::from_value(serde_json::json!({ "param": "co2", "parameter_code": "CO2" }))
            .expect("a manifest written before the rule still reads");
    assert_eq!(parsed.alignment, "exact");
}

/// Expected behaviour: the manifest names the codes a run holds between visits, and only those, so
/// a reader of a pinned version knows which inputs stand between visits without the source rows.
#[test]
fn test_the_manifest_names_the_codes_it_holds() {
    let mut pco2 = formula(
        "pco2_corr",
        1,
        "co2 * alkalinity",
        Some("pco2_corr"),
        &[("co2", "Vaisala_CO2_avg"), ("alkalinity", "Alkalinity")],
    );
    pco2.held = vec!["alkalinity".to_string()];
    let manifest: crate::routes::private::tools::models::Manifest = serde_json::from_value(
        manifest_json("pCO2", None, &[pco2], &[]).expect("the set has an order"),
    )
    .expect("the manifest reads back");
    assert_eq!(manifest.held_codes(), vec!["alkalinity".to_string()]);
}

fn shared(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(c, f)| ((*c).to_string(), (*f).to_string()))
        .collect()
}

#[test]
fn test_received_steps_walks_what_a_declared_step_reads() {
    let steps = shared(&[
        ("water_k", "WTW_Temp_degC_1 + 273.15"),
        ("kh", "0.034 * exp(c_const * (1 / water_k - 1 / 298.15))"),
        ("apart", "Dissolved_O2"),
    ]);
    let mut received = received_steps(&["kh".to_string()], &steps);
    received.sort();
    assert_eq!(received, ["kh", "water_k"]);
}

#[test]
fn test_received_steps_takes_a_step_once_by_two_paths() {
    let steps = shared(&[("a", "Dissolved_O2"), ("b", "a + 1"), ("c", "a + b")]);
    let mut received = received_steps(&["c".to_string(), "b".to_string()], &steps);
    received.sort();
    assert_eq!(received, ["a", "b", "c"]);
}

#[test]
fn test_received_steps_with_nothing_declared_is_empty() {
    let steps = shared(&[("a", "Dissolved_O2")]);
    assert!(received_steps(&[], &steps).is_empty());
}
