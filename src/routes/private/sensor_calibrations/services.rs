use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashMap;
use uuid::Uuid;

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

    let rows_affected = if let Some(ref next) = next_cal {
        let next_from: chrono::DateTime<chrono::FixedOffset> = next.try_get("", "valid_from")?;
        db.execute(Statement::from_sql_and_values(
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
        db.execute(Statement::from_sql_and_values(
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

pub async fn recalculate_derived_at_timestamp(
    db: &DatabaseConnection,
    site_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    let derived_params = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sp.id, sp.variable_mappings, d.formula, sp.site_id, sp.parameter_id
              FROM site_parameters sp
              JOIN derived_parameter_definitions d ON sp.derived_definition_id = d.id
              WHERE sp.site_id = $1 AND sp.is_derived = true",
            [site_id.into()],
        ))
        .await?;

    if derived_params.is_empty() {
        return Ok(());
    }

    struct DerivedWork {
        site_param_id: Uuid,
        mappings: serde_json::Value,
        formula: String,
        derived_site_id: Uuid,
        derived_parameter_id: Uuid,
    }

    let mut work_items: Vec<DerivedWork> = Vec::new();
    for row in &derived_params {
        let site_param_id: Uuid = row.try_get("", "id")?;
        let mappings: serde_json::Value = row.try_get("", "variable_mappings")?;
        let formula: String = row.try_get("", "formula")?;
        let derived_site_id: Uuid = row.try_get("", "site_id")?;
        let derived_parameter_id: Uuid = row.try_get("", "parameter_id")?;
        work_items.push(DerivedWork {
            site_param_id,
            mappings,
            formula,
            derived_site_id,
            derived_parameter_id,
        });
    }

    let derived_param_ids: std::collections::HashSet<Uuid> =
        work_items.iter().map(|w| w.derived_parameter_id).collect();

    let mut deps: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut sp_to_param: HashMap<Uuid, Uuid> = HashMap::new();

    for item in &work_items {
        sp_to_param.insert(item.site_param_id, item.derived_parameter_id);
        let mut item_deps = Vec::new();

        if let Some(mapping_obj) = item.mappings.as_object() {
            for (_var_name, source_val) in mapping_obj {
                if let Some(source_id_str) = source_val.as_str()
                    && let Ok(source_sp_id) = source_id_str.parse::<Uuid>() {
                        if let Some(other) = work_items.iter().find(|w| w.site_param_id == source_sp_id)
                            && derived_param_ids.contains(&other.derived_parameter_id) {
                                item_deps.push(other.site_param_id);
                            }
                    }
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
            site_id = %site_id,
            remaining = remaining.len(),
            "Topological sort could not resolve all derived parameter dependencies"
        );
    }

    for idx in ordered {
        let item = &work_items[idx];

        let Some(mapping_obj) = item.mappings.as_object() else {
            continue;
        };

        let mut variables = HashMap::new();
        let mut all_present = true;

        for (var_name, source_param_id_val) in mapping_obj {
            let Some(source_id_str) = source_param_id_val.as_str() else {
                all_present = false;
                break;
            };
            let Ok(source_sp_id) = source_id_str.parse::<Uuid>() else {
                all_present = false;
                break;
            };

            let value_row = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT COALESCE(r.calibrated_value, r.raw_value) as val
                      FROM readings r
                      JOIN site_parameters sp ON sp.site_id = r.site_id AND sp.parameter_id = r.parameter_id
                      WHERE sp.id = $1 AND r.time = $2",
                    [source_sp_id.into(), time.into()],
                ))
                .await?;

            if let Some(vr) = value_row {
                let val: f64 = vr.try_get("", "val")?;
                variables.insert(var_name.clone(), val);
            } else {
                all_present = false;
                break;
            }
        }

        if !all_present {
            continue;
        }

        if let Ok(result) = evaluate_formula(&item.formula, &variables) {
            if !result.is_finite() {
                continue;
            }

            let stream_row = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT id FROM data_streams WHERE site_parameter_id = $1 LIMIT 1",
                    [item.site_param_id.into()],
                ))
                .await?;

            let Some(stream_row) = stream_row else {
                tracing::warn!(
                    site_parameter_id = %item.site_param_id,
                    "No data stream found for derived site_parameter, skipping"
                );
                continue;
            };
            let stream_id: Uuid = stream_row.try_get("", "id")?;

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
        }
    }

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

pub async fn reprocess_sensor_readings(
    db: &DatabaseConnection,
    sensor_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    let cal_result = db
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

    db.execute(Statement::from_sql_and_values(
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

pub async fn spawn_reprocessing_job(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
) -> Result<Uuid, sea_orm::DbErr> {
    let job_id = Uuid::new_v4();
    let trigger_id_sql = trigger_id
        .map(|id| format!("'{id}'"))
        .unwrap_or_else(|| "NULL".to_string());

    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status) \
             VALUES ('{job_id}', '{sensor_id}', '{trigger_type}', {trigger_id_sql}, 'pending')"
        ),
    ))
    .await?;

    let db = db.clone();
    let trigger_type = trigger_type.to_string();
    tokio::spawn(async move {
        let _ = db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "UPDATE reprocessing_jobs SET status = 'running' WHERE id = '{job_id}'"
                ),
            ))
            .await;

        match reprocess_sensor_readings(&db, sensor_id).await {
            Ok(count) => {
                let _ = db
                    .execute(Statement::from_string(
                        sea_orm::DatabaseBackend::Postgres,
                        format!(
                            "UPDATE reprocessing_jobs \
                             SET status = 'completed', readings_updated = {count}, \
                                 completed_at = NOW() \
                             WHERE id = '{job_id}'"
                        ),
                    ))
                    .await;
                tracing::info!(
                    sensor_id = %sensor_id,
                    readings_updated = count,
                    trigger = %trigger_type,
                    "Reprocessing completed"
                );
            }
            Err(e) => {
                let msg = e.to_string().replace('\'', "''");
                let _ = db
                    .execute(Statement::from_string(
                        sea_orm::DatabaseBackend::Postgres,
                        format!(
                            "UPDATE reprocessing_jobs \
                             SET status = 'failed', error_message = '{msg}', \
                                 completed_at = NOW() \
                             WHERE id = '{job_id}'"
                        ),
                    ))
                    .await;
                tracing::error!(
                    error = %e,
                    sensor_id = %sensor_id,
                    trigger = %trigger_type,
                    "Reprocessing failed"
                );
            }
        }
    });

    Ok(job_id)
}
