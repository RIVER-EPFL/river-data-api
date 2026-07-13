use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, TransactionTrait};
use std::collections::HashMap;
use uuid::Uuid;

// The generic tracked-job lifecycle now lives in `reprocessing_jobs::lifecycle` (the jobs home).
// Re-exported here so existing `sensor_calibrations::services::{spawn_tracked_job, ...}` call sites
// and tests keep compiling against the same path.
pub use crate::routes::private::reprocessing_jobs::lifecycle::{
    JobContext, RetryPolicy, set_job_retry_policy, spawn_tracked_job, spawn_tracked_job_ctx,
    spawn_tracked_job_with_retry,
};

#[must_use]
pub fn apply_calibration(raw: f64, slope: f64, intercept: f64) -> f64 {
    slope * raw + intercept
}

pub async fn recalculate_for_calibration(
    db: &DatabaseConnection,
    calibration_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    let cal_row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sensor_id, slope, intercept, valid_from, parameter_id, mode
              FROM sensor_calibrations WHERE id = $1",
            [calibration_id.into()],
        ))
        .await?;

    let Some(cal) = cal_row else {
        return Ok(0);
    };

    let sensor_id: Uuid = cal.try_get("", "sensor_id")?;
    let slope: f64 = cal.try_get("", "slope")?;
    let intercept: f64 = cal.try_get("", "intercept")?;
    let valid_from: chrono::DateTime<chrono::FixedOffset> = cal.try_get("", "valid_from")?;
    let parameter_id: Option<Uuid> = cal.try_get("", "parameter_id").ok();
    let mode: String = cal
        .try_get::<String>("", "mode")
        .unwrap_or_else(|_| "windowed".to_string());

    // Instant (grab) curves are applied per-reading and never re-windowed: an edit rewrites only the
    // readings stamped with this exact calibration, by calibration_id, not by time window.
    if mode == "instant" {
        let txn = db.begin().await?;
        txn.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
        ))
        .await?;
        let n = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE readings SET calibrated_value = $1 * raw_value + $2 WHERE calibration_id = $3",
                [slope.into(), intercept.into(), calibration_id.into()],
            ))
            .await?
            .rows_affected();
        txn.commit().await?;
        return Ok(n as usize);
    }

    // The next windowed calibration for the SAME parameter bounds this one's window. A NULL
    // parameter_id is a wildcard (pre-decoupling calibrations carry no parameter); once populated,
    // a multi-parameter instrument's parameters each chain independently.
    let next_cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT valid_from FROM sensor_calibrations
              WHERE sensor_id = $1 AND valid_from > $2 AND mode = 'windowed'
                AND ($3::uuid IS NULL OR parameter_id IS NULL OR parameter_id = $3)
              ORDER BY valid_from ASC LIMIT 1",
            [sensor_id.into(), valid_from.into(), parameter_id.into()],
        ))
        .await?;

    // The calibrated_value rewrite runs in a txn with TimescaleDB's per-statement decompression cap
    // lifted (see `reprocess_sensor_readings`); the derived cascade below runs after commit.
    let txn = db.begin().await?;
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;
    let rows_affected = if let Some(ref next) = next_cal {
        let next_from: chrono::DateTime<chrono::FixedOffset> = next.try_get("", "valid_from")?;
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET calibrated_value = $1 * raw_value + $2
              WHERE sensor_id = $3 AND time >= $4 AND time < $5
                AND ($6::uuid IS NULL OR parameter_id IS NULL OR parameter_id = $6)",
            [
                slope.into(),
                intercept.into(),
                sensor_id.into(),
                valid_from.into(),
                next_from.into(),
                parameter_id.into(),
            ],
        ))
        .await?
        .rows_affected()
    } else {
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET calibrated_value = $1 * raw_value + $2
              WHERE sensor_id = $3 AND time >= $4
                AND ($5::uuid IS NULL OR parameter_id IS NULL OR parameter_id = $5)",
            [
                slope.into(),
                intercept.into(),
                sensor_id.into(),
                valid_from.into(),
                parameter_id.into(),
            ],
        ))
        .await?
        .rows_affected()
    };
    txn.commit().await?;

    if rows_affected > 0 {
        let affected_rows = if let Some(ref next) = next_cal {
            let next_from: chrono::DateTime<chrono::FixedOffset> =
                next.try_get("", "valid_from")?;
            db.query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT site_id, time FROM readings
                  WHERE sensor_id = $1 AND time >= $2 AND time < $3",
                [sensor_id.into(), valid_from.into(), next_from.into()],
            ))
            .await?
        } else {
            db.query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT site_id, time FROM readings
                  WHERE sensor_id = $1 AND time >= $2",
                [sensor_id.into(), valid_from.into()],
            ))
            .await?
        };

        for row in &affected_rows {
            let site_id: Uuid = row.try_get("", "site_id")?;
            let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
            let utc_time = time.with_timezone(&chrono::Utc);
            if let Err(e) = recalculate_derived_at_timestamp(db, site_id, utc_time).await {
                tracing::warn!(
                    error = %e,
                    site_id = %site_id,
                    time = %utc_time,
                    "Failed to cascade recalculation to derived parameter"
                );
            }
        }
    }

    Ok(rows_affected as usize)
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

        let value_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT COALESCE(r.calibrated_value, r.raw_value) as val
                  FROM readings r
                  WHERE r.site_id = $1 AND r.parameter_id = $2 AND r.time = $3
                  ORDER BY r.replicate_index ASC
                  LIMIT 1",
                [item.derived_site_id.into(), source_param_id.into(), time.into()],
            ))
            .await?;

        match value_row {
            Some(vr) => variables.insert(var_name, vr.try_get("", "val")?),
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

pub async fn recompute_valid_until(
    db: &DatabaseConnection,
    sensor_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        // Windows chain within a (sensor, parameter): a multi-parameter instrument holds one
        // calibration timeline per parameter, so LEAD must partition by parameter_id (never let one
        // parameter's next calibration truncate another's window). Instant curves (grab curves) are
        // matched by calibration_id, never windowed, so they are excluded from the chain.
        r"WITH ordered AS (
            SELECT id,
                   LEAD(valid_from) OVER (PARTITION BY parameter_id ORDER BY valid_from) AS next_from
            FROM sensor_calibrations
            WHERE sensor_id = $1 AND mode = 'windowed'
        )
        UPDATE sensor_calibrations sc
        SET valid_until = ordered.next_from
        FROM ordered
        WHERE sc.id = ordered.id AND sc.sensor_id = $1",
        [sensor_id.into()],
    ))
    .await?;
    Ok(())
}

