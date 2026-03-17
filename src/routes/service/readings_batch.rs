use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, readings};
use crate::error::{AppError, AppResult};

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

/// Get or create an "api" stream for a given (site_id, parameter_id) pair.
async fn get_or_create_api_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    // Try to find existing
    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("api"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        return Ok(stream.id);
    }

    // Create new
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

    // Re-fetch in case of race condition
    let stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("api"))
        .filter(data_streams::Column::SourceKey.eq(format!("{site_id}:{parameter_id}")))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create API stream".to_string()))?;

    Ok(stream.id)
}

pub async fn insert_batch_readings(
    State(state): State<AppState>,
    Json(payload): Json<BatchReadingsRequest>,
) -> AppResult<Json<BatchReadingsResponse>> {
    // Collect unique (site_id, parameter_id) pairs and resolve stream_ids
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();

    for r in &payload.readings {
        let key = (r.site_id, r.parameter_id);
        if !stream_cache.contains_key(&key) {
            let stream_id = get_or_create_api_stream(&state.db, r.site_id, r.parameter_id).await?;
            stream_cache.insert(key, stream_id);
        }
    }

    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .map(|r| {
            let stream_id = stream_cache[&(r.site_id, r.parameter_id)];
            readings::ActiveModel {
                stream_id: Set(stream_id),
                site_id: Set(Some(r.site_id)),
                parameter_id: Set(Some(r.parameter_id)),
                time: Set(r.time.into()),
                replicate_index: Set(0),
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
                    readings::Column::StreamId,
                    readings::Column::Time,
                    readings::Column::ReplicateIndex,
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
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(crate::error::AppError::Database(e));
                }
            }
        }
    }

    tracing::info!(total, inserted, "Batch readings insert complete");

    // Invalidate response cache for all affected sites
    if inserted > 0 {
        let affected_site_ids: std::collections::HashSet<Uuid> =
            stream_cache.keys().map(|(site_id, _)| *site_id).collect();
        for site_id in affected_site_ids {
            crate::services::cache::invalidate_prefix(&state, &format!("readings:{site_id}")).await;
            crate::services::cache::invalidate_prefix(&state, &format!("aggregates:{site_id}")).await;
        }
    }

    Ok(Json(BatchReadingsResponse { inserted }))
}
