use async_trait::async_trait;
use chrono::Utc;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    QueryFilter, QueryOrder, Set, Statement,
};
use std::collections::HashMap;
use uuid::Uuid;

use crate::routes::private::{data_streams, sensor_calibrations, sensor_deployments, sensors};
use super::model::Sensor;
use crate::error::{AppError, AppResult};

pub struct SensorOperations;

fn build_in_clause(count: usize) -> String {
    (1..=count).map(|i| format!("${i}")).collect::<Vec<_>>().join(", ")
}

fn uuid_values(ids: &[Uuid]) -> Vec<sea_orm::Value> {
    ids.iter().map(|id| (*id).into()).collect()
}

#[async_trait]
impl CRUDOperations for SensorOperations {
    type Resource = Sensor;

    async fn after_get_one(
        &self,
        db: &DatabaseConnection,
        entity: &mut Sensor,
    ) -> Result<(), ApiError> {
        let id = entity.id;

        let reading_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*) as count, MAX(time) as last_time FROM readings WHERE sensor_id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?;

        if let Some(row) = reading_row {
            entity.reading_count = Some(row.try_get("", "count").unwrap_or(0));
            entity.last_reading_at = row
                .try_get::<chrono::DateTime<chrono::FixedOffset>>("", "last_time")
                .ok()
                .map(|t| t.with_timezone(&Utc));
        }

        let cal_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT MAX(valid_from) as last_cal FROM sensor_calibrations WHERE sensor_id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?;

        entity.last_calibration_at = cal_row
            .and_then(|r| r.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "last_cal").ok())
            .map(|t| t.with_timezone(&Utc));

        // Also populate current_site fields for detail view
        let dep_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT sd.site_id, s.name as site_name
                  FROM sensor_deployments sd JOIN sites s ON s.id = sd.site_id
                  WHERE sd.sensor_id = $1 AND sd.deployed_until IS NULL
                  LIMIT 1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?;

        if let Some(row) = dep_row {
            entity.current_site_id = row.try_get("", "site_id").ok();
            entity.current_site_name = row.try_get("", "site_name").ok();
        }

        Ok(())
    }

    async fn after_get_all(
        &self,
        db: &DatabaseConnection,
        entities: &mut Vec<<Sensor as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        if entities.is_empty() {
            return Ok(());
        }

        let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let placeholders = build_in_clause(ids.len());
        let values = uuid_values(&ids);

        // Query 1: Active deployments + site names
        let dep_sql = format!(
            r"SELECT sd.sensor_id, sd.site_id, s.name as site_name
              FROM sensor_deployments sd JOIN sites s ON s.id = sd.site_id
              WHERE sd.sensor_id IN ({placeholders}) AND sd.deployed_until IS NULL"
        );
        let dep_rows = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres, &dep_sql, values.clone(),
            ))
            .await
            .map_err(ApiError::database)?;

        let mut site_by_sensor: HashMap<Uuid, (Uuid, String)> = HashMap::new();
        for row in &dep_rows {
            if let (Ok(sensor_id), Ok(site_id), Ok(site_name)) = (
                row.try_get::<Uuid>("", "sensor_id"),
                row.try_get::<Uuid>("", "site_id"),
                row.try_get::<String>("", "site_name"),
            ) {
                site_by_sensor.entry(sensor_id).or_insert((site_id, site_name));
            }
        }

        // Query 2: Latest reading per sensor
        let reading_sql = format!(
            r"SELECT DISTINCT ON (sensor_id) sensor_id, time, COALESCE(calibrated_value, raw_value) as value
              FROM readings
              WHERE sensor_id IN ({placeholders})
              ORDER BY sensor_id, time DESC"
        );
        let reading_rows = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres, &reading_sql, values.clone(),
            ))
            .await
            .map_err(ApiError::database)?;

        let mut reading_by_sensor: HashMap<Uuid, (chrono::DateTime<Utc>, f64)> = HashMap::new();
        for row in &reading_rows {
            if let (Ok(sensor_id), Ok(time), Ok(value)) = (
                row.try_get::<Uuid>("", "sensor_id"),
                row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "time"),
                row.try_get::<f64>("", "value"),
            ) {
                reading_by_sensor.insert(sensor_id, (time.with_timezone(&Utc), value));
            }
        }

        // Query 3: Latest calibration per sensor
        let cal_sql = format!(
            r"SELECT DISTINCT ON (sensor_id) sensor_id, valid_from
              FROM sensor_calibrations
              WHERE sensor_id IN ({placeholders})
              ORDER BY sensor_id, valid_from DESC"
        );
        let cal_rows = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres, &cal_sql, values,
            ))
            .await
            .map_err(ApiError::database)?;

        let mut cal_by_sensor: HashMap<Uuid, chrono::DateTime<Utc>> = HashMap::new();
        for row in &cal_rows {
            if let (Ok(sensor_id), Ok(valid_from)) = (
                row.try_get::<Uuid>("", "sensor_id"),
                row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "valid_from"),
            ) {
                cal_by_sensor.insert(sensor_id, valid_from.with_timezone(&Utc));
            }
        }

        // Populate entities
        for entity in entities.iter_mut() {
            if let Some((site_id, site_name)) = site_by_sensor.get(&entity.id) {
                entity.current_site_id = Some(*site_id);
                entity.current_site_name = Some(site_name.clone());
            }
            if let Some((time, value)) = reading_by_sensor.get(&entity.id) {
                entity.last_reading_at = Some(*time);
                entity.last_reading_value = Some(*value);
            }
            if let Some(cal_time) = cal_by_sensor.get(&entity.id) {
                entity.last_calibration_at = Some(*cal_time);
            }
        }

        Ok(())
    }
}

