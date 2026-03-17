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
use crate::services::sync_state::refresh_continuous_aggregates_full;

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
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
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
// List Streams
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ListStreamsQuery {
    pub source_system: Option<String>,
    pub paired: Option<bool>,
    pub is_active: Option<bool>,
}

pub async fn list_streams(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<ListStreamsQuery>,
) -> AppResult<Json<Vec<StreamResponse>>> {
    let mut finder = data_streams::Entity::find();

    if let Some(ref ss) = query.source_system {
        finder = finder.filter(data_streams::Column::SourceSystem.eq(ss.as_str()));
    }
    if let Some(paired) = query.paired {
        if paired {
            finder = finder.filter(data_streams::Column::SiteParameterId.is_not_null());
        } else {
            finder = finder.filter(data_streams::Column::SiteParameterId.is_null());
        }
    }
    if let Some(active) = query.is_active {
        finder = finder.filter(data_streams::Column::IsActive.eq(active));
    }

    let streams = finder.all(&state.db).await?;
    Ok(Json(streams.into_iter().map(Into::into).collect()))
}

// ============================================================================
// Get Stream
// ============================================================================

pub async fn get_stream(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<StreamResponse>> {
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
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

    // Update stream: set pairing
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(Some(payload.site_parameter_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Backfill: update readings with site_id + parameter_id, apply identity calibration
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings
              SET site_id = $1, parameter_id = $2,
                  calibrated_value = COALESCE(calibrated_value, raw_value)
              WHERE stream_id = $3 AND site_id IS NULL",
            [sp.site_id.into(), sp.parameter_id.into(), stream_id.into()],
        ))
        .await?;

    let backfilled = result.rows_affected();

    // Also backfill status_events
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events
          SET site_id = $1, parameter_id = $2
          WHERE stream_id = $3 AND site_id IS NULL",
        [sp.site_id.into(), sp.parameter_id.into(), stream_id.into()],
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

    let now = Utc::now();

    // Clear pairing on stream
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(None);
    active.paired_at = Set(None);
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Clear site_id/parameter_id on readings
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
