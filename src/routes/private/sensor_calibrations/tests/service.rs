use super::*;

/// The SQL a built statement runs as, for the assertions below.
fn rendered(query: impl sea_orm::sea_query::QueryStatementBuilder) -> String {
    build(query).sql
}

/// One expression, rendered on its own, so a test can read what a statement carries.
fn expr_sql(e: Expr) -> String {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};
    Query::select()
        .expr(e)
        .to_string(PostgresQueryBuilder)
        .trim_start_matches("SELECT ")
        .to_owned()
}

/// Scenario: a site declares a slot for the output of a formula calculation.
///
/// Expected behaviour: the stream engine's unit of work is the calculation, so the query selects
/// the slots whose producing formula belongs to one. A formula belonging to no calculation is a
/// shared step (Q156) and computes nothing on its own.
#[test]
fn test_derived_work_query_selects_calculation_outputs() {
    let sql = rendered(derived_work_query(Uuid::nil()));
    assert!(
        sql.contains(r#""d"."tool_script_id" IS NOT NULL"#),
        "a formula owned by no calculation computes nothing on its own: {sql}"
    );
    assert!(
        sql.contains(r#""d"."tool_script_id" AS "tool_script_id""#),
        "the calculation is what the work is grouped by: {sql}"
    );
}

/// Scenario: a calculation publishes two outputs, and the site declares a slot for each, so the
/// work query returns two rows for one calculation.
///
/// Expected behaviour: they group into one unit of work carrying both outputs. This is what makes
/// the set evaluate once per instant: grouped per output instead, a two-output set would resolve
/// its sources and run its formulas twice at every instant and write the same two readings.
#[test]
fn test_two_output_rows_of_one_calculation_are_one_unit_of_work() {
    let calculation = Uuid::from_u128(1);
    let site = Uuid::from_u128(2);
    let row = |slot: u128, parameter: u128, code: &str| super::DerivedWorkRow {
        id: Uuid::from_u128(slot),
        tool_script_id: calculation,
        site_id: site,
        parameter_id: Uuid::from_u128(parameter),
        parameter_code: code.to_string(),
    };

    let grouped = super::group_by_calculation(vec![row(10, 20, "k_half"), row(11, 21, "k_tenth")]);
    assert_eq!(grouped.len(), 1, "one calculation is one pass");
    assert_eq!(grouped[0].0, calculation);
    assert_eq!(grouped[0].1, site);
    let codes: Vec<&str> = grouped[0]
        .2
        .iter()
        .map(|o| o.parameter_code.as_str())
        .collect();
    assert_eq!(codes, ["k_half", "k_tenth"], "both outputs ride the pass");
}

/// Two calculations at one site stay two units of work: they are ordered against each other, and
/// one reading the other's output.
#[test]
fn test_two_calculations_stay_two_units_of_work() {
    let site = Uuid::from_u128(2);
    let row = |script: u128, slot: u128| super::DerivedWorkRow {
        id: Uuid::from_u128(slot),
        tool_script_id: Uuid::from_u128(script),
        site_id: site,
        parameter_id: Uuid::from_u128(slot + 100),
        parameter_code: format!("out_{slot}"),
    };
    let grouped = super::group_by_calculation(vec![row(1, 10), row(2, 11), row(1, 12)]);
    assert_eq!(grouped.len(), 2);
    assert_eq!(grouped[0].2.len(), 2, "the first calculation's two outputs");
    assert_eq!(grouped[1].2.len(), 1);
}

/// A slot the site declares low cadence is filled at a visit, and the chain computes it there.
/// Without the predicate the stream engine evaluates it at the same instant and upserts over the
/// chain's row, since both write `(stream_id, time, replicate_index)`.
#[test]
fn test_derived_work_query_takes_only_high_cadence_slots() {
    use sea_orm::sea_query::PostgresQueryBuilder;
    let sql = derived_work_query(Uuid::nil()).to_string(PostgresQueryBuilder);
    assert!(
        sql.contains(r#""sp"."cadence" = 'high'"#),
        "a low-cadence slot is the chain's, computed at its visit: {sql}"
    );
}

/// The report and the split ask the same question at different moments: a reading whose curve
/// belongs to another instrument. Keeping the predicate in one place is what stops the report
/// listing rows the split would not have asked about.
#[test]
fn a_foreign_curve_is_one_whose_owner_is_not_the_reading_s_instrument() {
    assert_eq!(
        foreign_curve_rows("r", "sc"),
        "sc.sensor_id IS DISTINCT FROM r.sensor_id"
    );
    // NULL on either side is foreign, not skipped: a reading with no instrument corrected by
    // somebody's curve is exactly the case worth listing.
    assert!(foreign_curve_rows("r", "sc").contains("IS DISTINCT FROM"));
}

/// A reprocess visits far more readings than it moves, and Q125 bounds the ledger to the ones
/// that moved.
#[test]
fn a_recording_statement_inserts_only_where_a_written_column_differs() {
    let write = SeaQuery::update()
        .table(readings::Entity)
        .value(readings::Column::SiteId, Expr::val(Option::<Uuid>::None))
        .take();
    let sql = rendered(record_moved(
        write,
        &["site_id", "deployment_id"],
        Some(Uuid::nil()),
    ));
    assert!(sql.contains("m.was_site_id IS DISTINCT FROM m.now_site_id"));
    assert!(sql.contains("m.was_deployment_id IS DISTINCT FROM m.now_deployment_id"));
    assert!(sql.contains("'site_id', to_jsonb(m.was_site_id)"));
    assert!(sql.contains("'site_id', to_jsonb(m.now_site_id)"));
    assert!(
        sql.contains(Kind::Reprocess.as_str()) || sql.contains("$4"),
        "the kind is named or bound: {sql}"
    );
    assert!(
        sql.find("moved").unwrap() < sql.find("recorded").unwrap(),
        "the write comes first, so the ledger reads what it returned: {sql}"
    );
}

#[test]
fn the_drift_sweep_repairs_exactly_what_the_recompose_writes() {
    let drifted = own_curve_rows(corrected_rows("r").and(Expr::cust_with_exprs(
        "tgt.calibrated_value IS DISTINCT FROM ($1)",
        [recomposed_own_curve_value()],
    )));
    let sweep = rendered(recompose_statement(drifted));
    assert!(
        sweep.contains(&expr_sql(recomposed_own_curve_value())),
        "the sweep writes the value the recompose computes: {sweep}"
    );
    assert!(
        sweep.contains(&expr_sql(orphaned_correction_rows("r"))),
        "and leaves an orphaned correction alone, as the recompose does: {sweep}"
    );
    let spot = rendered(recompose_statement(own_curve_rows(Expr::cust(
        "r.measurement_type = 'spot'",
    ))));
    let qualifier = |sql: &str| {
        let at = sql.find("AND (").expect("the qualifying predicate");
        sql[at..].to_owned()
    };
    assert_eq!(
        sweep.replace(&qualifier(&sweep), "QUALIFY"),
        spot.replace(&qualifier(&spot), "QUALIFY"),
        "the two statements differ only in which rows qualify"
    );
}

/// A retired curve is out of circulation: no write path and no reprocess may resolve one, and
/// the one producer of the ranking is where that is said.
#[test]
fn a_retired_curve_is_never_a_candidate() {
    let rendered_pick = |q: sea_orm::sea_query::SelectStatement| {
        q.to_string(sea_orm::sea_query::PostgresQueryBuilder)
    };
    for pick in [
        rendered_pick(super::super::resolver::pick_calibration_query("$1")),
        rendered_pick(super::super::resolver::pick_calibration_query_excluding(
            "$2",
            Some("$1"),
        )),
    ] {
        assert!(
            pick.replace('"', "").contains("c.retired_at IS NULL"),
            "the ranking excludes retired curves: {pick}"
        );
    }
}

/// The reprocess engine and the calibration-delete hook repoint readings by the same rule. They
/// were two copies of it, and a fix landing on one is the way they diverge.
#[test]
fn both_repoint_callers_emit_one_statement() {
    let selection = || Expr::cust("SELECTION");
    let engine = rendered(repoint_statement(
        super::super::resolver::pick_calibration_query("$1"),
        selection(),
        None,
    ));
    let delete_hook = rendered(repoint_statement(
        super::super::resolver::pick_calibration_query_excluding("$2", Some("$1")),
        selection(),
        None,
    ));
    let pick_of = |sql: &str| {
        let start = sql.find("LEFT JOIN LATERAL").expect("lateral");
        let end = sql.find("AS \"cw\"").expect("lateral alias");
        sql[start..end].to_owned()
    };
    assert_eq!(
        engine.replace(&pick_of(&engine), "PICK"),
        delete_hook.replace(&pick_of(&delete_hook), "PICK"),
        "the two differ only in which windows the lateral ranks"
    );
    assert!(
        engine.contains("LEFT JOIN LATERAL"),
        "the lateral stays an outer join, so a reading no window covers is repointed to none \
         rather than skipped: {engine}"
    );
    assert!(
        engine.contains("standard_curves"),
        "and the operator's standard curve is re-applied on top of the new base: {engine}"
    );
}

/// A derived slot is re-asserted, not inserted twice: the same instant recomputed carries the new
/// number, the site it belongs to and the formula version it came from.
#[test]
fn a_recomputed_derived_slot_reasserts_every_column_it_owns() {
    let sql = rendered(derived_upsert(
        Uuid::nil(),
        Uuid::nil(),
        Uuid::nil(),
        chrono::Utc::now(),
        1.0,
        None,
    ));
    assert!(
        sql.contains("ON CONFLICT") && sql.contains("DO UPDATE"),
        "the upsert re-asserts rather than failing: {sql}"
    );
    for column in [
        "raw_value",
        "calibrated_value",
        "measurement_type",
        "site_id",
        "parameter_id",
        "derived_version_id",
    ] {
        assert!(
            sql.contains(&format!(r#""{column}" = "excluded"."{column}""#)),
            "a recompute re-asserts {column}: {sql}"
        );
    }
}

/// A derived value whose inputs stopped resolving leaves the site, and takes nothing else with it:
/// the number stays, so a later pass that resolves its inputs again writes the same row.
#[test]
fn unattributing_a_derived_row_clears_the_site_and_nothing_else() {
    let sql = rendered(unattribute_statement(
        Uuid::nil(),
        Uuid::nil(),
        chrono::Utc::now(),
    ));
    assert!(
        sql.contains(r#"SET "site_id" = $1"#),
        "the site is bound NULL: {sql}"
    );
    assert!(
        !sql.contains("raw_value") && !sql.contains("calibrated_value"),
        "the value is not touched: {sql}"
    );
    assert!(
        sql.contains(r#""measurement_type" = $"#),
        "and only a derived row is reached: {sql}"
    );
}

/// The engine's four writes, each read without a database: what it selects, what it writes, and
/// the ledger row it records. The order is the contract (attribution before the curve pick), and
/// each write's ledger insert reads the CTE that write returned.
#[test]
fn every_reprocess_step_records_what_it_moved() {
    for scope in [
        Scope::Sensor(Uuid::nil()),
        Scope::Slot {
            site_id: Uuid::nil(),
            parameter_id: Uuid::nil(),
        },
    ] {
        let steps = reprocess_statements(scope, None);
        for (name, query) in [
            ("attribution", &steps.attribution),
            ("calibration", &steps.calibration),
            ("spot", &steps.spot),
            ("recall", &steps.recall),
        ] {
            let sql = rendered(query.clone());
            assert!(
                sql.contains(r#"INSERT INTO "reading_decisions""#),
                "{name} records its move: {sql}"
            );
            assert!(
                sql.contains("IS DISTINCT FROM m.now_"),
                "{name} records only the rows that moved: {sql}"
            );
            assert!(
                sql.ends_with(r#"SELECT "site_id", "time" FROM "moved""#),
                "{name} reports the instants the cascade follows: {sql}"
            );
        }
        assert!(
            rendered(steps.spot.clone()).contains("r.measurement_type = 'spot'"),
            "the grab step takes the rows the window resolution holds back"
        );
        assert!(
            !rendered(steps.calibration.clone()).contains("r.measurement_type = 'spot'"),
            "and the window resolution leaves them alone"
        );
    }
}

/// A multi-parameter instrument holds one calibration timeline per parameter, so one parameter's
/// next curve must never truncate another's window, and two curves sharing an instant must still
/// chain to one answer.
#[test]
fn the_calibration_chain_is_per_parameter_and_single_valued() {
    let sql = rendered(calibration_chain_statement(Uuid::nil()));
    assert!(
        sql.contains(r#"PARTITION BY "parameter_id""#),
        "the chain is per parameter: {sql}"
    );
    assert!(
        sql.contains(r#"ORDER BY "valid_from" ASC, "id" ASC"#),
        "and single-valued on a shared instant: {sql}"
    );
    assert!(
        sql.contains(r#""retired_at" IS NULL"#),
        "a retired curve takes no part in the chain: {sql}"
    );
    assert!(
        sql.contains("ordered.next_from > ordered.valid_from"),
        "and a zero-width window is refused: {sql}"
    );
}

/// An operator-written bound is data: the chain may shorten it to keep windows from overlapping,
/// never extend it. A deployment's end date is always the caller's, so its chain only shortens.
#[test]
fn a_chain_written_bound_is_derived_and_an_operator_written_one_is_only_shortened() {
    let calibration = rendered(calibration_chain_statement(Uuid::nil()));
    assert!(
        calibration.contains(
            "CASE WHEN sc.valid_until_explicit THEN LEAST(sc.valid_until, ordered.next_from)"
        ),
        "an explicit bound survives unless the next curve is earlier: {calibration}"
    );
    let deployment = rendered(deployment_chain_statement(Uuid::nil()));
    assert!(
        deployment.contains("LEAST(COALESCE(deployed_until"),
        "a deployment's own end date is kept when it is the earlier one: {deployment}"
    );
    assert!(
        deployment
            .contains("COALESCE(d.deployed_until, 'infinity'::timestamptz) <> ordered.new_until"),
        "and the write is held to the rows the chain moves: {deployment}"
    );
}

fn candidate(
    measurement_type: Option<&str>,
    replicate_index: i16,
    stream: u128,
    value: f64,
) -> InputCandidate {
    InputCandidate {
        measurement_type: measurement_type.map(str::to_string),
        replicate_index,
        stream_id: Uuid::from_u128(stream),
        value,
        from_mean: false,
        revision: None,
    }
}

#[test]
fn test_chosen_input_prefers_continuous_over_spot() {
    let candidates = [
        candidate(Some("spot"), 0, 1, 4.0),
        candidate(Some("continuous"), 0, 2, 7.0),
    ];
    assert_eq!(chosen_input(&candidates).map(|c| c.value), Some(7.0));
}

#[test]
fn test_chosen_input_reads_null_measurement_type_as_continuous() {
    let candidates = [
        candidate(Some("spot"), 0, 1, 4.0),
        candidate(None, 0, 2, 7.0),
    ];
    assert_eq!(chosen_input(&candidates).map(|c| c.value), Some(7.0));
}

#[test]
fn test_chosen_input_takes_lowest_replicate_then_lowest_stream() {
    let candidates = [
        candidate(Some("spot"), 1, 1, 5.0),
        candidate(Some("spot"), 0, 3, 6.0),
        candidate(Some("spot"), 0, 2, 4.0),
    ];
    assert_eq!(chosen_input(&candidates).map(|c| c.value), Some(4.0));
}

#[test]
fn test_chosen_input_empty_slot() {
    assert!(chosen_input(&[]).is_none());
}

/// A derived value's replay: the formula the computation recorded, over the numbers it recorded.
mod replay {
    use super::super::{captured_set, replay_captured};
    use crate::routes::private::readings::models::ConsumedInput;

    fn input(variable: &str, kind: &str, value: serde_json::Value) -> ConsumedInput {
        ConsumedInput {
            variable: variable.to_string(),
            kind: kind.to_string(),
            subject: None,
            property: None,
            revision: None,
            members: Vec::new(),
            value,
        }
    }

    fn step(formula: &str) -> ConsumedInput {
        input("DOmgL", "step", serde_json::json!(formula))
    }

    #[test]
    fn test_the_replay_is_the_recorded_formula_over_the_recorded_values() {
        let consumed = [
            step("Dissolved_O2 * 2"),
            input("Dissolved_O2", "reading", serde_json::json!(10.0)),
        ];
        assert_eq!(replay_captured(&consumed), Ok(20.0));
    }

    #[test]
    fn test_a_constant_and_a_site_property_bind_like_any_other_value() {
        let consumed = [
            step("(Depth - datum) * k"),
            input("Depth", "reading", serde_json::json!(3.0)),
            input("datum", "site", serde_json::json!(1.0)),
            input("k", "constant", serde_json::json!(2.0)),
        ];
        assert_eq!(replay_captured(&consumed), Ok(4.0));
    }

    #[test]
    fn test_a_value_that_was_not_there_binds_as_na_where_a_guard_reads_it() {
        let consumed = [
            step("coalesce(Dissolved_O2, 5)"),
            input("Dissolved_O2", "reading", serde_json::Value::Null),
        ];
        assert_eq!(replay_captured(&consumed), Ok(5.0));
    }

    #[test]
    fn test_a_value_that_was_not_there_refuses_the_replay_when_nothing_guards_it() {
        let consumed = [
            step("Dissolved_O2 * 2"),
            input("Dissolved_O2", "reading", serde_json::Value::Null),
        ];
        let refusal = replay_captured(&consumed).expect_err("nothing to evaluate over");
        assert!(refusal.contains("Dissolved_O2"), "{refusal}");
    }

    /// A set captured before the step entry existed names no formula, so there is nothing to
    /// replay and the reader is told that rather than shown a number.
    #[test]
    fn test_a_set_naming_no_formula_is_refused() {
        let consumed = [input("Dissolved_O2", "reading", serde_json::json!(10.0))];
        assert!(captured_set(&consumed).is_err());
        assert!(replay_captured(&consumed).is_err());
    }

    #[test]
    fn test_the_step_is_not_bound_as_a_variable_of_its_own_formula() {
        let consumed = [
            step("Dissolved_O2 * 2"),
            input("Dissolved_O2", "reading", serde_json::json!(10.0)),
        ];
        let set = captured_set(&consumed).expect("a captured set");
        assert_eq!(set.formula, "Dissolved_O2 * 2");
        assert_eq!(set.variables.len(), 1, "{:?}", set.variables);
    }
}

/// Scenario: a continuous calculation whose set is a step read by two outputs, the shape a portal
/// calculation has.
///
/// Expected behaviour: the unit of work is the calculation, so the step is evaluated once and both
/// outputs come out of that one evaluation. Evaluating per output would run the step twice.
mod a_set_is_one_unit_of_work {
    use crate::routes::private::sensor_calibrations::service::runs_on_streams;
    use crate::routes::private::tools::models::PinnedFormula;
    use crate::routes::private::tools::service::evaluate;
    use std::collections::HashMap;

    fn formula(code: &str, text: &str, sources: &[&str], step: bool) -> PinnedFormula {
        PinnedFormula {
            code: code.to_string(),
            label: code.to_string(),
            units: None,
            formula: text.to_string(),
            ordinal: 0,
            output_parameter_code: (!step).then(|| code.to_string()),
            sources: sources
                .iter()
                .map(|s| ((*s).to_string(), (*s).to_string()))
                .collect(),
            site_sources: Vec::new(),
            curve_slot: None,
            per_replicate: None,
            intermediate: step,
        }
    }

    fn set() -> Vec<PinnedFormula> {
        vec![
            formula("water_k", "WTW_Temp_degC_1 + 273.15", &["WTW_Temp_degC_1"], true),
            formula("k_half", "water_k / 2", &[], false),
            formula("k_tenth", "water_k / 10", &[], false),
        ]
    }

    #[test]
    fn a_step_two_outputs_read_is_evaluated_once_and_both_outputs_store() {
        let inputs = HashMap::from([("WTW_Temp_degC_1".to_string(), 8.85)]);
        let cells = evaluate(&set(), &inputs, &HashMap::new(), &HashMap::new()).unwrap();
        let value = |code: &str| {
            cells
                .iter()
                .find(|cell| cell.code == code)
                .and_then(|cell| cell.value)
        };
        // 8.85 + 273.15
        assert_eq!(value("water_k"), Some(282.0));
        assert_eq!(value("k_half"), Some(141.0));
        assert_eq!(value("k_tenth"), Some(28.2));
        assert_eq!(cells.len(), 3, "one cell per formula, the step included");
    }

    /// The step's own cell carries no output parameter, so the pass stores two readings, not
    /// three: a step is a value the set passes forward, not a measurement.
    #[test]
    fn a_step_is_not_one_of_the_values_stored() {
        let set = set();
        let stored: Vec<&str> = set
            .iter()
            .filter_map(|f| f.output_parameter_code.as_deref())
            .collect();
        assert_eq!(stored, ["k_half", "k_tenth"]);
    }

    /// A set whose formula corrects with a lab curve is chosen per grab sample, so there is nobody
    /// to choose it at an ingest (Q228).
    #[test]
    fn a_set_declaring_a_curve_slot_is_the_visit_arm_s() {
        assert!(runs_on_streams(&set()));
        let mut with_curve = set();
        with_curve[1].curve_slot = Some("doc".to_string());
        assert!(!runs_on_streams(&with_curve));
    }
}
