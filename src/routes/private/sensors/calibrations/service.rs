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

// The generic tracked-job lifecycle now lives in `reprocessing_jobs::lifecycle` (the jobs home).
// Re-exported here so existing `calibrations::service::{spawn_tracked_job, ...}` call sites
// and tests keep compiling against the same path.
pub use crate::routes::private::reprocessing_jobs::lifecycle::{
    JobContext, RetryPolicy, set_job_retry_policy, spawn_tracked_job, spawn_tracked_job_ctx,
    spawn_tracked_job_with_retry,
};

#[must_use]
pub fn apply_calibration(raw: f64, slope: f64, intercept: f64) -> f64 {
    slope * raw + intercept
}

/// Whether a calibration is the identity transform (slope 1, intercept 0), i.e. `calibrated == raw`.
#[must_use]
pub fn is_identity_calibration(slope: f64, intercept: f64) -> bool {
    (slope - 1.0).abs() < f64::EPSILON && intercept.abs() < f64::EPSILON
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
        .query_all(Statement::from_sql_and_values(
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
        let source_param_ids = source_parameter_ids_for_definition(db, item.derived_definition_id).await?;
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
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id FROM data_streams WHERE site_parameter_id = $1 LIMIT 1",
            [item.site_param_id.into()],
        ))
        .await?;
    if let Some(row) = existing {
        return row.try_get::<Uuid>("", "id");
    }

    let def_row = db
        .query_one(Statement::from_sql_and_values(
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
    db.execute(Statement::from_sql_and_values(
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
        .query_one(Statement::from_sql_and_values(
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
        .query_all(Statement::from_sql_and_values(
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
) -> Result<Option<HashMap<String, f64>>, sea_orm::DbErr> {
    let mapping_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT variable_name, parameter_id
              FROM derived_parameter_sources
              WHERE derived_definition_id = $1",
            [item.derived_definition_id.into()],
        ))
        .await?;

    if mapping_rows.is_empty() {
        return Ok(None);
    }

    let mut variables = HashMap::new();
    for row in &mapping_rows {
        let var_name: String = row.try_get("", "variable_name")?;
        let source_param_id: Uuid = row.try_get("", "parameter_id")?;

        // Deterministic input pick when a sensor point and a grab share the timestamp:
        // prefer the continuous reading, then tie-break by stream_id (stable across VACUUM).
        let value_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT COALESCE(smp.mean, r.calibrated_value, r.raw_value) as val,
                         r.measurement_type
                  FROM readings r
                  LEFT JOIN samples smp ON smp.id = r.sample_id
                  WHERE r.site_id = $1 AND r.parameter_id = $2 AND r.time = $3
                  ORDER BY (r.measurement_type IS NOT DISTINCT FROM 'spot') ASC,
                           r.replicate_index ASC, r.stream_id
                  LIMIT 1",
                [item.derived_site_id.into(), source_param_id.into(), time.into()],
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
    Ok(Some(variables))
}

async fn evaluate_and_upsert_derived(
    db: &DatabaseConnection,
    item: &DerivedWork,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    let Some(variables) = resolve_variables_for_derived(db, item, time).await? else {
        return Ok(());
    };

    let Ok(result) = evaluate_formula(&item.formula, &variables) else {
        return Ok(());
    };
    if !result.is_finite() {
        return Ok(());
    }

    let stream_id = get_or_create_derived_stream(db, item).await?;

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index, measurement_type)
          VALUES ($1, $2, $3, $4, $5, $5, 0, 'derived')
          ON CONFLICT (stream_id, time, replicate_index) DO UPDATE
            SET calibrated_value = $5, measurement_type = 'derived'",
        [
            stream_id.into(),
            item.derived_site_id.into(),
            item.derived_parameter_id.into(),
            time.into(),
            result.into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn recompute_valid_until<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_sql_and_values(
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
        r"WITH ordered AS (
            SELECT id, valid_from,
                   LEAD(valid_from) OVER (PARTITION BY parameter_id ORDER BY valid_from, id) AS next_from
            FROM sensor_calibrations
            WHERE sensor_id = $1 AND mode = 'windowed'
        )
        UPDATE sensor_calibrations sc
        SET valid_until = ordered.next_from
        FROM ordered
        WHERE sc.id = ordered.id AND sc.sensor_id = $1
          AND (ordered.next_from IS NULL OR ordered.next_from > ordered.valid_from)",
        [sensor_id.into()],
    ))
    .await?;
    Ok(())
}

/// Twin of [`recompute_valid_until`] for the deployment timeline: chain each of a sensor's
/// deployments' `deployed_until` down to the next deployment's `deployed_from`. Unlike calibrations
///, which absorb gaps (`valid_until = LEAD(valid_from)`) so coverage is continuous, deployments
/// may legitimately have gaps (a sensor sitting in the lab between field campaigns), so this only
/// ever *shortens* a window to remove overlap (`LEAST` keeps an existing earlier bound) and never
/// extends one. Shortening can't create an overlap, so the result always satisfies the per-(site,
/// parameter) exclusion constraint.
pub async fn recompute_deployed_until<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_sql_and_values(
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

/// Extend the sensor's calibration timeline backwards so `earliest` is covered, and no further.
///
/// The only writer of an auto-created identity curve. Three shapes, all of which only ever grow
/// coverage at the front of the timeline:
///
/// - no timeline yet, insert an identity curve starting at `earliest`;
/// - the timeline opens with an identity curve, backdate its `valid_from` to `earliest`;
/// - the timeline opens with a real curve, insert an identity curve ahead of it.
///
/// `earliest` at or after the first curve's `valid_from` is a no-op. That bound is what keeps an
/// identity curve out of the middle of a timeline: inserted there it becomes the next window, the
/// chain retracts the scientist's curve to it, and the readings after that instant revert to raw.
/// Existing curves are otherwise never modified.
pub async fn ensure_identity_covers<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    earliest: DateTime<Utc>,
) -> Result<(), sea_orm::DbErr> {
    const INSERT_IDENTITY: &str = r"INSERT INTO sensor_calibrations
            (id, sensor_id, slope, intercept, valid_from, performed_by, notes, created_at)
        VALUES (gen_random_uuid(), $1, 1.0, 0.0, $2, 'system', 'Identity calibration (auto-created)', NOW())";

    let first_cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, slope, intercept, valid_from
              FROM sensor_calibrations
              WHERE sensor_id = $1 AND mode = 'windowed'
              ORDER BY valid_from ASC, id ASC
              LIMIT 1",
            [sensor_id.into()],
        ))
        .await?;

    match first_cal {
        None => {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                INSERT_IDENTITY,
                [sensor_id.into(), earliest.into()],
            ))
            .await?;
        }
        Some(ref cal) => {
            let first_from: chrono::DateTime<chrono::FixedOffset> = cal.try_get("", "valid_from")?;
            if earliest >= first_from.with_timezone(&Utc) {
                return Ok(());
            }
            let slope: f64 = cal.try_get("", "slope").unwrap_or(1.0);
            let intercept: f64 = cal.try_get("", "intercept").unwrap_or(0.0);
            let cal_id: Uuid = cal.try_get("", "id")?;

            if is_identity_calibration(slope, intercept) {
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE sensor_calibrations SET valid_from = $1 WHERE id = $2",
                    [earliest.into(), cal_id.into()],
                ))
                .await?;
            } else {
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    INSERT_IDENTITY,
                    [sensor_id.into(), earliest.into()],
                ))
                .await?;
            }
        }
    }

    recompute_valid_until(db, sensor_id).await?;
    tracing::info!(
        sensor_id = %sensor_id,
        extended_to = %earliest,
        "auto-extended calibration coverage"
    );
    Ok(())
}

