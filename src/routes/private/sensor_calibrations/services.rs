use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, TransactionTrait};
use std::collections::HashMap;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

/// Retry policy for tracked jobs. Set once at startup from `Config`; code paths that don't run
/// `main.rs` (integration tests) see the default — no retries — so their behavior is unchanged.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub backoff_base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            backoff_base: Duration::from_secs(60),
        }
    }
}

static JOB_RETRY_POLICY: OnceLock<RetryPolicy> = OnceLock::new();

/// Initialise the global tracked-job retry policy (call once at startup, from `main.rs`).
pub fn set_job_retry_policy(policy: RetryPolicy) {
    let _ = JOB_RETRY_POLICY.set(policy);
}

fn job_retry_policy() -> RetryPolicy {
    JOB_RETRY_POLICY.get().copied().unwrap_or_default()
}

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
            r"SELECT sensor_id, slope, intercept, valid_from FROM sensor_calibrations WHERE id = $1",
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

    let next_cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT valid_from FROM sensor_calibrations
              WHERE sensor_id = $1 AND valid_from > $2
              ORDER BY valid_from ASC LIMIT 1",
            [sensor_id.into(), valid_from.into()],
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
              WHERE sensor_id = $3 AND time >= $4 AND time < $5",
            [
                slope.into(),
                intercept.into(),
                sensor_id.into(),
                valid_from.into(),
                next_from.into(),
            ],
        ))
        .await?
        .rows_affected()
    } else {
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET calibrated_value = $1 * raw_value + $2
              WHERE sensor_id = $3 AND time >= $4",
            [
                slope.into(),
                intercept.into(),
                sensor_id.into(),
                valid_from.into(),
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
            (id, source_system, source_key, source_name, site_parameter_id, is_active, discovered_at, paired_at)
          VALUES ($1, 'derived', $2, $3, $4, true, NOW(), NOW())
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
        r"INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index)
          VALUES ($1, $2, $3, $4, $5, $5, 0)
          ON CONFLICT (stream_id, time, replicate_index) DO UPDATE SET calibrated_value = $5",
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
        r"WITH ordered AS (
            SELECT id,
                   LEAD(valid_from) OVER (ORDER BY valid_from) AS next_from
            FROM sensor_calibrations
            WHERE sensor_id = $1
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
                       COALESCE(LEAD(deployed_from) OVER (ORDER BY deployed_from), 'infinity'::timestamptz)
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

pub async fn reprocess_sensor_readings(
    db: &DatabaseConnection,
    sensor_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
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
                SELECT id, slope, intercept, valid_from,
                       COALESCE(valid_until, 'infinity'::timestamptz) AS valid_until
                FROM sensor_calibrations
                WHERE sensor_id = $1
            ) cw
            WHERE r.sensor_id = $1
              AND r.time >= cw.valid_from
              AND r.time < cw.valid_until",
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
            SELECT id, site_id, deployed_from,
                   COALESCE(deployed_until, 'infinity'::timestamptz) AS deployed_until
            FROM sensor_deployments
            WHERE sensor_id = $1
        ) dw
        WHERE r.sensor_id = $1
          AND r.time >= dw.deployed_from
          AND r.time < dw.deployed_until",
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
            AND r.time >= (SELECT MIN(deployed_from) FROM sensor_deployments WHERE sensor_id = $1)
            AND NOT EXISTS (
                SELECT 1 FROM sensor_deployments d
                WHERE d.sensor_id = $1
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

/// Generic tracked-job lifecycle with the process-wide retry policy (see [`set_job_retry_policy`]).
/// Inserts a `reprocessing_jobs` row (`status = 'pending'`), emits `JobCreated`, then spawns a
/// background task that runs `work` and records `completed`/`failed`. `work` is a factory (`Fn`)
/// because it may be invoked more than once when retries are enabled.
pub async fn spawn_tracked_job<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    work: F,
) -> Result<Uuid, sea_orm::DbErr>
where
    F: Fn(DatabaseConnection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>> + Send,
{
    let policy = job_retry_policy();
    spawn_tracked_job_with_retry(
        db,
        sensor_id,
        trigger_type,
        trigger_id,
        events,
        policy.max_retries,
        policy.backoff_base,
        work,
    )
    .await
}

/// Like [`spawn_tracked_job`] but with an explicit retry policy. On a failed attempt the row is set
/// to `retrying` (bumping `retry_count`) and `make_work` is re-invoked after an exponential backoff
/// (`backoff_base * 2^(attempt-1)`); only after `max_retries` exhausted is the job marked `failed`.
/// `make_work` must be `Fn` (callable once per attempt). `sensor_id`/`trigger_id` are nullable.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_tracked_job_with_retry<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    max_retries: u32,
    backoff_base: Duration,
    make_work: F,
) -> Result<Uuid, sea_orm::DbErr>
where
    F: Fn(DatabaseConnection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>> + Send,
{
    use sea_orm::Value;

    let job_id = Uuid::new_v4();
    let sensor_id_value: Value = match sensor_id {
        Some(id) => id.into(),
        None => Value::Uuid(None),
    };
    let trigger_id_value: Value = match trigger_id {
        Some(id) => id.into(),
        None => Value::Uuid(None),
    };

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status) \
         VALUES ($1, $2, $3, $4, 'pending')",
        [job_id.into(), sensor_id_value, trigger_type.into(), trigger_id_value],
    ))
    .await?;

    let _ = events.send(crate::common::AppEvent::JobCreated { job_id });

    let db = db.clone();
    let trigger_type = trigger_type.to_string();
    let events = events.clone();
    tokio::spawn(async move {
        if let Err(e) = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                [job_id.into()],
            ))
            .await
        {
            tracing::warn!(error = %e, job_id = %job_id, "Failed to set reprocessing job to running");
        }

        let _ = events.send(crate::common::AppEvent::JobProgress {
            job_id,
            status: "running".into(),
            progress: Some(0),
            total: None,
        });

        let mut attempt = 0u32;
        let outcome = loop {
            match make_work(db.clone()).await {
                Ok(count) => break Ok(count),
                Err(e) if attempt < max_retries => {
                    attempt += 1;
                    let msg = e.to_string();
                    let delay = backoff_base.saturating_mul(2u32.saturating_pow(attempt - 1));
                    tracing::warn!(
                        error = %e,
                        attempt,
                        max_retries,
                        trigger = %trigger_type,
                        delay_ms = delay.as_millis() as u64,
                        "Tracked job attempt failed; retrying"
                    );
                    let _ = db
                        .execute(Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE reprocessing_jobs SET status = 'retrying', error_message = $1, retry_count = $2 WHERE id = $3",
                            [msg.as_str().into(), (attempt as i32).into(), job_id.into()],
                        ))
                        .await;
                    let _ = events.send(crate::common::AppEvent::JobProgress {
                        job_id,
                        status: "retrying".into(),
                        progress: Some(attempt as i32),
                        total: Some(max_retries as i32),
                    });
                    tokio::time::sleep(delay).await;
                    let _ = db
                        .execute(Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                            [job_id.into()],
                        ))
                        .await;
                }
                Err(e) => break Err(e),
            }
        };

        match outcome {
            Ok(count) => {
                if let Err(e) = db
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE reprocessing_jobs \
                         SET status = 'completed', readings_updated = $1, \
                             completed_at = NOW() \
                         WHERE id = $2",
                        [count.into(), job_id.into()],
                    ))
                    .await
                {
                    tracing::warn!(error = %e, job_id = %job_id, "Failed to mark reprocessing job completed");
                }
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id,
                    status: "completed".into(),
                    readings_updated: Some(count as i32),
                    error_message: None,
                });
                tracing::info!(
                    readings_updated = count,
                    trigger = %trigger_type,
                    "Tracked job completed"
                );
            }
            Err(e) => {
                let msg = e.to_string();
                if let Err(db_err) = db
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE reprocessing_jobs \
                         SET status = 'failed', error_message = $1, \
                             completed_at = NOW() \
                         WHERE id = $2",
                        [msg.as_str().into(), job_id.into()],
                    ))
                    .await
                {
                    tracing::warn!(error = %db_err, job_id = %job_id, "Failed to mark reprocessing job failed");
                }
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id,
                    status: "failed".into(),
                    readings_updated: None,
                    error_message: Some(msg.clone()),
                });
                tracing::error!(
                    error = %e,
                    trigger = %trigger_type,
                    "Tracked job failed"
                );
            }
        }
    });

    Ok(job_id)
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
    spawn_tracked_job(
        db,
        Some(sensor_id),
        trigger_type,
        trigger_id,
        events,
        move |db| async move { reprocess_sensor_readings(&db, sensor_id).await.map(|c| c as i64) },
    )
    .await
}
