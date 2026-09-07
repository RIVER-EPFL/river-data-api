use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashMap;
use uuid::Uuid;

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

/// What a curve-drift sweep moved: the row count and the span those rows cover.
pub struct CurveDrift {
    pub moved: u64,
    pub span: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Rewrite every corrected reading whose stored value is not what its own curves produce.
///
/// Answers self-consistency only, so it needs no window resolution and reaches grabs. A row
/// attributed to the wrong curve for its timestamp is consistent by this measure and is the
/// reprocess engines' subject, not this one's. The span is returned for the caller's aggregate
/// refresh, since a rewritten value leaves the rollups holding the old one.
pub async fn sweep_curve_drift(db: &DatabaseConnection) -> crate::error::AppResult<CurveDrift> {
    let drifted = format!(
        "{corrected} AND tgt.calibrated_value IS DISTINCT FROM ({value})",
        corrected = corrected_rows("r"),
        value = recomposed_own_curve_value(),
    );
    let sql = format!(
        "WITH drift AS (
            {update}
            RETURNING tgt.time
          )
          SELECT count(*) AS moved, min(time) AS lo, max(time) AS hi FROM drift",
        update = recompose_statement(&drifted, "TRUE"),
    );

    // Drift in a chunk past the compression policy has to decompress, and the roll-up carries its
    // own `RETURNING tgt.time`, so this is `guarded` rather than `guarded_mutation`.
    let row = crate::common::bulk_write::guarded(db, async |txn| {
        txn.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await
        .map_err(crate::error::AppError::Database)
    })
    .await?;

    let Some(row) = row else {
        return Ok(CurveDrift {
            moved: 0,
            span: None,
        });
    };
    let moved = u64::try_from(row.try_get::<i64>("", "moved").unwrap_or(0)).unwrap_or(0);
    let lo = row
        .try_get::<Option<DateTime<Utc>>>("", "lo")
        .ok()
        .flatten();
    let hi = row
        .try_get::<Option<DateTime<Utc>>>("", "hi")
        .ok()
        .flatten();
    Ok(CurveDrift {
        moved,
        span: lo.zip(hi),
    })
}

pub fn evaluate_formula(formula: &str, variables: &HashMap<String, f64>) -> Result<f64, String> {
    let expr: meval::Expr = formula.parse().map_err(|e| format!("Parse error: {e}"))?;

    let mut ctx = meval::Context::new();
    for (name, value) in variables {
        ctx.var(name.clone(), *value);
    }

    expr.eval_with_context(ctx)
        .map_err(|e| format!("Evaluation error: {e}"))
}

struct DerivedWork {
    site_param_id: Uuid,
    derived_definition_id: Uuid,
    formula: String,
    derived_site_id: Uuid,
    derived_parameter_id: Uuid,
}

async fn fetch_derived_work_items(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> Result<Vec<DerivedWork>, sea_orm::DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sp.id, sp.derived_definition_id, d.formula, sp.site_id, sp.parameter_id
              FROM site_parameters sp
              JOIN derived_parameter_definitions d ON sp.derived_definition_id = d.id
              WHERE sp.site_id = $1 AND sp.is_derived = true",
            [site_id.into()],
        ))
        .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(DerivedWork {
            site_param_id: row.try_get("", "id")?,
            derived_definition_id: row.try_get("", "derived_definition_id")?,
            formula: row.try_get("", "formula")?,
            derived_site_id: row.try_get("", "site_id")?,
            derived_parameter_id: row.try_get("", "parameter_id")?,
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

    let mut deps: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for item in work_items {
        let source_param_ids =
            source_parameter_ids_for_definition(db, item.derived_definition_id).await?;
        let mut item_deps = Vec::new();
        for source_param_id in source_param_ids {
            if derived_param_ids.contains(&source_param_id)
                && let Some(other) = work_items
                    .iter()
                    .find(|w| w.derived_parameter_id == source_param_id)
            {
                item_deps.push(other.site_param_id);
            }
        }
        deps.insert(item.site_param_id, item_deps);
    }

    let mut evaluated: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let mut ordered: Vec<usize> = Vec::with_capacity(work_items.len());
    let mut remaining: Vec<usize> = (0..work_items.len()).collect();

    for _ in 0..=work_items.len() {
        let mut progress = false;
        remaining.retain(|&idx| {
            let sp_id = work_items[idx].site_param_id;
            let item_deps = deps.get(&sp_id).cloned().unwrap_or_default();
            if item_deps.iter().all(|dep| evaluated.contains(dep)) {
                evaluated.insert(sp_id);
                ordered.push(idx);
                progress = true;
                false
            } else {
                true
            }
        });
        if remaining.is_empty() || !progress {
            break;
        }
    }

    if !remaining.is_empty() {
        tracing::warn!(
            remaining = remaining.len(),
            "Topological sort could not resolve all derived parameter dependencies"
        );
    }

    Ok(ordered)
}

async fn get_or_create_derived_stream(
    db: &DatabaseConnection,
    item: &DerivedWork,
) -> Result<Uuid, sea_orm::DbErr> {
    let existing = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id FROM data_streams WHERE site_parameter_id = $1 LIMIT 1",
            [item.site_param_id.into()],
        ))
        .await?;
    if let Some(row) = existing {
        return row.try_get::<Uuid>("", "id");
    }

    let def_row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT name FROM derived_parameter_definitions WHERE id = $1",
            [item.derived_definition_id.into()],
        ))
        .await?
        .ok_or_else(|| {
            sea_orm::DbErr::Custom(format!(
                "derived_parameter_definition {} not found",
                item.derived_definition_id
            ))
        })?;
    let def_name: String = def_row.try_get("", "name")?;

    let source_key = format!("{}_{}", def_name, item.derived_site_id);
    let stream_id = Uuid::new_v4();
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO data_streams
            (id, source_system, source_key, source_name, site_parameter_id, is_active, discovered_at, paired_at, measurement_type)
          VALUES ($1, 'derived', $2, $3, $4, true, NOW(), NOW(), 'derived')
          ON CONFLICT (source_system, source_key) DO UPDATE
            SET site_parameter_id = EXCLUDED.site_parameter_id
          RETURNING id",
        [
            stream_id.into(),
            source_key.into(),
            def_name.into(),
            item.site_param_id.into(),
        ],
    ))
    .await?;

    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id FROM data_streams WHERE site_parameter_id = $1 LIMIT 1",
            [item.site_param_id.into()],
        ))
        .await?
        .ok_or_else(|| {
            sea_orm::DbErr::Custom(
                "Failed to retrieve derived data stream after upsert".to_string(),
            )
        })?;
    row.try_get::<Uuid>("", "id")
}

