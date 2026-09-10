use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter,
    QuerySelect, Set, Statement,
};
use std::collections::HashMap;
use uuid::Uuid;

use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::parameters::derived::definition_model as calculation_formulas;
use crate::routes::private::parameters::derived::source_model as derived_sources;
use crate::routes::private::readings::decisions;

/// The reprocess engines are driven by `Job::run`, whose error type is `DbErr`. The shared bulk-write
/// and aggregate-refresh primitives report `AppError`; carrying the message through keeps a failed
/// refresh a failed job rather than a job that reports `completed`.
fn app_error_as_db_err(e: crate::error::AppError) -> sea_orm::DbErr {
    match e {
        crate::error::AppError::Database(inner) => inner,
        other => sea_orm::DbErr::Custom(other.to_string()),
    }
}

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
pub fn calibrated_value_sql(raw_expr: &str, slope_expr: &str, intercept_expr: &str) -> String {
    format!("{slope_expr} * {raw_expr} + {intercept_expr}")
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
pub fn recomposed_value_sql(
    raw_expr: &str,
    base: &CurveColumns,
    standard: &CurveColumns,
) -> String {
    let after_base = format!(
        "CASE WHEN {base_id} IS NULL THEN {raw_expr} ELSE {applied} END",
        base_id = base.id,
        applied = calibrated_value_sql(raw_expr, base.slope, base.intercept),
    );
    let after_standard = calibrated_value_sql(
        &format!("({after_base})"),
        standard.slope,
        standard.intercept,
    );
    format!(
        "CASE WHEN {base_id} IS NULL AND {std_id} IS NULL THEN NULL \
              WHEN {std_id} IS NULL THEN {after_base} \
              ELSE {after_standard} END",
        base_id = base.id,
        std_id = standard.id,
    )
}

/// The rows a window resolution owns, ie. everything but a grab.
///
/// A grab's base calibration is resolved once, at entry, and its standard curve is chosen by hand;
/// no window query can recover either choice, so re-deriving one would replace a deliberate
/// correction with whatever the timeline currently says. `alias` names the readings row in the
/// caller's query.
#[must_use]
pub fn window_resolved_rows(alias: &str) -> String {
    format!("{alias}.measurement_type IS DISTINCT FROM 'spot'")
}

/// A row whose calibration a window may author: window-resolved and not pinned to a calibration
/// (ADR 0008, M59).
pub fn calibration_derivable(alias: &str) -> String {
    format!(
        "{} AND {}",
        window_resolved_rows(alias),
        crate::routes::private::readings::decisions::not_pinned_sql(
            alias,
            crate::routes::private::readings::decisions::Kind::CalibrationPin
        )
    )
}

/// A row whose instrument and deployment a window may author: window-resolved and not pinned to
/// an instrument.
pub fn attribution_derivable(alias: &str) -> String {
    format!(
        "{} AND {}",
        window_resolved_rows(alias),
        crate::routes::private::readings::decisions::not_pinned_sql(
            alias,
            crate::routes::private::readings::decisions::Kind::InstrumentPin
        )
    )
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
/// this code cannot recover, so no rewrite here can be more than a guess.
///
/// They are therefore held out of every recomposition and reported instead, by
/// `GET /actions/calibration_candidates`. A row whose stored value merely COPIES its raw value is
/// NOT one of these: that copy is what the old writers materialised for an uncorrected reading, it
/// carries no information, and clearing it changes nothing the API serves.
#[must_use]
pub fn orphaned_correction_rows(alias: &str) -> String {
    format!(
        "{alias}.calibration_id IS NULL AND {alias}.standard_curve_id IS NULL \
         AND {alias}.calibrated_value IS NOT NULL \
         AND {alias}.calibrated_value IS DISTINCT FROM {alias}.raw_value"
    )
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
    recompose_from_own_curves(db, "r.measurement_type = 'spot'", scope_sql, params).await
}

/// What the curves a row itself names produce from its raw value. Both the recompose and the drift
/// sweep judge against this one expression, so what the sweep repairs is what the recompose writes.
fn recomposed_own_curve_value() -> String {
    recomposed_value_sql(
        "tgt.raw_value",
        &CurveColumns {
            id: "c.id",
            slope: "c.slope",
            intercept: "c.intercept",
        },
        &CurveColumns {
            id: "sc.id",
            slope: "sc.slope",
            intercept: "sc.intercept",
        },
    )
}

/// The `UPDATE readings` both curve-recomposing statements are: `rows_sql` narrows which readings
/// qualify, `scope_sql` selects them as `r`.
fn recompose_statement(rows_sql: &str, scope_sql: &str) -> String {
    format!(
        r"UPDATE readings tgt
          SET calibrated_value = {value}
          FROM readings r
          LEFT JOIN sensor_calibrations c ON c.id = r.calibration_id
          LEFT JOIN standard_curves sc ON sc.id = r.standard_curve_id
          WHERE tgt.stream_id = r.stream_id
            AND tgt.time = r.time
            AND tgt.replicate_index = r.replicate_index
            AND ({rows_sql})
            AND NOT ({orphaned})
            AND ({scope_sql})",
        value = recomposed_own_curve_value(),
        orphaned = orphaned_correction_rows("r"),
    )
}

/// The one statement that repoints readings onto the calibration window covering them and rebuilds
/// `calibrated_value` from it, the operator's standard curve re-applied on top.
///
/// `pick` is the lateral that ranks the windows, `selection` chooses the rows as `r`, and
/// `returning` is appended verbatim so a caller that needs the instants it wrote can ask for them.
/// `picked` carries the row's state before the write (`p_was_*`) for a caller that records the move.
/// The lateral is an outer join: a reading no window covers has to be reachable, because a repoint
/// must be able to clear a correction as well as replace one.
pub(super) fn repoint_statement(pick: &str, selection: &str, returning: &str) -> String {
    let value = recomposed_value_sql(
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
    );
    format!(
        r"UPDATE readings tgt
            SET calibration_id = picked.cal_id,
                calibrated_value = {value}
            FROM (
                SELECT r.stream_id AS p_stream_id, r.time AS p_time,
                       r.replicate_index AS p_replicate_index,
                       r.standard_curve_id AS p_standard_curve_id,
                       r.calibration_id AS p_was_calibration_id,
                       r.calibrated_value AS p_was_calibrated_value,
                       cw.id AS cal_id, cw.slope, cw.intercept
                FROM readings r
                LEFT JOIN LATERAL ({pick}) cw ON true
                WHERE {selection}
            ) picked
            LEFT JOIN standard_curves sc ON sc.id = picked.p_standard_curve_id
            WHERE tgt.stream_id = picked.p_stream_id
              AND tgt.time = picked.p_time
              AND tgt.replicate_index = picked.p_replicate_index{returning}"
    )
}

/// Rewrite `calibrated_value` from the curves each row itself names, for a corrected measurement.
///
/// `rows_sql` narrows which readings qualify and `scope_sql` selects them as `r` against `params`.
/// Idempotent, so a scope wider than the rows that changed is safe.
pub async fn recompose_from_own_curves<C: ConnectionTrait>(
    db: &C,
    rows_sql: &str,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> Result<u64, sea_orm::DbErr> {
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &recompose_statement(rows_sql, scope_sql),
            params,
        ))
        .await?;
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
    let sql = format!(
        r"UPDATE readings tgt
          SET calibrated_value = {value}
          FROM readings r
          LEFT JOIN sensor_calibrations c ON c.id = r.calibration_id
          LEFT JOIN standard_curves sc ON sc.id = r.standard_curve_id
          WHERE tgt.stream_id = r.stream_id
            AND tgt.time = r.time
            AND tgt.replicate_index = r.replicate_index
            AND EXISTS (SELECT 1 FROM reading_decisions d
                         WHERE d.set_id = $1
                           AND d.stream_id = r.stream_id AND d.time = r.time
                           AND d.replicate_index IS NOT DISTINCT FROM r.replicate_index)",
        value = recomposed_own_curve_value(),
    );
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            [set_id.into()],
        ))
        .await?;
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
    rows_sql: &str,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> crate::error::AppResult<u64> {
    crate::common::bulk_write::guarded(db, async |txn| {
        recompose_from_own_curves(txn, rows_sql, scope_sql, params)
            .await
            .map_err(crate::error::AppError::Database)
    })
    .await
}

