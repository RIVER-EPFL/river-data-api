use axum::{Json, extract::{Path, State}};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, Statement,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, site_parameters};
use crate::error::{AppError, AppResult};
use crate::services::operations::{close_sensor_deployment, create_sensor_for_stream};
use crate::services::sync_state::refresh_continuous_aggregates_full;

// ============================================================================
// Stream Stats
// ============================================================================

#[derive(Debug, Serialize)]
pub struct StreamStatsResponse {
    pub stream_id: Uuid,
    pub reading_count: i64,
    pub min_time: Option<chrono::DateTime<Utc>>,
    pub max_time: Option<chrono::DateTime<Utc>>,
    pub latest_value: Option<f64>,
}

/// `GET /api/service/streams/{id}/stats` — reading stats for a stream.
pub async fn stream_stats(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<StreamStatsResponse>> {
    // Verify stream exists
    data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let row = state.db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) as count, MIN(time) as min_time, MAX(time) as max_time FROM readings WHERE stream_id = $1",
            [id.into()],
        ))
        .await?;

    let (count, min_time, max_time) = if let Some(row) = row {
        let count: i64 = row.try_get("", "count").unwrap_or(0);
        let min_time: Option<chrono::DateTime<chrono::FixedOffset>> = row.try_get("", "min_time").ok();
        let max_time: Option<chrono::DateTime<chrono::FixedOffset>> = row.try_get("", "max_time").ok();
        (count, min_time.map(|t| t.with_timezone(&Utc)), max_time.map(|t| t.with_timezone(&Utc)))
    } else {
        (0, None, None)
    };

    // Get latest value
    let latest_row = state.db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT raw_value FROM readings WHERE stream_id = $1 ORDER BY time DESC LIMIT 1",
            [id.into()],
        ))
        .await?;
    let latest_value: Option<f64> = latest_row.and_then(|r| r.try_get("", "raw_value").ok());

    Ok(Json(StreamStatsResponse {
        stream_id: id,
        reading_count: count,
        min_time,
        max_time,
        latest_value,
    }))
}

// ============================================================================
// Stream Registration
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RegisterStreamRequest {
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    #[serde(default = "default_metadata")]
    pub metadata: serde_json::Value,
}

fn default_metadata() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Serialize)]
pub struct StreamResponse {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    pub metadata: serde_json::Value,
    pub site_parameter_id: Option<Uuid>,
    pub sensor_id: Option<Uuid>,
    pub is_active: bool,
    pub discovered_at: chrono::DateTime<Utc>,
    pub paired_at: Option<chrono::DateTime<Utc>>,
    pub last_data_time: Option<chrono::DateTime<Utc>>,
}

impl From<data_streams::Model> for StreamResponse {
    fn from(m: data_streams::Model) -> Self {
        Self {
            id: m.id,
            source_system: m.source_system,
            source_key: m.source_key,
            source_name: m.source_name,
            source_path: m.source_path,
            metadata: m.metadata,
            site_parameter_id: m.site_parameter_id,
            sensor_id: m.sensor_id,
            is_active: m.is_active,
            discovered_at: m.discovered_at.with_timezone(&Utc),
            paired_at: m.paired_at.map(|t| t.with_timezone(&Utc)),
            last_data_time: m.last_data_time.map(|t| t.with_timezone(&Utc)),
        }
    }
}

/// `POST /api/service/streams/register` — upsert on (source_system, source_key).
pub async fn register_stream(
    State(state): State<AppState>,
    Json(payload): Json<RegisterStreamRequest>,
) -> AppResult<Json<StreamResponse>> {
    let now = Utc::now();

    let model = data_streams::ActiveModel {
        id: Set(Uuid::new_v4()),
        source_system: Set(payload.source_system.clone()),
        source_key: Set(payload.source_key.clone()),
        source_name: Set(payload.source_name.clone()),
        source_path: Set(payload.source_path.clone()),
        metadata: Set(payload.metadata.clone()),
        site_parameter_id: Set(None),
        sensor_id: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    // Upsert on (source_system, source_key)
    data_streams::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                data_streams::Column::SourceSystem,
                data_streams::Column::SourceKey,
            ])
            .update_columns([
                data_streams::Column::SourceName,
                data_streams::Column::SourcePath,
                data_streams::Column::Metadata,
                data_streams::Column::UpdatedAt,
            ])
            .to_owned(),
        )
        .exec(&state.db)
        .await?;

    // Re-fetch the upserted row
    let stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(&payload.source_system))
        .filter(data_streams::Column::SourceKey.eq(&payload.source_key))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch registered stream".to_string()))?;

    Ok(Json(stream.into()))
}

