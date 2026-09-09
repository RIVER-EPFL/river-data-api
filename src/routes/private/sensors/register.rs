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

/// One instrument from a source's own register. The instrument's own fields are
/// `river_data_core::models::SensorUpsert`, which the sync services build from; the API adds the
/// source the caller is speaking for, and supplies the `is_lab_instrument` default this route has
/// always accepted an omitted flag under.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterSensorRequest {
    /// The sync source the instrument comes from, e.g. "metalp".
    pub source_system: String,
    #[serde(flatten)]
    pub instrument: river_data_core::models::SensorUpsert,
}

impl<'de> Deserialize<'de> for RegisterSensorRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (source_system, instrument) = crate::routes::private::wire::with_source_system(
            deserializer,
            &[("is_lab_instrument", serde_json::json!(false))],
        )?;
        Ok(Self {
            source_system,
            instrument,
        })
    }
}

/// The cadence a registration declares, or the default this route has always applied.
fn declared_frequency(instrument: &river_data_core::models::SensorUpsert) -> &str {
    instrument.data_frequency.as_deref().unwrap_or("high")
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
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(payload): Json<RegisterSensorRequest>,
) -> AppResult<Json<RegisterSensorResponse>> {
    let source_system = crate::common::provenance::source_system(&auth, &payload.source_system)?;
    let instrument = &payload.instrument;
    if instrument.source_key.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_key identifies the instrument and cannot be empty".to_string(),
        ));
    }
    let data_frequency = declared_frequency(instrument);
    if !matches!(data_frequency, "high" | "low") {
        return Err(AppError::BadRequest(format!(
            "data_frequency must be 'high' or 'low', got '{data_frequency}'"
        )));
    }

    let existing = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(source_system.clone()))
        .filter(sensors::Column::SourceKey.eq(instrument.source_key.clone()))
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
        &source_system,
        &instrument.source_key,
        &instrument.name,
        if instrument.is_lab_instrument {
            InstrumentKind::Lab
        } else {
            InstrumentKind::Device
        },
        data_frequency,
        instrument.metadata.clone(),
    )
    .await?;

    let held_by = match instrument.serial_number.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => serial_holder(&state.db, s).await?,
        _ => None,
    };
    let serial = serial_to_claim(instrument.serial_number.as_deref(), held_by);

    let mut active = sensors::ActiveModel {
        id: Set(id),
        ..Default::default()
    };
    active.serial_number = Set(serial);
    active.manufacturer = Set(instrument.manufacturer.clone());
    active.model = Set(instrument.model.clone());
    active.notes = Set(instrument.notes.clone());
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

/// A source's whole instrument register, offered for a plan to admit.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeInstrumentsRequest {
    /// The sync source the register belongs to, e.g. "metalp".
    pub source_system: String,
    pub instruments: Vec<river_data_core::models::SensorUpsert>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProposeInstrumentsResponse {
    /// Rows now held as proposals, whether this call created or refreshed them.
    pub stored: usize,
    /// Proposals a plan has already admitted, which are instruments now and are left alone.
    pub already_admitted: usize,
}

/// Store a source's instrument register as proposals. Requires `write_metadata`.
///
/// Nothing is created: an instrument exists once a pairing plan an operator validated creates it
/// (Q134). A row already admitted as an instrument under the same provenance is skipped rather than
/// re-proposed, so a sync does not offer back what the operator already took.
#[utoipa::path(
    post,
    path = "/api/sensors/proposals",
    request_body = ProposeInstrumentsRequest,
    responses((status = 200, description = "Register stored", body = ProposeInstrumentsResponse)),
    tag = "sensors"
)]
pub async fn propose_instruments(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(payload): Json<ProposeInstrumentsRequest>,
) -> AppResult<Json<ProposeInstrumentsResponse>> {
    let source_system = crate::common::provenance::source_system(&auth, &payload.source_system)?;
    let mut stored = 0usize;
    let mut already_admitted = 0usize;
    for instrument in &payload.instruments {
        let key = instrument.source_key.trim();
        if key.is_empty() {
            return Err(AppError::BadRequest(
                "source_key identifies the instrument and cannot be empty".to_string(),
            ));
        }
        let admitted = sensors::Entity::find()
            .filter(sensors::Column::SourceSystem.eq(source_system.clone()))
            .filter(sensors::Column::SourceKey.eq(key.to_string()))
            .one(&state.db)
            .await?;
        if admitted.is_some() {
            already_admitted += 1;
            continue;
        }
        state
            .db
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO instrument_proposals \
                     (source_system, source_key, name, serial_number, manufacturer, model, notes, \
                      is_lab_instrument, data_frequency, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (source_system, source_key) DO UPDATE SET \
                     name = EXCLUDED.name, serial_number = EXCLUDED.serial_number, \
                     manufacturer = EXCLUDED.manufacturer, model = EXCLUDED.model, \
                     notes = EXCLUDED.notes, is_lab_instrument = EXCLUDED.is_lab_instrument, \
                     data_frequency = EXCLUDED.data_frequency, metadata = EXCLUDED.metadata, \
                     last_seen_at = now()",
                [
                    source_system.clone().into(),
                    key.to_string().into(),
                    instrument.name.clone().into(),
                    instrument.serial_number.clone().into(),
                    instrument.manufacturer.clone().into(),
                    instrument.model.clone().into(),
                    instrument.notes.clone().into(),
                    instrument.is_lab_instrument.into(),
                    instrument.data_frequency.clone().into(),
                    instrument.metadata.clone().into(),
                ],
            ))
            .await?;
        stored += 1;
    }
    Ok(Json(ProposeInstrumentsResponse {
        stored,
        already_admitted,
    }))
}
