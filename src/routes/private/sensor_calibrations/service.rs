use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource, MergeIntoActiveModel};
use sea_orm::sea_query::{
    Alias, CommonTableExpression, Condition, Expr, ExprTrait as _, Func, IntoIden, IntoTableRef,
    JoinType, OnConflict, Order, PostgresQueryBuilder, Query as SeaQuery, ReturningClause,
    SelectStatement, SubQueryStatement, TableRef, UpdateStatement, WindowStatement, WithClause,
    WithQuery,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait,
    FromQueryResult, PaginatorTrait, QueryFilter, QuerySelect, Set, Statement, TransactionTrait,
};
use std::collections::HashMap;
use uuid::Uuid;

use super::models::SensorCalibration;
use crate::routes::private::change_audit::service::{entity_revision, entity_revisions};
use crate::routes::private::constants::models as constants;
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::derived_parameters::models::definition as calculation_formulas;
use crate::routes::private::derived_parameters::service as derived;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings::decision_model as reading_decisions;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::models::{ConsumedInput, ConsumedReading};
use crate::routes::private::readings::models::{Kind, Origin};
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::sensor_deployments::models as sensor_deployments;
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::sites;
use crate::routes::private::standard_curves::models as standard_curves;
use crate::routes::private::tools::models::PinnedFormula;
use crate::routes::private::tools::service::{
    StreamCalculation, free_identifiers, read_only_through_guards, reading_revision_expr,
};

/// The reprocess engines are driven by `Job::run`, whose error type is `DbErr`. The shared bulk-write
/// and aggregate-refresh primitives report `AppError`; carrying the message through keeps a failed
/// refresh a failed job rather than a job that reports `completed`.
fn app_error_as_db_err(e: crate::error::AppError) -> sea_orm::DbErr {
    match e {
        crate::error::AppError::Database(inner) => inner,
        other => sea_orm::DbErr::Custom(other.to_string()),
    }
}

/// A computed reading's cadence, and the provenance it is written with.
const DERIVED: &str = "derived";

#[must_use]
pub fn apply_calibration(raw: f64, slope: f64, intercept: f64) -> f64 {
    slope * raw + intercept
}

/// A pair of coefficients and the row they came from, whether that row is a windowed
/// `sensor_calibration` or a hand-picked `standard_curve`. Both tables correct a value the same way,
/// so they share one struct and one arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Curve {
    pub id: Uuid,
    pub slope: f64,
    pub intercept: f64,
}

impl Curve {
    /// The corrected value for `raw`.
    #[must_use]
    pub fn apply(&self, raw: f64) -> f64 {
        apply_calibration(raw, self.slope, self.intercept)
    }
}

/// The value a reading is served at, given the curves that apply to it.
///
/// The base calibration corrects the instrument, so it runs first; a standard curve maps the
/// instrument's corrected output onto the quantity the operator wants and runs on that result. The
/// order is not recoverable from a stored row, so it is fixed here and nowhere else.
#[must_use]
pub fn apply_curves(raw: f64, base: Option<Curve>, standard: Option<Curve>) -> f64 {
    let corrected = base.map_or(raw, |c| c.apply(raw));
    standard.map_or(corrected, |c| c.apply(corrected))
}

/// [`apply_calibration`] as a SQL expression, for the set-based writers that correct millions of
/// rows in one statement. `raw_expr`, `slope_expr` and `intercept_expr` name the operands in the
/// caller's query.
#[must_use]
pub fn calibrated_value(raw: Expr, slope: Expr, intercept: Expr) -> Expr {
    slope.mul(raw).add(intercept)
}

/// What names a curve in a caller's query: the id column that says whether the curve is there at
/// all, and its two coefficients.
pub struct CurveColumns<'a> {
    pub id: &'a str,
    pub slope: &'a str,
    pub intercept: &'a str,
}

/// [`apply_curves`] as a SQL expression, for the set-based writers.
///
/// The two forms must agree on more than the arithmetic: a NULL `calibrated_value` means no curve
/// was applied, so a row that resolves neither curve is written NULL rather than a copy of its raw
/// value. Writing the raw value there would make an uncorrected reading indistinguishable from one
/// an identity curve corrected, which is the distinction the two curve references exist to keep.
#[must_use]
pub fn recomposed_value(raw_expr: &str, base: &CurveColumns, standard: &CurveColumns) -> Expr {
    let raw = || Expr::cust(raw_expr.to_string());
    let named = |name: &str| Expr::cust(name.to_string());
    let after_base = || -> Expr {
        Expr::case(named(base.id).is_null(), raw())
            .finally(calibrated_value(
                raw(),
                named(base.slope),
                named(base.intercept),
            ))
            .into()
    };
    Expr::case(
        named(base.id).is_null().and(named(standard.id).is_null()),
        Expr::null(),
    )
    .case(named(standard.id).is_null(), after_base())
    .finally(calibrated_value(
        after_base(),
        named(standard.slope),
        named(standard.intercept),
    ))
    .into()
}

/// The rows a window resolution owns, ie. everything but a grab.
///
/// A grab's base calibration is resolved once, at entry, and its standard curve is chosen by hand;
/// no window query can recover either choice, so re-deriving one would replace a deliberate
/// correction with whatever the timeline currently says. `alias` names the readings row in the
/// caller's query.
#[must_use]
pub fn window_resolved_rows(alias: &str) -> Expr {
    Expr::cust(format!("{alias}.measurement_type IS DISTINCT FROM 'spot'"))
}

/// A row whose calibration a window may author: window-resolved and not pinned to a calibration
/// (ADR 0008, M59).
#[must_use]
pub fn calibration_derivable(alias: &str) -> Expr {
    window_resolved_rows(alias).and(crate::routes::private::readings::service::not_pinned(
        alias,
        crate::routes::private::readings::models::Kind::CalibrationPin,
    ))
}

/// A row whose instrument and deployment a window may author: window-resolved and not pinned to
/// an instrument.
#[must_use]
pub fn attribution_derivable(alias: &str) -> Expr {
    window_resolved_rows(alias).and(crate::routes::private::readings::service::not_pinned(
        alias,
        crate::routes::private::readings::models::Kind::InstrumentPin,
    ))
}

/// A reading corrected by a standard curve that belongs to some other instrument.
///
/// A curve is bound to exactly one instrument (`standard_curves.sensor_id NOT NULL`), so this row
/// says its value was corrected by something that did not measure it. Splitting readings onto
/// another instrument is what produces them, and the split now asks instead (Q112, B181); these are
/// the rows that predate the question. `readings` is the reading's alias, `curve` the joined
/// `standard_curves`.
#[must_use]
pub fn foreign_curve_rows(readings: &str, curve: &str) -> String {
    format!("{curve}.sensor_id IS DISTINCT FROM {readings}.sensor_id")
}

/// A reading holding a correction no curve accounts for: it names neither curve, yet carries a
/// `calibrated_value` that is a different number from its raw value.
///
/// Nothing this code does produces such a row. Every path that drops a curve reference clears the
/// value in the same statement (`SensorCalibrationOperations::perform_delete`, the reprocess
/// engines, the identity retirement migration), so one of these arrived by a writer that supplied a
/// corrected number with no provenance: `POST /readings/batch` accepts a bare `calibrated_value`,
/// and historical imports did the same. The number is somebody's measurement, produced by a method
/// this code cannot recover.
///
/// One a calibration window covers is recomputed from that curve, and the move is appended to
/// `reading_decisions` under the job that made it, so the number it held is still readable and the
/// repair reversible (Q114). One no window covers is left where it is and reported by
/// `GET /actions/calibration_candidates`. A row whose stored value merely COPIES its raw value is
/// NOT one of these: that copy is what the old writers materialised for an uncorrected reading, it
/// carries no information, and clearing it changes nothing the API serves.
#[must_use]
pub fn orphaned_correction_rows(alias: &str) -> Expr {
    let r = Alias::new(alias);
    Expr::col((r.clone(), readings::Column::CalibrationId))
        .is_null()
        .and(Expr::col((r.clone(), readings::Column::StandardCurveId)).is_null())
        .and(Expr::col((r, readings::Column::CalibratedValue)).is_not_null())
        .and(Expr::cust(format!(
            "{alias}.calibrated_value IS DISTINCT FROM {alias}.raw_value"
        )))
}

/// Rewrite each spot reading's `calibrated_value` from the curves the row itself names.
///
/// This is the other half of [`window_resolved_rows`]. A grab keeps the curves it was entered
/// against, but the value it serves is whatever those curves produce now: editing a base
/// calibration's coefficients moves every grab that carries it, so the served value and the
/// provenance beside it cannot drift apart. Both reprocess engines call this, differing only in
/// `scope_sql`, which selects the readings (as `r`) and is written against `params`.
///
/// A grab naming neither curve is in scope too, and is written NULL: no window resolution will ever
/// claim such a row, so this is the only statement that can reach the copy of the raw value the old
/// writers left in `calibrated_value`. The one exception is [`orphaned_correction_rows`], a value
/// that is not that copy and that no curve here can reproduce; those are left exactly as they are.
pub async fn recompose_spot_readings<C: ConnectionTrait>(
    db: &C,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> Result<u64, sea_orm::DbErr> {
    recompose_from_own_curves(
        db,
        Expr::cust("r.measurement_type = 'spot'"),
        scope_sql,
        params,
    )
    .await
}

/// What the curves a row itself names produce from its raw value. Both the recompose and the drift
/// sweep judge against this one expression, so what the sweep repairs is what the recompose writes.
fn recomposed_own_curve_value() -> Expr {
    recomposed_value(
        "tgt.raw_value",
        &CurveColumns {
            id: "r.cal_id",
            slope: "r.cal_slope",
            intercept: "r.cal_intercept",
        },
        &CurveColumns {
            id: "r.std_id",
            slope: "r.std_slope",
            intercept: "r.std_intercept",
        },
    )
}

/// The readings a recomposition reads from, each row beside the curves it names, as `r`.
///
/// A subquery rather than a join list because `UPDATE ... FROM` takes tables and not joins.
/// Postgres flattens it, so the rows the statement visits are the ones the join would have given.
fn recompose_source() -> TableRef {
    let r = Alias::new("r");
    let joined = SeaQuery::select()
        .expr(Expr::cust("r.*"))
        .expr_as(Expr::cust("c.id"), Alias::new("cal_id"))
        .expr_as(Expr::cust("c.slope"), Alias::new("cal_slope"))
        .expr_as(Expr::cust("c.intercept"), Alias::new("cal_intercept"))
        .expr_as(Expr::cust("sc.id"), Alias::new("std_id"))
        .expr_as(Expr::cust("sc.slope"), Alias::new("std_slope"))
        .expr_as(Expr::cust("sc.intercept"), Alias::new("std_intercept"))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            super::models::Entity,
            Alias::new("c"),
            Condition::all().add(Expr::cust("c.id = r.calibration_id")),
        )
        .join_as(
            JoinType::LeftJoin,
            standard_curves::Entity,
            Alias::new("sc"),
            Condition::all().add(Expr::cust("sc.id = r.standard_curve_id")),
        )
        .take();
    TableRef::SubQuery(Box::new(joined), r.into_iden())
}

/// The `UPDATE readings` every curve recomposition is: `qualify` narrows which readings it writes,
/// against `r`.
fn recompose_statement(qualify: Expr) -> UpdateStatement {
    SeaQuery::update()
        .table(readings::Entity.into_table_ref().alias(Alias::new("tgt")))
        .value(
            readings::Column::CalibratedValue,
            recomposed_own_curve_value(),
        )
        .from(recompose_source())
        .and_where(Expr::cust("tgt.stream_id = r.stream_id"))
        .and_where(Expr::cust("tgt.time = r.time"))
        .and_where(Expr::cust("tgt.replicate_index = r.replicate_index"))
        .and_where(qualify)
        .take()
}

/// `qualify`, less the corrections no curve on the row accounts for. A recomposition driven by the
/// row's own curves cannot reproduce one of those numbers, so it leaves them standing.
fn own_curve_rows(qualify: Expr) -> Expr {
    qualify.and(orphaned_correction_rows("r").not())
}

