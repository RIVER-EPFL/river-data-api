use std::collections::HashMap;

use sea_orm::{ConnectionTrait, Statement};
use uuid::Uuid;

use crate::error::AppError;

/// Why this classification is not admissible, or `None` when it is. Callers that refuse the whole
/// request raise it as a 400; callers that skip the offending reading need the reason as a value.
pub fn measurement_type_rejection(value: Option<&str>) -> Option<String> {
    match value {
        None | Some("continuous" | "spot" | "derived") => None,
        Some(other) => Some(format!(
            "invalid measurement_type '{other}' (expected continuous, spot, or derived)"
        )),
    }
}

/// Reject anything outside the readings.measurement_type vocabulary with a clean 400 (the DB has
/// no CHECK on readings.measurement_type, so bad values would otherwise persist silently).
pub fn validate_measurement_type(value: Option<&str>) -> Result<(), AppError> {
    measurement_type_rejection(value).map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
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
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, data_frequency FROM sensors WHERE id = ANY($1)",
            [sensor_ids.to_vec().into()],
        ))
        .await?;
    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        let id: Uuid = row.try_get("", "id")?;
        let freq: String = row.try_get("", "data_frequency")?;
        map.insert(id, if freq == "low" { "spot" } else { "continuous" });
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
        .unwrap_or_else(|| "continuous".to_string())
}