/// Rows a curve-drift sweep can judge: the value is a claim about curves the row names, so a row
/// naming neither carries nothing to check against.
#[must_use]
pub fn corrected_rows(alias: &str) -> String {
    format!("({alias}.calibration_id IS NOT NULL OR {alias}.standard_curve_id IS NOT NULL)")
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
    let drifted = format!(
        "{corrected} AND tgt.calibrated_value IS DISTINCT FROM ({value})",
        corrected = corrected_rows("r"),
        value = recomposed_own_curve_value(),
    );
    let sql = format!(
        "WITH drift AS (
            {update}
            RETURNING tgt.stream_id, tgt.time, tgt.replicate_index, tgt.collection_event_id,
                      tgt.parameter_id, r.calibrated_value AS was, tgt.calibrated_value AS became
          ), recorded AS (
            INSERT INTO reading_decisions
                (stream_id, time, replicate_index, kind, old, new, actor, origin, supersedes,
                 job_id)
            SELECT d.stream_id, d.time, d.replicate_index, '{kind}',
                   jsonb_build_object('calibrated_value', to_jsonb(d.was)),
                   jsonb_build_object('calibrated_value', to_jsonb(d.became)),
                   'system', '{origin}',
                   (SELECT p.id FROM reading_decisions p
                     WHERE p.stream_id = d.stream_id AND p.time = d.time
                       AND p.replicate_index IS NOT DISTINCT FROM d.replicate_index
                       AND p.kind = '{kind}' AND p.rolled_back_by IS NULL
                     ORDER BY p.at DESC, p.id DESC LIMIT 1),
                   $1
              FROM drift d
          )
          SELECT count(*) AS moved, min(time) AS lo, max(time) AS hi,
                 (SELECT jsonb_agg(DISTINCT jsonb_build_array(collection_event_id, parameter_id))
                    FROM drift
                   WHERE collection_event_id IS NOT NULL AND parameter_id IS NOT NULL) AS touched
            FROM drift",
        update = recompose_statement(&drifted, "TRUE"),
        kind = decisions::Kind::CurveRecompose.as_str(),
        origin = decisions::Origin::Janitor.as_str(),
    );

    // Drift in a chunk past the compression policy has to decompress, and the roll-up carries its
    // own `RETURNING tgt.time`, so this is `guarded` rather than `guarded_mutation`.
    let row = crate::common::bulk_write::guarded(db, async |txn| {
        txn.query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [job_id.into()],
        ))
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
    derived_definition_id: Uuid,
    formula: String,
    site_id: Uuid,
    parameter_id: Uuid,
    parameter_code: String,
}