/// The one statement that repoints readings onto the calibration window covering them and rebuilds
/// `calibrated_value` from it, the operator's standard curve re-applied on top.
///
/// `pick` is the ranking of the windows, `selection` chooses the rows as `r`, and `returning` is
/// what a caller that needs the instants it wrote asks for. `picked` carries the row's state before
/// the write (`p_was_*`) for a caller that records the move. The lateral is an outer join: a
/// reading no window covers has to be reachable, because a repoint must be able to clear a
/// correction as well as replace one.
pub(super) fn repoint_statement(
    pick: SelectStatement,
    selection: Expr,
    returning: Option<ReturningClause>,
) -> UpdateStatement {
    let value = recomposed_value(
        "tgt.raw_value",
        &CurveColumns {
            id: "picked.cal_id",
            slope: "picked.slope",
            intercept: "picked.intercept",
        },
        &CurveColumns {
            id: "picked.std_id",
            slope: "picked.std_slope",
            intercept: "picked.std_intercept",
        },
    );
    let mut update = SeaQuery::update();
    update
        .table(readings::Entity.into_table_ref().alias(Alias::new("tgt")))
        .value(readings::Column::CalibrationId, Expr::cust("picked.cal_id"))
        .value(readings::Column::CalibratedValue, value)
        .from(repoint_source(pick, selection))
        .and_where(Expr::cust("tgt.stream_id = picked.p_stream_id"))
        .and_where(Expr::cust("tgt.time = picked.p_time"))
        .and_where(Expr::cust("tgt.replicate_index = picked.p_replicate_index"));
    if let Some(returning) = returning {
        update.returning(returning);
    }
    update.take()
}

/// One row per reading the repoint selects: the key it is found by, the state it is about to leave,
/// the window curve ranked for it and the standard curve it already names.
fn repoint_source(pick: SelectStatement, selection: Expr) -> TableRef {
    let r = Alias::new("r");
    let picked = SeaQuery::select()
        .expr_as(Expr::cust("r.stream_id"), Alias::new("p_stream_id"))
        .expr_as(Expr::cust("r.time"), Alias::new("p_time"))
        .expr_as(
            Expr::cust("r.replicate_index"),
            Alias::new("p_replicate_index"),
        )
        .expr_as(
            Expr::cust("r.calibration_id"),
            Alias::new("p_was_calibration_id"),
        )
        .expr_as(
            Expr::cust("r.calibrated_value"),
            Alias::new("p_was_calibrated_value"),
        )
        .expr_as(Expr::cust("cw.id"), Alias::new("cal_id"))
        .expr(Expr::cust("cw.slope"))
        .expr(Expr::cust("cw.intercept"))
        .expr_as(Expr::cust("sc.id"), Alias::new("std_id"))
        .expr_as(Expr::cust("sc.slope"), Alias::new("std_slope"))
        .expr_as(Expr::cust("sc.intercept"), Alias::new("std_intercept"))
        .from_as(readings::Entity, r)
        .join_lateral(
            JoinType::LeftJoin,
            pick,
            Alias::new("cw"),
            Condition::all().add(Expr::cust("true")),
        )
        .join_as(
            JoinType::LeftJoin,
            standard_curves::Entity,
            Alias::new("sc"),
            Condition::all().add(Expr::cust("sc.id = r.standard_curve_id")),
        )
        .and_where(selection)
        .take();
    TableRef::SubQuery(Box::new(picked), Alias::new("picked").into_iden())
}

