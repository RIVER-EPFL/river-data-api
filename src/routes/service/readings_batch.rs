use axum::{Json, extract::State};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::readings;
use crate::error::AppResult;

#[derive(Debug, Deserialize)]
pub struct BatchReadingsRequest {
    pub readings: Vec<ReadingInput>,
}

#[derive(Debug, Deserialize)]
pub struct ReadingInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct BatchReadingsResponse {
    pub inserted: usize,
}

const BATCH_SIZE: usize = 1000;

pub async fn insert_batch_readings(
    State(state): State<AppState>,
    Json(payload): Json<BatchReadingsRequest>,
) -> AppResult<Json<BatchReadingsResponse>> {
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .map(|r| {
            use sea_orm::Set;
            readings::ActiveModel {
                site_id: Set(r.site_id),
                parameter_id: Set(r.parameter_id),
                time: Set(r.time.into()),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(r.calibrated_value),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(r.calibration_id),
                deployment_id: Set(r.deployment_id),
                logged: Set(Some(true)),
                measurement_type: Set(Some("continuous".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                field_trip_id: Set(None),
            }
        })
        .collect();

    let total = models.len();
    let mut inserted = 0usize;

    for chunk in models.chunks(BATCH_SIZE) {
        match readings::Entity::insert_many(chunk.to_vec())
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    readings::Column::SiteId,
                    readings::Column::ParameterId,
                    readings::Column::Time,
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
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(crate::error::AppError::Database(e));
                }
            }
        }
    }

    tracing::info!(total, inserted, "Batch readings insert complete");
    Ok(Json(BatchReadingsResponse { inserted }))
}
