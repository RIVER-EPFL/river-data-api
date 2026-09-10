use super::*;

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
    let sql = record_moved("UPDATE readings", &["site_id", "deployment_id"], 3);
    assert!(sql.contains("m.was_site_id IS DISTINCT FROM m.now_site_id"));
    assert!(sql.contains("m.was_deployment_id IS DISTINCT FROM m.now_deployment_id"));
    assert!(sql.contains("'site_id', to_jsonb(m.was_site_id)"));
    assert!(sql.contains("'site_id', to_jsonb(m.now_site_id)"));
    assert!(sql.contains("'reprocess'"), "the kind is named: {sql}");
    assert!(
        sql.contains("$3"),
        "the job is the statement's last bind: {sql}"
    );
}

#[test]
fn the_drift_sweep_repairs_exactly_what_the_recompose_writes() {
    let drifted = format!(
        "{corrected} AND tgt.calibrated_value IS DISTINCT FROM ({value})",
        corrected = corrected_rows("r"),
        value = recomposed_own_curve_value(),
    );
    let sweep = recompose_statement(&drifted, "TRUE");
    assert!(
        sweep.contains(&recomposed_own_curve_value()),
        "the sweep writes the value the recompose computes: {sweep}"
    );
    assert!(
        sweep.contains(&orphaned_correction_rows("r")),
        "and leaves an orphaned correction alone, as the recompose does: {sweep}"
    );
    assert_eq!(
        recompose_statement("r.measurement_type = 'spot'", "TRUE")
            .replace("r.measurement_type = 'spot'", &drifted),
        sweep,
        "the two statements differ only in which rows qualify"
    );
}

/// A retired curve is out of circulation: no write path and no reprocess may resolve one, and
/// the one producer of the ranking is where that is said.
#[test]
fn a_retired_curve_is_never_a_candidate() {
    for pick in [
        super::super::resolver::pick_calibration_lateral("$1"),
        super::super::resolver::pick_calibration_lateral_excluding("$2", Some("$1")),
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
    let engine = repoint_statement(
        &super::super::resolver::pick_calibration_lateral("$1"),
        "SELECTION",
        "",
    );
    let delete_hook = repoint_statement(
        &super::super::resolver::pick_calibration_lateral_excluding("$2", Some("$1")),
        "SELECTION",
        "",
    );
    let pick_of = |sql: &str| {
        let start = sql.find("LEFT JOIN LATERAL (").expect("lateral");
        let end = sql.find(") cw ON true").expect("lateral close");
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
        engine.contains("LEFT JOIN standard_curves sc"),
        "and the operator's standard curve is re-applied on top of the new base: {engine}"
    );
}