/// Rewrite `calibrated_value` from the curves each row itself names, for a corrected measurement.
///
/// `rows_sql` narrows which readings qualify and `scope_sql` selects them as `r` against `params`.
/// Idempotent, so a scope wider than the rows that changed is safe.
pub async fn recompose_from_own_curves<C: ConnectionTrait>(
    db: &C,
    rows: Expr,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> Result<u64, sea_orm::DbErr> {
    let qualify = own_curve_rows(rows.and(Expr::cust_with_values(scope_sql.to_string(), params)));
    let result = db.execute_raw(build(recompose_statement(qualify))).await?;
    Ok(result.rows_affected())
}

/// Rebuild `calibrated_value` from the curves each row names, for the readings one decision set
/// moved.
///
/// Unlike [`recompose_from_own_curves`] this reaches a row that now names no curve at all: a
/// retirement may leave a reading uncorrected, and [`orphaned_correction_rows`], which protects
/// rows written before the two curve references existed, would otherwise leave the old number
/// standing beside no curve.
pub async fn recompose_decided_rows<C: ConnectionTrait>(
    db: &C,
    set_id: uuid::Uuid,
) -> Result<u64, sea_orm::DbErr> {
    let qualify = Expr::cust_with_values(
        "EXISTS (SELECT 1 FROM reading_decisions d \
                  WHERE d.set_id = $1 \
                    AND d.stream_id = r.stream_id AND d.time = r.time \
                    AND d.replicate_index IS NOT DISTINCT FROM r.replicate_index)",
        [set_id],
    );
    let result = db.execute_raw(build(recompose_statement(qualify))).await?;
    Ok(result.rows_affected())
}

/// [`recompose_from_own_curves`] in a lifted transaction of its own.
///
/// The write paths that correct a stored value recompose after their own guarded block has
/// committed, over the whole corrected window, so this bulk `UPDATE readings` reaches compressed
/// chunks with no lift in scope. Callers already inside a guarded transaction use the plain
/// function and stay in one transaction.
pub async fn recompose_from_own_curves_guarded<C: sea_orm::TransactionTrait>(
    db: &C,
    rows: Expr,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> crate::error::AppResult<u64> {
    crate::common::bulk_write::guarded(db, async |txn| {
        recompose_from_own_curves(txn, rows, scope_sql, params)
            .await
            .map_err(crate::error::AppError::Database)
    })
    .await
}

/// Rows a curve-drift sweep can judge: the value is a claim about curves the row names, so a row
/// naming neither carries nothing to check against.
#[must_use]
pub fn corrected_rows(alias: &str) -> Expr {
    let r = Alias::new(alias);
    Expr::col((r.clone(), readings::Column::CalibrationId))
        .is_not_null()
        .or(Expr::col((r, readings::Column::StandardCurveId)).is_not_null())
}

/// The sweep's own summary row. `moved` is a `count(*)`, so it is a non-null bigint; the span and
/// the touched pairs are aggregates over a set that may be empty.
#[derive(FromQueryResult)]
struct DriftRow {
    moved: i64,
    lo: Option<DateTime<Utc>>,
    hi: Option<DateTime<Utc>>,
    touched: Option<serde_json::Value>,
}

/// What a curve-drift sweep moved: the row count and the span those rows cover.
pub struct CurveDrift {
    pub moved: u64,
    pub span: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The `(collection_event_id, parameter_id)` pairs the sweep moved, so the caller can recompute
    /// the visits whose calculations read a value that just changed under them (Q108).
    pub touched: Vec<(Uuid, Uuid)>,
}

/// The columns every ledger insert here names: the decision's own, plus the run that made the move.
fn decision_columns() -> Vec<Alias> {
    crate::routes::private::readings::service::DECISION_COLUMNS
        .split(", ")
        .chain(std::iter::once("job_id"))
        .map(Alias::new)
        .collect()
}

/// The decision this one supersedes: the newest live decision of the same kind on the same reading.
fn supersedes(alias: &str, kind: Kind) -> Expr {
    Expr::cust(format!(
        "(SELECT p.id FROM reading_decisions p \
           WHERE p.stream_id = {alias}.stream_id AND p.time = {alias}.time \
             AND p.replicate_index IS NOT DISTINCT FROM {alias}.replicate_index \
             AND p.kind = '{kind}' AND p.rolled_back_by IS NULL \
           ORDER BY p.at DESC, p.id DESC LIMIT 1)",
        kind = kind.as_str(),
    ))
}

/// One `reading_decisions` row per reading a run moved, read from the CTE `source` (as `alias`) the
/// write returned. `filter` holds the insert to the rows that actually moved; a statement whose
/// returned rows all moved by construction passes none.
#[allow(clippy::too_many_arguments)]
fn ledger_insert(
    kind: Kind,
    origin: Origin,
    source: &str,
    alias: &str,
    old: Expr,
    new: Expr,
    filter: Option<Expr>,
    job_id: Option<Uuid>,
) -> sea_orm::sea_query::InsertStatement {
    let mut select = SeaQuery::select();
    select
        .expr(Expr::cust(format!("{alias}.stream_id")))
        .expr(Expr::cust(format!("{alias}.time")))
        .expr(Expr::cust(format!("{alias}.replicate_index")))
        .expr(Expr::val(kind.as_str()))
        .expr(old)
        .expr(new)
        .expr(Expr::val("system"))
        .expr(Expr::val(origin.as_str()))
        .expr(supersedes(alias, kind))
        .expr(Expr::val(job_id))
        .from_as(Alias::new(source), Alias::new(alias));
    if let Some(filter) = filter {
        select.and_where(filter);
    }
    SeaQuery::insert()
        .into_table(reading_decisions::Entity)
        .columns(decision_columns())
        .select_from(select.take())
        .expect("the ledger insert names one column per selected expression")
        .take()
}

/// The write and its ledger insert as one statement's `WITH` clause, the write first so the insert
/// reads what it returned.
fn with_ledger(
    source: &str,
    write: UpdateStatement,
    recorded: sea_orm::sea_query::InsertStatement,
) -> WithClause {
    let mut moved = CommonTableExpression::new();
    moved
        .table_name(Alias::new(source))
        .query(SubQueryStatement::UpdateStatement(write));
    let mut ledger = CommonTableExpression::new();
    ledger
        .table_name(Alias::new("recorded"))
        .query(SubQueryStatement::InsertStatement(recorded));
    WithClause::new().cte(moved).cte(ledger).to_owned()
}

/// Rewrite every corrected reading whose stored value is not what its own curves produce.
///
/// Answers self-consistency only, so it needs no window resolution and reaches grabs. A row
/// attributed to the wrong curve for its timestamp is consistent by this measure and is the
/// reprocess engines' subject, not this one's. The span is returned for the caller's aggregate
/// refresh, since a rewritten value leaves the rollups holding the old one.
///
/// Every moved row records the move as a `curve_recompose` decision naming the run (Q118), in the
/// same statement, so a value the sweep changed is not a number the ledger cannot account for.
pub async fn sweep_curve_drift(
    db: &DatabaseConnection,
    job_id: Option<Uuid>,
) -> crate::error::AppResult<CurveDrift> {
    let drifted = own_curve_rows(corrected_rows("r").and(Expr::cust_with_exprs(
        "tgt.calibrated_value IS DISTINCT FROM ($1)",
        [recomposed_own_curve_value()],
    )));
    let update = recompose_statement(drifted)
        .returning(ReturningClause::Exprs(vec![Expr::cust(
            "tgt.stream_id, tgt.time, tgt.replicate_index, tgt.collection_event_id, \
             tgt.parameter_id, r.calibrated_value AS was, tgt.calibrated_value AS became",
        )]))
        .take();
    let recorded = ledger_insert(
        Kind::CurveRecompose,
        Origin::Janitor,
        "drift",
        "d",
        Expr::cust("jsonb_build_object('calibrated_value', to_jsonb(d.was))"),
        Expr::cust("jsonb_build_object('calibrated_value', to_jsonb(d.became))"),
        None,
        job_id,
    );
    let query = SeaQuery::select()
        .expr_as(Expr::cust("count(*)"), Alias::new("moved"))
        .expr_as(Expr::cust("min(time)"), Alias::new("lo"))
        .expr_as(Expr::cust("max(time)"), Alias::new("hi"))
        .expr_as(
            Expr::cust(
                "(SELECT jsonb_agg(DISTINCT jsonb_build_array(collection_event_id, parameter_id)) \
                    FROM drift \
                   WHERE collection_event_id IS NOT NULL AND parameter_id IS NOT NULL)",
            ),
            Alias::new("touched"),
        )
        .from(Alias::new("drift"))
        .take()
        .with(with_ledger("drift", update, recorded));

    // Drift in a chunk past the compression policy has to decompress, and the roll-up carries its
    // own `RETURNING tgt.time`, so this is `guarded` rather than `guarded_mutation`.
    let row = crate::common::bulk_write::guarded(db, async |txn| {
        txn.query_one_raw(build(query))
            .await
            .map_err(crate::error::AppError::Database)
    })
    .await?;

    let Some(row) = row else {
        return Ok(CurveDrift {
            moved: 0,
            span: None,
            touched: Vec::new(),
        });
    };
    let row = DriftRow::from_query_result(&row, "")?;
    let moved = u64::try_from(row.moved).unwrap_or(0);
    let (lo, hi) = (row.lo, row.hi);
    let touched = row
        .touched
        .and_then(|v| serde_json::from_value::<Vec<(Uuid, Uuid)>>(v).ok())
        .unwrap_or_default();
    Ok(CurveDrift {
        moved,
        span: lo.zip(hi),
        touched,
    })
}

pub fn evaluate_formula(formula: &str, variables: &HashMap<String, f64>) -> Result<f64, String> {
    let expr: meval::Expr = formula.parse().map_err(|e| format!("Parse error: {e}"))?;

    let mut ctx = meval::Context::new();
    register_guards(&mut ctx);
    for (name, value) in variables {
        ctx.var(name.clone(), *value);
    }

    expr.eval_with_context(ctx)
        .map_err(|e| format!("Evaluation error: {e}"))
}

/// The selection and missing-value guards the portal's calculations are written with, as
/// functions, because meval's grammar has operators for arithmetic only.
///
/// NaN is the portal's NA throughout: a comparison against it is false, as the portal's explicit
/// `!is.na(x)` guards make it, and `na` is how a formula says a value could not be computed.
/// The guards are scalar and total, so none of them can introduce iteration.
fn register_guards(ctx: &mut meval::Context) {
    let truthy = |x: f64| x != 0.0 && !x.is_nan();
    ctx.func3("if", move |cond, a, b| if truthy(cond) { a } else { b });
    ctx.func2("and", move |a, b| f64::from(truthy(a) && truthy(b)));
    ctx.func2("or", move |a, b| f64::from(truthy(a) || truthy(b)));
    ctx.func("not", move |a| f64::from(!truthy(a)));
    ctx.func2("lt", |a, b| f64::from(a < b));
    ctx.func2("le", |a, b| f64::from(a <= b));
    ctx.func2("gt", |a, b| f64::from(a > b));
    ctx.func2("ge", |a, b| f64::from(a >= b));
    ctx.func2("eq", |a, b| f64::from(a == b));
    ctx.func2("ne", |a, b| f64::from(a != b));
    ctx.func2("coalesce", |a, b| if a.is_nan() { b } else { a });
    ctx.func("is_missing", |a| f64::from(a.is_nan()));
    ctx.var("na", f64::NAN);
}

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct DerivedWorkRow {
    id: Uuid,
    tool_script_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    parameter_code: String,
}

/// A live reading at a derived input's slot, valued as a formula reads it: its sample's mean when
/// the sample holds any, else its calibrated value, else its raw value.
#[derive(Debug, Clone, FromQueryResult)]
pub struct InputCandidate {
    /// The instant the reading stands at, which is the instant being computed for a source read
    /// exactly and an earlier visit's for one that is held (Q230).
    pub time: DateTime<Utc>,
    pub measurement_type: Option<String>,
    pub replicate_index: i16,
    pub stream_id: Uuid,
    pub value: f64,
    pub from_mean: bool,
    /// The newest ledger sequence at the row's key, or `None` at its arrival state (Q215).
    pub revision: Option<i64>,
}

impl InputCandidate {
    fn consumed(&self) -> ConsumedReading {
        ConsumedReading {
            stream_id: self.stream_id,
            time: self.time,
            replicate_index: self.replicate_index,
            revision: self.revision,
            value: Some(self.value),
        }
    }
}

/// The reading a derived formula reads at a slot: continuous before spot, then the lowest
/// replicate, then the lowest stream. The compute and the provenance record both choose here.
#[must_use]
pub fn chosen_input(candidates: &[InputCandidate]) -> Option<&InputCandidate> {
    candidates.iter().min_by_key(|c| {
        (
            c.measurement_type.as_deref() == Some("spot"),
            c.replicate_index,
            c.stream_id,
        )
    })
}

/// One output of a calculation, at the site that configured a slot for it.
struct DerivedOutput {
    site_param_id: Uuid,
    parameter_id: Uuid,
    /// The catalog code, which is the key the set's evaluation reports the value under.
    parameter_code: String,
}

/// One calculation's work at one site: the pinned set its active version renders, and the outputs
/// the site declared a stream-arm slot for. The unit of work is the calculation, not a formula: a
/// set with steps produces its outputs from one evaluation, so resolving and evaluating per
/// formula would compute the shared steps once per output.
struct DerivedWork {
    calculation: StreamCalculation,
    derived_site_id: Uuid,
    outputs: Vec<DerivedOutput>,
}

impl DerivedWork {
    /// The catalog codes this set reads that it does not produce itself, lowercased.
    fn source_codes(&self) -> Vec<String> {
        let produced: Vec<String> = self
            .calculation
            .formulas
            .iter()
            .filter_map(|f| f.output_parameter_code.as_ref())
            .map(|c| c.to_lowercase())
            .collect();
        let mut codes: Vec<String> = Vec::new();
        for formula in &self.calculation.formulas {
            for (_, code) in &formula.sources {
                let code = code.to_lowercase();
                if !produced.contains(&code) && !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
        codes
    }

    /// The catalog codes this set produces, lowercased.
    fn output_codes(&self) -> Vec<String> {
        self.calculation
            .formulas
            .iter()
            .filter_map(|f| f.output_parameter_code.as_ref())
            .map(|c| c.to_lowercase())
            .collect()
    }
}

/// The query behind [`fetch_derived_work_items`].
///
/// One row per output slot the site fills on the stream arm: `entry_mode` is its declaration that
/// the slot computes rather than being typed into, `cadence` that a stream carries it rather than
/// a visit (Q234). The producing calculation is the one whose formula outputs the slot's
/// parameter.
fn derived_work_query(site_id: Uuid) -> SelectStatement {
    let sp = Alias::new("sp");
    let d = Alias::new("d");
    let param = Alias::new("p");
    SeaQuery::select()
        .column((sp.clone(), site_parameters::Column::Id))
        .expr_as(
            Expr::col((d.clone(), calculation_formulas::Column::ToolScriptId)),
            Alias::new("tool_script_id"),
        )
        .column((sp.clone(), site_parameters::Column::SiteId))
        .column((sp.clone(), site_parameters::Column::ParameterId))
        .expr_as(
            Expr::col((param.clone(), parameters::Column::Code)),
            Alias::new("parameter_code"),
        )
        .from_as(site_parameters::Entity, sp.clone())
        .join_as(
            JoinType::InnerJoin,
            calculation_formulas::Entity,
            d.clone(),
            Expr::col((d.clone(), calculation_formulas::Column::OutputParameterId))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .join_as(
            JoinType::InnerJoin,
            parameters::Entity,
            param.clone(),
            Expr::col((param, parameters::Column::Id))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .and_where(Expr::col((sp.clone(), site_parameters::Column::SiteId)).eq(site_id))
        .and_where(Expr::col((d, calculation_formulas::Column::ToolScriptId)).is_not_null())
        .and_where(Expr::col((sp.clone(), site_parameters::Column::EntryMode)).eq("tool"))
        .and_where(Expr::col((sp, site_parameters::Column::Cadence)).eq("high"))
        .take()
}

/// Whether a calculation computes on the stream arm at all (Q228).
///
/// A curve slot is chosen by hand per grab sample, so a set that declares one has no stream
/// equivalent: there is nobody to choose the curve at an ingest. Such a set is the visit arm's
/// alone, and a slot configured for it here computes nothing rather than computing uncorrected.
#[must_use]
pub fn runs_on_streams(formulas: &[PinnedFormula]) -> bool {
    formulas.iter().all(|f| f.curve_slot.is_none())
}

/// The calculations this site computes on a stream, each with the outputs it fills here.
async fn fetch_derived_work_items(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> Result<Vec<DerivedWork>, sea_orm::DbErr> {
    let rows = DerivedWorkRow::find_by_statement(build(derived_work_query(site_id)))
        .all(db)
        .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut items: Vec<DerivedWork> = Vec::new();
    for (tool_script_id, site_id, outputs) in group_by_calculation(rows) {
        let Some(calculation) =
            crate::routes::private::tools::service::stream_calculation(db, tool_script_id)
                .await
                .map_err(app_error_as_db_err)?
        else {
            continue;
        };
        if !runs_on_streams(&calculation.formulas) {
            continue;
        }
        items.push(DerivedWork {
            calculation,
            derived_site_id: site_id,
            outputs,
        });
    }
    Ok(items)
}

/// The work query returns one row per output slot; a calculation with two outputs at a site is
/// still one unit of work. Grouping here is what makes the set evaluate once per instant rather
/// than once per output, so the whole set is read and computed a single time.
fn group_by_calculation(rows: Vec<DerivedWorkRow>) -> Vec<(Uuid, Uuid, Vec<DerivedOutput>)> {
    let mut grouped: Vec<(Uuid, Uuid, Vec<DerivedOutput>)> = Vec::new();
    for row in rows {
        let output = DerivedOutput {
            site_param_id: row.id,
            parameter_id: row.parameter_id,
            parameter_code: row.parameter_code,
        };
        if let Some((_, _, outputs)) = grouped
            .iter_mut()
            .find(|(script_id, _, _)| *script_id == row.tool_script_id)
        {
            outputs.push(output);
        } else {
            grouped.push((row.tool_script_id, row.site_id, vec![output]));
        }
    }
    grouped
}

/// The order the site's calculations evaluate in: a calculation reading a parameter another one
/// produces runs after it, so a chained output reads the value this pass just stored rather than
/// the one the last pass left. The relation is between calculations, since a set's own formulas
/// are already ordered by `in_order`.
fn build_evaluation_order(work_items: &[DerivedWork]) -> Result<Vec<usize>, sea_orm::DbErr> {
    let deps: Vec<Vec<usize>> = work_items
        .iter()
        .map(|item| {
            let sources = item.source_codes();
            work_items
                .iter()
                .enumerate()
                .filter(|(_, other)| {
                    !std::ptr::eq(*other, item)
                        && other
                            .output_codes()
                            .iter()
                            .any(|code| sources.contains(code))
                })
                .map(|(index, _)| index)
                .collect()
        })
        .collect();

    crate::common::dependency::order(&deps).map_err(|cycle| {
        let members: Vec<&str> = cycle
            .iter()
            .map(|&idx| work_items[idx].calculation.name.as_str())
            .collect();
        sea_orm::DbErr::Custom(format!(
            "Calculations form a dependency cycle and cannot be evaluated: {}",
            members.join(", ")
        ))
    })
}

/// The stream one output writes on: the slot's own, minted on first use.
async fn get_or_create_derived_stream(
    db: &DatabaseConnection,
    tool_name: &str,
    site_id: Uuid,
    output: &DerivedOutput,
) -> Result<Uuid, sea_orm::DbErr> {
    if let Some(id) = stream_for_slot(db, output.site_param_id).await? {
        return Ok(id);
    }

    let source_key = format!("{}_{}_{}", tool_name, output.parameter_code, site_id);
    let stream_id = Uuid::new_v4();
    let now = Utc::now();
    data_streams::Entity::insert(data_streams::ActiveModel {
        id: Set(stream_id),
        source_system: Set("derived".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some(tool_name.to_string())),
        site_parameter_id: Set(Some(output.site_param_id)),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(Some(now.into())),
        measurement_type: Set(Some("derived".to_string())),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            data_streams::Column::SourceSystem,
            data_streams::Column::SourceKey,
        ])
        .update_column(data_streams::Column::SiteParameterId)
        .to_owned(),
    )
    .exec(db)
    .await?;

    stream_for_slot(db, output.site_param_id)
        .await?
        .ok_or_else(|| {
            sea_orm::DbErr::Custom(
                "Failed to retrieve derived data stream after upsert".to_string(),
            )
        })
}

/// The stream a slot already has, if any.
async fn stream_for_slot(
    db: &DatabaseConnection,
    site_parameter_id: Uuid,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    data_streams::Entity::find()
        .filter(data_streams::Column::SiteParameterId.eq(site_parameter_id))
        .select_only()
        .column(data_streams::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await
}

/// What one derived slot's pass did at one instant. The instant is the caller's, which is what
/// lets a run report a slot once for the whole pass rather than once per instant (Q172).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotPass {
    /// A value was written.
    Stored,
    /// The formula produced a number that is not finite: the divide by zero, refused.
    Refused,
}

/// One slot of a site, and what the pass did there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedSlot {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    /// The calculation that produced the value.
    pub calculation_id: Uuid,
    pub pass: SlotPass,
}

/// Recompute every derived slot of a site at one instant, returning the slots that stored a value
/// and the slots that refused. A slot that had nothing to do here is in neither.
pub async fn recalculate_derived_at_timestamp(
    db: &DatabaseConnection,
    site_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<DerivedSlot>, sea_orm::DbErr> {
    let work_items = fetch_derived_work_items(db, site_id).await?;
    if work_items.is_empty() {
        return Ok(Vec::new());
    }

    let ordered = build_evaluation_order(&work_items)?;
    let mut passes = Vec::new();
    for idx in ordered {
        passes.extend(evaluate_set_and_upsert(db, &work_items[idx], time).await?);
    }
    Ok(passes)
}

/// A built query as the statement sea-orm executes.
fn build(query: impl sea_orm::sea_query::QueryStatementBuilder) -> Statement {
    let (sql, values) = query.build_any(&PostgresQueryBuilder);
    Statement::from_sql_and_values(DatabaseBackend::Postgres, sql, values)
}

/// The last instant a parameter was measured at a site, at or before `time`: what a held source
/// binds to (Q230). A withdrawn or flagged row is not a measurement, here as in the binder.
fn last_measured_query(
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> sea_orm::sea_query::SelectStatement {
    let h = Alias::new("h");
    SeaQuery::select()
        .expr(Func::max(Expr::col((h.clone(), readings::Column::Time))))
        .from_as(readings::Entity, h.clone())
        .and_where(Expr::col((h.clone(), readings::Column::SiteId)).eq(site_id))
        .and_where(Expr::col((h.clone(), readings::Column::ParameterId)).eq(parameter_id))
        .and_where(Expr::col((h.clone(), readings::Column::Time)).lte(time))
        .and_where(Expr::col((h, readings::Column::WithdrawnAt)).is_null())
        .and_where(Expr::cust("h.is_flagged IS NOT TRUE"))
        .take()
}

/// The value one derived input resolves to at `time`, and the cadence it came from.
///
/// Deterministic input pick when a sensor point and a grab share the timestamp: prefer the
/// continuous reading, then tie-break by stream_id (stable across VACUUM). A withdrawn or flagged
/// row is not a measurement, and a sample whose members are all gone carries n = 0 with a NULL
/// mean, so neither may reach the formula.
///
/// `held` is Q230's second rule: the rows of the last instant at or before `time` rather than the
/// rows at it, for an input the lab measures at a visit and a calculation on a stream reads. The
/// whole instant is taken either way, because a replicate group's mean stands on its members.
fn input_value_query(
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    held: bool,
) -> sea_orm::sea_query::SelectStatement {
    let r = Alias::new("r");
    let smp = Alias::new("smp");
    SeaQuery::select()
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::MeasurementType))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::StreamId))
        .expr_as(
            Expr::cust(
                "COALESCE(CASE WHEN smp.n > 0 THEN smp.mean END, r.calibrated_value, r.raw_value)",
            ),
            Alias::new("value"),
        )
        .expr_as(
            Expr::cust("COALESCE(smp.n > 0 AND smp.mean IS NOT NULL, false)"),
            Alias::new("from_mean"),
        )
        .expr_as(reading_revision_expr(), Alias::new("revision"))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            smp.clone(),
            Expr::col((smp, samples::Column::Id)).equals((r.clone(), readings::Column::SampleId)),
        )
        .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
        .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).eq(parameter_id))
        .and_where(if held {
            // The last instant this parameter was measured at, at or before the one being
            // computed. A parameter with nothing before it selects NULL and binds nothing, which
            // is what an input the instant holds no value for already does.
            Expr::col((r.clone(), readings::Column::Time)).in_subquery(last_measured_query(
                site_id,
                parameter_id,
                time,
            ))
        } else {
            Expr::col((r.clone(), readings::Column::Time)).eq(time)
        })
        .and_where(Expr::col((r, readings::Column::WithdrawnAt)).is_null())
        .and_where(Expr::cust("r.is_flagged IS NOT TRUE"))
        .take()
}