/// Resolved sensor context for readings.
#[derive(Debug, Clone)]
pub struct SensorContext {
    pub sensor_id: Uuid,
    pub calibration_id: Uuid,
    /// `None` when the sensor is not deployed to the target site at this time — the slot may be
    /// occupied by another sensor, or the sensor isn't adopted yet. Readings still carry
    /// `sensor_id`/`calibration_id`; only the deployment FK is absent.
    pub deployment_id: Option<Uuid>,
}

/// Extract Vaisala device metadata from stream metadata for storage on the sensor.
fn extract_source_metadata(stream_metadata: &serde_json::Value) -> Option<serde_json::Value> {
    let device = stream_metadata.get("device")?;
    let mut meta = serde_json::Map::new();

    if let Some(v) = device.get("logger_serial").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_device_serial".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = device.get("probe_serial").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_probe_serial".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = device.get("logger_device").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_device_model".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = device.get("device_class").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_device_class".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }

    if meta.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta))
    }
}

/// Find an existing sensor by its natural key `(serial_number, parameter_id)`.
/// Returns `None` when no serial is available (we never dedupe serial-less sensors).
async fn find_sensor_by_serial_param<C: ConnectionTrait>(
    db: &C,
    serial: Option<&str>,
    parameter_id: Uuid,
) -> AppResult<Option<sensors::Model>> {
    let Some(serial) = serial else {
        return Ok(None);
    };
    let existing = sensors::Entity::find()
        .filter(
            Condition::all()
                .add(sensors::Column::SerialNumber.eq(serial))
                .add(sensors::Column::ParameterId.eq(parameter_id)),
        )
        .one(db)
        .await?;
    Ok(existing)
}

/// Link a data stream to a sensor (`data_streams.sensor_id`) as the pairing hint.
async fn link_stream_to_sensor<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    sensor_id: Uuid,
) -> AppResult<()> {
    if stream.sensor_id == Some(sensor_id) {
        return Ok(());
    }
    let mut stream_active: data_streams::ActiveModel = stream.clone().into();
    stream_active.sensor_id = Set(Some(sensor_id));
    stream_active.updated_at = Set(Utc::now().into());
    stream_active.update(db).await?;
    Ok(())
}

