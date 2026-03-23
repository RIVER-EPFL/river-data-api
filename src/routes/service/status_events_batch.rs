use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, status_events};
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct BatchStatusEventsRequest {
    pub events: Vec<StatusEventInput>,
}

#[derive(Debug, Deserialize)]
pub struct StatusEventInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub value: String,
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct BatchStatusEventsResponse {
    pub inserted: usize,
}

const BATCH_SIZE: usize = 1000;

/// Get or create an "api" stream for a given (site_id, parameter_id) pair.
async fn get_or_create_api_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("api"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        return Ok(stream.id);
    }

    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let model = data_streams::ActiveModel {
        id: Set(id),
        source_system: Set("api".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some("API batch insert".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        site_parameter_id: Set(None),
        sensor_id: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    data_streams::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                data_streams::Column::SourceSystem,
                data_streams::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    let stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("api"))
        .filter(data_streams::Column::SourceKey.eq(format!("{site_id}:{parameter_id}")))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create API stream".to_string()))?;

    Ok(stream.id)
}

pub async fn insert_batch_status_events(
    State(state): State<AppState>,
    Json(payload): Json<BatchStatusEventsRequest>,
) -> AppResult<Json<BatchStatusEventsResponse>> {
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();

    for e in &payload.events {
        let key = (e.site_id, e.parameter_id);
        if !stream_cache.contains_key(&key) {
            let stream_id = get_or_create_api_stream(&state.db, e.site_id, e.parameter_id).await?;
            stream_cache.insert(key, stream_id);
        }
    }

    let models: Vec<status_events::ActiveModel> = payload
        .events
        .into_iter()
        .map(|e| {
            let stream_id = stream_cache[&(e.site_id, e.parameter_id)];
            status_events::ActiveModel {
                stream_id: Set(stream_id),
                time: Set(e.time.into()),
                site_id: Set(Some(e.site_id)),
                parameter_id: Set(Some(e.parameter_id)),
                value: Set(e.value),
                sensor_id: Set(e.sensor_id),
            }
        })
        .collect();

    let total = models.len();
    let mut inserted = 0usize;

    for chunk in models.chunks(BATCH_SIZE) {
        match status_events::Entity::insert_many(chunk.to_vec())
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    status_events::Column::StreamId,
                    status_events::Column::Time,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(&state.db)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("None of the records") {
                    // All duplicates in this chunk
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert status event batch");
                    return Err(crate::error::AppError::Database(e));
                }
            }
        }
    }

    tracing::info!(total, inserted, "Batch status events insert complete");
    Ok(Json(BatchStatusEventsResponse { inserted }))
}