/// Twin of [`recompute_valid_until`] for the deployment timeline: chain each of a sensor's
/// deployments' `deployed_until` down to the next deployment's `deployed_from`. Unlike calibrations
/// — which absorb gaps (`valid_until = LEAD(valid_from)`) so coverage is continuous — deployments
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

/// If readings exist before the sensor's first calibration, create or extend an identity
/// calibration (slope=1, intercept=0) to cover them. Never modifies existing non-identity
/// calibrations — the identity only fills the uncovered region before the first real calibration.
async fn ensure_calibration_coverage(
    db: &DatabaseConnection,
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
    let earliest = match earliest {
        Some(t) => t,
        None => return Ok(()),
    };

    let first_cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, slope, intercept, valid_from
              FROM sensor_calibrations
              WHERE sensor_id = $1
              ORDER BY valid_from ASC
              LIMIT 1",
            [sensor_id.into()],
        ))
        .await?;

    match first_cal {
        None => {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO sensor_calibrations
                      (id, sensor_id, slope, intercept, valid_from, performed_by, notes, created_at)
                  VALUES (gen_random_uuid(), $1, 1.0, 0.0, $2, 'system', 'Identity calibration (auto-created)', NOW())",
                [sensor_id.into(), earliest.into()],
            ))
            .await?;
        }
        Some(ref cal) => {
            let slope: f64 = cal.try_get("", "slope").unwrap_or(1.0);
            let intercept: f64 = cal.try_get("", "intercept").unwrap_or(0.0);
            let cal_id: Uuid = cal.try_get("", "id")?;
            let is_identity =
                (slope - 1.0).abs() < f64::EPSILON && intercept.abs() < f64::EPSILON;

            if is_identity {
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE sensor_calibrations SET valid_from = $1 WHERE id = $2",
                    [earliest.into(), cal_id.into()],
                ))
                .await?;
            } else {
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"INSERT INTO sensor_calibrations
                          (id, sensor_id, slope, intercept, valid_from, performed_by, notes, created_at)
                      VALUES (gen_random_uuid(), $1, 1.0, 0.0, $2, 'system', 'Identity calibration (auto-created)', NOW())",
                    [sensor_id.into(), earliest.into()],
                ))
                .await?;
            }
        }
    }

    recompute_valid_until(db, sensor_id).await?;
    tracing::info!(
        sensor_id = %sensor_id,
        uncalibrated = cnt,
        extended_to = %earliest,
        "auto-extended calibration coverage"
    );
    Ok(())
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

    // The bulk re-derivation runs in one transaction with TimescaleDB's per-statement decompression
    // cap lifted (default 100k tuples). A deep-historical reprocess rewrites rows in compressed
    // (>30-day) chunks and would otherwise abort the job; SET LOCAL resets on commit and is a no-op
    // on uncompressed data. The read-back, derived cascade, and continuous-aggregate refresh run
    // AFTER commit — a CAGG refresh cannot run inside a transaction.
    let txn = db.begin().await?;
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

    let cal_result = txn
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
            SET calibration_id = cw.id,
                calibrated_value = cw.slope * r.raw_value + cw.intercept
            FROM (
                SELECT id, slope, intercept, valid_from, parameter_id,
                       COALESCE(valid_until, 'infinity'::timestamptz) AS valid_until
                FROM sensor_calibrations
                WHERE sensor_id = $1 AND mode = 'windowed'
            ) cw
            WHERE r.sensor_id = $1
              AND r.time >= cw.valid_from
              AND r.time < cw.valid_until
              AND (cw.parameter_id IS NULL OR r.parameter_id IS NULL OR cw.parameter_id = r.parameter_id)",
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
          AND r.time >= dw.deployed_from
          AND r.time < dw.deployed_until
          AND (dw.parameter_id IS NULL OR r.parameter_id IS NULL OR dw.parameter_id = r.parameter_id)",
        [sensor_id.into()],
    ))
    .await?;

    // Recall: a reading that falls in a gap between/after the sensor's deployments (the sensor was
    // pulled out — e.g. sitting in the lab) belongs to no site. Clear its site/deployment so it
    // drops out of the continuous aggregates. Guarded to `time >= the sensor's first deployment` so
    // readings that predate any deployment keep the site_id the stream pairing gave them (auto-created
    // deployments start at pairing time, not data start — without this guard a reprocess would
    // un-attribute all historical data).
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET site_id = NULL, deployment_id = NULL
          WHERE r.sensor_id = $1
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

    txn.commit().await?;

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
            crate::common::sync_state::refresh_continuous_aggregates(db, Some(since)).await;
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
    // Steps 1-3 run in one transaction with TimescaleDB's per-statement decompression cap lifted
    // (see `reprocess_sensor_readings`); the derived cascade + aggregate refresh follow after commit.
    let txn = db.begin().await?;
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

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
                AND (r.site_id = $1 OR r.sensor_id = dw.sensor_id)
                AND r.time >= dw.deployed_from
                AND r.time < dw.deployed_until",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    let updated = dep_result.rows_affected() as usize;

    // 2. Re-derive calibrated_value/calibration_id for the (now correct) owner per cal window.
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET calibration_id = cw.id,
              calibrated_value = cw.slope * r.raw_value + cw.intercept
          FROM sensor_calibrations cw
          WHERE r.site_id = $1 AND r.parameter_id = $2
            AND cw.sensor_id = r.sensor_id
            AND cw.mode = 'windowed'
            AND (cw.parameter_id IS NULL OR cw.parameter_id = $2)
            AND r.time >= cw.valid_from
            AND r.time < COALESCE(cw.valid_until, 'infinity'::timestamptz)",
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

    txn.commit().await?;

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
        crate::common::sync_state::refresh_continuous_aggregates(db, Some(since)).await;
    }
    Ok(updated)
}