/// Insert a new sensor for `(serial, parameter)`, or return the existing one if a sensor with that
/// natural key already exists. Race-safe: the `ON CONFLICT … DO NOTHING` targets the partial unique
/// index `idx_sensors_serial_parameter (serial_number, parameter_id) WHERE serial_number IS NOT NULL`,
/// so concurrent pairings of the same device converge on one row WITHOUT raising a unique violation.
/// That matters because some callers run inside a transaction (sync plan/discovery apply): a raised
/// violation there would poison the whole transaction, not just this insert. A serial-less sensor has
/// no dedupe key (the predicate excludes it), so it always inserts and the new id comes back via
/// `RETURNING`. The conflict branch re-selects the winner.
async fn insert_or_get_sensor<C: ConnectionTrait>(
    db: &C,
    serial: Option<&str>,
    parameter_id: Uuid,
    name: &str,
    metadata: Option<serde_json::Value>,
) -> AppResult<Uuid> {
    let serial_val: sea_orm::Value = match serial {
        Some(s) => s.to_string().into(),
        None => sea_orm::Value::String(None),
    };
    let metadata_val: sea_orm::Value = match &metadata {
        Some(v) => serde_json::to_string(v)
            .unwrap_or_else(|_| "null".to_string())
            .into(),
        None => sea_orm::Value::String(None),
    };

    let inserted = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r#"INSERT INTO sensors
                   (id, serial_number, name, parameter_id, is_active, is_lab_instrument, metadata, created_at)
               VALUES (gen_random_uuid(), $1, $2, $3, true, false, $4::jsonb, now())
               ON CONFLICT (serial_number, parameter_id) WHERE serial_number IS NOT NULL
               DO NOTHING
               RETURNING id"#,
            [serial_val, name.into(), parameter_id.into(), metadata_val],
        ))
        .await?;

    if let Some(row) = inserted {
        let id: Uuid = row.try_get("", "id")?;
        return Ok(id);
    }

    // Conflict on (serial, parameter): a sensor already exists (possibly a concurrent winner) — reuse it.
    let existing = find_sensor_by_serial_param(db, serial, parameter_id)
        .await?
        .ok_or_else(|| {
            AppError::Internal(
                "sensor upsert conflicted but the existing (serial, parameter) row was not found"
                    .to_string(),
            )
        })?;
    Ok(existing.id)
}

/// Create or reuse a sensor for a data stream being paired.
///
/// If the stream already has a `sensor_id`, reuses that sensor (ensures deployment exists for the target site).
/// Otherwise creates a new sensor with identity calibration and deployment.
/// Updates `data_streams.sensor_id` to link the stream to the sensor.
pub async fn create_sensor_for_stream<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    parameter_id: Uuid,
    site_id: Uuid,
) -> AppResult<SensorContext> {
    let ctx = import_sensor_for_stream(db, stream, parameter_id).await?;
    // Ensure active deployment exists for this sensor+site (None if the slot is occupied).
    let deployment_id = find_or_create_deployment(db, ctx.sensor_id, site_id).await?;
    Ok(SensorContext {
        deployment_id,
        ..ctx
    })
}

/// Import-only: create or reuse a sensor for a stream and resolve its latest calibration, WITHOUT
/// deploying it to a site. The "imported, not adopted" state — readings get `sensor_id`/
/// `calibration_id` (so calibration math applies) but no `deployment_id`/`site_id` until an explicit
/// adopt. Idempotent: reuses the stream's linked sensor, else the existing `(serial, parameter)`
/// sensor, else inserts one (race-safe via `insert_or_get_sensor`). Updates `data_streams.sensor_id`.
pub async fn import_sensor_for_stream<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    parameter_id: Uuid,
) -> AppResult<SensorContext> {
    let serial = extract_vaisala_device_serial(&stream.metadata);

    let (sensor_id, calibration_id) = if let Some(existing_sensor_id) = stream.sensor_id {
        let cal_id = get_latest_calibration(db, existing_sensor_id).await?;
        (existing_sensor_id, cal_id)
    } else {
        let sensor_name = stream
            .source_name
            .clone()
            .unwrap_or_else(|| format!("Stream {}", stream.source_key));
        let metadata = extract_source_metadata(&stream.metadata);
        let sensor_id =
            insert_or_get_sensor(db, serial.as_deref(), parameter_id, &sensor_name, metadata)
                .await?;
        let cal_id = get_latest_calibration(db, sensor_id).await?;
        link_stream_to_sensor(db, stream, sensor_id).await?;
        (sensor_id, cal_id)
    };

    Ok(SensorContext {
        sensor_id,
        calibration_id,
        deployment_id: None,
    })
}