/// The variables one instant binds, and every input as it was read (Q215).
struct ResolvedDerived {
    /// The inputs the set binds, by variable name, as [`evaluate`] takes them.
    inputs: HashMap<String, f64>,
    /// The constants the set names, resolved once for the whole set.
    constants: HashMap<String, f64>,
    /// Every input as it was read (Q215), keyed by the variable it bound.
    consumed: Vec<ConsumedInput>,
}

/// The parameter a set reads into one variable, resolved from the catalog once per pass.
async fn source_parameter_ids<C: ConnectionTrait>(
    db: &C,
    codes: &[String],
) -> Result<HashMap<String, Uuid>, sea_orm::DbErr> {
    if codes.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Uuid, String)> = parameters::Entity::find()
        .filter(
            Expr::expr(Func::lower(Expr::col(parameters::Column::Code)))
                .is_in(codes.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>()),
        )
        .select_only()
        .column(parameters::Column::Id)
        .column(parameters::Column::Code)
        .into_tuple()
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, code)| (code.to_lowercase(), id))
        .collect())
}

/// Resolve every source the set reads at one instant, once for the whole set.
///
/// A variable the instant holds no value for is left unbound rather than refused here: the set
/// evaluator decides per formula whether a missing value is a guarded NA or a skip, so a set
/// whose second formula reads a parameter this visit lacks still computes its first.
async fn resolve_set_inputs(
    db: &DatabaseConnection,
    item: &DerivedWork,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<ResolvedDerived, sea_orm::DbErr> {
    let formulas = &item.calculation.formulas;
    let produced: Vec<String> = item.output_codes();
    let catalog = source_parameter_ids(db, &item.source_codes()).await?;

    let mut inputs: HashMap<String, f64> = HashMap::new();
    let mut consumed: Vec<ConsumedInput> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for formula in formulas {
        for (variable, code) in &formula.sources {
            if seen.contains(variable) || produced.contains(&code.to_lowercase()) {
                continue;
            }
            seen.push(variable.clone());
            let Some(&parameter_id) = catalog.get(&code.to_lowercase()) else {
                continue;
            };
            let held = formula.held.iter().any(|v| v == variable);
            let candidates = InputCandidate::find_by_statement(build(input_value_query(
                item.derived_site_id,
                parameter_id,
                time,
                held,
            )))
            .all(db)
            .await?;
            let Some(input) = chosen_input(&candidates) else {
                continue;
            };
            // A mean stands on every member of its sample; a single row on itself.
            let members: Vec<ConsumedReading> = if input.from_mean {
                candidates
                    .iter()
                    .filter(|c| c.from_mean)
                    .map(InputCandidate::consumed)
                    .collect()
            } else {
                vec![input.consumed()]
            };
            consumed.push(ConsumedInput {
                variable: variable.clone(),
                // The rule that reached this reading, so the record says why a member stands at
                // an instant the run did not compute at, and a reader can tell a held value from
                // a mis-stamped one (Q230).
                alignment: held.then(|| derived::HOLD.to_string()),
                kind: if input.from_mean { "mean" } else { "reading" }.to_string(),
                subject: None,
                property: None,
                revision: None,
                members,
                value: serde_json::json!(input.value),
            });
            inputs.insert(variable.clone(), input.value);
        }
    }

    let properties: Vec<(String, String)> = formulas
        .iter()
        .flat_map(|f| f.site_sources.iter().cloned())
        .collect();
    let site_properties = site_property_values(db, item.derived_site_id, &properties).await?;
    if !site_properties.is_empty() {
        let subject = format!("site:{}", item.derived_site_id);
        let revision = entity_revision(db, &subject)
            .await
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        for ((variable, value), (_, property)) in site_properties.iter().zip(&properties) {
            if let Some(value) = value {
                inputs.insert(variable.clone(), *value);
            }
            consumed.push(ConsumedInput {
                variable: variable.clone(),
                alignment: None,
                kind: "site".to_string(),
                subject: Some(subject.clone()),
                property: Some(property.clone()),
                revision,
                members: Vec::new(),
                value: value.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            });
        }
    }

    let declared: Vec<String> = inputs
        .keys()
        .cloned()
        .chain(site_properties.iter().map(|(variable, _)| variable.clone()))
        .chain(
            formulas
                .iter()
                .filter(|f| f.intermediate)
                .map(|f| f.code.clone()),
        )
        .collect();
    let set_text = formulas
        .iter()
        .map(|f| f.formula.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let (constants, constants_read) = constants_consumed(db, &set_text, &declared).await?;
    consumed.extend(constants_read);

    Ok(ResolvedDerived {
        inputs,
        constants,
        consumed,
    })
}

/// The capture one output of a set carries: every input its own formula bound, named as it was
/// read, plus the formula itself. A step it read is captured as the number that step computed,
/// under the step's own code, so the replay is the output's arithmetic over what it consumed and
/// needs nothing from the store.
async fn output_capture<C: ConnectionTrait>(
    db: &C,
    formula: &PinnedFormula,
    formula_id: Option<Uuid>,
    evaluated: &crate::routes::private::tools::models::Evaluated,
    resolved: &ResolvedDerived,
    steps: &HashMap<String, (Uuid, f64)>,
) -> Result<Vec<ConsumedInput>, sea_orm::DbErr> {
    let mut consumed: Vec<ConsumedInput> = Vec::new();
    for (variable, value) in &evaluated.bindings {
        if let Some(input) = resolved.consumed.iter().find(|c| &c.variable == variable) {
            consumed.push(input.clone());
            continue;
        }
        if let Some((step_id, computed)) = steps.get(&variable.to_lowercase()) {
            let subject = format!("calculation_formula:{step_id}");
            consumed.push(ConsumedInput {
                variable: variable.clone(),
                alignment: None,
                kind: "computed".to_string(),
                revision: entity_revision(db, &subject)
                    .await
                    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?,
                subject: Some(subject),
                property: None,
                members: Vec::new(),
                value: serde_json::json!(computed),
            });
            continue;
        }
        consumed.push(ConsumedInput {
            variable: variable.clone(),
            alignment: None,
            kind: "constant".to_string(),
            subject: None,
            property: None,
            revision: None,
            members: Vec::new(),
            value: serde_json::json!(value),
        });
    }
    let subject = formula_id.map(|id| format!("calculation_formula:{id}"));
    let revision = match &subject {
        Some(subject) => entity_revision(db, subject)
            .await
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?,
        None => None,
    };
    consumed.push(ConsumedInput {
        variable: formula.code.clone(),
        alignment: None,
        kind: "step".to_string(),
        revision,
        subject,
        property: None,
        members: Vec::new(),
        value: serde_json::Value::String(formula.formula.clone()),
    });
    Ok(consumed)
}

/// The formula a captured set carries, and the numbers it was evaluated over.
///
/// A derived computation records its own arithmetic: the `step` entry's value is the formula text
/// as it stood, so a replay needs no version lookup and cannot read a formula the computation did
/// not use.
pub struct CapturedSet<'a> {
    pub formula: &'a str,
    pub variables: HashMap<String, f64>,
}

/// Read a captured set out of the `consumed` a `derived_computed` or `formula_transition`
/// decision stored. Every entry but the step binds its variable to the number it held; an entry
/// whose value is not a number is a value that was not there, which binds as NA where the formula
/// only reads it through a guard and refuses the replay otherwise, exactly as the computation did.
pub fn captured_set(consumed: &[ConsumedInput]) -> Result<CapturedSet<'_>, String> {
    let step = consumed
        .iter()
        .find(|input| input.kind == "step")
        .ok_or_else(|| "the captured set names no formula".to_string())?;
    let formula = step
        .value
        .as_str()
        .ok_or_else(|| "the captured set's formula is not text".to_string())?;
    let mut variables = HashMap::new();
    for input in consumed.iter().filter(|input| input.kind != "step") {
        match input.value.as_f64() {
            Some(value) => {
                variables.insert(input.variable.clone(), value);
            }
            None if read_only_through_guards(formula, &input.variable) => {
                variables.insert(input.variable.clone(), f64::NAN);
            }
            None => return Err(format!("no value for {}", input.variable)),
        }
    }
    Ok(CapturedSet { formula, variables })
}

