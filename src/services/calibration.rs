use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashMap;
use uuid::Uuid;

/// Apply a linear calibration: calibrated = slope * raw + intercept
pub fn apply_calibration(raw: f64, slope: f64, intercept: f64) -> f64 {
    slope * raw + intercept
}

/// Recalculate calibrated_value for readings affected by a calibration.
/// Finds the next calibration's valid_from as the upper boundary.
/// Returns the number of updated rows.
pub async fn recalculate_for_calibration(
    db: &DatabaseConnection,
    calibration_id: Uuid,
) -> Result<usize, sea_orm::DbErr> {
    // Get the calibration details
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

    // Find the next calibration's valid_from for this sensor (upper boundary)
    let next_cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT valid_from FROM sensor_calibrations
              WHERE sensor_id = $1 AND valid_from > $2
              ORDER BY valid_from ASC LIMIT 1",
            [sensor_id.into(), valid_from.into()],
        ))
        .await?;

    // Build the UPDATE query with appropriate time boundary
    let result = if let Some(next) = next_cal {
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
    };

    Ok(result.rows_affected() as usize)
}

/// Evaluate a mathematical formula with given variable values.
/// Uses meval for sandboxed expression evaluation (no code execution).
pub fn evaluate_formula(formula: &str, variables: &HashMap<String, f64>) -> Result<f64, String> {
    let expr: meval::Expr = formula.parse().map_err(|e| format!("Parse error: {e}"))?;

    // Bind variables and evaluate
    let mut ctx = meval::Context::new();
    for (name, value) in variables {
        ctx.var(name.clone(), *value);
    }

    expr.eval_with_context(ctx)
        .map_err(|e| format!("Evaluation error: {e}"))
}

/// Recalculate derived parameter values at a specific timestamp for a site.
pub async fn recalculate_derived_at_timestamp(
    db: &DatabaseConnection,
    site_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Result<(), sea_orm::DbErr> {
    // Find derived parameters at this site
    let derived_params = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT p.id, p.variable_mappings, d.formula
              FROM parameters p
              JOIN derived_parameter_definitions d ON p.derived_definition_id = d.id
              WHERE p.site_id = $1 AND p.is_derived = true",
            [site_id.into()],
        ))
        .await?;

    for row in derived_params {
        let param_id: Uuid = row.try_get("", "id")?;
        let mappings: serde_json::Value = row.try_get("", "variable_mappings")?;
        let formula: String = row.try_get("", "formula")?;

        let Some(mapping_obj) = mappings.as_object() else {
            continue;
        };

        // Gather source values
        let mut variables = HashMap::new();
        let mut all_present = true;

        for (var_name, source_param_id_val) in mapping_obj {
            let Some(source_id_str) = source_param_id_val.as_str() else {
                all_present = false;
                break;
            };
            let Ok(source_id) = source_id_str.parse::<Uuid>() else {
                all_present = false;
                break;
            };

            let value_row = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT COALESCE(calibrated_value, raw_value) as val
                      FROM readings WHERE parameter_id = $1 AND time = $2",
                    [source_id.into(), time.into()],
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

        // Evaluate formula
        if let Ok(result) = evaluate_formula(&formula, &variables) {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO readings (parameter_id, time, raw_value, calibrated_value)
                  VALUES ($1, $2, $3, $3)
                  ON CONFLICT (parameter_id, time) DO UPDATE SET calibrated_value = $3",
                [param_id.into(), time.into(), result.into()],
            ))
            .await?;
        }
    }

    Ok(())
}
