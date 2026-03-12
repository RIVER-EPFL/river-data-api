use axum::{Json, extract::State};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::status_events;
use crate::error::AppResult;

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

pub async fn insert_batch_status_events(
    State(state): State<AppState>,
    Json(payload): Json<BatchStatusEventsRequest>,
) -> AppResult<Json<BatchStatusEventsResponse>> {
    let models: Vec<status_events::ActiveModel> = payload
        .events
        .into_iter()
        .map(|e| {
            use sea_orm::Set;
            status_events::ActiveModel {
                site_id: Set(e.site_id),
                parameter_id: Set(e.parameter_id),
                time: Set(e.time.into()),
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
                    status_events::Column::SiteId,
                    status_events::Column::ParameterId,
                    status_events::Column::Time,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&state.db)
            .await
        {
            Ok(_) => inserted += chunk.len(),
            Err(e) => {
                let msg = e.to_string();
                // "None of the records" means all were duplicates (not an error)
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
