//! Provenance-keyed upsert of standard curves for sync services replicating a portal's own
//! curve table. Idempotent per `(source_system, source_key)`, and bound by the same freeze rule
//! as the CRUD surface: a curve any reading references never changes in place. A portal that
//! edits a used curve's coefficients gets a NEW row minted under the same provenance, so the
//! mapping follows the portal forward while history keeps the curve that produced it.

use axum::{Json, extract::State};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Column, Entity, Model};
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors;

#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterStandardCurveRequest {
    /// The sync source the curve comes from, e.g. "cnet".
    pub source_system: String,
    /// The curve's identity within that source, e.g. "standard_curves:17". Stable across
    /// re-registration; the upsert key is (source_system, source_key).
    pub source_key: String,
    /// The lab instrument family the curve was fitted for, e.g. "DOC corr". A lab-instrument
    /// sensor is found or created per (source_system, instrument_label) and the curve attaches to
    /// it; readings claiming the curve must resolve to the same instrument.
    pub instrument_label: String,
    pub slope: f64,
    pub intercept: f64,
    #[serde(default)]
    pub r_squared: Option<f64>,
    /// Human label for the curve, e.g. the portal's date + parameter. Falls back to source_key.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterStandardCurveResponse {
    pub id: Uuid,
    pub sensor_id: Uuid,
    /// True when the stored coefficients differed and the curve was already applied to readings,
    /// so a new row was minted under this provenance. History keeps the old row.
    pub superseded: bool,
}

/// Whether any reading was corrected with this curve; a used curve's coefficients are frozen.
async fn curve_is_used<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<bool> {
    Ok(conn
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 AS one FROM readings WHERE standard_curve_id = $1 LIMIT 1",
            [id.into()],
        ))
        .await?
        .is_some())
}

/// Find or create the lab instrument a source's curves attach to, one per
/// (source_system, instrument_label). Serial is the deterministic identity, so re-registration
/// resolves the same instrument instead of minting one per cycle.
async fn resolve_lab_instrument(
    state: &AppState,
    source_system: &str,
    instrument_label: &str,
) -> AppResult<Uuid> {
    let serial = format!("{source_system}:{instrument_label}");
    if let Some(existing) = sensors::Entity::find()
        .filter(sensors::Column::SerialNumber.eq(serial.clone()))
        .one(&state.db)
        .await?
    {
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    sensors::ActiveModel {
        id: Set(id),
        serial_number: Set(Some(serial)),
        name: Set(Some(format!("{instrument_label} ({source_system})"))),
        is_active: Set(Some(true)),
        is_lab_instrument: Set(Some(true)),
        data_frequency: Set("low".to_string()),
        created_at: Set(Some(Utc::now())),
        ..Default::default()
    }
    .insert(&state.db)
    .await?;
    Ok(id)
}

/// Upsert a standard curve by provenance. Requires `write_metadata` (sync session tokens carry
/// it). Identical coefficients return the existing row; changed coefficients update in place only
/// while the curve is unused, and mint a successor row once any reading references it.
#[utoipa::path(
    post,
    path = "/standard_curves/register",
    request_body = RegisterStandardCurveRequest,
    responses(
        (status = 200, description = "Curve registered (created, unchanged, updated, or superseded)", body = RegisterStandardCurveResponse),
    ),
    tag = "sensors"
)]
pub async fn register_standard_curve(
    State(state): State<AppState>,
    Json(payload): Json<RegisterStandardCurveRequest>,
) -> AppResult<Json<RegisterStandardCurveResponse>> {
    if payload.slope == 0.0 {
        return Err(AppError::BadRequest(
            "Slope cannot be zero: all readings would produce a constant value".to_string(),
        ));
    }
    if payload.source_system.trim().is_empty() || payload.source_key.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_system and source_key identify the curve and cannot be empty".to_string(),
        ));
    }

    let sensor_id =
        resolve_lab_instrument(&state, &payload.source_system, &payload.instrument_label).await?;

    let existing = Entity::find()
        .filter(Column::SourceSystem.eq(payload.source_system.clone()))
        .filter(Column::SourceKey.eq(payload.source_key.clone()))
        .one(&state.db)
        .await?;

    let coefficients_match = |c: &Model| {
        c.slope == payload.slope && c.intercept == payload.intercept && c.sensor_id == sensor_id
    };

    if let Some(current) = existing {
        if coefficients_match(&current) {
            return Ok(Json(RegisterStandardCurveResponse {
                id: current.id,
                sensor_id,
                superseded: false,
            }));
        }
        if !curve_is_used(&state.db, current.id).await? {
            let mut active: super::ActiveModel = current.into();
            active.sensor_id = Set(sensor_id);
            active.slope = Set(payload.slope);
            active.intercept = Set(payload.intercept);
            active.r_squared = Set(payload.r_squared);
            if let Some(name) = payload.name.clone() {
                active.name = Set(Some(name));
            }
            let updated = active.update(&state.db).await?;
            return Ok(Json(RegisterStandardCurveResponse {
                id: updated.id,
                sensor_id,
                superseded: false,
            }));
        }
        // Used curve edited upstream: mint a successor and move the provenance to it. The old row
        // keeps the readings it produced; only its provenance columns are cleared so the partial
        // unique index admits the successor. The clearing must come first: while the old row still
        // holds the key, the successor insert conflicts, does nothing, and resolves back to the
        // old row.
        let old_id = current.id;
        state
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE standard_curves SET source_system = NULL, source_key = NULL WHERE id = $1",
                [old_id.into()],
            ))
            .await?;
        let minted = insert_curve(&state, &payload, sensor_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!("minting successor for edited curve {old_id}: {e}"))
            })?;
        tracing::warn!(
            source_system = %payload.source_system,
            source_key = %payload.source_key,
            %old_id,
            new_id = %minted,
            "Portal edited a standard curve already applied to readings; minted a successor"
        );
        return Ok(Json(RegisterStandardCurveResponse {
            id: minted,
            sensor_id,
            superseded: true,
        }));
    }

    let id = insert_curve(&state, &payload, sensor_id).await?;
    Ok(Json(RegisterStandardCurveResponse {
        id,
        sensor_id,
        superseded: false,
    }))
}

async fn insert_curve(
    state: &AppState,
    payload: &RegisterStandardCurveRequest,
    sensor_id: Uuid,
) -> AppResult<Uuid> {
    let id = Uuid::new_v4();
    state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO standard_curves
                 (id, sensor_id, name, slope, intercept, r_squared, notes, created_at,
                  source_system, source_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), $8, $9)
             ON CONFLICT (source_system, source_key)
                 WHERE source_system IS NOT NULL AND source_key IS NOT NULL
                 DO NOTHING",
            [
                id.into(),
                sensor_id.into(),
                payload
                    .name
                    .clone()
                    .unwrap_or_else(|| payload.source_key.clone())
                    .into(),
                payload.slope.into(),
                payload.intercept.into(),
                payload.r_squared.into(),
                payload.notes.clone().into(),
                payload.source_system.clone().into(),
                payload.source_key.clone().into(),
            ],
        ))
        .await?;
    // A concurrent register of the same provenance wins the insert; resolve to whichever row holds
    // the key now.
    let row = Entity::find()
        .filter(Column::SourceSystem.eq(payload.source_system.clone()))
        .filter(Column::SourceKey.eq(payload.source_key.clone()))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Internal("registered curve not found after upsert".to_string()))?;
    Ok(row.id)
}
