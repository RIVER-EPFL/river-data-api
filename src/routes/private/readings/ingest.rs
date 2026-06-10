use axum::{Json, extract::State};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Set, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::common::{AppEvent, AppState};
use crate::routes::private::{data_streams, readings, status_events};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::operations::{
    resolve_slot_owner_for_times, resolve_windows_for_times,
};

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
    ProjectScope(scope): ProjectScope,
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

    // A project-scoped token may only ingest into a stream paired to a site within its project.
    // An unpaired stream has no project, so a scoped token is rejected outright.
    enforce_ingest_scope(&state.db, scope, site_id).await?;

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

    // Fallback when the stream carries no frozen sensor: attribute by the (site, parameter)
    // deployment timeline so readings still land owned when a deployment covers their time.
    let slot_owner = match (stream.sensor_id, site_id, parameter_id) {
        (None, Some(s), Some(p)) => {
            let times: Vec<chrono::DateTime<Utc>> =
                payload.readings.iter().map(|r| r.time).collect();
            resolve_slot_owner_for_times(db, s, p, &times)
                .await
                .unwrap_or_default()
        }
        _ => std::collections::HashMap::new(),
    };

    // Build reading models
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .iter()
        .map(|r| {
            let slot = resolved.get(&r.time);
            let owner = slot_owner.get(&r.time);
            readings::ActiveModel {
                stream_id: Set(payload.stream_id),
                time: Set(r.time.into()),
                replicate_index: Set(r.replicate_index),
                // Deployment-derived site when a deployment covers the time; else the pairing site.
                site_id: Set(slot.and_then(|s| s.site_id).or(site_id)),
                parameter_id: Set(parameter_id),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(Some(r.raw_value)),
                sensor_id: Set(r.sensor_id.or(stream.sensor_id).or_else(|| owner.and_then(|o| o.sensor_id))),
                calibration_id: Set(r
                    .calibration_id
                    .or_else(|| slot.and_then(|s| s.calibration_id))
                    .or_else(|| owner.and_then(|o| o.calibration_id))),
                deployment_id: Set(r
                    .deployment_id
                    .or_else(|| slot.and_then(|s| s.deployment_id))
                    .or_else(|| owner.and_then(|o| o.deployment_id))),
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

        // Event-driven open-alarm reconcile for this slot (error-safe; backstop still covers it),
        // plus historical episode reconstruction over the ingested window so back-dated breaches
        // land in alarm_events like the batch/import paths. Inline rather than a tracked job:
        // single ingest fires every sync cycle per stream and would spam reprocessing_jobs.
        if let (Some(s), Some(p)) = (site_id, parameter_id) {
            crate::routes::private::alarms::sweeper::reconcile_and_notify(
                &state.db,
                &state.events,
                &[(s, p)],
            )
            .await;

            let times = payload.readings.iter().map(|r| r.time);
            if let (Some(lo), Some(hi)) = (times.clone().min(), times.max())
                && let Err(e) = crate::routes::private::alarms::episodes::evaluate_alarm_episodes(
                    &state.db, s, p, lo, hi,
                )
                .await
            {
                tracing::warn!(error = %e, site_id = %s, parameter_id = %p, "alarm episode reconstruction failed");
            }
        }
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
    // Spawn-guard: skip entirely when the site has no active derived parameter — the job would
    // compute nothing, and this is the dominant source of empty `ingest_derived` jobs.
    if paired
        && inserted > 0
        && let Some(sid) = site_id
        && crate::routes::private::derived_parameters::janitor::site_has_active_derived(db, sid)
            .await
            .unwrap_or(true)
    {
        let mut unique_timestamps: Vec<chrono::DateTime<Utc>> =
            payload.readings.iter().map(|r| r.time).collect();
        unique_timestamps.sort();
        unique_timestamps.dedup();
        let job_total = i32::try_from(unique_timestamps.len()).unwrap_or(i32::MAX);
        let source_stream = payload.stream_id;

        crate::routes::private::reprocessing_jobs::lifecycle::spawn_tracked_job_ctx(
            db,
            None,
            "ingest_derived",
            None,
            state.events.clone(),
            move |ctx| {
                let timestamps = unique_timestamps.clone();
                async move {
                    ctx.set_site(sid).await;
                    ctx.set_detail(serde_json::json!({
                        "scope": { "site_id": sid },
                        "source": { "stream_id": source_stream },
                        "counts": { "timestamps": job_total },
                    }))
                    .await;
                    ctx.set_progress(0, Some(job_total)).await;
                    let mut progress = 0i32;
                    for time in timestamps {
                        if let Err(e) =
                            crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                                ctx.db(), sid, time,
                            )
                            .await
                        {
                            tracing::warn!(error = %e, site_id = %sid, time = %time, "Failed to auto-compute derived values after ingest");
                        }
                        progress += 1;
                        if progress % 500 == 0 {
                            ctx.set_progress(progress, Some(job_total)).await;
                        }
                    }
                    ctx.set_progress(progress, Some(job_total)).await;
                    Ok(i64::from(progress))
                }
            },
        )
        .await?;
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
    ProjectScope(scope): ProjectScope,
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

    enforce_ingest_scope(&state.db, scope, site_id).await?;

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

/// Project-scope check for stream-based ingest. A scoped token may only write to a stream paired
/// to a site within its project; an unpaired stream (no resolved site) is rejected outright so a
/// scoped key cannot create unattributed, project-less data.
async fn enforce_ingest_scope(
    db: &sea_orm::DatabaseConnection,
    scope: Option<Uuid>,
    site_id: Option<Uuid>,
) -> AppResult<()> {
    if scope.is_none() {
        return Ok(());
    }
    match site_id {
        Some(sid) => enforce_project_scope_for_sites(db, scope, &[sid]).await?,
        None => {
            return Err(AppError::Forbidden(
                "Project-scoped token cannot ingest into an unpaired stream".to_string(),
            ));
        }
    }
    Ok(())
}