/// The formula a derived value was computed with, run again over the values it consumed.
///
/// Nothing is read from the store and nothing is written: the answer is the arithmetic behind the
/// stored number, which is what lets a reader check it against the inputs as they stand now.
pub fn replay_captured(consumed: &[ConsumedInput]) -> Result<f64, String> {
    let set = captured_set(consumed)?;
    evaluate_formula(set.formula, &set.variables)
}

/// The constants a formula names, read from the constants table: every free identifier that is
/// not one of its declared variables. A name that is neither is left unbound, so the evaluation
/// The constants a formula names, and each one as it was read: its row and revision (Q215).
async fn constants_consumed<C: ConnectionTrait>(
    db: &C,
    formula: &str,
    declared: &[String],
) -> Result<(HashMap<String, f64>, Vec<ConsumedInput>), sea_orm::DbErr> {
    let names: Vec<String> = free_identifiers(formula)
        .into_iter()
        .filter(|name| !declared.contains(name))
        .collect();
    if names.is_empty() {
        return Ok((HashMap::new(), Vec::new()));
    }
    let rows = constants::Entity::find()
        .filter(constants::Column::Name.is_in(names))
        .all(db)
        .await?;
    let subjects: Vec<String> = rows.iter().map(|c| format!("constant:{}", c.id)).collect();
    let revisions = entity_revisions(db, &subjects)
        .await
        .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    let consumed = rows
        .iter()
        .map(|c| {
            let subject = format!("constant:{}", c.id);
            ConsumedInput {
                variable: c.name.clone(),
                alignment: None,
                kind: "constant".to_string(),
                revision: revisions.get(&subject).copied(),
                subject: Some(subject),
                property: None,
                members: Vec::new(),
                value: serde_json::json!(c.value),
            }
        })
        .collect();
    Ok((
        rows.into_iter().map(|c| (c.name, c.value)).collect(),
        consumed,
    ))
}

/// The value of each `(variable, site column)` on the site's own row, `None` where the column is
/// null or not a number.
pub(crate) async fn site_property_values<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    properties: &[(String, String)],
) -> Result<Vec<(String, Option<f64>)>, sea_orm::DbErr> {
    if properties.is_empty() {
        return Ok(Vec::new());
    }
    let site = sites::Entity::find_by_id(site_id)
        .one(db)
        .await?
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    Ok(properties
        .iter()
        .map(|(variable, property)| {
            let value = site
                .as_ref()
                .and_then(|row| row.get(property))
                .and_then(serde_json::Value::as_f64);
            (variable.clone(), value)
        })
        .collect())
}

/// The statement [`unattribute_derived_at`] runs.
fn unattribute_statement(
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> UpdateStatement {
    SeaQuery::update()
        .table(readings::Entity)
        .value(readings::Column::SiteId, Expr::val(Option::<Uuid>::None))
        .and_where(readings::Column::SiteId.eq(site_id))
        .and_where(readings::Column::ParameterId.eq(parameter_id))
        .and_where(readings::Column::Time.eq(time))
        .and_where(readings::Column::MeasurementType.eq(DERIVED))
        .take()
}

/// Clear the site off a stored derived row, the unattributed state a recalled input leaves it in.
async fn unattribute_derived_at(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    crate::common::bulk_write::guarded(db, async |txn| {
        crate::common::bulk_write::mutation_rows(
            txn,
            unattribute_statement(site_id, parameter_id, time),
        )
        .await?;
        Ok(())
    })
    .await
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    Ok(())
}

/// The stored derived row at a slot instant, as the transition record compares against.
#[derive(sea_orm::FromQueryResult)]
struct StoredDerived {
    raw_value: Option<f64>,
    derived_version_id: Option<Uuid>,
}

/// Record a derived value's move onto a new formula version, if it moved.
///
/// The kind projects no column: the upsert that follows is what writes the value. Nothing is
/// recorded when no row is stored yet, because a first computation came from no version; the
/// caller is told so, and records the arrival once the row exists.
async fn record_formula_transition<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    result: f64,
    version: Option<Uuid>,
    consumed: &[ConsumedInput],
) -> Result<bool, sea_orm::DbErr> {
    let stored = readings::Entity::find()
        .filter(readings::Column::StreamId.eq(stream_id))
        .filter(readings::Column::Time.eq(time))
        .filter(readings::Column::ReplicateIndex.eq(0_i16))
        .select_only()
        .column(readings::Column::RawValue)
        .column(readings::Column::DerivedVersionId)
        .into_model::<StoredDerived>()
        .one(db)
        .await?;
    let Some(prior) = stored else { return Ok(true) };
    if prior.raw_value == Some(result) && prior.derived_version_id == version {
        return Ok(false);
    }
    crate::routes::private::readings::service::record(
        db,
        &crate::routes::private::readings::service::Decision {
            key: crate::routes::private::readings::service::DecisionKey {
                stream_id,
                time,
                replicate_index: Some(0),
            },
            kind: crate::routes::private::readings::models::Kind::FormulaTransition,
            new: serde_json::json!({
                "raw_value": result,
                "derived_version_id": version,
                "consumed": consumed,
            }),
            actor: "system".to_string(),
            reason: None,
            origin: crate::routes::private::readings::models::Origin::System,
            set_id: None,
        },
    )
    .await
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    Ok(false)
}

/// The statement that stores a computed derived value at one slot instant.
///
/// `raw_value` is the authoritative column for a derived reading and `calibrated_value` is always
/// NULL. A derived value is a computed quantity, not an instrument measurement plus a correction:
/// it has no sensor, no curve and therefore nothing a calibration id could point at, which is
/// exactly the state this model spells NULL. Every consumer reads
/// COALESCE(calibrated_value, raw_value), the four continuous aggregates included, so the computed
/// number is what is served either way, but only this arrangement survives a recomposition pass,
/// which resolves no curve for a sensor-less row and would otherwise clear the value outright.
///
/// The slot is re-asserted on conflict as well as on insert: a row this engine unattributed when
/// its inputs stopped resolving is the same row it writes when they resolve again, and leaving
/// `site_id` NULL there would recompute a value nothing serves.
fn derived_upsert(
    stream_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    result: f64,
    version: Option<Uuid>,
) -> sea_orm::sea_query::InsertStatement {
    SeaQuery::insert()
        .into_table(readings::Entity)
        .columns([
            readings::Column::StreamId,
            readings::Column::SiteId,
            readings::Column::ParameterId,
            readings::Column::Time,
            readings::Column::RawValue,
            readings::Column::CalibratedValue,
            readings::Column::ReplicateIndex,
            readings::Column::MeasurementType,
            readings::Column::ProvenanceKind,
            readings::Column::DerivedVersionId,
        ])
        .values_panic([
            Expr::val(stream_id),
            Expr::val(site_id),
            Expr::val(parameter_id),
            Expr::val(time),
            Expr::val(result),
            Expr::null(),
            Expr::val(0_i16),
            Expr::val(DERIVED),
            Expr::val(DERIVED),
            Expr::val(version),
        ])
        .on_conflict(
            OnConflict::columns([
                readings::Column::StreamId,
                readings::Column::Time,
                readings::Column::ReplicateIndex,
            ])
            .update_columns([
                readings::Column::RawValue,
                readings::Column::CalibratedValue,
                readings::Column::MeasurementType,
                readings::Column::SiteId,
                readings::Column::ParameterId,
                readings::Column::DerivedVersionId,
            ])
            .to_owned(),
        )
        .take()
}

/// Evaluate one calculation's set at one instant and store every output the site fills here.
///
/// The set is evaluated once: a step two outputs read is computed once, and each output is stored
/// on its own stream with its own decision. A formula the instant cannot bind is skipped by the
/// evaluator and the rest of the set still stores (Q228).
async fn evaluate_set_and_upsert(
    db: &DatabaseConnection,
    item: &DerivedWork,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<DerivedSlot>, sea_orm::DbErr> {
    use crate::routes::private::tools::service::evaluate;

    let resolved = resolve_set_inputs(db, item, time).await?;
    let evaluated = match evaluate(
        &item.calculation.formulas,
        &resolved.inputs,
        &resolved.constants,
        &HashMap::new(),
    ) {
        Ok(cells) => cells,
        Err(error) => {
            tracing::warn!(
                calculation = %item.calculation.name,
                time = %time,
                error,
                "Continuous calculation could not be evaluated"
            );
            return Ok(Vec::new());
        }
    };

    let ordered = crate::routes::private::tools::service::in_order(&item.calculation.formulas)
        .map_err(sea_orm::DbErr::Custom)?;
    let codes: Vec<String> = item
        .calculation
        .formulas
        .iter()
        .map(|f| f.code.clone())
        .collect();
    let formula_ids = formula_ids_by_code(db, &codes).await?;
    // Every step's number, so an output that read one captures the value rather than the code.
    let steps: HashMap<String, (Uuid, f64)> = ordered
        .iter()
        .zip(&evaluated)
        .filter(|(formula, _)| formula.intermediate)
        .filter_map(|(formula, cell)| {
            let id = *formula_ids.get(&formula.code.to_lowercase())?;
            Some((formula.code.to_lowercase(), (id, cell.value?)))
        })
        .collect();

    let mut passes = Vec::new();
    for (formula, cell) in ordered.iter().zip(&evaluated) {
        let Some(code) = formula.output_parameter_code.as_ref() else {
            continue;
        };
        let Some(output) = item
            .outputs
            .iter()
            .find(|o| o.parameter_code.eq_ignore_ascii_case(code))
        else {
            continue;
        };
        let site_id = item.derived_site_id;
        // A divide by zero is refused, not cleared (Q172): the value that stands stays served and
        // the finding is the only thing that says the formula stopped computing.
        if cell.refused {
            passes.push(DerivedSlot {
                site_id,
                parameter_id: output.parameter_id,
                calculation_id: item.calculation.id,
                pass: SlotPass::Refused,
            });
            continue;
        }
        // Everything else with no value is the unattribution arm: an input the formula reads
        // outside a guard went away, or the arithmetic came out NA. Either way the slot stops
        // serving a number it no longer computes.
        let Some(value) = cell.value else {
            unattribute_derived_at(db, site_id, output.parameter_id, time).await?;
            continue;
        };

        let consumed = output_capture(
            db,
            formula,
            formula_ids.get(&formula.code.to_lowercase()).copied(),
            cell,
            &resolved,
            &steps,
        )
        .await?;
        let stream_id =
            get_or_create_derived_stream(db, &item.calculation.name, site_id, output).await?;
        // The value names the version of the set that made it, which is what lets a reader open
        // the formula text behind a number computed months ago (C310).
        let version = item.calculation.active_version_id;
        let parameter_id = output.parameter_id;
        // A recompute that moves a stored value or the version it names records the move (Q116),
        // so a person opening the value reads what it was and which formula edit changed it.
        // Recorded before the write, because the decision captures `old` from the row as it still
        // stands; a first insert is not a transition, and a pass that changes neither is not a
        // decision. The decision, the write and the arrival commit together, so the value and what
        // it consumed (Q215) cannot be read apart.
        crate::common::bulk_write::guarded(db, async |txn| {
            let born = record_formula_transition(txn, stream_id, time, value, version, &consumed)
                .await
                .map_err(crate::error::AppError::from)?;
            crate::common::bulk_write::mutation_rows(
                txn,
                derived_upsert(stream_id, site_id, parameter_id, time, value, version),
            )
            .await?;
            // A slot's first number is a change to the readings as much as a move is (Q57, Q118),
            // and it is recorded after the write because the decision reads the row it is about.
            if born {
                record_derived_arrival(txn, stream_id, time, &consumed)
                    .await
                    .map_err(crate::error::AppError::from)?;
            }
            follow_inputs_pending(txn, stream_id, time, &consumed).await?;
            Ok(())
        })
        .await
        .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        passes.push(DerivedSlot {
            site_id,
            parameter_id,
            calculation_id: item.calculation.id,
            pass: SlotPass::Stored,
        });
    }
    Ok(passes)
}

/// The `calculation_formulas` row behind each code of a set, so a captured step names the formula
/// it came from. Read by code rather than by owner, because a step the calculation declared
/// rather than wrote belongs to no calculation (Q156) and is still part of the set.
async fn formula_ids_by_code<C: ConnectionTrait>(
    db: &C,
    codes: &[String],
) -> Result<HashMap<String, Uuid>, sea_orm::DbErr> {
    if codes.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(calculation_formulas::Entity::find()
        .filter(
            Expr::expr(Func::lower(Expr::col(calculation_formulas::Column::Code)))
                .is_in(codes.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>()),
        )
        .select_only()
        .column(calculation_formulas::Column::Id)
        .column(calculation_formulas::Column::Code)
        .into_tuple::<(Uuid, String)>()
        .all(db)
        .await?
        .into_iter()
        .map(|(id, code)| (code.to_lowercase(), id))
        .collect())
}

/// The decision that brings a stored value's pending state to its inputs': pending while any input
/// it was computed from is, released once none is (Q257). `None` where the state already agrees.
#[must_use]
pub fn pending_follow(
    stored: bool,
    pending: bool,
) -> Option<crate::routes::private::readings::models::Kind> {
    use crate::routes::private::readings::models::Kind;
    match (stored, pending) {
        (false, true) => Some(Kind::UnverifiedEntry),
        (true, false) => Some(Kind::Verify),
        _ => None,
    }
}

/// Hold a continuous value pending while an input it was computed from awaits verification, and
/// release it once none does. The state moves by a decision, so the record says why it is held.
async fn follow_inputs_pending<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    consumed: &[ConsumedInput],
) -> crate::error::AppResult<()> {
    use crate::routes::private::readings::service::{Decision, DecisionKey, record};
    let pending = crate::routes::private::readings::consumed::any_pending(db, consumed).await?;
    let stored = readings::Entity::find()
        .filter(readings::Column::StreamId.eq(stream_id))
        .filter(readings::Column::Time.eq(time))
        .filter(readings::Column::ReplicateIndex.eq(0_i16))
        .select_only()
        .column(readings::Column::Unverified)
        .into_tuple::<bool>()
        .one(db)
        .await?;
    let Some(kind) = pending_follow(stored.unwrap_or(false), pending) else {
        return Ok(());
    };
    record(
        db,
        &Decision {
            key: DecisionKey {
                stream_id,
                time,
                replicate_index: Some(0),
            },
            kind,
            new: serde_json::json!({ "unverified": pending }),
            actor: "system".to_string(),
            reason: Some(
                if pending {
                    "computed from an input awaiting verification"
                } else {
                    "every input it was computed from is verified"
                }
                .to_string(),
            ),
            origin: crate::routes::private::readings::models::Origin::System,
            set_id: None,
        },
    )
    .await
    .map(|_| ())
}

