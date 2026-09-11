use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::AppResult;
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::service::get_or_create_api_stream;
use crate::routes::private::readings::status_events;

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

/// Batch insert non-numeric device status events (e.g. "low_battery", "offline").
/// Auto-creates "api" streams as needed. 10MB body limit. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/status_events/batch",
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

    // The instrument each stream names, for an event that names none of its own.
    let stream_instrument: HashMap<Uuid, Option<Uuid>> = data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(stream_cache.values().copied()))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.sensor_id))
        .collect();

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
                sensor_id: Set(e
                    .sensor_id
                    .or_else(|| stream_instrument.get(&stream_id).copied().flatten())),
            }
        })
        .collect();

    let total = models.len();
    let inserted = status_events::service::insert_ignoring_duplicates(&state.db, models).await?;
    tracing::info!(total, inserted, "Batch status events insert complete");
    Ok(Json(BatchStatusEventsResponse { inserted }))
}