async fn source_parameter_ids_for_definition(
    db: &DatabaseConnection,
    derived_definition_id: Uuid,
) -> Result<Vec<Uuid>, sea_orm::DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT parameter_id FROM derived_parameter_sources
              WHERE derived_definition_id = $1",
            [derived_definition_id.into()],
        ))
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        ids.push(row.try_get::<Uuid>("", "parameter_id")?);
    }
    Ok(ids)
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
    let mapping_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT variable_name, parameter_id
              FROM derived_parameter_sources
              WHERE derived_definition_id = $1",
            [item.derived_definition_id.into()],
        ))
        .await?;

    // A definition with no declared sources computes nothing at any instant. That is a definition
    // that was never finished, not an input that went away, so it leaves whatever is stored alone.
    if mapping_rows.is_empty() {
        return Ok(Some(None));
    }

    let mut variables = HashMap::new();
    for row in &mapping_rows {
        let var_name: String = row.try_get("", "variable_name")?;
        let source_param_id: Uuid = row.try_get("", "parameter_id")?;

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
                let mt: Option<String> = vr.try_get("", "measurement_type")?;
                if mt.as_deref() == Some("spot") {
                    tracing::debug!(
                        variable = %var_name,
                        parameter_id = %source_param_id,
                        time = %time,
                        "Derived input resolved from a grab (spot) reading"
                    );
                }
                variables.insert(var_name, vr.try_get("", "val")?)
            }
            None => return Ok(None),
        };
    }
    Ok(Some(Some(variables)))
}

/// The `(site, instant)` pairs a recall is about to clear, read before the UPDATE runs. A cleared
/// instant is no longer selected by the cascade's own query, so it is carried across explicitly:
/// the derived value computed from an input that has just left the site must go with it.
async fn recalled_instants<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    values: Vec<sea_orm::Value>,
) -> Result<Vec<(Uuid, chrono::DateTime<Utc>)>, sea_orm::DbErr> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT DISTINCT r.site_id, r.time FROM readings r WHERE {predicate}"),
            values,
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let site_id: Uuid = row.try_get("", "site_id")?;
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        out.push((site_id, time.with_timezone(&Utc)));
    }
    Ok(out)
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
            r"INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index, measurement_type)
          VALUES ($1, $2, $3, $4, $5, NULL, 0, 'derived')
          ON CONFLICT (stream_id, time, replicate_index) DO UPDATE
            SET raw_value = $5, calibrated_value = NULL, measurement_type = 'derived'",
            [
                stream_id.into(),
                item.derived_site_id.into(),
                item.derived_parameter_id.into(),
                time.into(),
                result.into(),
            ],
        ),
    )
    .await
    .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    Ok(())
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
            WHERE sensor_id = $1
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
    Slot {
        site_id: Uuid,
        parameter_id: Uuid,
    },
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
            Self::Slot { .. } => "sensor_id = dw.sensor_id, deployment_id = dw.id, site_id = dw.site_id",
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
            Self::Slot { .. } => "r.parameter_id = $2 AND (r.site_id = $1 OR r.sensor_id = dw.sensor_id)",
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

pub async fn reprocess_sensor_readings(
    db: &DatabaseConnection,
    sensor_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    // Repair a `valid_until` a bulk load left NULL, so the stored window agrees with the one the
    // resolver serves. `pick_calibration_lateral` is single-valued whether or not windows overlap,
    // so the derivation does not need this; what needs it is the curve editor, which reads
    // `valid_until` and would otherwise show a window open past the point a later curve takes over.
    recompute_valid_until(db, sensor_id).await?;
    reprocess(db, Scope::Sensor(sensor_id)).await
}