/// Record the arrival of a derived value, the state it arrived in read from the row itself.
async fn record_derived_arrival<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    consumed: &[ConsumedInput],
) -> Result<(), sea_orm::DbErr> {
    crate::routes::private::readings::service::record_many(
        db,
        crate::routes::private::readings::models::Kind::DerivedComputed,
        {
            use crate::routes::private::collection_events::flows::row;
            Condition::all()
                .add(row(readings::Column::StreamId).eq(stream_id))
                .add(row(readings::Column::Time).eq(time))
                .add(row(readings::Column::ReplicateIndex).eq(0_i16))
        },
        crate::routes::private::readings::service::NewValue::BornWith(
            serde_json::json!({ "consumed": consumed }),
        ),
        "system",
        None,
        crate::routes::private::readings::models::Origin::System,
        None,
    )
    .await
    .map(|_| ())
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))
}

/// The window chain each of a sensor's calibrations is bounded by.
///
/// Windows chain within a (sensor, parameter): a multi-parameter instrument holds one calibration
/// timeline per parameter, so `LEAD` partitions by `parameter_id`, never letting one parameter's
/// next calibration truncate another's window. Instant curves (grab curves) are matched by
/// `calibration_id` and never windowed, so they take no part.
///
/// `id` breaks a tie on `valid_from` so the chain is single-valued, and the guard refuses to write
/// a zero-width `valid_until = valid_from` window, which would leave a curve the operator can see
/// applying to nothing. Duplicate instants are refused at create (`SensorCalibrationOperations`);
/// the guard covers rows loaded outside the API.
///
/// A chain-written bound is derived state and is rebuilt from scratch each time. An
/// operator-written one (`valid_until_explicit`) is data, so it is only ever shortened, and then
/// only far enough to keep windows non-overlapping, because the resolver depends on at most one
/// curve covering an instant. `LEAST` ignores a NULL `next_from`, so an explicit bound on the
/// newest curve survives. This is the same policy [`deployment_chain_statement`] applies to a
/// deployment's end date.
fn calibration_chain_statement(sensor_id: Uuid) -> UpdateStatement {
    let ordered = SeaQuery::select()
        .column(super::models::Column::Id)
        .column(super::models::Column::ValidFrom)
        .expr_window_as(
            Expr::cust("LEAD(valid_from)"),
            WindowStatement::partition_by(super::models::Column::ParameterId)
                .order_by(super::models::Column::ValidFrom, Order::Asc)
                .order_by(super::models::Column::Id, Order::Asc)
                .take(),
            Alias::new("next_from"),
        )
        .from(super::models::Entity)
        .and_where(super::models::Column::SensorId.eq(sensor_id))
        .and_where(super::models::Column::RetiredAt.is_null())
        .take();
    SeaQuery::update()
        .table(
            super::models::Entity
                .into_table_ref()
                .alias(Alias::new("sc")),
        )
        .value(
            super::models::Column::ValidUntil,
            Expr::cust(
                "CASE WHEN sc.valid_until_explicit \
                      THEN LEAST(sc.valid_until, ordered.next_from) \
                      ELSE ordered.next_from END",
            ),
        )
        .from(TableRef::SubQuery(
            Box::new(ordered),
            Alias::new("ordered").into_iden(),
        ))
        .and_where(Expr::cust("sc.id = ordered.id"))
        .and_where(Expr::cust_with_values("sc.sensor_id = $1", [sensor_id]))
        .and_where(Expr::cust(
            "(ordered.next_from IS NULL OR ordered.next_from > ordered.valid_from)",
        ))
        .take()
}

pub async fn recompute_valid_until<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(build(calibration_chain_statement(sensor_id)))
        .await?;
    Ok(())
}

/// Twin of [`calibration_chain_statement`] for the deployment timeline: chain each of a sensor's
/// deployments' `deployed_until` down to the next deployment's `deployed_from`.
///
/// A deployment's end date is always caller-settable, so this only ever shortens a window to remove
/// overlap (`LEAST` keeps an existing earlier bound) and never extends one; a calibration's is
/// chain-written unless an operator set it, and shortens only in that case. Shortening cannot
/// create an overlap, so the result always satisfies the per-(site, parameter) exclusion
/// constraint. The final predicate holds the write to the rows the chain actually moves.
fn deployment_chain_statement(sensor_id: Uuid) -> UpdateStatement {
    let ordered = SeaQuery::select()
        .column(sensor_deployments::Column::Id)
        .expr_as(
            Expr::cust(
                "LEAST(COALESCE(deployed_until, 'infinity'::timestamptz), \
                       COALESCE(next_from, 'infinity'::timestamptz))",
            ),
            Alias::new("new_until"),
        )
        .from_subquery(
            SeaQuery::select()
                .column(sensor_deployments::Column::Id)
                .column(sensor_deployments::Column::DeployedUntil)
                .expr_window_as(
                    Expr::cust("LEAD(deployed_from)"),
                    WindowStatement::partition_by(sensor_deployments::Column::ParameterId)
                        .order_by(sensor_deployments::Column::DeployedFrom, Order::Asc)
                        .take(),
                    Alias::new("next_from"),
                )
                .from(sensor_deployments::Entity)
                .and_where(sensor_deployments::Column::SensorId.eq(sensor_id))
                .take(),
            Alias::new("chained"),
        )
        .take();
    SeaQuery::update()
        .table(
            sensor_deployments::Entity
                .into_table_ref()
                .alias(Alias::new("d")),
        )
        .value(
            sensor_deployments::Column::DeployedUntil,
            Expr::cust("NULLIF(ordered.new_until, 'infinity'::timestamptz)"),
        )
        .from(TableRef::SubQuery(
            Box::new(ordered),
            Alias::new("ordered").into_iden(),
        ))
        .and_where(Expr::cust("d.id = ordered.id"))
        .and_where(Expr::cust_with_values("d.sensor_id = $1", [sensor_id]))
        .and_where(Expr::cust(
            "COALESCE(d.deployed_until, 'infinity'::timestamptz) <> ordered.new_until",
        ))
        .take()
}

pub async fn recompute_deployed_until<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(build(deployment_chain_statement(sensor_id)))
        .await?;
    Ok(())
}

/// What a reprocess run covers.
///
/// The two scopes ask different questions of the same timelines. `Sensor` re-derives the columns of
/// the readings one instrument already owns; `Slot` re-derives the owner too, which is what makes a
/// swap (instrument B replaces A at one feed) hand A's post-swap readings to B. Everything after
/// that choice, the curve resolution, the grab recomposition, the recall, the derived cascade and
/// the rollup refresh, is one piece of code, so a fix cannot land on one arm and miss the other.
#[derive(Clone, Copy, Debug)]
pub enum Scope {
    /// One instrument's own readings, wherever they sit.
    Sensor(Uuid),
    /// One (site, parameter) slot, whatever measured it.
    Slot { site_id: Uuid, parameter_id: Uuid },
}

impl Scope {
    /// The readings this run may rewrite, as `r`.
    fn readings_predicate(self) -> Expr {
        match self {
            // A derived row carries the slot but no instrument, so no window resolves for it and
            // the outer join below would erase the value the cascade wrote. The sensor arm needs no
            // such guard: a derived row names no instrument to be in scope by.
            Self::Sensor(sensor_id) => Expr::cust_with_values("r.sensor_id = $1", [sensor_id]),
            Self::Slot {
                site_id,
                parameter_id,
            } => Expr::cust_with_values(
                "r.site_id = $1 AND r.parameter_id = $2 \
                 AND r.measurement_type IS DISTINCT FROM 'derived'",
                [site_id, parameter_id],
            ),
        }
    }

    /// Where the curve pick reads the instrument from: the scope's own on the sensor arm, the row's
    /// (which step 1 has just re-owned) on the slot arm.
    fn pick_owner(self) -> Expr {
        match self {
            Self::Sensor(sensor_id) => Expr::cust_with_values("c.sensor_id = $1", [sensor_id]),
            Self::Slot { .. } => Expr::cust("c.sensor_id = r.sensor_id"),
        }
    }

    /// The deployments whose windows attribute this run's readings.
    fn deployments_predicate(self) -> Expr {
        match self {
            Self::Sensor(sensor_id) => Expr::cust_with_values("sensor_id = $1", [sensor_id]),
            Self::Slot {
                site_id,
                parameter_id,
            } => Expr::cust_with_values(
                "site_id = $1 AND parameter_id = $2",
                [site_id, parameter_id],
            ),
        }
    }

    /// What the attribution step writes onto a reading. Only the slot arm re-owns.
    fn attribution_values(self) -> Vec<(readings::Column, Expr)> {
        let mut values = vec![
            (readings::Column::DeploymentId, Expr::cust("dw.id")),
            (readings::Column::SiteId, Expr::cust("dw.site_id")),
        ];
        if matches!(self, Self::Slot { .. }) {
            values.insert(0, (readings::Column::SensorId, Expr::cust("dw.sensor_id")));
        }
        values
    }