#[derive(FromQueryResult)]
struct InputRow {
    val: f64,
    measurement_type: Option<String>,
}

struct DerivedWork {
    site_param_id: Uuid,
    derived_definition_id: Uuid,
    formula: String,
    derived_site_id: Uuid,
    derived_parameter_id: Uuid,
    derived_parameter_code: String,
}

/// The newest version of a definition's formula, which is the text this engine is about to
/// evaluate. `None` for a definition minted before versioning, whose rows carry no version rather
/// than a claim about which text produced them (Q89, M134).
async fn newest_derived_version(
    db: &DatabaseConnection,
    definition_id: Uuid,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id FROM derived_parameter_definition_versions \
              WHERE definition_id = $1 ORDER BY version_no DESC LIMIT 1",
        [definition_id.into()],
    ))
    .await?
    .map(|row| row.try_get::<Uuid>("", "id"))
    .transpose()
}

/// The slots this site computes. The producing definition is the one whose output is the slot's
/// parameter; `entry_mode` is the site's own declaration that it computes the slot rather than
/// taking it by hand.
async fn fetch_derived_work_items(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> Result<Vec<DerivedWork>, sea_orm::DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sp.id, d.id AS derived_definition_id, d.formula, sp.site_id, sp.parameter_id,
                     p.code AS parameter_code
              FROM site_parameters sp
              JOIN calculation_formulas d ON d.output_parameter_id = sp.parameter_id
              JOIN parameters p ON p.id = sp.parameter_id
              WHERE sp.site_id = $1 AND sp.entry_mode = 'tool'",
            [site_id.into()],
        ))
        .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let row = DerivedWorkRow::from_query_result(row, "")?;
        items.push(DerivedWork {
            site_param_id: row.id,
            derived_definition_id: row.derived_definition_id,
            formula: row.formula,
            derived_site_id: row.site_id,
            derived_parameter_id: row.parameter_id,
            derived_parameter_code: row.parameter_code,
        });
    }
    Ok(items)
}

async fn build_evaluation_order(
    db: &DatabaseConnection,
    work_items: &[DerivedWork],
) -> Result<Vec<usize>, sea_orm::DbErr> {
    let derived_param_ids: std::collections::HashSet<Uuid> =
        work_items.iter().map(|w| w.derived_parameter_id).collect();

    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(work_items.len());
    for item in work_items {
        let source_param_ids =
            source_parameter_ids_for_definition(db, item.derived_definition_id).await?;
        let mut item_deps = Vec::new();
        for source_param_id in source_param_ids {
            if derived_param_ids.contains(&source_param_id)
                && let Some(other) = work_items
                    .iter()
                    .position(|w| w.derived_parameter_id == source_param_id)
            {
                item_deps.push(other);
            }
        }
        deps.push(item_deps);
    }

    crate::common::dependency::order(&deps).map_err(|cycle| {
        let members: Vec<&str> = cycle
            .iter()
            .map(|&idx| work_items[idx].derived_parameter_code.as_str())
            .collect();
        sea_orm::DbErr::Custom(format!(
            "Derived parameters form a dependency cycle and cannot be evaluated: {}",
            members.join(", ")
        ))
    })
}

