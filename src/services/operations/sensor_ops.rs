use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    QueryFilter, QueryOrder, Set, Statement,
};
use uuid::Uuid;

use crate::entity::{data_streams, sensor_calibrations, sensor_deployments, sensors};
use crate::error::AppResult;

/// Resolved sensor context for readings.
#[derive(Debug, Clone)]
pub struct SensorContext {
    pub sensor_id: Uuid,
    pub calibration_id: Uuid,
    pub deployment_id: Uuid,
}

/// Extract Vaisala device metadata from stream metadata for storage on the sensor.
fn extract_source_metadata(stream_metadata: &serde_json::Value) -> Option<serde_json::Value> {
    let device = stream_metadata.get("device")?;
    let mut meta = serde_json::Map::new();

    if let Some(v) = device.get("logger_serial").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            meta.insert(
                "source_device_serial".to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
    }
    if let Some(v) = device.get("probe_serial").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            meta.insert(
                "source_probe_serial".to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
    }
    if let Some(v) = device.get("logger_device").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            meta.insert(
                "source_device_model".to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
    }
    if let Some(v) = device.get("device_class").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            meta.insert(
                "source_device_class".to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
    }

    if meta.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta))
    }
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
    let (sensor_id, calibration_id) = if let Some(existing_sensor_id) = stream.sensor_id {
        // Reuse existing sensor — find its latest calibration
        let cal_id = get_latest_calibration(db, existing_sensor_id).await?;
        (existing_sensor_id, cal_id)
    } else {
        // Create new sensor
        let sensor_name = stream
            .source_name
            .clone()
            .unwrap_or_else(|| format!("Stream {}", stream.source_key));

        let metadata = extract_source_metadata(&stream.metadata);

        let sensor = sensors::ActiveModel {
            id: Set(Uuid::new_v4()),
            serial_number: Set(None),
            name: Set(Some(sensor_name)),
            parameter_id: Set(parameter_id),
            manufacturer: Set(None),
            model: Set(None),
            is_active: Set(Some(true)),
            is_lab_instrument: Set(Some(false)),
            notes: Set(None),
            metadata: Set(metadata),
            created_at: Set(Some(Utc::now())),
        };
        let sensor = sensor.insert(db).await?;
        let sensor_id = sensor.id;

        // Create identity calibration
        let cal = sensor_calibrations::ActiveModel {
            id: Set(Uuid::new_v4()),
            sensor_id: Set(sensor_id),
            slope: Set(1.0),
            intercept: Set(0.0),
            valid_from: Set(Utc::now()),
            performed_by: Set(Some("system".to_string())),
            notes: Set(Some("Identity calibration (auto-created)".to_string())),
            created_at: Set(Some(Utc::now())),
        };
        let cal = cal.insert(db).await?;

        // Link stream to sensor
        let mut stream_active: data_streams::ActiveModel = stream.clone().into();
        stream_active.sensor_id = Set(Some(sensor_id));
        stream_active.updated_at = Set(Utc::now().into());
        stream_active.update(db).await?;

        (sensor_id, cal.id)
    };

    // Ensure active deployment exists for this sensor+site
    let deployment_id = find_or_create_deployment(db, sensor_id, site_id).await?;

    Ok(SensorContext {
        sensor_id,
        calibration_id,
        deployment_id,
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
            created_at: Set(Some(Utc::now())),
        };
        let cal = cal.insert(db).await?;
        Ok(cal.id)
    }
}

/// Find an active deployment for sensor+site, or create one.
async fn find_or_create_deployment<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    site_id: Uuid,
) -> AppResult<Uuid> {
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
        return Ok(dep.id);
    }

    let dep = sensor_deployments::ActiveModel {
        id: Set(Uuid::new_v4()),
        sensor_id: Set(sensor_id),
        site_id: Set(site_id),
        deployed_from: Set(Utc::now()),
        deployed_until: Set(None),
        deployment_type: Set("permanent".to_string()),
        notes: Set(Some("Auto-created during stream pairing".to_string())),
        created_at: Set(Some(Utc::now())),
    };
    let dep = dep.insert(db).await?;
    Ok(dep.id)
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

/// Resolve sensor context for a stream (for ingestion).
/// Returns None if the stream has no sensor_id or if context can't be resolved.
pub async fn resolve_sensor_context(
    db: &DatabaseConnection,
    stream: &data_streams::Model,
    site_id: Uuid,
) -> Option<SensorContext> {
    let sensor_id = stream.sensor_id?;

    let cal_id = get_latest_calibration(db, sensor_id).await.ok()?;

    // Find active deployment for this sensor+site
    let dep = sensor_deployments::Entity::find()
        .filter(
            Condition::all()
                .add(sensor_deployments::Column::SensorId.eq(sensor_id))
                .add(sensor_deployments::Column::SiteId.eq(site_id))
                .add(sensor_deployments::Column::DeployedUntil.is_null()),
        )
        .one(db)
        .await
        .ok()??;

    Some(SensorContext {
        sensor_id,
        calibration_id: cal_id,
        deployment_id: dep.id,
    })
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