/// Per-(site, parameter) reprocess. See [`Scope::Slot`].
pub async fn reprocess_site_parameter_readings(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    reprocess(
        db,
        Scope::Slot {
            site_id,
            parameter_id,
        },
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
pub async fn reprocess(db: &DatabaseConnection, scope: Scope) -> Result<usize, sea_orm::DbErr> {
    let values = scope.values();

    // Step 1, attribution. On the slot arm this runs BEFORE the curve resolution and the order is
    // the contract: step 2 resolves against `r.sensor_id`, so it picks the curves of the owner
    // step 1 just wrote. Resolving first would stamp the outgoing instrument's curve on a reading
    // the swap hands to the incoming one, and nothing repairs that afterwards.
    let attribution_sql = format!(
        r"UPDATE readings r
            SET {set}
            FROM (
                SELECT id, sensor_id, site_id, parameter_id, deployed_from,
                       COALESCE(deployed_until, 'infinity'::timestamptz) AS deployed_until
                FROM sensor_deployments
                WHERE {deployments}
            ) dw
            WHERE {scope_sql}
              AND {windowed}
              AND r.time >= dw.deployed_from
              AND r.time < dw.deployed_until
            RETURNING r.site_id, r.time",
        set = scope.attribution_set(),
        deployments = scope.deployments_predicate(),
        scope_sql = scope.attribution_scope(),
        windowed = attribution_derivable("r"),
    );

    // Step 2, the curve. The pick is `resolver::pick_calibration_lateral`, the same ranking the
    // write paths resolve with, so a reprocess recomputes the value ingest already stored rather
    // than a different one. Which rows a window may claim is `window_resolved_rows`; the spot rows
    // it holds back are step 3's.
    let calibration_sql = format!(
        r"UPDATE readings tgt
            SET calibration_id = picked.cal_id,
                calibrated_value = {value}
            FROM (
                SELECT r.stream_id AS p_stream_id, r.time AS p_time,
                       r.replicate_index AS p_replicate_index,
                       r.standard_curve_id AS p_standard_curve_id,
                       cw.id AS cal_id, cw.slope, cw.intercept
                FROM readings r
                LEFT JOIN LATERAL ({pick}) cw ON true
                WHERE {scope_sql}
                  AND {windowed}
                  AND NOT (cw.id IS NULL AND ({orphaned}))
            ) picked
            LEFT JOIN standard_curves sc ON sc.id = picked.p_standard_curve_id
            WHERE tgt.stream_id = picked.p_stream_id
              AND tgt.time = picked.p_time
              AND tgt.replicate_index = picked.p_replicate_index
            RETURNING tgt.site_id, tgt.time",
        scope_sql = scope.readings_predicate(),
        windowed = calibration_derivable("r"),
        orphaned = orphaned_correction_rows("r"),
        value = recomposed_value_sql(
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
        ),
        pick = super::resolver::pick_calibration_lateral(scope.pick_sensor()),
    );

    // Step 3, the grabs: they keep the curves they were entered against, and their value follows
    // those curves' current coefficients.
    let spot_sql = format!(
        "{} RETURNING tgt.site_id, tgt.time",
        recompose_statement("r.measurement_type = 'spot'", scope.readings_predicate())
    );

    let recall_predicate = scope.recall_predicate();
    let recall_sql =
        format!("UPDATE readings r SET site_id = NULL, deployment_id = NULL WHERE {recall_predicate}");

    let (readings_updated, cascade) = crate::common::bulk_write::guarded(db, async |txn| {
        let mut touched: Vec<(Uuid, DateTime<Utc>)> = Vec::new();
        let mut readings_updated = 0usize;
        for sql in [&attribution_sql, &calibration_sql, &spot_sql] {
            readings_updated += write_and_collect(txn, sql, values.clone(), &mut touched).await?;
        }

        // The instants the recall clears are read before it runs: afterwards they carry no site,
        // and they are exactly the ones whose derived output has to follow its input out of it.
        touched.extend(recalled_instants(txn, &recall_predicate, values.clone()).await?);
        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &recall_sql,
            values.clone(),
        ))
        .await?;

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
        && let Ok(since) = row.try_get::<DateTime<Utc>>("", "min_time")
    {
        crate::common::aggregates::refresh(db, crate::common::aggregates::Window::Since(since))
            .await
            .map_err(app_error_as_db_err)?;
    }

    Ok(readings_updated)
}

/// Run one of the engine's statements, collecting the attributed instants it wrote. Each ends in
/// `RETURNING <target>.site_id, <target>.time`; an unattributed row returns a NULL site and is not
/// an instant anything derives from.
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
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        if let Ok(site_id) = row.try_get::<Uuid>("", "site_id") {
            touched.push((site_id, time.with_timezone(&Utc)));
        }
    }
    Ok(rows.len())
}


#[cfg(test)]
mod tests {
    use super::*;

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
}
