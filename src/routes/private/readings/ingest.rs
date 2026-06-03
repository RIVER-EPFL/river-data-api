use axum::{Json, extract::State};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Set, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::{AppEvent, AppState};
use crate::routes::private::{data_streams, readings, status_events};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::operations::resolve_windows_for_times;

#[derive(Debug, Deserialize, ToSchema)]
pub struct IngestReadingsRequest {
    pub stream_id: Uuid,
    pub readings: Vec<IngestReading>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct IngestReading {
    pub time: chrono::DateTime<Utc>,
    pub raw_value: f64,
    #[serde(default)]
    pub replicate_index: i16,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestResponse {
    pub inserted: usize,
    pub stream_id: Uuid,
    pub paired: bool,
}

const BATCH_SIZE: usize = 1000;

/// Stream-based data ingestion. Inserts readings keyed by `stream_id`. If the stream is
/// paired to a `site_parameter`, readings are stamped with `site_id`/`parameter_id` and an
/// identity calibration. Unpaired streams insert with `site_id = NULL` (and won't show up
/// in continuous aggregates until paired). Requires `write_data`.
#[utoipa::path(
    post,
    path = "/ingest",
    request_body = IngestReadingsRequest,
    responses(
        (status = 200, description = "Inserted count and pairing state", body = IngestResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_readings(
    State(state): State<AppState>,
    Json(payload): Json<IngestReadingsRequest>,
) -> AppResult<Json<IngestResponse>> {
    if payload.readings.is_empty() {
        return Ok(Json(IngestResponse {
            inserted: 0,
            stream_id: payload.stream_id,
            paired: false,
        }));
    }

    let db = &state.db;

    // Look up stream to get pairing info
    let stream = data_streams::Entity::find_by_id(payload.stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    // Resolve pairing: site_id and parameter_id from site_parameter
    let (site_id, parameter_id) = if let Some(sp_id) = stream.site_parameter_id {
        let sp = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT site_id, parameter_id FROM site_parameters WHERE id = $1",
                [sp_id.into()],
            ))
            .await?;

        if let Some(row) = sp {
            let sid: Uuid = row.try_get("", "site_id").map_err(|e| {
                AppError::Internal(format!("Failed to read site_id: {e}"))
            })?;
            let pid: Uuid = row.try_get("", "parameter_id").map_err(|e| {
                AppError::Internal(format!("Failed to read parameter_id: {e}"))
            })?;
            (Some(sid), Some(pid))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let paired = site_id.is_some();

    // Window-aware attribution: resolve calibration/deployment/site per reading TIME from the
    // sensor's windows, agreeing with reprocess_sensor_readings. The stream's frozen sensor_id is the
    // owner; cal/deployment/site come from whichever window covers each timestamp. `calibrated_value`
    // is written as identity (raw) here; reprocess (fired by calibration edits) refines it.
    let resolved = if let Some(stream_sensor) = stream.sensor_id {
        let times: Vec<chrono::DateTime<Utc>> = payload.readings.iter().map(|r| r.time).collect();
        resolve_windows_for_times(db, stream_sensor, None, &times)
            .await
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    // Build reading models
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .iter()
        .map(|r| {
            let slot = resolved.get(&r.time);
            readings::ActiveModel {
                stream_id: Set(payload.stream_id),
                time: Set(r.time.into()),
                replicate_index: Set(r.replicate_index),
                // Deployment-derived site when a deployment covers the time; else the pairing site.
                site_id: Set(slot.and_then(|s| s.site_id).or(site_id)),
                parameter_id: Set(parameter_id),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(Some(r.raw_value)),
                sensor_id: Set(r.sensor_id.or(stream.sensor_id)),
                calibration_id: Set(r.calibration_id.or_else(|| slot.and_then(|s| s.calibration_id))),
                deployment_id: Set(r.deployment_id.or_else(|| slot.and_then(|s| s.deployment_id))),
                logged: Set(Some(true)),
                measurement_type: Set(Some("continuous".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(None),
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
            .exec_without_returning(db)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("None of the records") {
                    // All duplicates in this chunk
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(AppError::Database(e));
                }
            }
        }
    }

    // Emit ingestion event
    if inserted > 0 {
        let _ = state.events.send(AppEvent::DataIngested {
            site_id,
            parameter_id,
            stream_id: payload.stream_id,
            count: inserted,
        });
    }

    // Update last_data_time on the stream
    if let Some(max_time) = payload.readings.iter().map(|r| r.time).max() {
        let should_update = stream
            .last_data_time
            .map(|t| max_time > t.with_timezone(&Utc))
            .unwrap_or(true);

        if should_update {
            let mut active: data_streams::ActiveModel = stream.into();
            active.last_data_time = Set(Some(max_time.into()));
            active.updated_at = Set(Utc::now().into());
            if let Err(e) = active.update(db).await {
                tracing::warn!(error = %e, "Failed to update stream last_data_time");
            }
        }
    }

    // Auto-compute derived parameters for newly ingested timestamps (batched), tracked as a job.
    if paired && inserted > 0
        && let Some(sid) = site_id
    {
        let mut unique_timestamps: Vec<chrono::DateTime<Utc>> =
            payload.readings.iter().map(|r| r.time).collect();
        unique_timestamps.sort();
        unique_timestamps.dedup();

        let job_id = Uuid::new_v4();
        let job_total = i32::try_from(unique_timestamps.len()).unwrap_or(i32::MAX);
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status, total, progress) \
             VALUES ($1, NULL, 'ingest_derived', NULL, 'pending', $2, 0)",
            [job_id.into(), job_total.into()],
        ))
        .await?;
        let _ = state.events.send(AppEvent::JobCreated { job_id });

        let db_clone = state.db.clone();
        let events = state.events.clone();
        tokio::spawn(async move {
            let _ = db_clone
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                    [job_id.into()],
                ))
                .await;

            let mut progress = 0i32;
            for time in unique_timestamps {
                if let Err(e) =
                    crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                        &db_clone, sid, time,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        site_id = %sid,
                        time = %time,
                        "Failed to auto-compute derived values after ingest"
                    );
                }
                progress += 1;
                if progress % 500 == 0 {
                    let _ = db_clone
                        .execute(Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE reprocessing_jobs SET progress = $1 WHERE id = $2",
                            [progress.into(), job_id.into()],
                        ))
                        .await;
                    let _ = events.send(AppEvent::JobProgress {
                        job_id,
                        status: "running".to_string(),
                        progress: Some(progress),
                        total: Some(job_total),
                    });
                }
            }

            let _ = db_clone
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'completed', progress = total, \
                     completed_at = NOW() WHERE id = $1",
                    [job_id.into()],
                ))
                .await;
            let _ = events.send(AppEvent::JobCompleted {
                job_id,
                status: "completed".to_string(),
                readings_updated: None,
                error_message: None,
            });
        });
    }

    tracing::info!(total, inserted, stream_id = %payload.stream_id, paired, "Ingest complete");
    Ok(Json(IngestResponse {
        inserted,
        stream_id: payload.stream_id,
        paired,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct IngestStatusEventsRequest {
    pub stream_id: Uuid,
    pub events: Vec<IngestStatusEvent>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct IngestStatusEvent {
    pub time: chrono::DateTime<Utc>,
    pub value: String,
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestStatusEventsResponse {
    pub inserted: usize,
    pub stream_id: Uuid,
    pub paired: bool,
}

/// Stream-based status event ingestion (non-numeric device states like "low_battery").
/// Hypertable inserts keyed by stream_id. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/ingest/status_events",
    request_body = IngestStatusEventsRequest,
    responses(
        (status = 200, description = "Inserted count and pairing state", body = IngestStatusEventsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_status_events(
    State(state): State<AppState>,
    Json(payload): Json<IngestStatusEventsRequest>,
) -> AppResult<Json<IngestStatusEventsResponse>> {
    if payload.events.is_empty() {
        return Ok(Json(IngestStatusEventsResponse {
            inserted: 0,
            stream_id: payload.stream_id,
            paired: false,
        }));
    }

    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(payload.stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let (site_id, parameter_id) = if let Some(sp_id) = stream.site_parameter_id {
        let sp = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT site_id, parameter_id FROM site_parameters WHERE id = $1",
                [sp_id.into()],
            ))
            .await?;

        if let Some(row) = sp {
            let sid: Uuid = row.try_get("", "site_id").map_err(|e| {
                AppError::Internal(format!("Failed to read site_id: {e}"))
            })?;
            let pid: Uuid = row.try_get("", "parameter_id").map_err(|e| {
                AppError::Internal(format!("Failed to read parameter_id: {e}"))
            })?;
            (Some(sid), Some(pid))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let paired = site_id.is_some();

    let models: Vec<status_events::ActiveModel> = payload
        .events
        .iter()
        .map(|e| status_events::ActiveModel {
            stream_id: Set(payload.stream_id),
            time: Set(e.time.into()),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            value: Set(e.value.clone()),
            sensor_id: Set(e.sensor_id),
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
            .exec_without_returning(db)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("None of the records") {
                    // All duplicates
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert status event batch");
                    return Err(AppError::Database(e));
                }
            }
        }
    }

    tracing::info!(total, inserted, stream_id = %payload.stream_id, paired, "Status events ingest complete");
    Ok(Json(IngestStatusEventsResponse {
        inserted,
        stream_id: payload.stream_id,
        paired,
    }))
}
