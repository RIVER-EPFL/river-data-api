use axum::{Json, extract::State};
use sea_orm::{EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::common::AppState;
use crate::routes::private::readings;
use crate::error::AppResult;
use crate::routes::private::data_streams::operations::get_or_create_api_stream;

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
    #[serde(default)]
    pub replicate_index: Option<i16>,
    #[serde(default)]
    pub sample_id: Option<Uuid>,
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
    // Validate timestamps are within reasonable bounds
    let now = chrono::Utc::now();
    let min_time = now - chrono::Duration::days(365 * 10); // 10 years ago
    let max_time = now + chrono::Duration::days(1); // 1 day in the future
    for r in &payload.readings {
        if r.time < min_time || r.time > max_time {
            return Err(crate::error::AppError::BadRequest(format!(
                "Reading timestamp {} is outside valid range ({} to {})",
                r.time.to_rfc3339(),
                min_time.to_rfc3339(),
                max_time.to_rfc3339(),
            )));
        }
    }

    // Collect unique (site_id, parameter_id) pairs and resolve stream_ids
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();

    for r in &payload.readings {
        let key = (r.site_id, r.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, r.site_id, r.parameter_id).await?;
            entry.insert(stream_id);
        }
    }

    // Collect unique (site_id, time) pairs for derived auto-compute
    let site_timestamps_for_derived: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = {
        let mut map: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            map.entry(r.site_id).or_default().push(r.time);
        }
        for timestamps in map.values_mut() {
            timestamps.sort();
            timestamps.dedup();
        }
        map
    };

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
                replicate_index: Set(r.replicate_index.unwrap_or(0)),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(r.calibrated_value),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(r.calibration_id),
                deployment_id: Set(r.deployment_id),
                logged: Set(Some(true)),
                measurement_type: Set(Some("continuous".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(r.sample_id),
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

    // Invalidate response cache and auto-compute derived parameters for all affected sites
    if inserted > 0 {
        let affected_site_ids: std::collections::HashSet<Uuid> =
            stream_cache.keys().map(|(site_id, _)| *site_id).collect();
        for site_id in &affected_site_ids {
            crate::common::cache::invalidate_prefix(&state, &format!("readings:{site_id}")).await;
            crate::common::cache::invalidate_prefix(&state, &format!("aggregates:{site_id}")).await;
        }

        // Auto-compute derived values for affected sites
        let db_clone = state.db.clone();
        tokio::spawn(async move {
            for (site_id, timestamps) in site_timestamps_for_derived {
                for time in timestamps {
                    if let Err(e) =
                        crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                            &db_clone, site_id, time,
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            site_id = %site_id,
                            time = %time,
                            "Failed to auto-compute derived values after batch insert"
                        );
                    }
                }
            }
        });
    }

    Ok(Json(BatchReadingsResponse { inserted }))
}
