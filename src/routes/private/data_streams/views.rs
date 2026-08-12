use axum::{Json, extract::{Path, State}};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, Statement,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::routes::private::{data_streams, sites::parameters as site_parameters};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::operations::{close_sensor_deployment, create_sensor_for_stream};

#[derive(Debug, Serialize, ToSchema)]
pub struct StreamStatsResponse {
    pub stream_id: Uuid,
    pub reading_count: i64,
    pub min_time: Option<chrono::DateTime<Utc>>,
    pub max_time: Option<chrono::DateTime<Utc>>,
    pub latest_value: Option<f64>,
}

/// Reading statistics for a single data stream: count, time range, latest value.
/// Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/streams/{id}/stats",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    responses(
        (status = 200, description = "Stream statistics", body = StreamStatsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn stream_stats(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<StreamStatsResponse>> {
    // Verify stream exists
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    // A project-scoped key may only inspect a stream paired into its own project. An unpaired or
    // cross-project stream is reported as not-found (rather than 403) so a scoped key can't even
    // confirm the existence of another project's streams.
    if scope.is_restricted() {
        let stream_project = match stream.site_parameter_id {
            Some(sp_id) => state
                .db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT s.project_id FROM site_parameters sp JOIN sites s ON s.id = sp.site_id WHERE sp.id = $1",
                    [sp_id.into()],
                ))
                .await?
                .and_then(|r| r.try_get::<Uuid>("", "project_id").ok()),
            None => None,
        };
        if !scope.allows_project_opt(stream_project) {
            return Err(AppError::NotFound("Stream not found".to_string()));
        }
    }

    let row = state.db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) as count, MIN(time) as min_time, MAX(time) as max_time FROM readings WHERE stream_id = $1",
            [id.into()],
        ))
        .await?;

    let (count, min_time, max_time) = if let Some(row) = row {
        let count: i64 = row.try_get("", "count").unwrap_or(0);
        let min_time: Option<chrono::DateTime<chrono::FixedOffset>> = row.try_get("", "min_time").ok();
        let max_time: Option<chrono::DateTime<chrono::FixedOffset>> = row.try_get("", "max_time").ok();
        (count, min_time.map(|t| t.with_timezone(&Utc)), max_time.map(|t| t.with_timezone(&Utc)))
    } else {
        (0, None, None)
    };

    // Get latest value
    let latest_row = state.db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT raw_value FROM readings WHERE stream_id = $1 ORDER BY time DESC LIMIT 1",
            [id.into()],
        ))
        .await?;
    let latest_value: Option<f64> = latest_row.and_then(|r| r.try_get("", "raw_value").ok());

    Ok(Json(StreamStatsResponse {
        stream_id: id,
        reading_count: count,
        min_time,
        max_time,
        latest_value,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterStreamRequest {
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    #[serde(default = "default_metadata")]
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    /// Stream-level default for readings.measurement_type ('continuous' | 'spot' | 'derived').
    /// Omit to defer to the owning sensor's data_frequency.
    #[serde(default)]
    pub measurement_type: Option<String>,
}

fn default_metadata() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StreamResponse {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    pub site_parameter_id: Option<Uuid>,
    pub sensor_id: Option<Uuid>,
    pub measurement_type: Option<String>,
    pub is_active: bool,
    pub discovered_at: chrono::DateTime<Utc>,
    pub paired_at: Option<chrono::DateTime<Utc>>,
    pub last_data_time: Option<chrono::DateTime<Utc>>,
}

impl From<data_streams::Model> for StreamResponse {
    fn from(m: data_streams::Model) -> Self {
        Self {
            id: m.id,
            source_system: m.source_system,
            source_key: m.source_key,
            source_name: m.source_name,
            source_path: m.source_path,
            metadata: m.metadata,
            site_parameter_id: m.site_parameter_id,
            sensor_id: m.sensor_id,
            measurement_type: m.measurement_type,
            is_active: m.is_active,
            discovered_at: m.discovered_at.with_timezone(&Utc),
            paired_at: m.paired_at.map(|t| t.with_timezone(&Utc)),
            last_data_time: m.last_data_time.map(|t| t.with_timezone(&Utc)),
        }
    }
}

/// Upsert a data stream by (source_system, source_key). Used by sync microservices on
/// discovery to register streams before pairing. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/streams/register",
    request_body = RegisterStreamRequest,
    responses(
        (status = 200, description = "Stream registered (created or updated)", body = StreamResponse),
    ),
    tag = "streams"
)]
pub async fn register_stream(
    State(state): State<AppState>,
    Json(payload): Json<RegisterStreamRequest>,
) -> AppResult<Json<StreamResponse>> {
    if let Some(mt) = payload.measurement_type.as_deref()
        && !matches!(mt, "continuous" | "spot" | "derived")
    {
        return Err(AppError::BadRequest(format!(
            "invalid measurement_type '{mt}' (expected continuous, spot, or derived)"
        )));
    }
    let now = Utc::now();

    let model = data_streams::ActiveModel {
        id: Set(Uuid::new_v4()),
        source_system: Set(payload.source_system.clone()),
        source_key: Set(payload.source_key.clone()),
        source_name: Set(payload.source_name.clone()),
        source_path: Set(payload.source_path.clone()),
        metadata: Set(payload.metadata.clone()),
        site_parameter_id: Set(None),
        sensor_id: Set(None),
        measurement_type: Set(payload.measurement_type.clone()),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    // Upsert on (source_system, source_key)
    data_streams::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                data_streams::Column::SourceSystem,
                data_streams::Column::SourceKey,
            ])
            .update_columns([
                data_streams::Column::SourceName,
                data_streams::Column::SourcePath,
                data_streams::Column::Metadata,
                data_streams::Column::UpdatedAt,
            ])
            .to_owned(),
        )
        .exec(&state.db)
        .await?;

    // Re-fetch the upserted row
    let mut stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(&payload.source_system))
        .filter(data_streams::Column::SourceKey.eq(&payload.source_key))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch registered stream".to_string()))?;

    // A declared classification wins on re-registration; an omitted one (None) never clears an
    // operator-set value, so measurement_type is not in the upsert's update_columns.
    if payload.measurement_type.is_some() && stream.measurement_type != payload.measurement_type {
        let mut active: data_streams::ActiveModel = stream.clone().into();
        active.measurement_type = Set(payload.measurement_type.clone());
        active.updated_at = Set(now.into());
        stream = active.update(&state.db).await?;
    }

    Ok(Json(stream.into()))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ImportStreamRequest {
    /// Legacy field, ignored: a sensor is imported parameter-free (parameter is bound at
    /// deploy/grab time). Retained so existing callers keep deserializing.
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportStreamResponse {
    pub stream: StreamResponse,
    pub sensor_id: Uuid,
    pub attributed: u64,
}

/// Import a stream's sensor into inventory WITHOUT deploying it to a site. Creates/reuses the sensor
/// by (serial, parameter), links it to the stream, and stamps `sensor_id`/`calibration_id` on the
/// stream's site-less readings (calibration math applies; the readings stay un-attributed to any
/// site until an explicit adopt). Idempotent: re-import reuses the same sensor and only fills
/// readings missing this attribution. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/streams/{id}/import",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    request_body = ImportStreamRequest,
    responses(
        (status = 200, description = "Sensor imported; attribution count returned", body = ImportStreamResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn import_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
    Json(_payload): Json<ImportStreamRequest>,
) -> AppResult<Json<ImportStreamResponse>> {
    use crate::routes::private::sensors::operations::import_sensor_for_stream;
    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    // Import is parameter-free: a sensor is a device, its parameter is bound at deploy/grab time.
    let ctx = import_sensor_for_stream(db, &stream).await?;

    // Stamp sensor/calibration on site-less readings only; do NOT touch
    // site_id/parameter_id/deployment_id (those are set at adopt). Idempotent.
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings
              SET sensor_id = $2, calibration_id = $3,
                  calibrated_value = COALESCE(calibrated_value, raw_value)
              WHERE stream_id = $1 AND sensor_id IS NULL",
            [stream_id.into(), ctx.sensor_id.into(), ctx.calibration_id.into()],
        ))
        .await?;
    let attributed = result.rows_affected();

    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(ImportStreamResponse {
        stream: updated.into(),
        sensor_id: ctx.sensor_id,
        attributed,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PairStreamRequest {
    pub site_parameter_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PairStreamResponse {
    pub stream: StreamResponse,
    pub backfilled: u64,
}

/// Pair a stream to a site_parameter. Sets `site_parameter_id`/`paired_at` on the stream,
/// backfills existing unpaired readings with site_id/parameter_id, applies identity
/// calibration, and triggers an aggregate refresh. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/streams/{id}/pair",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    request_body = PairStreamRequest,
    responses(
        (status = 200, description = "Stream paired, backfill count returned", body = PairStreamResponse),
        (status = 404, description = "Stream not found"),
        (status = 409, description = "Stream already paired"),
    ),
    tag = "streams"
)]
pub async fn pair_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
    Json(payload): Json<PairStreamRequest>,
) -> AppResult<Json<PairStreamResponse>> {
    let db = &state.db;

    // Validate stream exists and is unpaired
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    if stream.site_parameter_id.is_some() {
        return Err(AppError::BadRequest(
            "Stream is already paired. Unpair it first.".to_string(),
        ));
    }

    // Validate site_parameter exists and get its site_id + parameter_id
    let sp = site_parameters::Entity::find_by_id(payload.site_parameter_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Site parameter not found".to_string()))?;

    let now = Utc::now();

    // Create/reuse sensor for this stream
    let sensor_ctx =
        create_sensor_for_stream(db, &stream, sp.parameter_id, sp.site_id).await?;

    // Update stream: set pairing
    // Re-fetch stream since create_sensor_for_stream may have updated sensor_id
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to re-fetch stream".to_string()))?;
    let stream_measurement_type = stream.measurement_type.clone();
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(Some(payload.site_parameter_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // The backfill runs in one transaction with decompression unlimited, so a stream whose history
    // reaches into compressed chunks can still be attributed.
    let txn = db.begin().await?;
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

    // Backfill: update readings with site_id + parameter_id + sensor context, apply identity
    // calibration, and adopt the stream's declared classification for its history. A per-reading
    // measurement_type set at ingest outranks the stream declaration and must survive pairing.
    let result = txn
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings
              SET site_id = $1, parameter_id = $2,
                  sensor_id = $4, calibration_id = $5, deployment_id = $6,
                  calibrated_value = COALESCE(calibrated_value, raw_value),
                  measurement_type = COALESCE(measurement_type, $7)
              WHERE stream_id = $3 AND site_id IS NULL",
            [
                sp.site_id.into(),
                sp.parameter_id.into(),
                stream_id.into(),
                sensor_ctx.sensor_id.into(),
                sensor_ctx.calibration_id.into(),
                sensor_ctx.deployment_id.into(),
                stream_measurement_type.into(),
            ],
        ))
        .await?;

    let backfilled = result.rows_affected();

    crate::routes::private::readings::replicates::densify_stream_replicates(&txn, &[stream_id])
        .await?;

    // Replicate groups (2+ readings sharing a timestamp, e.g. migrated NOMIS A/B/C rows) form
    // samples at pairing time: find-or-create the samples row per group, then stamp sample_id on
    // the group's readings. The row-level triggers populate the sample statistics.
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO samples (site_id, parameter_id, collected_at)
          SELECT site_id, parameter_id, time FROM readings
          WHERE stream_id = $1 AND sample_id IS NULL AND site_id IS NOT NULL
          GROUP BY site_id, parameter_id, time
          HAVING COUNT(*) >= 2
          ON CONFLICT (site_id, parameter_id, collected_at) DO NOTHING",
        [stream_id.into()],
    ))
    .await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET sample_id = s.id
          FROM (
              SELECT stream_id, time FROM readings
              WHERE stream_id = $1 AND sample_id IS NULL AND site_id IS NOT NULL
              GROUP BY stream_id, time
              HAVING COUNT(*) >= 2
          ) g, samples s
          WHERE r.stream_id = g.stream_id AND r.time = g.time AND r.sample_id IS NULL
            AND s.site_id = r.site_id AND s.parameter_id = r.parameter_id
            AND s.collected_at = r.time",
        [stream_id.into()],
    ))
    .await?;

    // Also backfill status_events
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events
          SET site_id = $1, parameter_id = $2, sensor_id = $4
          WHERE stream_id = $3 AND site_id IS NULL",
        [
            sp.site_id.into(),
            sp.parameter_id.into(),
            stream_id.into(),
            sensor_ctx.sensor_id.into(),
        ],
    ))
    .await?;

    txn.commit().await?;

    // Window-reprocess the slot in the background (tracked): re-attributes the backfilled readings
    // to whichever sensor's deployment window covers each time, so pairing a stream into a slot with
    // a real deployment timeline is attributed by window, not by the single frozen sensor context,
    // and refreshes continuous aggregates + cascades derived params. (For a fresh pair whose
    // auto-deployment starts now, this is effectively the aggregate refresh.)
    if backfilled > 0 {
        let slot_site = sp.site_id;
        let slot_param = sp.parameter_id;
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "pairing_backfill",
            None,
            Some(stream_id),
            &serde_json::json!({ "site_id": slot_site, "parameter_id": slot_param }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // Re-fetch updated stream
    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(PairStreamResponse {
        stream: updated.into(),
        backfilled,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UnpairStreamResponse {
    pub stream: StreamResponse,
    pub cleared: u64,
}

/// Remove pairing from a stream. Clears `site_parameter_id`/`paired_at` on the stream and
/// nulls out `site_id`/`parameter_id`/`sample_id` on all readings (effectively hiding them from
/// continuous aggregates); samples left unreferenced are deleted. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/streams/{id}/unpair",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    responses(
        (status = 200, description = "Stream unpaired, cleared count returned", body = UnpairStreamResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn unpair_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
) -> AppResult<Json<UnpairStreamResponse>> {
    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    if stream.site_parameter_id.is_none() {
        return Err(AppError::BadRequest("Stream is not paired".to_string()));
    }

    // Close sensor deployment if stream has a sensor
    if let Some(sensor_id) = stream.sensor_id
        && let Some(sp_id) = stream.site_parameter_id
        && let Ok(Some(sp)) = site_parameters::Entity::find_by_id(sp_id).one(db).await
    {
        let site_id = sp.site_id;
        match close_sensor_deployment(db, sensor_id, site_id).await {
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = %e,
                sensor_id = %sensor_id,
                site_id = %site_id,
                "Failed to close sensor deployment during unpair"
            ),
        }
    }

    let now = Utc::now();

    // Clear pairing on stream (keep sensor_id, sensor persists)
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(None);
    active.paired_at = Set(None);
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Samples referenced by this stream's readings, so the ones left unreferenced after the
    // clear below can be removed (the trigger also deletes zero-reference samples; this is the
    // explicit backstop).
    let sample_ids: Vec<Uuid> = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT sample_id AS id FROM readings
              WHERE stream_id = $1 AND sample_id IS NOT NULL",
            [stream_id.into()],
        ))
        .await?
        .iter()
        .filter_map(|row| row.try_get::<Uuid>("", "id").ok())
        .collect();

    // Clear site_id/parameter_id/sample_id on readings (keep sensor_id/calibration_id/deployment_id)
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET site_id = NULL, parameter_id = NULL, sample_id = NULL
              WHERE stream_id = $1",
            [stream_id.into()],
        ))
        .await?;

    let cleared = result.rows_affected();

    if !sample_ids.is_empty() {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"DELETE FROM samples s
              WHERE s.id = ANY($1)
                AND NOT EXISTS (SELECT 1 FROM readings r WHERE r.sample_id = s.id)",
            [sample_ids.into()],
        ))
        .await?;
    }

    // Clear on status_events too
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events SET site_id = NULL, parameter_id = NULL
          WHERE stream_id = $1",
        [stream_id.into()],
    ))
    .await?;

    // Refresh aggregates as a tracked job so a failure is visible and rerunnable
    if cleared > 0 {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "refresh_aggregates_full",
            None,
            Some(stream_id),
            &serde_json::json!({ "full": true }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(UnpairStreamResponse {
        stream: updated.into(),
        cleared,
    }))
}