/// Find the latest calibration for a sensor, or create an identity calibration if none exists.
async fn get_latest_calibration<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
) -> AppResult<Uuid> {
    let cal = sensor_calibrations::Entity::find()
        .filter(sensor_calibrations::Column::SensorId.eq(sensor_id))
        .order_by_desc(sensor_calibrations::Column::ValidFrom)
        .one(db)
        .await?;

    if let Some(cal) = cal {
        Ok(cal.id)
    } else {
        // Create identity calibration
        let cal = sensor_calibrations::ActiveModel {
            id: Set(Uuid::new_v4()),
            sensor_id: Set(sensor_id),
            slope: Set(1.0),
            intercept: Set(0.0),
            valid_from: Set(Utc::now()),
            performed_by: Set(Some("system".to_string())),
            notes: Set(Some("Identity calibration (auto-created)".to_string())),
            valid_until: Set(None),
            created_at: Set(Some(Utc::now())),
        };
        let cal = cal.insert(db).await?;
        Ok(cal.id)
    }
}

/// Find this sensor's open deployment at the site, or auto-create one — but only if the
/// `(site, parameter)` slot is free. Returns `None` when the slot is already occupied by another
/// sensor (the swap case), leaving the deployment to an explicit adopt.
///
/// One sensor per `(site, parameter)` is hard-enforced by the `excl_deployment_site_param_slot`
/// exclusion constraint. A blind insert onto an occupied slot would raise an exclusion violation —
/// which, in the sync apply path (this runs inside `create_sensor_for_stream` within a transaction),
/// would poison the whole pairing transaction. The conditional insert below skips cleanly when the
/// slot is occupied (the common swap case) instead of raising; the constraint remains the atomic
/// backstop for the rare concurrent-double-deploy race.
async fn find_or_create_deployment<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    site_id: Uuid,
) -> AppResult<Option<Uuid>> {
    let existing = sensor_deployments::Entity::find()
        .filter(
            Condition::all()
                .add(sensor_deployments::Column::SensorId.eq(sensor_id))
                .add(sensor_deployments::Column::SiteId.eq(site_id))
                .add(sensor_deployments::Column::DeployedUntil.is_null()),
        )
        .one(db)
        .await?;

    if let Some(dep) = existing {
        return Ok(Some(dep.id));
    }

    // Insert an open deployment only when no other deployment occupies the (site, parameter) slot at
    // now(). `parameter_id` is filled by the BEFORE INSERT trigger from the sensor.
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO sensor_deployments
                  (id, sensor_id, site_id, deployed_from, deployment_type, notes)
              SELECT gen_random_uuid(), $1, $2, NOW(), 'permanent', 'Auto-created during stream pairing'
              WHERE NOT EXISTS (
                  SELECT 1 FROM sensor_deployments d
                  WHERE d.site_id = $2
                    AND d.parameter_id = (SELECT parameter_id FROM sensors WHERE id = $1)
                    AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > NOW()
              )
              RETURNING id",
            [sensor_id.into(), site_id.into()],
        ))
        .await?;

    match row {
        Some(r) => Ok(Some(r.try_get("", "id")?)),
        None => {
            tracing::info!(
                %sensor_id, %site_id,
                "Deployment slot already occupied by another sensor; skipping auto-deploy (explicit adopt required)"
            );
            Ok(None)
        }
    }
}

