use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::actor::label;
use crate::common::middleware::{AuthContext, ProjectScope, enforce_project_scope_for_sites};
use crate::error::AppResult;
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::flows::pair_entry_channel;
use crate::routes::private::data_streams::service::{get_or_create_api_stream, site_parameter_of};
use crate::routes::private::readings::service::lock_stream_attributions;
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
/// Auto-creates "api" streams as needed, and pairs one to the site's slot once the site carries
/// it; an event is attributed from its stream's pairing and stored unattributed on an unpaired
/// stream. 10MB body limit. Requires `write_data`.
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
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BatchStatusEventsRequest>,
) -> AppResult<Json<BatchStatusEventsResponse>> {
    let target_sites: Vec<Uuid> = payload.events.iter().map(|e| e.site_id).collect();
    enforce_project_scope_for_sites(&state.db, &scope, &target_sites).await?;

    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    let actor = label(&auth);
    for e in &payload.events {
        let key = (e.site_id, e.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, e.site_id, e.parameter_id).await?;
            let slot = site_parameter_of(&state.db, e.site_id, e.parameter_id).await?;
            pair_entry_channel(&state.db, stream_id, slot, &actor).await?;
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

    let txn = sea_orm::TransactionTrait::begin(&state.db).await?;
    let attributions = lock_stream_attributions(&txn, stream_cache.values().copied()).await?;
    let models: Vec<status_events::ActiveModel> = payload
        .events
        .into_iter()
        .map(|e| {
            let stream_id = stream_cache[&(e.site_id, e.parameter_id)];
            let (site_id, parameter_id) = attributions[&stream_id];
            status_events::ActiveModel {
                stream_id: Set(stream_id),
                time: Set(e.time.into()),
                site_id: Set(site_id),
                parameter_id: Set(parameter_id),
                value: Set(e.value),
                sensor_id: Set(e
                    .sensor_id
                    .or_else(|| stream_instrument.get(&stream_id).copied().flatten())),
            }
        })
        .collect();

    let total = models.len();
    let inserted = status_events::service::insert_ignoring_duplicates(&txn, models).await?;
    txn.commit().await?;

    tracing::info!(total, inserted, "Batch status events insert complete");
    Ok(Json(BatchStatusEventsResponse { inserted }))
}
