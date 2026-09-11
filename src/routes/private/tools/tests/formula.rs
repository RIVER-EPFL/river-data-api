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

#[test]
fn test_a_produced_value_is_not_declared_an_event_input_whatever_the_codes_sort_to() {
    let manifest =
        manifest_json("Carbonate", None, &shared_ordinal_chain()).expect("the set has an order");
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

    let manifest = manifest_json("pCO2", None, &set).expect("the set has an order");
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
    let manifest = manifest_json("Field Data", None, &[bp]).expect("the set has an order");

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
