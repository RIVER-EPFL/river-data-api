//! Provenance-keyed upsert of instruments for sync services replicating a portal's own instrument
//! register. Idempotent per `(source_system, source_key)`, and bound by the same rule as every
//! other source-registered instrument: a claimed row is never rewritten by a later cycle, because
//! what an operator recorded on it outranks what the source repeats.

use axum::{Json, extract::State};
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors;
use crate::routes::private::sensors::identity::{InstrumentKind, upsert_source_instrument};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterSensorRequest {
    /// The sync source the instrument comes from, e.g. "metalp".
    pub source_system: String,
    /// The instrument's identity within that source, e.g. "sensor_inventory:62". Stable across
    /// re-registration; the upsert key is (source_system, source_key).
    pub source_key: String,
    pub name: String,
    /// The lab's own serial for the instrument. Claimed only when no other instrument holds it;
    /// see [`serial_to_claim`].
    #[serde(default)]
    pub serial_number: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// False for a field instrument, true for one that corrects a grab in the lab.
    #[serde(default)]
    pub is_lab_instrument: bool,
    /// 'high' or 'low'. Read as a cadence declaration when a stream classifies its readings, so
    /// leave it 'high' unless the source knows the instrument logs at grab cadence.
    #[serde(default = "default_data_frequency")]
    pub data_frequency: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

fn default_data_frequency() -> String {
    "high".to_string()
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterSensorResponse {
    pub id: Uuid,
    /// False when the provenance key already named an instrument, in which case nothing on it was
    /// changed.
    pub created: bool,
    /// The instrument already holding the serial this registration offered, when that is why the
    /// serial was not claimed. The source's register is wrong or the two rows are one instrument;
    /// either way it is a person's call, so the registration succeeds and says so.
    #[schema(required)]
    pub serial_claimed_by: Option<Uuid>,
}

/// Which serial a newly minted instrument may claim.
///
/// `idx_sensors_serial_unique` is partial and unique, so a serial already on another row cannot be
/// written to this one. The registration is not refused over it: the source's register is what it
/// is (METALP's has `919402` on two stations' turbidity probes), and losing the whole instrument
/// over a duplicated serial would be worse than storing it without one.
#[must_use]
pub fn serial_to_claim(offered: Option<&str>, held_by: Option<Uuid>) -> Option<String> {
    let offered = offered.map(str::trim).filter(|s| !s.is_empty())?;
    match held_by {
        Some(_) => None,
        None => Some(offered.to_string()),
    }
}

async fn serial_holder<C: ConnectionTrait>(db: &C, serial: &str) -> AppResult<Option<Uuid>> {
    Ok(sensors::Entity::find()
        .filter(sensors::Column::SerialNumber.eq(serial))
        .one(db)
        .await?
        .map(|s| s.id))
}

/// Upsert an instrument by provenance. Requires `write_metadata` (sync session tokens carry it).
///
/// A source that has no stream for an instrument has no other way to introduce it: every other
/// instrument in the system is minted as a side effect of registering the stream that names it.
/// A portal's instrument register is exactly that case, so this is its wire.
#[utoipa::path(
    post,
    path = "/api/sensors/register",
    request_body = RegisterSensorRequest,
    responses(
        (status = 200, description = "Instrument registered (created or already present)", body = RegisterSensorResponse),
    ),
    tag = "sensors"
)]
pub async fn register_sensor(
    State(state): State<AppState>,
    Json(payload): Json<RegisterSensorRequest>,
) -> AppResult<Json<RegisterSensorResponse>> {
    if payload.source_system.trim().is_empty() || payload.source_key.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_system and source_key identify the instrument and cannot be empty".to_string(),
        ));
    }
    if !matches!(payload.data_frequency.as_str(), "high" | "low") {
        return Err(AppError::BadRequest(format!(
            "data_frequency must be 'high' or 'low', got '{}'",
            payload.data_frequency
        )));
    }

    let existing = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(payload.source_system.clone()))
        .filter(sensors::Column::SourceKey.eq(payload.source_key.clone()))
        .one(&state.db)
        .await?;
    if let Some(current) = existing {
        return Ok(Json(RegisterSensorResponse {
            id: current.id,
            created: false,
            serial_claimed_by: None,
        }));
    }

    let id = upsert_source_instrument(
        &state.db,
        &payload.source_system,
        &payload.source_key,
        &payload.name,
        if payload.is_lab_instrument {
            InstrumentKind::Lab
        } else {
            InstrumentKind::Device
        },
        &payload.data_frequency,
        payload.metadata.clone(),
    )
    .await?;

    let held_by = match payload.serial_number.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => serial_holder(&state.db, s).await?,
        _ => None,
    };
    let serial = serial_to_claim(payload.serial_number.as_deref(), held_by);

    let mut active = sensors::ActiveModel {
        id: Set(id),
        ..Default::default()
    };
    active.serial_number = Set(serial);
    active.manufacturer = Set(payload.manufacturer.clone());
    active.model = Set(payload.model.clone());
    active.notes = Set(payload.notes.clone());
    active.update(&state.db).await?;

    Ok(Json(RegisterSensorResponse {
        id,
        created: true,
        serial_claimed_by: held_by,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_a_free_serial() {
        assert_eq!(
            serial_to_claim(Some("919402"), None),
            Some("919402".to_string())
        );
    }

    #[test]
    fn leaves_a_held_serial_alone() {
        assert_eq!(serial_to_claim(Some("919402"), Some(Uuid::nil())), None);
    }

    #[test]
    fn treats_an_absent_or_blank_serial_as_none() {
        assert_eq!(serial_to_claim(None, None), None);
        assert_eq!(serial_to_claim(Some("   "), None), None);
    }

    #[test]
    fn trims_the_claimed_serial() {
        assert_eq!(
            serial_to_claim(Some(" 4000138 "), None),
            Some("4000138".to_string())
        );
    }
}