async fn get_or_create_derived_stream(
    db: &DatabaseConnection,
    item: &DerivedWork,
) -> Result<Uuid, sea_orm::DbErr> {
    let existing = stream_for_slot(db, item.site_param_id).await?;
    if let Some(id) = existing {
        return Ok(id);
    }

    let def_name: String = calculation_formulas::Entity::find_by_id(item.derived_definition_id)
        .select_only()
        .column(calculation_formulas::Column::Name)
        .into_tuple::<String>()
        .one(db)
        .await?
        .ok_or_else(|| {
            sea_orm::DbErr::Custom(format!(
                "derived_parameter_definition {} not found",
                item.derived_definition_id
            ))
        })?;

    let source_key = format!("{}_{}", def_name, item.derived_site_id);
    let stream_id = Uuid::new_v4();
    let now = Utc::now();
    data_streams::Entity::insert(data_streams::ActiveModel {
        id: Set(stream_id),
        source_system: Set("derived".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some(def_name)),
        site_parameter_id: Set(Some(item.site_param_id)),
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

    stream_for_slot(db, item.site_param_id)
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

async fn source_parameter_ids_for_definition(
    db: &DatabaseConnection,
    derived_definition_id: Uuid,
) -> Result<Vec<Uuid>, sea_orm::DbErr> {
    Ok(derived_sources::Entity::find()
        .filter(derived_sources::Column::DerivedDefinitionId.eq(derived_definition_id))
        .select_only()
        .column(derived_sources::Column::ParameterId)
        .into_tuple::<Option<Uuid>>()
        .all(db)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

pub async fn recalculate_derived_at_timestamp(
    db: &DatabaseConnection,
    site_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    let work_items = fetch_derived_work_items(db, site_id).await?;
    if work_items.is_empty() {
        return Ok(());
    }

    let ordered = build_evaluation_order(db, &work_items).await?;
    for idx in ordered {
        evaluate_and_upsert_derived(db, &work_items[idx], time).await?;
    }
    Ok(())
}

async fn resolve_variables_for_derived(
    db: &DatabaseConnection,
    item: &DerivedWork,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<Option<Option<HashMap<String, f64>>>, sea_orm::DbErr> {
    let mapping_rows: Vec<(String, Option<Uuid>)> = derived_sources::Entity::find()
        .filter(derived_sources::Column::DerivedDefinitionId.eq(item.derived_definition_id))
        .select_only()
        .column(derived_sources::Column::VariableName)
        .column(derived_sources::Column::ParameterId)
        .into_tuple()
        .all(db)
        .await?;

    // A definition with no declared sources computes nothing at any instant. That is a definition
    // that was never finished, not an input that went away, so it leaves whatever is stored alone.
    if mapping_rows.is_empty() {
        return Ok(Some(None));
    }

    let mut variables = HashMap::new();
    for (var_name, source_param_id) in mapping_rows {
        let Some(source_param_id) = source_param_id else {
            continue;
        };

        // Deterministic input pick when a sensor point and a grab share the timestamp:
        // prefer the continuous reading, then tie-break by stream_id (stable across VACUUM).
        // A withdrawn or flagged row is not a measurement, and a sample whose members are all
        // gone carries n = 0 with a NULL mean, so neither may reach the formula.
        let value_row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT COALESCE(CASE WHEN smp.n > 0 THEN smp.mean END,
                                  r.calibrated_value, r.raw_value) as val,
                         r.measurement_type
                  FROM readings r
                  LEFT JOIN samples smp ON smp.id = r.sample_id
                  WHERE r.site_id = $1 AND r.parameter_id = $2 AND r.time = $3
                    AND r.withdrawn_at IS NULL AND r.is_flagged IS NOT TRUE
                  ORDER BY (r.measurement_type IS NOT DISTINCT FROM 'spot') ASC,
                           r.replicate_index ASC, r.stream_id
                  LIMIT 1",
                [
                    item.derived_site_id.into(),
                    source_param_id.into(),
                    time.into(),
                ],
            ))
            .await?;

        match value_row {
            Some(vr) => {
                let input = InputRow::from_query_result(&vr, "")?;
                if input.measurement_type.as_deref() == Some("spot") {
                    tracing::debug!(
                        variable = %var_name,
                        parameter_id = %source_param_id,
                        time = %time,
                        "Derived input resolved from a grab (spot) reading"
                    );
                }
                variables.insert(var_name, input.val)
            }
            None => return Ok(None),
        };
    }
    Ok(Some(Some(variables)))
}

/// Clear the site off a stored derived row, the unattributed state a recalled input leaves it in.
async fn unattribute_derived_at(
    db: &DatabaseConnection,
    item: &DerivedWork,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    crate::common::bulk_write::guarded_mutation(
        db,
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET site_id = NULL
              WHERE site_id = $1 AND parameter_id = $2 AND time = $3
                AND measurement_type = 'derived'",
            [
                item.derived_site_id.into(),
                item.derived_parameter_id.into(),
                time.into(),
            ],
        ),
    )
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
async fn record_formula_transition(
    db: &DatabaseConnection,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    result: f64,
    version: Option<Uuid>,
) -> Result<bool, sea_orm::DbErr> {
    let stored = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT raw_value, derived_version_id FROM readings \
             WHERE stream_id = $1 AND time = $2 AND replicate_index = 0",
            [stream_id.into(), time.into()],
        ))
        .await?;
    let Some(row) = stored else { return Ok(true) };
    let prior = StoredDerived::from_query_result(&row, "")?;
    if prior.raw_value == Some(result) && prior.derived_version_id == version {
        return Ok(false);
    }
    decisions::record(
        db,
        &decisions::Decision {
            key: decisions::DecisionKey {
                stream_id,
                time,
                replicate_index: Some(0),
            },
            kind: decisions::Kind::FormulaTransition,
            new: serde_json::json!({
                "raw_value": result,
                "derived_version_id": version,
            }),
            actor: "system".to_string(),
            reason: None,
            origin: decisions::Origin::System,
            set_id: None,
        },
    )
    .await
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    Ok(false)
}

async fn evaluate_and_upsert_derived(
    db: &DatabaseConnection,
    item: &DerivedWork,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    let Some(resolved) = resolve_variables_for_derived(db, item, time).await? else {
        // The inputs no longer resolve at this instant, so the stored derived value is the output
        // of a measurement that is not served any more. It leaves the site the same way its input
        // did rather than staying in the aggregates and the public arm.
        unattribute_derived_at(db, item, time).await?;
        return Ok(());
    };
    let Some(variables) = resolved else {
        return Ok(());
    };

    let Ok(result) = evaluate_formula(&item.formula, &variables) else {
        return Ok(());
    };
    if !result.is_finite() {
        return Ok(());
    }

    let stream_id = get_or_create_derived_stream(db, item).await?;
    // The row names the formula text it was made with, so a later edit cannot rewrite the story of
    // what this number came from (Q89).
    let version = newest_derived_version(db, item.derived_definition_id).await?;

    // A recompute that moves a stored value or the version it names records the move (Q116), so a
    // person opening the value reads what it was and which formula edit changed it. Recorded
    // before the write, because the decision captures `old` from the row as it still stands; a
    // first insert is not a transition, and a pass that changes neither is not a decision.
    let born = record_formula_transition(db, stream_id, time, result, version).await?;

    // The slot is re-asserted on conflict as well as on insert: a row this engine unattributed
    // when its inputs stopped resolving is the same row it writes when they resolve again, and
    // leaving `site_id` NULL there would recompute a value nothing serves.
    //
    // `raw_value` is the authoritative column for a derived reading and `calibrated_value` is
    // always NULL. A derived value is a computed quantity, not an instrument measurement plus a
    // correction: it has no sensor, no curve and therefore nothing a calibration id could point
    // at, which is exactly the state this model spells NULL. Every consumer reads
    // COALESCE(calibrated_value, raw_value) — including the four continuous aggregates — so the
    // computed number is what is served either way, but only this arrangement survives a
    // recomposition pass, which resolves no curve for a sensor-less row and would otherwise clear
    // the value outright. Writing both columns also made the upsert lopsided: the previous
    // ON CONFLICT maintained only `calibrated_value`, so a recomputed row's `raw_value` stayed
    // frozen at whatever the very first evaluation produced.
    crate::common::bulk_write::guarded_mutation(
        db,
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index, measurement_type, provenance_kind, derived_version_id)
          VALUES ($1, $2, $3, $4, $5, NULL, 0, 'derived', 'derived', $6)
          ON CONFLICT (stream_id, time, replicate_index) DO UPDATE
            SET raw_value = $5, calibrated_value = NULL, measurement_type = 'derived',
                site_id = $2, parameter_id = $3, derived_version_id = $6",
            [
                stream_id.into(),
                item.derived_site_id.into(),
                item.derived_parameter_id.into(),
                time.into(),
                result.into(),
                version.into(),
            ],
        ),
    )
    .await
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    // A slot's first number is a change to the readings as much as a move is (Q57, Q118), and it
    // is recorded after the write because the decision reads the row it is about.
    if born {
        record_derived_arrival(db, stream_id, time).await?;
    }
    Ok(())
}