    /// The reading columns the attribution step writes, which is what its ledger row records.
    fn attribution_columns(self) -> &'static [&'static str] {
        match self {
            Self::Sensor(_) => &["deployment_id", "site_id"],
            Self::Slot { .. } => &["sensor_id", "deployment_id", "site_id"],
        }
    }

    /// Which readings the attribution step considers, beyond the window overlap.
    fn attribution_scope(self) -> Expr {
        match self {
            // A deployment names one parameter, so it claims a row of that parameter or an
            // unpaired one.
            Self::Sensor(sensor_id) => Expr::cust_with_values(
                "r.sensor_id = $1 AND (r.parameter_id IS NULL OR dw.parameter_id = r.parameter_id)",
                [sensor_id],
            ),
            // Either the row is at the slot, or it belongs to the instrument the slot's deployment
            // names: that second half is what pulls a swapped instrument's readings back in.
            Self::Slot {
                site_id,
                parameter_id,
            } => Expr::cust_with_values(
                "r.parameter_id = $2 AND (r.site_id = $1 OR r.sensor_id = dw.sensor_id)",
                [site_id, parameter_id],
            ),
        }
    }

    /// A reading in a gap between deployments belongs to no site. Guarded to `time >= the scope's
    /// first deployment` so readings that predate any deployment keep the site the stream pairing
    /// gave them; an auto-created deployment opens at its stream's first reading, so the floor now
    /// protects hand-dated deployments only.
    fn recall_predicate(self) -> Expr {
        let windowed = attribution_derivable("r");
        match self {
            Self::Sensor(sensor_id) => Expr::cust_with_values(
                r"r.sensor_id = $1
                    AND r.site_id IS NOT NULL
                    AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments d2
                                   WHERE d2.sensor_id = $1
                                     AND (r.parameter_id IS NULL OR d2.parameter_id = r.parameter_id))
                    AND NOT EXISTS (
                        SELECT 1 FROM sensor_deployments d
                        WHERE d.sensor_id = $1
                          AND (r.parameter_id IS NULL OR d.parameter_id = r.parameter_id)
                          AND r.time >= d.deployed_from
                          AND r.time < COALESCE(d.deployed_until, 'infinity'::timestamptz)
                    )",
                [sensor_id],
            )
            .and(windowed),
            Self::Slot {
                site_id,
                parameter_id,
            } => Expr::cust_with_values(
                r"r.site_id = $1 AND r.parameter_id = $2
                    AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments
                                   WHERE site_id = $1 AND parameter_id = $2)
                    AND NOT EXISTS (
                        SELECT 1 FROM sensor_deployments d
                        WHERE d.site_id = $1 AND d.parameter_id = $2
                          AND r.time >= d.deployed_from
                          AND r.time < COALESCE(d.deployed_until, 'infinity'::timestamptz)
                    )",
                [site_id, parameter_id],
            )
            .and(windowed),
        }
    }

    /// The rows whose span the rollup refresh covers.
    fn refresh_condition(self) -> sea_orm::Condition {
        match self {
            Self::Sensor(sensor_id) => {
                sea_orm::Condition::all().add(readings::Column::SensorId.eq(sensor_id))
            }
            Self::Slot {
                site_id,
                parameter_id,
            } => sea_orm::Condition::all()
                .add(readings::Column::SiteId.eq(site_id))
                .add(readings::Column::ParameterId.eq(parameter_id)),
        }
    }
}

/// The `was_`/`now_` pairs a recording statement returns for the columns it wrote, read from the
/// pre-update snapshot and the target row.
fn moved_pairs(was: &str, now: &str, columns: &[&str]) -> String {
    columns
        .iter()
        .map(|c| format!("{was}.{c} AS was_{c}, {now}.{c} AS now_{c}"))
        .collect::<Vec<_>>()
        .join(",\n                      ")
}