/// Cover the sensor's readings that predate its first calibration, by delegating to
/// [`ensure_identity_covers`]. Readings from the first curve onwards are already covered by the
/// chain, so only the leading region is in question.
pub async fn ensure_calibration_coverage<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT COUNT(*) AS cnt, MIN(r.time) AS earliest
              FROM readings r
              WHERE r.sensor_id = $1
                AND r.calibration_id IS NULL
                AND r.time < COALESCE(
                    (SELECT MIN(valid_from) FROM sensor_calibrations WHERE sensor_id = $1),
                    'infinity'::timestamptz
                )",
            [sensor_id.into()],
        ))
        .await?;

    let (cnt, earliest): (i64, Option<DateTime<Utc>>) = match row {
        Some(ref r) => {
            let c: i64 = r.try_get("", "cnt").unwrap_or(0);
            let e = r
                .try_get::<chrono::DateTime<chrono::FixedOffset>>("", "earliest")
                .ok()
                .map(|t| t.with_timezone(&Utc));
            (c, e)
        }
        None => (0, None),
    };

    if cnt == 0 {
        return Ok(());
    }
    let Some(earliest) = earliest else {
        return Ok(());
    };

    ensure_identity_covers(db, sensor_id, earliest).await
}

pub async fn reprocess_sensor_readings(
    db: &DatabaseConnection,
    sensor_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    // Ensure calibration windows cover all readings before the main re-derivation.
    // Creates or extends an identity calibration for any pre-first-calibration gap.
    ensure_calibration_coverage(db, sensor_id).await?;

    // Chain each parameter's calibration windows (valid_until = next valid_from) before deriving, so
    // the window UPDATE below is single-valued. Calibrations inserted outside the CRUD hooks (bulk
    // load, tests) may carry no valid_until; without this two open windows on the same parameter would
    // both cover a reading and the UPDATE..FROM would pick one arbitrarily (nondeterministic).
    recompute_valid_until(db, sensor_id).await?;

    // The bulk re-derivation runs in one guarded transaction (`common::bulk_write`), which lifts
    // TimescaleDB's per-statement decompression cap: a deep-historical reprocess rewrites rows in
    // compressed (>30-day) chunks and would otherwise abort the job. The read-back, derived cascade
    // and continuous-aggregate refresh run AFTER commit, a CAGG refresh cannot run inside a
    // transaction.
    //
    // The calibration pick is `resolver::pick_calibration_lateral`, the same ranking the write paths
    // resolve with, so reprocess recomputes the value ingest already stored rather than a different
    // one.
    let cal_sql = format!(
        r"UPDATE readings tgt
            SET calibration_id = picked.cal_id,
                calibrated_value = picked.slope * tgt.raw_value + picked.intercept
            FROM (
                SELECT r.stream_id, r.time, r.replicate_index,
                       cw.id AS cal_id, cw.slope, cw.intercept
                FROM readings r
                JOIN LATERAL ({pick}) cw ON true
                WHERE r.sensor_id = $1
                  AND r.measurement_type IS DISTINCT FROM 'spot'
            ) picked
            WHERE tgt.stream_id = picked.stream_id
              AND tgt.time = picked.time
              AND tgt.replicate_index = picked.replicate_index",
        pick = super::resolver::pick_calibration_lateral("$1")
    );

    let readings_updated = crate::common::bulk_write::guarded(db, async |txn| {
        let cal_result = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &cal_sql,
                [sensor_id.into()],
            ))
            .await?;
        let readings_updated = cal_result.rows_affected() as usize;

        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
            SET deployment_id = dw.id,
                site_id = dw.site_id
            FROM (
                SELECT id, site_id, parameter_id, deployed_from,
                       COALESCE(deployed_until, 'infinity'::timestamptz) AS deployed_until
                FROM sensor_deployments
                WHERE sensor_id = $1
            ) dw
            WHERE r.sensor_id = $1
              AND r.measurement_type IS DISTINCT FROM 'spot'
              AND r.time >= dw.deployed_from
              AND r.time < dw.deployed_until
              AND (dw.parameter_id IS NULL OR r.parameter_id IS NULL OR dw.parameter_id = r.parameter_id)",
            [sensor_id.into()],
        ))
        .await?;

        // Recall: a reading that falls in a gap between/after the sensor's deployments (the sensor
        // was pulled out, e.g. sitting in the lab) belongs to no site. Clear its site/deployment so
        // it drops out of the continuous aggregates. Guarded to `time >= the sensor's first
        // deployment` so readings that predate any deployment keep the site_id the stream pairing
        // gave them (auto-created deployments start at pairing time, not data start; without this
        // guard a reprocess would un-attribute all historical data).
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
              SET site_id = NULL, deployment_id = NULL
              WHERE r.sensor_id = $1
                AND r.measurement_type IS DISTINCT FROM 'spot'
                AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments d2
                               WHERE d2.sensor_id = $1
                                 AND (d2.parameter_id IS NULL OR r.parameter_id IS NULL
                                      OR d2.parameter_id = r.parameter_id))
                AND NOT EXISTS (
                    SELECT 1 FROM sensor_deployments d
                    WHERE d.sensor_id = $1
                      AND (d.parameter_id IS NULL OR r.parameter_id IS NULL
                           OR d.parameter_id = r.parameter_id)
                      AND r.time >= d.deployed_from
                      AND r.time < COALESCE(d.deployed_until, 'infinity'::timestamptz)
                )",
            [sensor_id.into()],
        ))
        .await?;

        Ok(readings_updated)
    })
    .await
    .map_err(app_error_as_db_err)?;

    let affected = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT site_id, time FROM readings
              WHERE sensor_id = $1 AND site_id IS NOT NULL",
            [sensor_id.into()],
        ))
        .await?;

    for row in &affected {
        let site_id: Uuid = row.try_get("", "site_id")?;
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        let utc_time = time.with_timezone(&Utc);
        if let Err(e) = recalculate_derived_at_timestamp(db, site_id, utc_time).await {
            tracing::warn!(
                error = %e,
                site_id = %site_id,
                time = %utc_time,
                "Failed to cascade reprocessing to derived parameter"
            );
        }
    }

    let time_range = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT MIN(time) AS min_time, MAX(time) AS max_time
              FROM readings WHERE sensor_id = $1",
            [sensor_id.into()],
        ))
        .await?;

    if let Some(ref range) = time_range {
        let min_time: Option<DateTime<Utc>> = range.try_get("", "min_time").ok();
        if let Some(since) = min_time {
            crate::common::aggregates::refresh(db, crate::common::aggregates::Window::Since(since))
                .await
                .map_err(app_error_as_db_err)?;
        }
    }

    Ok(readings_updated)
}