// ============================================================================
// Pair Stream
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct PairStreamRequest {
    pub site_parameter_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct PairStreamResponse {
    pub stream: StreamResponse,
    pub backfilled: u64,
}

/// `POST /api/service/streams/{id}/pair` — pair a stream to a site_parameter.
///
/// 1. Sets `site_parameter_id` and `paired_at` on the stream.
/// 2. Backfills existing readings: `UPDATE readings SET site_id=X, parameter_id=Y WHERE stream_id=Z AND site_id IS NULL`.
/// 3. Applies identity calibration (`calibrated_value = raw_value`).
/// 4. Triggers aggregate refresh.
pub async fn pair_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
    Json(payload): Json<PairStreamRequest>,
) -> AppResult<Json<PairStreamResponse>> {
    let db = &state.db;

    // Validate stream exists and is unpaired
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    if stream.site_parameter_id.is_some() {
        return Err(AppError::BadRequest(
            "Stream is already paired. Unpair it first.".to_string(),
        ));
    }

    // Validate site_parameter exists and get its site_id + parameter_id
    let sp = site_parameters::Entity::find_by_id(payload.site_parameter_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Site parameter not found".to_string()))?;

    let now = Utc::now();

    // Create/reuse sensor for this stream
    let sensor_ctx =
        create_sensor_for_stream(db, &stream, sp.parameter_id, sp.site_id).await?;

    // Update stream: set pairing
    // Re-fetch stream since create_sensor_for_stream may have updated sensor_id
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to re-fetch stream".to_string()))?;
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(Some(payload.site_parameter_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Backfill: update readings with site_id + parameter_id + sensor context, apply identity calibration
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings
              SET site_id = $1, parameter_id = $2,
                  sensor_id = $4, calibration_id = $5, deployment_id = $6,
                  calibrated_value = COALESCE(calibrated_value, raw_value)
              WHERE stream_id = $3 AND site_id IS NULL",
            [
                sp.site_id.into(),
                sp.parameter_id.into(),
                stream_id.into(),
                sensor_ctx.sensor_id.into(),
                sensor_ctx.calibration_id.into(),
                sensor_ctx.deployment_id.into(),
            ],
        ))
        .await?;

    let backfilled = result.rows_affected();

    // Also backfill status_events
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events
          SET site_id = $1, parameter_id = $2, sensor_id = $4
          WHERE stream_id = $3 AND site_id IS NULL",
        [
            sp.site_id.into(),
            sp.parameter_id.into(),
            stream_id.into(),
            sensor_ctx.sensor_id.into(),
        ],
    ))
    .await?;

    // Trigger aggregate refresh in background
    if backfilled > 0 {
        let db_clone = db.clone();
        tokio::spawn(async move {
            refresh_continuous_aggregates_full(&db_clone).await;
        });
    }

    // Re-fetch updated stream
    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(PairStreamResponse {
        stream: updated.into(),
        backfilled,
    }))
}

// ============================================================================
// Unpair Stream
// ============================================================================

#[derive(Debug, Serialize)]
pub struct UnpairStreamResponse {
    pub stream: StreamResponse,
    pub cleared: u64,
}

/// `POST /api/service/streams/{id}/unpair` — remove pairing from a stream.
///
/// Clears `site_parameter_id` and `paired_at`, and nulls out `site_id`/`parameter_id`
/// on all readings for this stream.
pub async fn unpair_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
) -> AppResult<Json<UnpairStreamResponse>> {
    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    if stream.site_parameter_id.is_none() {
        return Err(AppError::BadRequest("Stream is not paired".to_string()));
    }

    // Close sensor deployment if stream has a sensor
    if let Some(sensor_id) = stream.sensor_id {
        if let Some(sp_id) = stream.site_parameter_id {
            if let Ok(Some(sp)) = site_parameters::Entity::find_by_id(sp_id).one(db).await {
                let _ = close_sensor_deployment(db, sensor_id, sp.site_id).await;
            }
        }
    }

    let now = Utc::now();

    // Clear pairing on stream (keep sensor_id — sensor persists)
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(None);
    active.paired_at = Set(None);
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Clear site_id/parameter_id on readings (keep sensor_id/calibration_id/deployment_id)
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET site_id = NULL, parameter_id = NULL
              WHERE stream_id = $1",
            [stream_id.into()],
        ))
        .await?;

    let cleared = result.rows_affected();

    // Clear on status_events too
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events SET site_id = NULL, parameter_id = NULL
          WHERE stream_id = $1",
        [stream_id.into()],
    ))
    .await?;

    // Trigger aggregate refresh in background
    if cleared > 0 {
        let db_clone = db.clone();
        tokio::spawn(async move {
            refresh_continuous_aggregates_full(&db_clone).await;
        });
    }

    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(UnpairStreamResponse {
        stream: updated.into(),
        cleared,
    }))
}