/// Wrap one of the engine's statements in the ledger insert Q118 and Q125 require: a row per
/// reading the run actually moved, naming the state of each written column on both sides and the
/// job that made the move, in the same transaction as the write.
///
/// `update_sql` ends in a `RETURNING` of `stream_id`, `time`, `replicate_index`, the `site_id` the
/// cascade follows, and a `was_<col>`/`now_<col>` pair per column in `columns`. A visited row whose
/// columns all came back the same is not a move and records nothing; the statement still returns
/// it, so the caller's count and cascade are unchanged.
fn record_moved(write: UpdateStatement, columns: &[&str], job_id: Option<Uuid>) -> WithQuery {
    let pairs = |side: &str| {
        columns
            .iter()
            .map(|c| format!("'{c}', to_jsonb(m.{side}_{c})"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let changed = columns
        .iter()
        .map(|c| format!("m.was_{c} IS DISTINCT FROM m.now_{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let recorded = ledger_insert(
        Kind::Reprocess,
        Origin::System,
        "moved",
        "m",
        Expr::cust(format!("jsonb_build_object({})", pairs("was"))),
        Expr::cust(format!("jsonb_build_object({})", pairs("now"))),
        Some(Expr::cust(format!("({changed})"))),
        job_id,
    );
    SeaQuery::select()
        .column(Alias::new("site_id"))
        .column(Alias::new("time"))
        .from(Alias::new("moved"))
        .take()
        .with(with_ledger("moved", write, recorded))
}

pub async fn reprocess_sensor_readings(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    job_id: Option<Uuid>,
) -> Result<usize, sea_orm::DbErr> {
    // Repair a `valid_until` a bulk load left NULL, so the stored window agrees with the one the
    // resolver serves. `pick_calibration_lateral` is single-valued whether or not windows overlap,
    // so the derivation does not need this; what needs it is the curve editor, which reads
    // `valid_until` and would otherwise show a window open past the point a later curve takes over.
    recompute_valid_until(db, sensor_id).await?;
    reprocess(db, Scope::Sensor(sensor_id), job_id).await
}

/// Per-(site, parameter) reprocess. See [`Scope::Slot`].
pub async fn reprocess_site_parameter_readings(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    job_id: Option<Uuid>,
) -> Result<usize, sea_orm::DbErr> {
    reprocess(
        db,
        Scope::Slot {
            site_id,
            parameter_id,
        },
        job_id,
    )
    .await
}

/// The four writes a reprocess run makes, in the order it runs them. Built without a database, so
/// what each one selects and writes is readable on its own.
struct ReprocessStatements {
    attribution: WithQuery,
    calibration: WithQuery,
    spot: WithQuery,
    recall: WithQuery,
}

fn reprocess_statements(scope: Scope, job_id: Option<Uuid>) -> ReprocessStatements {
    // Step 1, attribution. On the slot arm this runs BEFORE the curve resolution and the order is
    // the contract: step 2 resolves against `r.sensor_id`, so it picks the curves of the owner
    // step 1 just wrote. Resolving first would stamp the outgoing instrument's curve on a reading
    // the swap hands to the incoming one, and nothing repairs that afterwards.
    let deployments = SeaQuery::select()
        .expr(Expr::cust("id"))
        .expr(Expr::cust("sensor_id"))
        .expr(Expr::cust("site_id"))
        .expr(Expr::cust("parameter_id"))
        .expr(Expr::cust("deployed_from"))
        .expr_as(
            Expr::cust("COALESCE(deployed_until, 'infinity'::timestamptz)"),
            Alias::new("deployed_until"),
        )
        .from(sensor_deployments::Entity)
        .and_where(scope.deployments_predicate())
        .take();
    let mut attribution = SeaQuery::update();
    attribution.table(readings::Entity.into_table_ref().alias(Alias::new("r")));
    for (column, value) in scope.attribution_values() {
        attribution.value(column, value);
    }
    let attribution = record_moved(
        attribution
            .from(TableRef::SubQuery(
                Box::new(deployments),
                Alias::new("dw").into_iden(),
            ))
            .from(readings::Entity.into_table_ref().alias(Alias::new("prev")))
            .and_where(Expr::cust("prev.stream_id = r.stream_id"))
            .and_where(Expr::cust("prev.time = r.time"))
            .and_where(Expr::cust("prev.replicate_index = r.replicate_index"))
            .and_where(scope.attribution_scope())
            .and_where(attribution_derivable("r"))
            .and_where(Expr::cust("r.time >= dw.deployed_from"))
            .and_where(Expr::cust("r.time < dw.deployed_until"))
            .returning(ReturningClause::Exprs(vec![Expr::cust(format!(
                "r.stream_id, r.time, r.replicate_index, r.site_id, {pairs}",
                pairs = moved_pairs("prev", "r", scope.attribution_columns()),
            ))]))
            .take(),
        scope.attribution_columns(),
        job_id,
    );

    // Step 2, the curve. The pick is `resolver::pick_calibration_query_owned`, the same ranking the
    // write paths resolve with, so a reprocess recomputes the value ingest already stored rather
    // than a different one. Which rows a window may claim is `window_resolved_rows`; the spot rows
    // it holds back are step 3's.
    let calibration = record_moved(
        repoint_statement(
            super::resolver::pick_calibration_query_owned(scope.pick_owner(), None),
            scope
                .readings_predicate()
                .and(calibration_derivable("r"))
                .and(
                    Expr::cust("cw.id IS NULL")
                        .and(orphaned_correction_rows("r"))
                        .not(),
                ),
            Some(ReturningClause::Exprs(vec![Expr::cust(
                "tgt.stream_id, tgt.time, tgt.replicate_index, tgt.site_id, \
                 picked.p_was_calibration_id AS was_calibration_id, \
                 tgt.calibration_id AS now_calibration_id, \
                 picked.p_was_calibrated_value AS was_calibrated_value, \
                 tgt.calibrated_value AS now_calibrated_value",
            )])),
        ),
        &["calibration_id", "calibrated_value"],
        job_id,
    );

    // Step 3, the grabs: they keep the curves they were entered against, and their value follows
    // those curves' current coefficients.
    let spot = record_moved(
        recompose_statement(own_curve_rows(
            Expr::cust("r.measurement_type = 'spot'").and(scope.readings_predicate()),
        ))
        .returning(ReturningClause::Exprs(vec![Expr::cust(
            "tgt.stream_id, tgt.time, tgt.replicate_index, tgt.site_id, \
             r.calibrated_value AS was_calibrated_value, \
             tgt.calibrated_value AS now_calibrated_value",
        )]))
        .take(),
        &["calibrated_value"],
        job_id,
    );

    // Step 4, the recall. The site it clears is returned from the pre-update snapshot, because the
    // cascade has to reach the instant a derived value must follow its input out of, and after the
    // write the row names no site at all.
    let recall_columns = ["site_id", "deployment_id"];
    let recall = record_moved(
        SeaQuery::update()
            .table(readings::Entity.into_table_ref().alias(Alias::new("r")))
            .value(readings::Column::SiteId, Expr::val(Option::<Uuid>::None))
            .value(
                readings::Column::DeploymentId,
                Expr::val(Option::<Uuid>::None),
            )
            .from(readings::Entity.into_table_ref().alias(Alias::new("prev")))
            .and_where(Expr::cust("prev.stream_id = r.stream_id"))
            .and_where(Expr::cust("prev.time = r.time"))
            .and_where(Expr::cust("prev.replicate_index = r.replicate_index"))
            .and_where(scope.recall_predicate())
            .returning(ReturningClause::Exprs(vec![Expr::cust(format!(
                "r.stream_id, r.time, r.replicate_index, prev.site_id, {pairs}",
                pairs = moved_pairs("prev", "r", &recall_columns),
            ))]))
            .take(),
        &recall_columns,
        job_id,
    );

    ReprocessStatements {
        attribution,
        calibration,
        spot,
        recall,
    }
}

/// Re-derive a scope's readings from the deployment and calibration timelines, then follow the
/// change out: derived values at the instants it moved, and the rollups over the span it covers.
///
/// Nothing here manufactures coverage. A reading that predates the instrument's first curve, or
/// falls in a gap between two, is uncorrected: the resolution below resolves no curve for it and
/// clears both the reference and the value. The lateral is an outer join for that reason, so a
/// reading no window covers is in scope rather than skipped; a reprocess has to be able to CLEAR a
/// correction as well as replace one, or a curve deleted or moved off a reading would leave that
/// reading serving a number nothing on the row accounts for.
///
/// `orphaned_correction_rows` is what clear does not reach, and only while no window covers it: a
/// row resolving no window, naming no curve, and holding a number that is not a copy of its raw
/// value was written that way by a caller, and clearing it here would replace somebody's
/// measurement with a NULL. Those are reported by `GET /actions/calibration_candidates` and left
/// alone. One a window does cover is recomputed from that curve like any other row, its old value
/// on the `reading_decisions` row this step appends (Q114).
///
/// Steps 1 to 4 run in one guarded transaction (`common::bulk_write`), which lifts TimescaleDB's
/// per-statement decompression cap: a deep-historical reprocess rewrites rows in compressed
/// (>30-day) chunks and would otherwise abort the job. The cascade and the rollup refresh run after
/// the commit, since a continuous-aggregate refresh cannot run inside a transaction.
///
/// Each step records what it moved as a `reprocess` decision naming `job_id` (Q118, Q125), in its
/// own statement, so a reading that changed site, instrument or corrected value under a re-derived
/// timeline says so in the one place a value's history is read from. A visited row the step leaves
/// as it found it records nothing.
pub async fn reprocess(
    db: &DatabaseConnection,
    scope: Scope,
    job_id: Option<Uuid>,
) -> Result<usize, sea_orm::DbErr> {
    let steps = reprocess_statements(scope, job_id);

    let (readings_updated, cascade) = crate::common::bulk_write::guarded(db, async |txn| {
        let mut touched: Vec<(Uuid, DateTime<Utc>)> = Vec::new();
        let mut readings_updated = 0usize;
        for query in [&steps.attribution, &steps.calibration, &steps.spot] {
            readings_updated += write_and_collect(txn, query, &mut touched).await?;
        }

        // The recall's rows are not part of `readings_updated`: it clears an attribution rather
        // than re-deriving one.
        write_and_collect(txn, &steps.recall, &mut touched).await?;

        touched.sort_unstable();
        touched.dedup();
        Ok((readings_updated, touched))
    })
    .await
    .map_err(app_error_as_db_err)?;

    // The cascade runs over what this run moved, not over every instant in the scope: a derived
    // value at (site, time) is a function of the served values, and those changed only where a
    // statement above wrote. Costing a query per instant, the difference is the whole run.
    let mut refused = crate::routes::private::derived_parameters::service::DerivedPass::default();
    for (site_id, utc_time) in cascade {
        match recalculate_derived_at_timestamp(db, site_id, utc_time).await {
            Ok(slots) => refused.record(&slots, utc_time),
            Err(e) => tracing::warn!(
                error = %e,
                site_id = %site_id,
                time = %utc_time,
                "Failed to cascade reprocessing to derived parameter"
            ),
        }
    }
    refused.report(db).await?;

    let since = readings::Entity::find()
        .filter(scope.refresh_condition())
        .select_only()
        .expr_as(Func::min(Expr::col(readings::Column::Time)), "min_time")
        .into_tuple::<Option<DateTime<Utc>>>()
        .one(db)
        .await?
        .flatten();
    if let Some(since) = since {
        crate::common::aggregates::refresh(db, crate::common::aggregates::Window::Since(since))
            .await
            .map_err(app_error_as_db_err)?;
    }

    Ok(readings_updated)
}

/// Run one of the engine's statements, collecting the attributed instants it wrote. Each is a
/// [`record_moved`] wrapper returning `site_id` and `time` per row it touched; an unattributed row
/// returns a NULL site and is not an instant anything derives from.
/// One reading a recompute moved, and the slot it belongs to.
#[derive(FromQueryResult)]
struct MovedReading {
    site_id: Option<Uuid>,
    time: chrono::DateTime<chrono::FixedOffset>,
}

async fn write_and_collect<C: ConnectionTrait>(
    conn: &C,
    query: &WithQuery,
    touched: &mut Vec<(Uuid, DateTime<Utc>)>,
) -> Result<usize, sea_orm::DbErr> {
    let rows = conn.query_all_raw(build(query.clone())).await?;
    for row in &rows {
        // `site_id` is nullable on an unpaired reading, which has no slot to touch.
        let moved = MovedReading::from_query_result(row, "")?;
        if let Some(site_id) = moved.site_id {
            touched.push((site_id, moved.time.with_timezone(&Utc)));
        }
    }
    Ok(rows.len())
}

pub struct SensorCalibrationOperations;

const DUPLICATE_INSTANT: &str = "A calibration for this sensor and parameter already starts at that instant. Two \
     curves sharing a valid_from leave one with an empty window: edit the existing curve, or start \
     this one at a different instant.";

/// Whether another curve on the same `(sensor, parameter)` channel already opens at
/// `valid_from`. Zero-width windows are what a duplicate produces (`recompute_valid_until` chains
/// each curve's end to the next curve's start), and a curve applying to nothing is invisible in
/// every reading but visible in the editor.
///
/// A request that names no parameter matches any curve at that instant. The
/// `inherit_calibration_parameter_id` BEFORE-INSERT trigger fills a curve's parameter in
/// from the sensor's first parameter-bearing curve, so the channel the row lands on is not knowable
/// here without restating the trigger's rule: treating the instant itself as taken is the answer
/// that needs no second copy of it.
async fn duplicate_instant_exists<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    parameter_id: Option<Uuid>,
    valid_from: chrono::DateTime<chrono::Utc>,
    exclude: Option<Uuid>,
) -> Result<bool, ApiError> {
    let mut query = super::models::Entity::find()
        .filter(super::models::Column::SensorId.eq(sensor_id))
        .filter(super::models::Column::ValidFrom.eq(valid_from));
    // An unstated parameter takes the instant as taken whatever channel holds it, since the
    // BEFORE-INSERT trigger decides that channel and this cannot see its answer.
    if let Some(parameter_id) = parameter_id {
        query = query.filter(super::models::Column::ParameterId.eq(parameter_id));
    }
    if let Some(exclude) = exclude {
        query = query.filter(super::models::Column::Id.ne(exclude));
    }
    let found = query
        .select_only()
        .column(super::models::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await
        .map_err(ApiError::database)?;
    Ok(found.is_some())
}

/// Whether an update's `valid_until` makes the row's end date an operator's or the chain's again.
/// `None` when the update carried no end date at all, which leaves the provenance as it stands.
fn valid_until_provenance(
    valid_until: Option<Option<chrono::DateTime<chrono::Utc>>>,
) -> Option<bool> {
    match valid_until {
        Some(Some(_)) => Some(true),
        // Cleared, so the window chain reclaims the row on the next recompute.
        Some(None) => Some(false),
        None => None,
    }
}

/// Chain the sensor's windows, then enqueue the reprocess that re-derives its readings. Every
/// calibration write does exactly this and differs only in the trigger it records, so the three
/// hooks call it rather than restating it.
async fn reprocess_after_calibration_write<C: ConnectionTrait>(
    db: &C,
    trigger: &str,
    sensor_id: Uuid,
    calibration_id: Uuid,
) -> Result<(), ApiError> {
    recompute_valid_until(db, sensor_id)
        .await
        .map_err(ApiError::database)?;

    crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        trigger,
        Some(sensor_id),
        Some(calibration_id),
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(ApiError::database)?;

    Ok(())
}

/// How many readings hold a live `calibration_pin` naming this curve, or `None` when there are
/// none. The repoint that clears the way for the delete excludes pinned rows, so each one still
/// names the curve when the DELETE runs and the foreign key refuses the statement.
async fn pinned_readings<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<Option<i64>, ApiError> {
    let pinned = crate::routes::private::readings::service::not_pinned(
        "readings",
        crate::routes::private::readings::models::Kind::CalibrationPin,
    )
    .not();
    let n = readings::Entity::find()
        .filter(readings::Column::CalibrationId.eq(id))
        .filter(pinned)
        .count(db)
        .await
        .map_err(ApiError::database)?;
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    Ok((n > 0).then_some(n))
}

impl CRUDOperations for SensorCalibrationOperations {
    type Resource = SensorCalibration;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it.
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: &<SensorCalibration as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if duplicate_instant_exists(db, data.sensor_id, data.parameter_id, data.valid_from, None)
            .await?
        {
            return Err(ApiError::bad_request(DUPLICATE_INSTANT.to_string()));
        }
        Ok(())
    }

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<SensorCalibration as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if data.valid_from.is_none() && data.valid_until.is_none() {
            return Ok(());
        }
        let Some(existing) = super::models::Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?
        else {
            return Ok(()); // unknown id, let CrudCrate's update produce the 404
        };

        // Nothing here writes: `perform_update` carries the provenance flag in the row's own UPDATE,
        // so a request this hook goes on to reject leaves the chain treating the row exactly as it
        // did before.
        let stored_from = existing.valid_from;
        if let Some(Some(until)) = data.valid_until {
            let opens_at = match data.valid_from {
                Some(Some(patched)) => patched,
                _ => stored_from.with_timezone(&chrono::Utc),
            };
            if until <= opens_at {
                return Err(ApiError::bad_request(
                    "A calibration's end date must fall after its start date: a window that \
                     closes at or before it opens applies to no reading."
                        .to_string(),
                ));
            }
        }

        // Moving a curve's start onto another curve's start is the same collision as creating one
        // there.
        let Some(Some(new_from)) = data.valid_from else {
            return Ok(());
        };
        let sensor_id = existing.sensor_id;
        let parameter_id = existing.parameter_id;
        let parameter_id = match data.parameter_id {
            Some(patched) => patched,
            None => parameter_id,
        };

        if duplicate_instant_exists(db, sensor_id, parameter_id, new_from, Some(id)).await? {
            return Err(ApiError::bad_request(DUPLICATE_INSTANT.to_string()));
        }
        Ok(())
    }

    /// The default update, with the `valid_until_explicit` provenance carried in the same statement.
    ///
    /// The flag sits outside both CRUD models, so nothing writes it unless this hook does. Written
    /// from `before_update` instead, a request a later validation goes on to reject would still have
    /// moved the row onto the operator-window branch of `recompute_valid_until`, where `LEAST`
    /// ignores a NULL and the window can no longer reopen when the following curve is deleted. Set
    /// on the active model, a rejected update leaves the row exactly as it was.
    async fn perform_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: <SensorCalibration as CRUDResource>::UpdateModel,
    ) -> Result<SensorCalibration, ApiError> {
        use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};

        let provenance = valid_until_provenance(data.valid_until);
        let existing = super::models::Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?
            .ok_or_else(|| ApiError::not_found("sensor_calibration", Some(id.to_string())))?;

        let mut active = data.merge_into_activemodel(existing.into_active_model())?;
        if let Some(explicit) = provenance {
            active.valid_until_explicit = Set(explicit);
        }
        let updated = active.update(db).await.map_err(ApiError::database)?;

        Ok(SensorCalibration::from(updated))
    }

    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        reprocess_after_calibration_write(db, "calibration_create", entity.sensor_id, entity.id)
            .await
    }

    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        reprocess_after_calibration_write(db, "calibration_update", entity.sensor_id, entity.id)
            .await
    }

    async fn perform_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<Uuid, ApiError> {
        let row = super::models::Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?;

        let Some(row) = row else {
            return Err(ApiError::not_found(
                "sensor_calibration",
                Some(id.to_string()),
            ));
        };
        let sensor_id = row.sensor_id;

        if let Some(pinned) = pinned_readings(db, id).await? {
            return Err(ApiError::bad_request(format!(
                "Calibration {id} is pinned to {pinned} reading{plural}, so it cannot be deleted: \
                 a pin is attribution a person decided and the repoint leaves it alone. Roll the \
                 pin back first, then delete the curve.",
                plural = if pinned == 1 { "" } else { "s" }
            )));
        }

        // A curve that has corrected a reading is retired, never removed (Q107, M146): the row is
        // the provenance of every value it produced, and a delete would take that away and leave
        // the readings pointing at nothing. A curve nothing names has no history to keep.
        let used = readings::Entity::find()
            .filter(readings::Column::CalibrationId.eq(id))
            .count(db)
            .await
            .map_err(ApiError::database)?;
        if used > 0 {
            return Err(ApiError::bad_request(format!(
                "This calibration has corrected {used} reading(s), so it is retired rather than \
                 deleted: POST /api/sensor_calibrations/{id}/retire. Retiring moves those readings \
                 onto whatever else covers them, keeps the row and its provenance, and is \
                 reversible."
            )));
        }

        super::models::Entity::delete_by_id(id)
            .exec(db)
            .await
            .map_err(ApiError::database)?;

        reprocess_after_calibration_write(db, "calibration_delete", sensor_id, id).await?;

        Ok(id)
    }
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
