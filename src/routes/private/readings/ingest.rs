use axum::{Json, extract::State};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Set, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::middleware::{IsSyncService, ProjectScope, enforce_project_scope_for_sites};
use crate::common::{AppEvent, AppState};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::batch::{Replace, admission, readings_upsert};
use crate::routes::private::sensors::calibrations::{resolver, service::apply_curves};
use crate::routes::private::sensors::operations::{
    resolve_slot_owner_for_times, resolve_windows_for_times,
};
use crate::routes::private::{data_streams, readings, readings::status_events};

#[derive(Debug, Deserialize, ToSchema)]
pub struct IngestReadingsRequest {
    pub stream_id: Uuid,
    pub readings: Vec<IngestReading>,
    /// Update existing rows at the same (stream, time, replicate) key instead of skipping
    /// them, so source-side corrections propagate on re-sync. Sync-service callers only;
    /// flag state and sample links on the existing row are preserved.
    #[serde(default)]
    pub overwrite: bool,
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
    /// Per-reading override ('continuous' | 'spot' | 'derived'). Omit to resolve from the
    /// stream's measurement_type, then the owning sensor's data_frequency.
    #[serde(default)]
    pub measurement_type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestResponse {
    pub inserted: usize,
    pub stream_id: Uuid,
    pub paired: bool,
}

const BATCH_SIZE: usize = 1000;

/// Batched insert. With `overwrite`, conflicting rows are updated in place on the value
/// and attribution columns; operator state (is_flagged, flag_reason, sample_id) is never
/// touched so a correction cannot clear a flag or unlink a sample.
async fn insert_reading_chunks<C: ConnectionTrait>(
    conn: &C,
    models: &[readings::ActiveModel],
    overwrite: bool,
) -> Result<usize, AppError> {
    let conflict = readings_upsert(if overwrite {
        Replace::ValuesAndAttribution
    } else {
        Replace::Nothing
    });

    let mut inserted = 0usize;
    for chunk in models.chunks(BATCH_SIZE) {
        match readings::Entity::insert_many(chunk.to_vec())
            .on_conflict(conflict.clone())
            .exec_without_returning(conn)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if !overwrite && msg.contains("None of the records") {
                    // All duplicates in this chunk
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(AppError::Database(e));
                }
            }
        }
    }
    Ok(inserted)
}

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
        (status = 400, description = "Timestamp outside [-10 years, +1 day] window, or non-finite value"),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_readings(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    IsSyncService(is_sync_service): IsSyncService,
    Json(payload): Json<IngestReadingsRequest>,
) -> AppResult<Json<IngestResponse>> {
    if payload.overwrite && !is_sync_service {
        return Err(AppError::Forbidden(
            "overwrite is restricted to sync services".to_string(),
        ));
    }

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

    let (site_id, parameter_id) = resolve_stream_slot(db, stream.site_parameter_id).await?;
    let paired = site_id.is_some();

    // A project-scoped token may only ingest into a stream paired to a site within its project.
    // An unpaired stream has no project, so a scoped token is rejected outright.
    enforce_ingest_scope(&state.db, &scope, site_id).await?;

    // The same admission rules /readings/batch applies. An out-of-window timestamp is refused
    // before anything is written and before the stream cursor is touched: `last_data_time` only
    // ever moves forward, so one future timestamp would stall this stream's incremental sync
    // until wall-clock caught up.
    for r in &payload.readings {
        admission::admit(r.time, r.raw_value, r.measurement_type.as_deref())?;
    }

    // Window-aware attribution: resolve calibration/deployment/site per reading TIME from the
    // sensor's windows, agreeing with reprocess_sensor_readings. The stream's frozen sensor_id is the
    // owner; cal/deployment/site come from whichever window covers each timestamp.
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

    // The curve covering each reading's own time, ranked by the one resolver the set-based
    // reprocess UPDATEs use. A value stored here and the value a later reprocess recomputes are
    // therefore the same number, so a reading is correct the moment it lands rather than only
    // after the next reprocess.
    let curves = {
        let requests: Vec<(Uuid, Option<Uuid>, chrono::DateTime<Utc>)> = payload
            .readings
            .iter()
            .filter_map(|r| {
                let owner = slot_owner.get(&r.time);
                let sensor = r
                    .sensor_id
                    .or(stream.sensor_id)
                    .or_else(|| owner.and_then(|o| o.sensor_id))?;
                Some((sensor, parameter_id, r.time))
            })
            .collect();
        resolver::resolve_many(db, &requests).await?
    };

    // Sensor-frequency defaults for every sensor a reading could resolve to (explicit, stream, or
    // slot owner), fetched in one query. Applied when neither the reading nor the stream declares
    // a measurement_type.
    let sensor_types = {
        let mut candidate_sensors: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(stream.sensor_id)
            .chain(slot_owner.values().filter_map(|o| o.sensor_id))
            .collect();
        candidate_sensors.sort_unstable();
        candidate_sensors.dedup();
        crate::routes::private::readings::measurement::measurement_types_for_sensors(
            db,
            &candidate_sensors,
        )
        .await?
    };

    // Build reading models
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .iter()
        .map(|r| {
            let slot = resolved.get(&r.time);
            let owner = slot_owner.get(&r.time);
            let sensor_id = r
                .sensor_id
                .or(stream.sensor_id)
                .or_else(|| owner.and_then(|o| o.sensor_id));
            // No window covers the time: store the raw value uncorrected, which is what a
            // reprocess over the same windows would leave too.
            let curve = sensor_id.and_then(|s| curves.get(&(s, parameter_id, r.time)));
            readings::ActiveModel {
                // Sync ingestion never picks a lab curve; grabs are the only writer that does.
                standard_curve_id: Set(None),
                stream_id: Set(payload.stream_id),
                time: Set(r.time.into()),
                replicate_index: Set(r.replicate_index),
                // Pairing is what attributes a reading to a site, so an unpaired stream stores its
                // readings unattributed even when a deployment of its sensor covers their time.
                // Within a paired stream the deployment decides which site, since a sensor can move
                // between sites while the stream keeps pointing at one slot.
                site_id: Set(site_id.map(|paired| slot.and_then(|s| s.site_id).unwrap_or(paired))),
                parameter_id: Set(parameter_id),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(Some(apply_curves(r.raw_value, curve.copied(), None))),
                sensor_id: Set(sensor_id),
                calibration_id: Set(r
                    .calibration_id
                    .or_else(|| curve.map(|c| c.id))
                    .or_else(|| slot.and_then(|s| s.calibration_id))
                    .or_else(|| owner.and_then(|o| o.calibration_id))),
                deployment_id: Set(r
                    .deployment_id
                    .or_else(|| slot.and_then(|s| s.deployment_id))
                    .or_else(|| owner.and_then(|o| o.deployment_id))),
                logged: Set(Some(true)),
                measurement_type: Set(Some(
                    crate::routes::private::readings::measurement::resolve_measurement_type(
                        r.measurement_type.as_deref(),
                        stream.measurement_type.as_deref(),
                        sensor_id,
                        &sensor_types,
                    ),
                )),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(None),
            }
        })
        .collect();

    let total = models.len();

    // Overwrite updates existing rows, which may live in compressed chunks; run inside a
    // transaction with the decompression cap lifted, like every other back-dated write path.
    let inserted = if payload.overwrite {
        let txn = db.begin().await?;
        txn.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0",
        )
        .await?;
        let n = insert_reading_chunks(&txn, &models, true).await?;
        txn.commit().await?;
        n
    } else {
        insert_reading_chunks(db, &models, false).await?
    };

    // Corrections rewrite history that bounded queries may have cached.
    if payload.overwrite && inserted > 0 {
        state.response_cache.invalidate_all();
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
    // Spawn-guard: skip entirely when the site has no active derived parameter, the job would
    // compute nothing, and this is the dominant source of empty `ingest_derived` jobs.
    if paired
        && inserted > 0
        && let Some(sid) = site_id
        && crate::routes::private::parameters::derived::janitor::site_has_active_derived(db, sid)
            .await
            .unwrap_or(true)
    {
        let mut unique_timestamps: Vec<chrono::DateTime<Utc>> =
            payload.readings.iter().map(|r| r.time).collect();
        unique_timestamps.sort();
        unique_timestamps.dedup();
        let source_stream = payload.stream_id;

        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "ingest_derived",
            None,
            None,
            &serde_json::json!({
                "site_id": sid,
                "stream_id": source_stream,
                "timestamps": unique_timestamps,
            }),
            None,
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

    let (site_id, parameter_id) = resolve_stream_slot(db, stream.site_parameter_id).await?;
    let paired = site_id.is_some();

    enforce_ingest_scope(&state.db, &scope, site_id).await?;

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

/// The (site_id, parameter_id) a stream's pairing resolves to. Both are `None` when the stream is
/// unpaired, ie. its readings land unattributed and stay out of the rollups until it is paired.
async fn resolve_stream_slot(
    db: &sea_orm::DatabaseConnection,
    site_parameter_id: Option<Uuid>,
) -> AppResult<(Option<Uuid>, Option<Uuid>)> {
    let Some(sp_id) = site_parameter_id else {
        return Ok((None, None));
    };
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT site_id, parameter_id FROM site_parameters WHERE id = $1",
            [sp_id.into()],
        ))
        .await?
    else {
        return Ok((None, None));
    };
    let site_id: Uuid = row
        .try_get("", "site_id")
        .map_err(|e| AppError::Internal(format!("Failed to read site_id: {e}")))?;
    let parameter_id: Uuid = row
        .try_get("", "parameter_id")
        .map_err(|e| AppError::Internal(format!("Failed to read parameter_id: {e}")))?;
    Ok((Some(site_id), Some(parameter_id)))
}

/// Project-scope check for stream-based ingest. A scoped token may only write to a stream paired
/// to a site within its project; an unpaired stream (no resolved site) is rejected outright so a
/// scoped key cannot create unattributed, project-less data.
async fn enforce_ingest_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    site_id: Option<Uuid>,
) -> AppResult<()> {
    if !scope.is_restricted() {
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
