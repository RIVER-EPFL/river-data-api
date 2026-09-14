use super::*;

/// The ranking, rendered, so a test can read what a caller's statement carries.
fn pick_sql(sensor_expr: &str) -> String {
    pick_calibration_query(sensor_expr).to_string(sea_orm::sea_query::PostgresQueryBuilder)
}

/// One expression, rendered on its own, so a test can read what a caller's statement carries.
fn expr_sql(e: Expr) -> String {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};
    let sql = Query::select().expr(e).to_string(PostgresQueryBuilder);
    sql.trim_start_matches("SELECT ").to_owned()
}

#[test]
fn the_import_reads_the_streams_own_rows_and_writes_only_what_moves() {
    let stream = uuid::Uuid::from_u128(1);
    let sensor = uuid::Uuid::from_u128(2);
    let sql = super::attribute_by_window_query(stream, sensor)
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        sql.contains(&format!(
            "r.stream_id = '{stream}') AND ((r.sensor_id IS NULL OR r.sensor_id = '{sensor}')"
        )),
        "the row set is the stream's unowned rows and the ones this instrument already owns: \
         {sql}"
    );
    assert!(
        sql.contains(&format!("tgt.sensor_id IS DISTINCT FROM '{sensor}'")),
        "a row already owned is only rewritten for its curve: {sql}"
    );
    assert!(
        sql.contains("tgt.calibration_id IS DISTINCT FROM picked.cal_id"),
        "so the count reports rows that moved rather than rows that matched: {sql}"
    );
}

#[test]
fn the_ranking_is_one_expression_parameterised_only_by_the_sensor() {
    let by_bind = pick_sql("$1");
    let by_column = pick_sql("r.sensor_id");
    assert_eq!(
        by_bind.replace("c.sensor_id = $1", "c.sensor_id = r.sensor_id"),
        by_column,
        "the two call shapes differ only in what names the sensor"
    );
}

#[test]
fn the_ranking_is_deterministic_for_curves_sharing_a_valid_from() {
    let sql = pick_sql("$1");
    assert!(
        sql.replace('"', "").contains("c.valid_from DESC"),
        "recency ranks first: {sql}"
    );
    assert!(
        sql.replace('"', "").contains("c.id DESC"),
        "and a tie on valid_from still resolves to one row: {sql}"
    );
}

#[test]
fn the_window_is_half_open() {
    let sql = pick_sql("$1");
    assert!(sql.contains("r.time >= c.valid_from"), "{sql}");
    assert!(
        sql.contains("r.time < COALESCE(c.valid_until, 'infinity'::timestamptz)"),
        "{sql}"
    );
}

#[test]
fn applying_a_resolved_curve_is_slope_times_raw_plus_intercept() {
    let curve = Curve {
        id: Uuid::nil(),
        slope: 2.0,
        intercept: 5.0,
    };
    assert!((curve.apply(10.0) - 25.0).abs() < f64::EPSILON);
    let identity = Curve {
        id: Uuid::nil(),
        slope: 1.0,
        intercept: 0.0,
    };
    assert!((identity.apply(10.0) - 10.0).abs() < f64::EPSILON);
}

#[test]
fn the_sql_and_rust_forms_name_the_same_operands_in_the_same_order() {
    assert_eq!(
        expr_sql(calibrated_value(
            Expr::cust("tgt.raw_value"),
            Expr::cust("picked.slope"),
            Expr::cust("picked.intercept"),
        )),
        "((picked.slope) * (tgt.raw_value)) + (picked.intercept)",
        "the set-based writers correct a row the way `apply_calibration` does"
    );
}

/// The SQL form carries the same rule as the Rust one about what a missing curve means: a row
/// that resolves neither is uncorrected, and an uncorrected row's value is null rather than a
/// copy of its raw value.
#[test]
fn the_sql_recomposition_writes_null_when_no_curve_applies() {
    use super::super::service::{CurveColumns, recomposed_value};
    let sql = expr_sql(recomposed_value(
        "tgt.raw_value",
        &CurveColumns {
            id: "picked.cal_id",
            slope: "picked.slope",
            intercept: "picked.intercept",
        },
        &CurveColumns {
            id: "sc.id",
            slope: "sc.slope",
            intercept: "sc.intercept",
        },
    ));
    assert!(
        sql.contains("WHEN ((picked.cal_id) IS NULL AND (sc.id) IS NULL) THEN NULL"),
        "{sql}"
    );
    assert!(
        sql.contains("(sc.slope) * (CASE WHEN ((picked.cal_id) IS NULL) THEN tgt.raw_value"),
        "the standard curve corrects what the base produced: {sql}"
    );
}

#[test]
fn a_standard_curve_corrects_what_the_base_calibration_produced() {
    use super::super::service::apply_curves;
    let base = Curve {
        id: Uuid::nil(),
        slope: 2.0,
        intercept: 5.0,
    };
    let standard = Curve {
        id: Uuid::nil(),
        slope: 10.0,
        intercept: 1.0,
    };
    assert!((apply_curves(10.0, Some(base), Some(standard)) - 251.0).abs() < f64::EPSILON);
    assert!((apply_curves(10.0, Some(base), None) - 25.0).abs() < f64::EPSILON);
    assert!((apply_curves(10.0, None, Some(standard)) - 101.0).abs() < f64::EPSILON);
    assert!((apply_curves(10.0, None, None) - 10.0).abs() < f64::EPSILON);
}
