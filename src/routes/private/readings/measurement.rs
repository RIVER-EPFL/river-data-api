use std::collections::HashMap;

use river_data_core::models::MeasurementType;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use uuid::Uuid;

use crate::error::AppError;
use crate::routes::private::sensors;

/// `POST /streams/retag` and the `measurement_retag` job take this alongside the vocabulary: it
/// writes nothing to `data_streams` and aligns each reading with its own stream's declaration.
pub const RETAG_DECLARED: &str = "declared";

fn expected(extra: &[&str]) -> String {
    let mut names: Vec<&str> = MeasurementType::ALL
        .iter()
        .map(MeasurementType::as_str)
        .collect();
    names.extend_from_slice(extra);
    let last = names.pop().expect("the vocabulary is never empty");
    format!("{}, or {last}", names.join(", "))
}

/// Why this classification is not admissible, or `None` when it is. Callers that refuse the whole
/// request raise it as a 400; callers that skip the offending reading need the reason as a value.
pub fn measurement_type_rejection(value: Option<&str>) -> Option<String> {
    match value {
        None => None,
        Some(other) => MeasurementType::from_str(other).is_none().then(|| {
            format!(
                "invalid measurement_type '{other}' (expected {})",
                expected(&[])
            )
        }),
    }
}

/// Reject anything outside the readings.measurement_type vocabulary with a clean 400 (the DB has
/// no CHECK on readings.measurement_type, so bad values would otherwise persist silently).
pub fn validate_measurement_type(value: Option<&str>) -> Result<(), AppError> {
    measurement_type_rejection(value).map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
}

/// Why this retag target is not admissible, or `None` when it is. The route and the job body both
/// read it, so a stored job row replayed by rerun is held to the same vocabulary as the request
/// that made it.
pub fn retag_target_rejection(value: &str) -> Option<String> {
    (MeasurementType::from_str(value).is_none() && value != RETAG_DECLARED).then(|| {
        format!(
            "invalid measurement_type '{value}' (expected {})",
            expected(&[RETAG_DECLARED])
        )
    })
}

/// Map each sensor to the measurement_type its `data_frequency` implies: 'low' → 'spot'
/// (lab/campaign cadence), 'high' → 'continuous'. One query for the whole batch.
pub async fn measurement_types_for_sensors<C: ConnectionTrait>(
    db: &C,
    sensor_ids: &[Uuid],
) -> Result<HashMap<Uuid, &'static str>, sea_orm::DbErr> {
    if sensor_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(sensor_ids.to_vec()))
        .all(db)
        .await?;
    let mut map = HashMap::with_capacity(rows.len());
    for sensor in rows {
        map.insert(
            sensor.id,
            if sensor.data_frequency == "low" {
                MeasurementType::Spot.as_str()
            } else {
                MeasurementType::Continuous.as_str()
            },
        );
    }
    Ok(map)
}

/// Resolve one reading's measurement_type. Most specific wins:
/// explicit per-reading override → stream-level default → owning sensor's data_frequency →
/// 'continuous'.
pub fn resolve_measurement_type(
    override_value: Option<&str>,
    stream_default: Option<&str>,
    sensor_id: Option<Uuid>,
    sensor_types: &HashMap<Uuid, &'static str>,
) -> String {
    override_value
        .or(stream_default)
        .map(str::to_string)
        .or_else(|| {
            sensor_id
                .and_then(|id| sensor_types.get(&id))
                .map(|t| (*t).to_string())
        })
        .unwrap_or_else(|| MeasurementType::Continuous.as_str().to_string())
}

#[cfg(test)]
#[path = "tests/measurement.rs"]
mod tests;
