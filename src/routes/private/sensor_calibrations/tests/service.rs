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
        &[],
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

/// The engine's five writes, each read without a database: what it selects, what it writes, and
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
            ("release", &steps.release),
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
                sql.contains(r#"SELECT "m"."site_id", "m"."time", "m"."parameter_id", "#),
                "{name} reports the instants the cascade follows, and the slot of each, whose \
                 parameter a held pulse reads: {sql}"
            );
            assert!(
                sql.contains(r#"AS "changed""#) && sql.ends_with(r#"FROM "moved" AS "m""#),
                "{name} reports whether each visited row moved, which is what it announces: {sql}"
            );
        }
        assert!(
            rendered(steps.spot.clone())
                .ends_with(r#"AS "changed", "m"."collection_event_id" FROM "moved" AS "m""#),
            "the grab step reports the visit whose calculations read what it moved"
        );
        assert!(
            rendered(steps.attribution.clone()).contains(r#""m"."was_site_id" AS "left_site_id""#),
            "a row the attribution moves off a site leaves that site's series stale too"
        );
        assert!(
            rendered(steps.calibration.clone()).contains(r#"AS uuid) AS "left_site_id""#),
            "the curve step moves no row between sites"
        );
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

/// Expected behaviour: a `reprocess` or `curve_recompose` row takes its `supersedes` from the
/// ledger's family rule, and each of those kinds stands alone, so the row supersedes nothing: a
/// second reprocess does not hide the one before it.
#[test]
fn a_reprocess_or_recompose_row_supersedes_nothing() {
    for kind in [Kind::Reprocess, Kind::CurveRecompose] {
        let insert = ledger_insert(
            kind,
            Origin::System,
            "moved",
            "m",
            Expr::val(1),
            Expr::val(2),
            None,
            None,
        );
        let sql = insert.to_string(PostgresQueryBuilder);
        assert!(
            !sql.contains(&format!("p.kind = '{}'", kind.as_str())),
            "{kind:?} names no previous decision of its own kind: {sql}"
        );
        assert!(
            sql.contains(r#"FROM "reading_decisions" AS "d""#) && sql.contains("1 = 2"),
            "{kind:?} reads the family rule, whose empty family matches no decision: {sql}"
        );
    }
}

/// A reading whose deployment reference names another instrument, or a window that does not hold
/// its instant, loses the reference and nothing else.
#[test]
fn the_release_clears_only_a_deployment_reference_no_window_backs() {
    for scope in [
        Scope::Sensor(Uuid::nil()),
        Scope::Slot {
            site_id: Uuid::nil(),
            parameter_id: Uuid::nil(),
        },
    ] {
        let sql = rendered(reprocess_statements(scope, None).release);
        assert!(
            sql.contains(r#"SET "deployment_id" = $"#) && !sql.contains(r#""site_id" = $"#),
            "only the reference is written: {sql}"
        );
        for backing in [
            r#""d"."id" = "r"."deployment_id""#,
            r#""d"."sensor_id" = "r"."sensor_id""#,
            r#""d"."parameter_id" = "r"."parameter_id""#,
            r#""r"."time" >= "d"."deployed_from""#,
            r#""r"."time" < "d"."deployed_until""#,
            r#"NOT EXISTS"#,
        ] {
            assert!(sql.contains(backing), "the backing checks {backing}: {sql}");
        }
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
            r#"CASE WHEN ("sc"."valid_until_explicit") THEN LEAST("sc"."valid_until", "ordered"."next_from")"#
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

/// The chain runs inside a retirement's transaction as well as on its own in every reprocess, and
/// two chains rewriting the same rows lock them in opposite orders. Holding the write to the rows
/// whose bound moves keeps a chain with nothing to change from locking anything.
#[test]
fn the_calibration_chain_writes_only_the_bounds_it_moves() {
    let sql = rendered(calibration_chain_statement(Uuid::nil()));
    assert!(
        sql.contains(
            r#""sc"."valid_until" <> (CASE WHEN ("sc"."valid_until_explicit") THEN LEAST("sc"."valid_until", "ordered"."next_from") ELSE "ordered"."next_from" END)"#
        ),
        "the write is held to the rows the chain moves: {sql}"
    );
    assert!(
        sql.contains(r#"("sc"."valid_until" IS NULL) <> ((CASE"#),
        "including a move to or from an open end: {sql}"
    );
}

fn candidate(
    measurement_type: Option<&str>,
    replicate_index: i16,
    stream: u128,
    value: f64,
) -> InputCandidate {
    InputCandidate {
        time: DateTime::<Utc>::UNIX_EPOCH,
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
            alignment: None,
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
            held: Vec::new(),
            site_sources: Vec::new(),
            curve_slot: None,
            per_replicate: None,
            intermediate: step,
        }
    }

    fn set() -> Vec<PinnedFormula> {
        vec![
            formula(
                "water_k",
                "WTW_Temp_degC_1 + 273.15",
                &["WTW_Temp_degC_1"],
                true,
            ),
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

/// Scenario: a continuous calculation's set cannot be evaluated at an instant, because an
/// expression in it is one the engine cannot read.
///
/// Expected behaviour: every output the site fills reports its slot as unevaluable with the
/// evaluator's error, so the run raises a finding for each rather than leaving the stored value
/// standing with nothing said.
#[test]
fn test_an_unevaluable_set_reports_every_output_slot() {
    let site = Uuid::from_u128(1);
    let calculation = Uuid::from_u128(2);
    let output = |slot: u128, code: &str| DerivedOutput {
        site_param_id: Uuid::from_u128(slot),
        parameter_id: Uuid::from_u128(slot + 10),
        parameter_code: code.to_string(),
    };
    let item = DerivedWork {
        calculation: crate::routes::private::tools::service::StreamCalculation {
            id: calculation,
            name: "broken".to_string(),
            active_version_id: None,
            formulas: Vec::new(),
        },
        derived_site_id: site,
        outputs: vec![output(1, "k_half"), output(2, "k_tenth")],
    };
    let slot = |parameter: u128| DerivedSlot {
        site_id: site,
        parameter_id: Uuid::from_u128(parameter),
        calculation_id: calculation,
        pass: SlotPass::Unevaluable("unknown function 'lg'".to_string()),
    };
    assert_eq!(
        unevaluable_slots(&item, "unknown function 'lg'"),
        vec![slot(11), slot(12)]
    );
}

// Scenario: a calculation on a high-frequency stream reads one input from the stream and one the
// lab measures at a visit, the second declared held (Q230).
//
// Expected behaviour: the source read exactly binds at the instant being computed; the held one
// binds at the last instant its parameter was measured at or before it, so it stands at every
// pulse until the next visit. Both take the whole instant, because a replicate group's mean
// stands on its members, and neither takes a withdrawn or flagged row.
mod held_sources {
    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-02-01T{hour:02}:00:00Z"))
            .expect("an instant")
            .with_timezone(&Utc)
    }

    #[test]
    fn test_a_source_read_exactly_binds_at_the_instant_being_computed() {
        let sql = rendered(input_value_query(Uuid::nil(), Uuid::nil(), at(10), false));
        assert!(
            sql.contains(r#""r"."time" = $"#),
            "it reads the instant itself: {sql}"
        );
        assert!(
            !sql.contains(r#"MAX("h"."time")"#),
            "and looks no further back than it: {sql}"
        );
    }

    #[test]
    fn test_a_held_source_binds_at_the_last_instant_measured_at_or_before_it() {
        let sql = rendered(input_value_query(Uuid::nil(), Uuid::nil(), at(10), true));
        assert!(
            sql.contains(r#""r"."time" IN (SELECT MAX("h"."time")"#),
            "it reads the last instant the parameter was measured at: {sql}"
        );
        assert!(
            sql.contains(r#""h"."time" <= $"#),
            "at or before the one being computed: {sql}"
        );
    }

    #[test]
    fn test_the_instant_a_held_source_looks_back_over_takes_no_retracted_row() {
        let sql = rendered(input_value_query(Uuid::nil(), Uuid::nil(), at(10), true));
        assert!(
            sql.contains(r#""h"."withdrawn_at" IS NULL"#),
            "a withdrawn row is not a measurement to hold: {sql}"
        );
        assert!(
            sql.contains("h.is_flagged IS NOT TRUE"),
            "nor a flagged one: {sql}"
        );
    }

    #[test]
    fn test_both_arms_carry_the_instant_the_reading_stands_at() {
        for held in [false, true] {
            let sql = rendered(input_value_query(Uuid::nil(), Uuid::nil(), at(10), held));
            assert!(
                sql.contains(r#"SELECT "r"."time""#),
                "the record names the reading's own instant, not the one computed: {sql}"
            );
        }
    }
}

/// The order a site's calculations evaluate in, from what each set reads and produces.
mod evaluation_order {
    use super::super::{DerivedWork, build_evaluation_order};
    use crate::routes::private::tools::models::PinnedFormula;
    use crate::routes::private::tools::service::StreamCalculation;
    use uuid::Uuid;

    fn formula(code: &str, output: Option<&str>, sources: &[&str]) -> PinnedFormula {
        PinnedFormula {
            code: code.to_string(),
            label: code.to_string(),
            units: None,
            formula: sources.join(" + "),
            ordinal: 0,
            output_parameter_code: output.map(str::to_string),
            sources: sources
                .iter()
                .map(|s| ((*s).to_string(), (*s).to_string()))
                .collect(),
            held: Vec::new(),
            site_sources: Vec::new(),
            curve_slot: None,
            per_replicate: None,
            intermediate: output.is_none(),
        }
    }

    fn work(name: &str, formulas: Vec<PinnedFormula>) -> DerivedWork {
        DerivedWork {
            calculation: StreamCalculation {
                id: Uuid::nil(),
                name: name.to_string(),
                active_version_id: None,
                formulas,
            },
            derived_site_id: Uuid::nil(),
            outputs: Vec::new(),
        }
    }

    #[test]
    fn test_a_set_reads_only_what_it_does_not_produce_itself() {
        let item = work(
            "carbonate",
            vec![
                formula("dic", Some("DIC"), &["Alk", "pH"]),
                formula("co2", Some("CO2"), &["DIC", "pH"]),
            ],
        );
        assert_eq!(item.source_codes(), vec!["alk", "ph"]);
        assert_eq!(item.output_codes(), vec!["dic", "co2"]);
    }

    #[test]
    fn test_a_calculation_reading_another_ones_output_runs_after_it() {
        let items = [
            work("flux", vec![formula("flux", Some("Flux"), &["CO2"])]),
            work("carbonate", vec![formula("co2", Some("CO2"), &["Alk"])]),
        ];
        assert_eq!(build_evaluation_order(&items).expect("ordered"), vec![1, 0]);
    }

    #[test]
    fn test_a_source_matches_an_output_code_whatever_its_case() {
        let items = [
            work("flux", vec![formula("flux", Some("Flux"), &["co2"])]),
            work("carbonate", vec![formula("co2", Some("CO2"), &["Alk"])]),
        ];
        assert_eq!(build_evaluation_order(&items).expect("ordered"), vec![1, 0]);
    }

    #[test]
    fn test_a_calculation_reading_its_own_output_is_no_cycle() {
        let items = [work(
            "smooth",
            vec![
                formula("raw", Some("Raw"), &["Level"]),
                formula("smoothed", Some("Smoothed"), &["Raw", "Smoothed"]),
            ],
        )];
        assert_eq!(build_evaluation_order(&items).expect("ordered"), vec![0]);
    }

    #[test]
    fn test_two_calculations_reading_each_other_are_refused_naming_both() {
        let items = [
            work("left", vec![formula("a", Some("A"), &["B"])]),
            work("right", vec![formula("b", Some("B"), &["A"])]),
        ];
        let err = build_evaluation_order(&items)
            .expect_err("a cycle")
            .to_string();
        assert!(err.contains("left") && err.contains("right"), "{err}");
    }
}

mod end_date_provenance {
    use super::super::valid_until_provenance;

    #[test]
    fn test_an_end_date_set_by_hand_is_the_operators() {
        assert_eq!(
            valid_until_provenance(Some(Some(chrono::Utc::now()))),
            Some(true)
        );
    }

    #[test]
    fn test_a_cleared_end_date_goes_back_to_the_chain() {
        assert_eq!(valid_until_provenance(Some(None)), Some(false));
    }

    #[test]
    fn test_an_update_naming_no_end_date_leaves_the_provenance() {
        assert_eq!(valid_until_provenance(None), None);
    }
}

/// Scenario: a continuous value is written over a row whose pending state may differ from its
/// inputs'.
///
/// Expected behaviour: only a change of state is a decision, pending while any input is and
/// released once none is (Q257).
#[test]
fn test_pending_follow_moves_only_a_changed_state() {
    use crate::routes::private::readings::models::Kind;
    assert_eq!(pending_follow(false, true), Some(Kind::UnverifiedEntry));
    assert_eq!(pending_follow(true, false), Some(Kind::Verify));
    assert_eq!(pending_follow(true, true), None);
    assert_eq!(pending_follow(false, false), None);
}

fn visited(site: u128, left: Option<u128>, changed: bool) -> MovedReading {
    MovedReading {
        site_id: Some(Uuid::from_u128(site)),
        time: chrono::DateTime::parse_from_rfc3339("2025-01-10T10:00:00Z").unwrap(),
        parameter_id: Some(Uuid::from_u128(100)),
        left_site_id: left.map(Uuid::from_u128),
        changed,
    }
}

fn announced_sites(tally: &crate::common::SlotTally) -> Vec<(Uuid, usize)> {
    tally
        .events()
        .into_iter()
        .filter_map(|event| match event {
            crate::common::AppEvent::DataIngested {
                site_id: Some(site_id),
                count,
                ..
            } => Some((site_id, count)),
            _ => None,
        })
        .collect()
}

/// Scenario: a reprocess visits readings, re-deriving some and leaving others as they were.
///
/// Expected behaviour: only a reading whose columns moved is announced, at the site it is served at
/// now and, when the run moved it between sites, at the site it was served at before.
#[test]
fn test_tally_moved_counts_only_rows_that_changed() {
    let mut tally = crate::common::SlotTally::default();
    tally_moved(&mut tally, &visited(1, Some(1), false));
    assert!(
        tally.events().is_empty(),
        "a visited row left as it was announces nothing"
    );

    tally_moved(&mut tally, &visited(1, Some(1), true));
    tally_moved(&mut tally, &visited(1, None, true));
    assert_eq!(
        announced_sites(&tally),
        vec![(Uuid::from_u128(1), 2)],
        "a row that stayed on its site is counted there once"
    );
}

#[test]
fn test_tally_moved_counts_both_sites_of_a_row_moved_between_them() {
    let mut tally = crate::common::SlotTally::default();
    tally_moved(&mut tally, &visited(2, Some(1), true));
    assert_eq!(
        announced_sites(&tally),
        vec![(Uuid::from_u128(1), 1), (Uuid::from_u128(2), 1)],
        "the site the row left serves a stale series as much as the one it joined"
    );
}

/// Scenario: the drift sweep moves a continuous sensor's corrected values, which no visit owns.
///
/// Expected behaviour: the sweep's summary groups the moved rows that carry no collection event by
/// (site, parameter) with the span each covers, so the caller can recompute the stream-arm
/// calculations that read them; a visit's rows stay with the visit pairs.
#[test]
fn test_curve_drift_statement_reports_stream_slots_moved_outside_a_visit() {
    use sea_orm::sea_query::Query;
    let sql = rendered(curve_drift_statement(None));
    assert!(
        sql.contains("tgt.site_id"),
        "the write returns the site each moved row stands at: {sql}"
    );
    let slots = rendered(Query::select().expr(drifted_stream_slots()).take());
    assert!(
        slots.contains(r#"FROM "drift""#),
        "the slots are read from what the write returned: {slots}"
    );
    assert!(
        slots.contains(r#""collection_event_id" IS NULL"#),
        "a visit's rows are the chain's, not the stream arm's: {slots}"
    );
    assert!(
        slots.contains(r#"GROUP BY "site_id", "parameter_id""#),
        "one entry per slot: {slots}"
    );
    assert!(
        slots.contains(r#"MIN("time") AS "lo""#) && slots.contains(r#"MAX("time") AS "hi""#),
        "each slot carries the span it moved: {slots}"
    );
    assert!(
        sql.contains(r#"AS "stream_slots""#),
        "the sweep's summary carries them: {sql}"
    );
}

/// Expected behaviour: the jsonb the summary returns reads back as slots, timestamps in the
/// offset form Postgres renders them in.
#[test]
fn test_stream_slots_parse_from_the_summary_json() {
    let site = Uuid::from_u128(1);
    let parameter = Uuid::from_u128(2);
    let slots = stream_slots(Some(serde_json::json!([{
        "site_id": site,
        "parameter_id": parameter,
        "start": "2025-06-15T10:00:00+00:00",
        "end": "2025-06-15T12:30:00.5+00:00",
    }])));
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].site_id, site);
    assert_eq!(slots[0].parameter_id, parameter);
    assert_eq!(slots[0].start.to_rfc3339(), "2025-06-15T10:00:00+00:00");
    assert_eq!(slots[0].end.to_rfc3339(), "2025-06-15T12:30:00.500+00:00");
    assert!(stream_slots(None).is_empty(), "nothing moved");
}

/// Expected behaviour: a slot becomes the windowed `derived_recompute` scope, the shape the job
/// reads back (`site_ids`, `parameter_ids`, `start`, `end`).
#[test]
fn test_drift_slot_recompute_params_name_the_slot_and_its_span() {
    let slot = DriftSlot {
        site_id: Uuid::from_u128(1),
        parameter_id: Uuid::from_u128(2),
        start: "2025-06-15T10:00:00Z".parse().unwrap(),
        end: "2025-06-15T12:00:00Z".parse().unwrap(),
    };
    assert_eq!(
        slot.recompute_params(),
        serde_json::json!({
            "site_ids": [Uuid::from_u128(1).to_string()],
            "parameter_ids": [Uuid::from_u128(2).to_string()],
            "start": "2025-06-15T10:00:00+00:00",
            "end": "2025-06-15T12:00:00+00:00",
        })
    );
}

/// Scenario: an update to a curve names its parameter, its start, both, or neither.
///
/// Expected behaviour: any update that moves the channel or the start is checked for a collision at
/// the place the curve lands, the patched value winning over the stored one; an update moving
/// neither (coefficients, a label, an end date) has nothing to collide with.
#[test]
fn test_patched_opening_checks_where_the_curve_lands() {
    let stored_parameter = Some(Uuid::from_u128(1));
    let other_parameter = Some(Uuid::from_u128(2));
    let stored_from = chrono::DateTime::parse_from_rfc3339("2025-05-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let moved_from = chrono::DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    assert_eq!(
        patched_opening(stored_parameter, stored_from, None, None),
        None,
        "neither moved"
    );
    assert_eq!(
        patched_opening(stored_parameter, stored_from, Some(other_parameter), None),
        Some((other_parameter, stored_from)),
        "a channel move is checked at the stored start"
    );
    assert_eq!(
        patched_opening(stored_parameter, stored_from, Some(None), None),
        Some((None, stored_from)),
        "clearing the parameter lands the curve on every channel"
    );
    assert_eq!(
        patched_opening(stored_parameter, stored_from, None, Some(Some(moved_from))),
        Some((stored_parameter, moved_from)),
        "a start move is checked on the stored channel"
    );
    assert_eq!(
        patched_opening(
            stored_parameter,
            stored_from,
            Some(other_parameter),
            Some(Some(moved_from))
        ),
        Some((other_parameter, moved_from)),
        "both moved"
    );
}