#[derive(Debug, serde::Deserialize, ToSchema)]
pub struct RetagStreamsRequest {
    /// Explicit streams to classify. Combined with `source_system` when both are given.
    #[serde(default)]
    pub stream_ids: Vec<Uuid>,
    /// Classify every stream of a source system (e.g. 'metalp', 'nomis').
    #[serde(default)]
    pub source_system: Option<String>,
    /// 'continuous' | 'spot' | 'derived', or 'declared' to keep each stream's own classification
    /// and align its readings with it (mixed source systems such as cnet).
    pub measurement_type: String,
    /// Also retag the streams' existing readings and refresh aggregates (tracked job).
    #[serde(default)]
    pub retag_existing: bool,
}

#[derive(Debug, serde::Serialize, ToSchema)]
pub struct RetagStreamsResponse {
    pub streams_updated: u64,
    pub measurement_type: String,
    /// The tracked `measurement_retag` job, when `retag_existing` was requested.
    pub job_id: Option<Uuid>,
}

/// Classify data streams' measurement_type in bulk, the sensorless-stream counterpart of
/// `POST /sensors/retag_frequency` (portal imports like metalp/nomis carry no sensor to hang the
/// classification on). Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/streams/retag",
    request_body = RetagStreamsRequest,
    responses(
        (status = 200, description = "Streams reclassified", body = RetagStreamsResponse),
        (status = 400, description = "Invalid measurement_type or empty scope"),
    ),
    tag = "streams"
)]
pub async fn retag_streams(
    State(state): State<AppState>,
    Json(req): Json<RetagStreamsRequest>,
) -> AppResult<Json<RetagStreamsResponse>> {
    if req.stream_ids.is_empty() && req.source_system.is_none() {
        return Err(AppError::BadRequest(
            "provide stream_ids and/or source_system".to_string(),
        ));
    }
    if !matches!(
        req.measurement_type.as_str(),
        "continuous" | "spot" | "derived" | "declared"
    ) {
        return Err(AppError::BadRequest(format!(
            "invalid measurement_type '{}' (expected continuous, spot, derived, or declared)",
            req.measurement_type
        )));
    }

    let streams_updated = if req.measurement_type == "declared" {
        0
    } else {
        state
            .db
            .execute(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE data_streams SET measurement_type = $1, updated_at = now() \
                 WHERE id = ANY($2) OR ($3::text IS NOT NULL AND source_system = $3)",
                [
                    req.measurement_type.clone().into(),
                    req.stream_ids.clone().into(),
                    req.source_system.clone().into(),
                ],
            ))
            .await?
            .rows_affected()
    };

    let job_id = if req.retag_existing {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "measurement_retag",
            None,
            None,
            &serde_json::json!({
                "stream_ids": req.stream_ids,
                "source_system": req.source_system,
                "target": req.measurement_type,
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(RetagStreamsResponse {
        streams_updated,
        measurement_type: req.measurement_type,
        job_id,
    }))
}