/// Per-(site, parameter) twin of [`reprocess_sensor_readings`]. Where the per-sensor reprocess
/// re-derives FK columns for rows it already owns (`r.sensor_id = $sensor`), this re-derives the
/// OWNER too: for every reading at `(site_id, parameter_id)`, it sets `sensor_id`/`deployment_id`/
/// `calibration_id`/`calibrated_value` from whichever deployment+calibration window covers the
/// reading time. This is what makes a sensor SWAP (B replaces A at one feed) re-attribute A's
/// post-swap readings to B. The `excl_deployment_site_param_slot` constraint guarantees at most one
/// covering deployment per time, so the owner is unambiguous and the join is single-valued.
///
/// Like the per-sensor engine, the recall NULL-clear is guarded to `time >= the slot's first
/// deployment` so pre-deployment history keeps its pairing site_id.
pub async fn reprocess_site_parameter_readings(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    // Steps 1-3 run in one guarded transaction (`common::bulk_write`), which lifts TimescaleDB's
    // per-statement decompression cap; the derived cascade and aggregate refresh follow after commit.
    // Step 2 resolves with `resolver::pick_calibration_lateral`, the ranking every other path uses.
    let cal_sql = format!(
        r"UPDATE readings tgt
          SET calibration_id = picked.cal_id,
              calibrated_value = picked.slope * tgt.raw_value + picked.intercept
          FROM (
              SELECT r.stream_id, r.time, r.replicate_index,
                     cw.id AS cal_id, cw.slope, cw.intercept
              FROM readings r
              JOIN LATERAL ({pick}) cw ON true
              WHERE r.site_id = $1 AND r.parameter_id = $2
                AND r.measurement_type IS DISTINCT FROM 'spot'
          ) picked
          WHERE tgt.stream_id = picked.stream_id
            AND tgt.time = picked.time
            AND tgt.replicate_index = picked.replicate_index",
        pick = super::resolver::pick_calibration_lateral("r.sensor_id")
    );

    let updated = crate::common::bulk_write::guarded(db, async |txn| {
        // 1. Re-own + re-stamp deployment/site from the (site, parameter) deployment timeline.
        let dep_result = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE readings r
                  SET sensor_id = dw.sensor_id,
                      deployment_id = dw.id,
                      site_id = dw.site_id
                  FROM (
                      SELECT id, sensor_id, site_id, deployed_from,
                             COALESCE(deployed_until, 'infinity'::timestamptz) AS deployed_until
                      FROM sensor_deployments
                      WHERE site_id = $1 AND parameter_id = $2
                  ) dw
                  WHERE r.parameter_id = $2
                    AND r.measurement_type IS DISTINCT FROM 'spot'
                    AND (r.site_id = $1 OR r.sensor_id = dw.sensor_id)
                    AND r.time >= dw.deployed_from
                    AND r.time < dw.deployed_until",
                [site_id.into(), parameter_id.into()],
            ))
            .await?;
        let updated = dep_result.rows_affected() as usize;

        // 2. Re-derive calibrated_value/calibration_id for the (now correct) owner.
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &cal_sql,
            [site_id.into(), parameter_id.into()],
        ))
        .await?;

        // 3. Recall NULL-clear: a reading in a deployment gap drops out of the site (guarded to
        //    time >= the slot's first deployment so pre-deployment history is kept).
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
              SET site_id = NULL, deployment_id = NULL
              WHERE r.site_id = $1 AND r.parameter_id = $2
                AND r.measurement_type IS DISTINCT FROM 'spot'
                AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments
                               WHERE site_id = $1 AND parameter_id = $2)
                AND NOT EXISTS (
                    SELECT 1 FROM sensor_deployments d
                    WHERE d.site_id = $1 AND d.parameter_id = $2
                      AND r.time >= d.deployed_from
                      AND r.time < COALESCE(d.deployed_until, 'infinity'::timestamptz)
                )",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;

        Ok(updated)
    })
    .await
    .map_err(app_error_as_db_err)?;

    // 4. Cascade derived + refresh aggregates over the affected range (same tail as per-sensor).
    let affected = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT site_id, time FROM readings
              WHERE site_id = $1 AND parameter_id = $2",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    for row in &affected {
        let sid: Uuid = row.try_get("", "site_id")?;
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        let utc = time.with_timezone(&Utc);
        if let Err(e) = recalculate_derived_at_timestamp(db, sid, utc).await {
            tracing::warn!(error = %e, site_id = %sid, time = %utc,
                "Failed to cascade (site,parameter) reprocess to derived parameter");
        }
    }
    let range = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT MIN(time) AS min_time FROM readings WHERE site_id = $1 AND parameter_id = $2",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    if let Some(r) = range
        && let Ok(since) = r.try_get::<DateTime<Utc>>("", "min_time")
    {
        crate::common::aggregates::refresh(db, crate::common::aggregates::Window::Since(since))
            .await
            .map_err(app_error_as_db_err)?;
    }
    Ok(updated)
}