/// Close the active deployment for a sensor at a site.
pub async fn close_sensor_deployment(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    site_id: Uuid,
) -> AppResult<()> {
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE sensor_deployments
          SET deployed_until = $1
          WHERE sensor_id = $2 AND site_id = $3 AND deployed_until IS NULL",
        [Utc::now().into(), sensor_id.into(), site_id.into()],
    ))
    .await?;
    Ok(())
}

/// One resolved attribution slot for a reading time.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSlot {
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub site_id: Option<Uuid>,
}

/// Resolve attribution for a batch of reading times for one sensor, by window — the same half-open
/// `[from, COALESCE(until,'infinity'))` semantics `reprocess_sensor_readings` uses, so every write
/// path agrees with reprocess. Two indexed range scans regardless of batch size.
///
/// `expected_site`: when `Some`, only a deployment at that site can attribute a time (used by grabs,
/// which are site-fixed by the request); when `None`, whichever deployment covers the time wins
/// (matches reprocess, used by continuous ingest).
pub async fn resolve_windows_for_times<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    expected_site: Option<Uuid>,
    times: &[chrono::DateTime<Utc>],
) -> AppResult<std::collections::HashMap<chrono::DateTime<Utc>, ResolvedSlot>> {
    use std::collections::HashMap;
    let mut out: HashMap<chrono::DateTime<Utc>, ResolvedSlot> = HashMap::new();
    if times.is_empty() {
        return Ok(out);
    }

    let cal_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, valid_from, COALESCE(valid_until, 'infinity'::timestamptz) AS valid_until
              FROM sensor_calibrations WHERE sensor_id = $1 ORDER BY valid_from",
            [sensor_id.into()],
        ))
        .await?;
    let cals: Vec<(Uuid, chrono::DateTime<Utc>, chrono::DateTime<Utc>)> = cal_rows
        .iter()
        .map(|r| -> AppResult<_> {
            let id: Uuid = r.try_get("", "id")?;
            let from: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "valid_from")?;
            let until: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "valid_until")?;
            Ok((id, from.with_timezone(&Utc), until.with_timezone(&Utc)))
        })
        .collect::<AppResult<_>>()?;

    let dep_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, site_id, deployed_from, COALESCE(deployed_until, 'infinity'::timestamptz) AS deployed_until
              FROM sensor_deployments WHERE sensor_id = $1 ORDER BY deployed_from",
            [sensor_id.into()],
        ))
        .await?;
    let deps: Vec<(Uuid, Uuid, chrono::DateTime<Utc>, chrono::DateTime<Utc>)> = dep_rows
        .iter()
        .map(|r| -> AppResult<_> {
            let id: Uuid = r.try_get("", "id")?;
            let site_id: Uuid = r.try_get("", "site_id")?;
            let from: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "deployed_from")?;
            let until: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "deployed_until")?;
            Ok((id, site_id, from.with_timezone(&Utc), until.with_timezone(&Utc)))
        })
        .collect::<AppResult<_>>()?;

    for &t in times {
        let calibration_id = cals
            .iter()
            .find(|(_, from, until)| t >= *from && t < *until)
            .map(|(id, _, _)| *id);
        let dep = deps.iter().find(|(_, site_id, from, until)| {
            t >= *from && t < *until && expected_site.is_none_or(|s| *site_id == s)
        });
        out.insert(
            t,
            ResolvedSlot {
                calibration_id,
                deployment_id: dep.map(|(id, _, _, _)| *id),
                site_id: dep.map(|(_, site_id, _, _)| *site_id),
            },
        );
    }
    Ok(out)
}

/// Extract the Vaisala device serial from stream metadata (for discovery response).
pub fn extract_vaisala_device_serial(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("device")
        .and_then(|d| {
            d.get("logger_serial")
                .or_else(|| d.get("probe_serial"))
        })
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}