/// Record the arrival of a derived value, the state it arrived in read from the row itself.
async fn record_derived_arrival(
    db: &DatabaseConnection,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    decisions::record_many(
        db,
        decisions::Kind::DerivedComputed,
        "r.stream_id = $1 AND r.time = $2 AND r.replicate_index = 0",
        vec![stream_id.into(), time.into()],
        decisions::NewValue::Born,
        "system",
        None,
        decisions::Origin::System,
        None,
    )
    .await
    .map(|_| ())
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))
}

pub async fn recompute_valid_until<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        // Windows chain within a (sensor, parameter): a multi-parameter instrument holds one
        // calibration timeline per parameter, so LEAD must partition by parameter_id (never let one
        // parameter's next calibration truncate another's window). Instant curves (grab curves) are
        // matched by calibration_id, never windowed, so they are excluded from the chain.
        //
        // `, id` breaks a tie on valid_from so the chain is single-valued, and the guard refuses to
        // write a zero-width `valid_until = valid_from` window, which would leave a curve the
        // operator can see applying to nothing. Duplicate instants are refused at create
        // (`SensorCalibrationOperations`); the guard covers rows loaded outside the API.
        //
        // A chain-written bound is derived state and is rebuilt from scratch each time. An
        // operator-written one (`valid_until_explicit`) is data, so it is only ever shortened, and
        // then only far enough to keep windows non-overlapping, because the resolver depends on at
        // most one curve covering an instant. `LEAST` ignores a NULL `next_from`, so an explicit
        // bound on the newest curve survives. This is the same policy
        // `recompute_deployed_until` applies to a deployment's end date.
        r"WITH ordered AS (
            SELECT id, valid_from,
                   LEAD(valid_from) OVER (PARTITION BY parameter_id ORDER BY valid_from, id) AS next_from
            FROM sensor_calibrations
            WHERE sensor_id = $1 AND retired_at IS NULL
        )
        UPDATE sensor_calibrations sc
        SET valid_until = CASE
                WHEN sc.valid_until_explicit THEN LEAST(sc.valid_until, ordered.next_from)
                ELSE ordered.next_from
            END
        FROM ordered
        WHERE sc.id = ordered.id AND sc.sensor_id = $1
          AND (ordered.next_from IS NULL OR ordered.next_from > ordered.valid_from)",
        [sensor_id.into()],
    ))
    .await?;
    Ok(())
}

