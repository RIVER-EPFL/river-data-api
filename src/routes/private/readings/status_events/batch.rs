use axum::{Json, extract::State};
use sea_orm::{EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::routes::private::readings::status_events;
use crate::error::AppResult;
use crate::routes::private::data_streams::service::get_or_create_api_stream;

#[derive(Debug, Deserialize, ToSchema)]
pub struct BatchStatusEventsRequest {
    pub events: Vec<StatusEventInput>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct StatusEventInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub value: String,
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BatchStatusEventsResponse {
    pub inserted: usize,
}

const BATCH_SIZE: usize = 1000;

/// Batch insert non-numeric device status events (e.g. "low_battery", "offline").
/// Auto-creates "api" streams as needed. 10MB body limit. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/status_events/batch",
    request_body = BatchStatusEventsRequest,
    responses(
        (status = 200, description = "Inserted count", body = BatchStatusEventsResponse),
        (status = 413, description = "Body exceeds 10MB limit"),
    ),
    tag = "ingestion"
)]
pub async fn insert_batch_status_events(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BatchStatusEventsRequest>,
) -> AppResult<Json<BatchStatusEventsResponse>> {
    let target_sites: Vec<Uuid> = payload.events.iter().map(|e| e.site_id).collect();
    enforce_project_scope_for_sites(&state.db, &scope, &target_sites).await?;

    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();

    for e in &payload.events {
        let key = (e.site_id, e.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, e.site_id, e.parameter_id).await?;
            entry.insert(stream_id);
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
