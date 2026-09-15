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

#[test]
fn test_derived_work_query_selects_standalone_definitions() {
    let sql = rendered(derived_work_query(Uuid::nil()));
    assert!(
        sql.contains(r#""d"."tool_script_id" IS NULL"#),
        "calculation-owned formulas belong to the chain: {sql}"
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

/// A continuous derived formula binds what a formula set binds: a constant by name, a site column
/// from the site's row, and a guarded absent input as NaN so `coalesce` takes its other arm.
#[test]
fn a_derived_formula_binds_constants_site_properties_and_guarded_gaps() {
    let constants = HashMap::from([("lab_temp_avg_degC".to_string(), 22.5)]);
    let parameters = vec![("WTW_Temp_degC_1".to_string(), Some(8.7))];
    let site = vec![("altitude_m".to_string(), Some(801.0))];
    let vars = bind_derived_variables(
        "WTW_Temp_degC_1 * altitude_m + lab_temp_avg_degC",
        &parameters,
        &site,
        &constants,
    )
    .unwrap();
    assert_eq!(vars["lab_temp_avg_degC"], 22.5);
    assert_eq!(vars["altitude_m"], 801.0);
    assert_eq!(vars["WTW_Temp_degC_1"], 8.7);

    let absent = vec![("lab_co2_lab_temp".to_string(), None)];
    let vars = bind_derived_variables(
        "coalesce(lab_co2_lab_temp, lab_temp_avg_degC) + 273.15",
        &absent,
        &[],
        &constants,
    )
    .unwrap();
    assert!(vars["lab_co2_lab_temp"].is_nan());
    // 22.5 + 273.15
    let value = evaluate_formula(
        "coalesce(lab_co2_lab_temp, lab_temp_avg_degC) + 273.15",
        &vars,
    )
    .unwrap();
    assert!((value - 295.65).abs() < 1e-9);
}

/// An input read outside every guard skips the instant rather than computing over NaN, and the
/// reason names the variable.
#[test]
fn an_unguarded_absent_input_skips_the_instant() {
    let absent = vec![("Dissolved_O2".to_string(), None)];
    let skipped = bind_derived_variables("Dissolved_O2 * 0.032", &absent, &[], &HashMap::new());
    assert_eq!(skipped, Err("no value for Dissolved_O2".to_string()));

    let site = vec![("altitude_m".to_string(), None)];
    let skipped = bind_derived_variables("altitude_m / 2", &[], &site, &HashMap::new());
    assert_eq!(skipped, Err("no value for altitude_m".to_string()));
}

mod non_finite_results {
    use crate::routes::private::sensor_calibrations::service::{DerivedOutcome, derived_outcome};

    /// Scenario: an input is corrected so the formula evaluates to NA at an instant whose slot
    /// already holds a number from an earlier pass.
    ///
    /// Expected behaviour: NA is the formula saying there is no value, so the slot is cleared and
    /// stops serving a number the formula no longer produces (Q172).
    #[test]
    fn na_clears_the_slot_rather_than_leaving_the_old_value() {
        assert_eq!(derived_outcome(f64::NAN), DerivedOutcome::Clear);
    }

    /// A divide by zero says the formula could not compute, not that the quantity is absent, so
    /// the value that stands is left alone until the input is corrected (Q172).
    #[test]
    fn a_divide_by_zero_refuses_and_leaves_the_stored_value_standing() {
        assert_eq!(derived_outcome(f64::INFINITY), DerivedOutcome::Refuse);
        assert_eq!(derived_outcome(f64::NEG_INFINITY), DerivedOutcome::Refuse);
    }

    #[test]
    fn a_finite_result_is_stored_at_the_value_the_formula_produced() {
        assert_eq!(derived_outcome(4.2), DerivedOutcome::Store(4.2));
        assert_eq!(derived_outcome(0.0), DerivedOutcome::Store(0.0));
        assert_eq!(derived_outcome(-1.5), DerivedOutcome::Store(-1.5));
    }
}