/// Twin of [`recompute_valid_until`] for the deployment timeline: chain each of a sensor's
/// deployments' `deployed_until` down to the next deployment's `deployed_from`. A deployment's end
/// date is always caller-settable, so this only ever *shortens* a window to remove overlap
/// (`LEAST` keeps an existing earlier bound) and never extends one; a calibration's is
/// chain-written unless an operator set it, and shortens only in that case. Shortening can't create
/// an overlap, so the result always satisfies the per-(site, parameter) exclusion constraint.
pub async fn recompute_deployed_until<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"WITH ordered AS (
            SELECT id,
                   LEAST(
                       COALESCE(deployed_until, 'infinity'::timestamptz),
                       COALESCE(LEAD(deployed_from) OVER (PARTITION BY parameter_id ORDER BY deployed_from), 'infinity'::timestamptz)
                   ) AS new_until
            FROM sensor_deployments
            WHERE sensor_id = $1
        )
        UPDATE sensor_deployments d
        SET deployed_until = NULLIF(ordered.new_until, 'infinity'::timestamptz)
        FROM ordered
        WHERE d.id = ordered.id AND d.sensor_id = $1
          AND COALESCE(d.deployed_until, 'infinity'::timestamptz) <> ordered.new_until",
        [sensor_id.into()],
    ))
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
    fn values(self) -> Vec<sea_orm::Value> {
        match self {
            Self::Sensor(sensor_id) => vec![sensor_id.into()],
            Self::Slot {
                site_id,
                parameter_id,
            } => vec![site_id.into(), parameter_id.into()],
        }
    }

    /// The readings this run may rewrite, as `r`.
    fn readings_predicate(self) -> &'static str {
        match self {
            // A derived row carries the slot but no instrument, so no window resolves for it and
            // the outer join below would erase the value the cascade wrote. The sensor arm needs no
            // such guard: a derived row names no instrument to be in scope by.
            Self::Sensor(_) => "r.sensor_id = $1",
            Self::Slot { .. } => {
                "r.site_id = $1 AND r.parameter_id = $2 \
                 AND r.measurement_type IS DISTINCT FROM 'derived'"
            }
        }
    }

    /// Where the curve pick reads the instrument from: the scope's own on the sensor arm, the row's
    /// (which step 1 has just re-owned) on the slot arm.
    fn pick_sensor(self) -> &'static str {
        match self {
            Self::Sensor(_) => "$1",
            Self::Slot { .. } => "r.sensor_id",
        }
    }

    /// The deployments whose windows attribute this run's readings.
    fn deployments_predicate(self) -> &'static str {
        match self {
            Self::Sensor(_) => "sensor_id = $1",
            Self::Slot { .. } => "site_id = $1 AND parameter_id = $2",
        }
    }

    /// The columns the attribution step writes. Only the slot arm re-owns.
    fn attribution_set(self) -> &'static str {
        match self {
            Self::Sensor(_) => "deployment_id = dw.id, site_id = dw.site_id",
            Self::Slot { .. } => {
                "sensor_id = dw.sensor_id, deployment_id = dw.id, site_id = dw.site_id"
            }
        }
    }

    /// The reading columns the attribution step writes, which is what its ledger row records.
    fn attribution_columns(self) -> &'static [&'static str] {
        match self {
            Self::Sensor(_) => &["deployment_id", "site_id"],
            Self::Slot { .. } => &["sensor_id", "deployment_id", "site_id"],
        }
    }

    /// Which readings the attribution step considers, beyond the window overlap.
    fn attribution_scope(self) -> &'static str {
        match self {
            // A deployment names one parameter, so it claims a row of that parameter or an
            // unpaired one.
            Self::Sensor(_) => {
                "r.sensor_id = $1 AND (r.parameter_id IS NULL OR dw.parameter_id = r.parameter_id)"
            }
            // Either the row is at the slot, or it belongs to the instrument the slot's deployment
            // names: that second half is what pulls a swapped instrument's readings back in.
            Self::Slot { .. } => {
                "r.parameter_id = $2 AND (r.site_id = $1 OR r.sensor_id = dw.sensor_id)"
            }
        }
    }

    /// A reading in a gap between deployments belongs to no site. Guarded to `time >= the scope's
    /// first deployment` so readings that predate any deployment keep the site the stream pairing
    /// gave them; an auto-created deployment opens at its stream's first reading, so the floor now
    /// protects hand-dated deployments only.
    fn recall_predicate(self) -> String {
        let windowed = attribution_derivable("r");
        match self {
            Self::Sensor(_) => format!(
                r"r.sensor_id = $1
                    AND r.site_id IS NOT NULL
                    AND {windowed}
                    AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments d2
                                   WHERE d2.sensor_id = $1
                                     AND (r.parameter_id IS NULL OR d2.parameter_id = r.parameter_id))
                    AND NOT EXISTS (
                        SELECT 1 FROM sensor_deployments d
                        WHERE d.sensor_id = $1
                          AND (r.parameter_id IS NULL OR d.parameter_id = r.parameter_id)
                          AND r.time >= d.deployed_from
                          AND r.time < COALESCE(d.deployed_until, 'infinity'::timestamptz)
                    )"
            ),
            Self::Slot { .. } => format!(
                r"r.site_id = $1 AND r.parameter_id = $2
                    AND {windowed}
                    AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments
                                   WHERE site_id = $1 AND parameter_id = $2)
                    AND NOT EXISTS (
                        SELECT 1 FROM sensor_deployments d
                        WHERE d.site_id = $1 AND d.parameter_id = $2
                          AND r.time >= d.deployed_from
                          AND r.time < COALESCE(d.deployed_until, 'infinity'::timestamptz)
                    )"
            ),
        }
    }

    /// The rows whose span the rollup refresh covers.
    fn refresh_predicate(self) -> &'static str {
        match self {
            Self::Sensor(_) => "sensor_id = $1",
            Self::Slot { .. } => "site_id = $1 AND parameter_id = $2",
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
fn record_moved(update_sql: &str, columns: &[&str], job_param: usize) -> String {
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
    format!(
        r"WITH moved AS (
            {update_sql}
          ), recorded AS (
            INSERT INTO reading_decisions
                (stream_id, time, replicate_index, kind, old, new, actor, origin, supersedes,
                 job_id)
            SELECT m.stream_id, m.time, m.replicate_index, '{kind}',
                   jsonb_build_object({old}), jsonb_build_object({new}),
                   'system', '{origin}',
                   (SELECT p.id FROM reading_decisions p
                     WHERE p.stream_id = m.stream_id AND p.time = m.time
                       AND p.replicate_index IS NOT DISTINCT FROM m.replicate_index
                       AND p.kind = '{kind}' AND p.rolled_back_by IS NULL
                     ORDER BY p.at DESC, p.id DESC LIMIT 1),
                   ${job_param}
              FROM moved m
             WHERE {changed}
          )
          SELECT site_id, time FROM moved",
        old = pairs("was"),
        new = pairs("now"),
        kind = decisions::Kind::Reprocess.as_str(),
        origin = decisions::Origin::System.as_str(),
    )
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
/// `orphaned_correction_rows` is the one thing that clear does not reach: a row resolving no window,
/// naming no curve, and holding a number that is not a copy of its raw value was written that way by
/// a caller, and recomputing it here would replace somebody's measurement with a NULL. Those are
/// reported by `GET /actions/calibration_candidates` and left alone.
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
    let values = scope.values();
    // The ledger insert's own bind, after the scope's one or two.
    let job_param = values.len() + 1;
    let mut params = values.clone();
    params.push(job_id.into());

    // Step 1, attribution. On the slot arm this runs BEFORE the curve resolution and the order is
    // the contract: step 2 resolves against `r.sensor_id`, so it picks the curves of the owner
    // step 1 just wrote. Resolving first would stamp the outgoing instrument's curve on a reading
    // the swap hands to the incoming one, and nothing repairs that afterwards.
    let attribution_sql = record_moved(
        &format!(
            r"UPDATE readings r
            SET {set}
            FROM (
                SELECT id, sensor_id, site_id, parameter_id, deployed_from,
                       COALESCE(deployed_until, 'infinity'::timestamptz) AS deployed_until
                FROM sensor_deployments
                WHERE {deployments}
            ) dw, readings prev
            WHERE prev.stream_id = r.stream_id
              AND prev.time = r.time
              AND prev.replicate_index = r.replicate_index
              AND {scope_sql}
              AND {windowed}
              AND r.time >= dw.deployed_from
              AND r.time < dw.deployed_until
            RETURNING r.stream_id, r.time, r.replicate_index, r.site_id, {pairs}",
            set = scope.attribution_set(),
            deployments = scope.deployments_predicate(),
            scope_sql = scope.attribution_scope(),
            windowed = attribution_derivable("r"),
            pairs = moved_pairs("prev", "r", scope.attribution_columns()),
        ),
        scope.attribution_columns(),
        job_param,
    );

    // Step 2, the curve. The pick is `resolver::pick_calibration_lateral`, the same ranking the
    // write paths resolve with, so a reprocess recomputes the value ingest already stored rather
    // than a different one. Which rows a window may claim is `window_resolved_rows`; the spot rows
    // it holds back are step 3's.
    let calibration_sql = repoint_statement(
        &super::resolver::pick_calibration_lateral(scope.pick_sensor()),
        &format!(
            "{scope_sql} AND {windowed} AND NOT (cw.id IS NULL AND ({orphaned}))",
            scope_sql = scope.readings_predicate(),
            windowed = calibration_derivable("r"),
            orphaned = orphaned_correction_rows("r"),
        ),
        r"
            RETURNING tgt.stream_id, tgt.time, tgt.replicate_index, tgt.site_id,
                      picked.p_was_calibration_id AS was_calibration_id,
                      tgt.calibration_id AS now_calibration_id,
                      picked.p_was_calibrated_value AS was_calibrated_value,
                      tgt.calibrated_value AS now_calibrated_value",
    );
    let calibration_sql = record_moved(
        &calibration_sql,
        &["calibration_id", "calibrated_value"],
        job_param,
    );

    // Step 3, the grabs: they keep the curves they were entered against, and their value follows
    // those curves' current coefficients.
    let spot_sql = record_moved(
        &format!(
            r"{} RETURNING tgt.stream_id, tgt.time, tgt.replicate_index, tgt.site_id,
                      r.calibrated_value AS was_calibrated_value,
                      tgt.calibrated_value AS now_calibrated_value",
            recompose_statement("r.measurement_type = 'spot'", scope.readings_predicate())
        ),
        &["calibrated_value"],
        job_param,
    );

    // Step 4, the recall. The site it clears is returned from the pre-update snapshot, because the
    // cascade has to reach the instant a derived value must follow its input out of, and after the
    // write the row names no site at all.
    let recall_predicate = scope.recall_predicate();
    let recall_columns = ["site_id", "deployment_id"];
    let recall_sql = record_moved(
        &format!(
            r"UPDATE readings r
            SET site_id = NULL, deployment_id = NULL
            FROM readings prev
            WHERE prev.stream_id = r.stream_id
              AND prev.time = r.time
              AND prev.replicate_index = r.replicate_index
              AND ({recall_predicate})
            RETURNING r.stream_id, r.time, r.replicate_index, prev.site_id, {pairs}",
            pairs = moved_pairs("prev", "r", &recall_columns),
        ),
        &recall_columns,
        job_param,
    );

    let (readings_updated, cascade) = crate::common::bulk_write::guarded(db, async |txn| {
        let mut touched: Vec<(Uuid, DateTime<Utc>)> = Vec::new();
        let mut readings_updated = 0usize;
        for sql in [&attribution_sql, &calibration_sql, &spot_sql] {
            readings_updated += write_and_collect(txn, sql, params.clone(), &mut touched).await?;
        }

        // The recall's rows are not part of `readings_updated`: it clears an attribution rather
        // than re-deriving one.
        write_and_collect(txn, &recall_sql, params.clone(), &mut touched).await?;

        touched.sort_unstable();
        touched.dedup();
        Ok((readings_updated, touched))
    })
    .await
    .map_err(app_error_as_db_err)?;

    // The cascade runs over what this run moved, not over every instant in the scope: a derived
    // value at (site, time) is a function of the served values, and those changed only where a
    // statement above wrote. Costing a query per instant, the difference is the whole run.
    for (site_id, utc_time) in cascade {
        if let Err(e) = recalculate_derived_at_timestamp(db, site_id, utc_time).await {
            tracing::warn!(
                error = %e,
                site_id = %site_id,
                time = %utc_time,
                "Failed to cascade reprocessing to derived parameter"
            );
        }
    }

    let range = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT MIN(time) AS min_time FROM readings WHERE {}",
                scope.refresh_predicate()
            ),
            values,
        ))
        .await?;
    if let Some(row) = range
        && let Some(since) = row.try_get::<Option<DateTime<Utc>>>("", "min_time")?
    {
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
    sql: &str,
    values: Vec<sea_orm::Value>,
    touched: &mut Vec<(Uuid, DateTime<Utc>)>,
) -> Result<usize, sea_orm::DbErr> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    for row in &rows {
        // `site_id` is nullable on an unpaired reading, which has no slot to touch.
        let moved = MovedReading::from_query_result(row, "")?;
        if let Some(site_id) = moved.site_id {
            touched.push((site_id, moved.time.with_timezone(&Utc)));
        }
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
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
                pick.contains("c.retired_at IS NULL"),
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
}