/// Thin wrapper over [`spawn_tracked_job`] whose work re-derives FK columns and
/// `calibrated_value` for every reading owned by `sensor_id`. Signature unchanged so the
/// calibration/deployment CrudCrate hooks keep compiling against it.
pub async fn spawn_reprocessing_job(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
) -> Result<Uuid, sea_orm::DbErr> {
    spawn_tracked_job_ctx(
        db,
        Some(sensor_id),
        trigger_type,
        trigger_id,
        events,
        move |ctx| async move {
            ctx.info(&format!("Reprocessing readings for sensor {sensor_id}"))
                .await;
            let count = reprocess_sensor_readings(ctx.db(), sensor_id).await?;
            // Scope the job to the sensor's site(s) and record what it touched, so the timeline
            // shows which sensor/site and how many readings were re-derived.
            if let Ok(Some(row)) = ctx
                .db()
                .query_one(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT DISTINCT site_id FROM readings WHERE sensor_id = $1 AND site_id IS NOT NULL LIMIT 1",
                    [sensor_id.into()],
                ))
                .await
            {
                if let Ok(site_id) = row.try_get::<Uuid>("", "site_id") {
                    ctx.set_site(site_id).await;
                }
            }
            ctx.set_detail(serde_json::json!({
                "scope": { "sensor_id": sensor_id },
                "counts": { "readings_updated": count },
            }))
            .await;
            ctx.info(&format!("Re-derived {count} readings")).await;
            Ok(count as i64)
        },
    )
    .await
}
