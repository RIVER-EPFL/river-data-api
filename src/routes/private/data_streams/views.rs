use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Set, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::bulk_write::{self, TouchedRange};
use crate::common::middleware::ProjectScope;
use crate::common::scope;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors;
use crate::routes::private::sensors::calibrations;
use crate::routes::private::sensors::operations::{
    close_sensor_deployment, create_sensor_for_stream, extract_vaisala_device_serial,
};
use crate::routes::private::{data_streams, sites::parameters as site_parameters};

#[derive(Debug, Serialize, ToSchema)]
pub struct StreamStatsResponse {
    pub stream_id: Uuid,
    pub reading_count: i64,
    /// Rows stamped withdrawn by windowed reconciliation (included in `reading_count`).
    pub withdrawn_count: i64,
    pub min_time: Option<chrono::DateTime<Utc>>,
    pub max_time: Option<chrono::DateTime<Utc>>,
    pub latest_value: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewReplicate {
    pub replicate_index: i16,
    /// The source column this index is pinned to, when the stream declares a replicate spec.
    pub column: Option<String>,
    pub value: Option<f64>,
    pub is_flagged: bool,
    pub withdrawn: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewInstant {
    pub time: chrono::DateTime<Utc>,
    pub replicates: Vec<PreviewReplicate>,
    /// Recomputed here from the served replicates, which is what `samples` holds for a paired
    /// stream. Shown so the review can see the statistics the pairing will produce before it
    /// produces them.
    pub mean: Option<f64>,
    pub sd: Option<f64>,
    pub n: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StreamPreviewResponse {
    pub stream_id: Uuid,
    pub source_key: String,
    pub instants: Vec<PreviewInstant>,
}

/// The most recent instants a stream holds, as the replicate rows they will be served as.
///
/// The routing block tells an operator which column becomes which replicate index; this shows the
/// same thing with the stream's own values in it, which is the difference between reading a
/// mapping and seeing what pairing will do. Readings exist before pairing (an unpaired stream
/// stores its data unattributed), so this works at review time. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/streams/{id}/preview",
    params(
        ("id" = Uuid, Path, description = "Stream UUID"),
        ("limit" = Option<u32>, Query, description = "Instants to return (default 3, max 20)"),
    ),
    responses(
        (status = 200, description = "Recent instants as replicate rows", body = StreamPreviewResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn stream_preview(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> AppResult<Json<StreamPreviewResponse>> {
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
    guard_stream_scope(&state, &stream, &scope).await?;

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(3)
        .clamp(1, 20);

    // The pinned column-to-index mapping, so an index is labelled with the column it came from
    // rather than left as a bare number.
    let columns: std::collections::HashMap<i16, String> =
        super::replicates::ReplicateSpec::from_metadata(&stream.metadata)
            .map(|spec| {
                spec.assignments
                    .into_iter()
                    .map(|a| (a.index, a.column))
                    .collect()
            })
            .unwrap_or_default();

    // The newest `limit` instants, then every replicate at those instants.
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT r.time, r.replicate_index,
                    COALESCE(r.calibrated_value, r.raw_value) AS value,
                    COALESCE(r.is_flagged, false) AS is_flagged,
                    r.withdrawn_at IS NOT NULL AS withdrawn
             FROM readings r
             JOIN (
                 SELECT DISTINCT time FROM readings WHERE stream_id = $1
                 ORDER BY time DESC LIMIT $2
             ) t ON t.time = r.time
             WHERE r.stream_id = $1
             ORDER BY r.time DESC, r.replicate_index",
            [id.into(), limit.into()],
        ))
        .await?;

    let mut instants: Vec<PreviewInstant> = Vec::new();
    for row in &rows {
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        let time = time.with_timezone(&Utc);
        let replicate_index: i16 = row.try_get("", "replicate_index").unwrap_or(0);
        let replicate = PreviewReplicate {
            replicate_index,
            column: columns.get(&replicate_index).cloned(),
            value: row.try_get("", "value").ok(),
            is_flagged: row.try_get("", "is_flagged").unwrap_or(false),
            withdrawn: row.try_get("", "withdrawn").unwrap_or(false),
        };
        match instants.last_mut() {
            Some(last) if last.time == time => last.replicates.push(replicate),
            _ => instants.push(PreviewInstant {
                time,
                replicates: vec![replicate],
                mean: None,
                sd: None,
                n: 0,
            }),
        }
    }

    // Statistics over the replicates that would be served: flagged and withdrawn rows are excluded
    // from `samples`, so excluding them here is what makes the preview match the outcome.
    for instant in &mut instants {
        let values: Vec<f64> = instant
            .replicates
            .iter()
            .filter(|r| !r.is_flagged && !r.withdrawn)
            .filter_map(|r| r.value)
            .collect();
        instant.n = values.len();
        if values.is_empty() {
            continue;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        instant.mean = Some(mean);
        if values.len() > 1 {
            let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>()
                / (values.len() - 1) as f64;
            instant.sd = Some(variance.sqrt());
        }
    }

    Ok(Json(StreamPreviewResponse {
        stream_id: id,
        source_key: stream.source_key,
        instants,
    }))
}

/// A project-scoped key may only inspect a stream paired into its own project. An unpaired or
/// cross-project stream is reported as not-found (rather than 403) so a scoped key cannot even
/// confirm the existence of another project's streams.
async fn guard_stream_scope(
    state: &AppState,
    stream: &data_streams::Model,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<()> {
    if !scope.is_restricted() {
        return Ok(());
    }
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
    Ok(())
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

    guard_stream_scope(&state, &stream, &scope).await?;

    let row = state.db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) as count, COUNT(*) FILTER (WHERE withdrawn_at IS NOT NULL) as withdrawn, MIN(time) as min_time, MAX(time) as max_time FROM readings WHERE stream_id = $1",
            [id.into()],
        ))
        .await?;

    let (count, withdrawn, min_time, max_time) = if let Some(row) = row {
        let count: i64 = row.try_get("", "count").unwrap_or(0);
        let withdrawn: i64 = row.try_get("", "withdrawn").unwrap_or(0);
        let min_time: Option<chrono::DateTime<chrono::FixedOffset>> =
            row.try_get("", "min_time").ok();
        let max_time: Option<chrono::DateTime<chrono::FixedOffset>> =
            row.try_get("", "max_time").ok();
        (
            count,
            withdrawn,
            min_time.map(|t| t.with_timezone(&Utc)),
            max_time.map(|t| t.with_timezone(&Utc)),
        )
    } else {
        (0, 0, None, None)
    };

    // Get latest value
    let latest_row = state
        .db
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
        withdrawn_count: withdrawn,
        min_time,
        max_time,
        latest_value,
    }))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ReceiptsQuery {
    /// 1-based page, default 1.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, default 50, max 200.
    #[serde(default)]
    pub page_size: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptRow {
    pub id: Uuid,
    pub at: chrono::DateTime<Utc>,
    pub window_from: Option<chrono::DateTime<Utc>>,
    pub window_to: Option<chrono::DateTime<Utc>>,
    pub submitted: i32,
    pub new_rows: i32,
    pub changed: i32,
    pub unchanged: i32,
    pub retained: i32,
    pub rejected_total: i32,
    pub dropped: i32,
    pub withdrawn: i32,
    pub braked: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptsResponse {
    pub stream_id: Uuid,
    pub total: u64,
    pub receipts: Vec<ReceiptRow>,
}

/// The stream's windowed-ingest ledger: one row per reconciliation pass, newest first.
/// Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/streams/{id}/receipts",
    params(("id" = Uuid, Path, description = "Stream UUID"), ReceiptsQuery),
    responses(
        (status = 200, description = "Ingest receipts", body = ReceiptsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn stream_receipts(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<ReceiptsQuery>,
) -> AppResult<Json<ReceiptsResponse>> {
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
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

    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let total: i64 = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS n FROM ingest_receipts WHERE stream_id = $1",
            [id.into()],
        ))
        .await?
        .map(|r| r.try_get("", "n").unwrap_or(0))
        .unwrap_or(0);
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, at, window_from, window_to, submitted, new_rows, changed, unchanged, \
                    retained, rejected_total, dropped, withdrawn, braked \
             FROM ingest_receipts WHERE stream_id = $1 \
             ORDER BY at DESC LIMIT $2 OFFSET $3",
            [
                id.into(),
                (page_size as i64).into(),
                (((page - 1) * page_size) as i64).into(),
            ],
        ))
        .await?;
    let mut receipts = Vec::with_capacity(rows.len());
    for r in &rows {
        let fixed = |name: &str| -> Option<chrono::DateTime<Utc>> {
            r.try_get::<Option<chrono::DateTime<chrono::FixedOffset>>>("", name)
                .ok()
                .flatten()
                .map(|t| t.with_timezone(&Utc))
        };
        receipts.push(ReceiptRow {
            id: r.try_get("", "id")?,
            at: r
                .try_get::<chrono::DateTime<chrono::FixedOffset>>("", "at")?
                .with_timezone(&Utc),
            window_from: fixed("window_from"),
            window_to: fixed("window_to"),
            submitted: r.try_get("", "submitted")?,
            new_rows: r.try_get("", "new_rows")?,
            changed: r.try_get("", "changed")?,
            unchanged: r.try_get("", "unchanged")?,
            retained: r.try_get("", "retained")?,
            rejected_total: r.try_get("", "rejected_total")?,
            dropped: r.try_get("", "dropped")?,
            withdrawn: r.try_get("", "withdrawn")?,
            braked: r.try_get("", "braked")?,
        });
    }
    Ok(Json(ReceiptsResponse {
        stream_id: id,
        total: u64::try_from(total).unwrap_or(0),
        receipts,
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
    /// The instrument that produces this feed. Omit when the caller does not know it: the sensor
    /// is then resolved from the metadata serial at import or pairing time. Declaring it is what
    /// stops pairing minting a second, serial-less instrument alongside the real one.
    #[serde(default)]
    pub sensor_id: Option<Uuid>,
    /// Declares this stream a replicate family: each reading carries the replicate index the
    /// stored column-to-index mapping assigns to its source column (a group can be sparse and
    /// need not include index 0), and groups form `samples` rows. Validated here (two or more
    /// unique members, spot classification) and stored under `metadata["replicates"]`. The
    /// mapping is pinned append-only across re-registrations; the response's `replicates` field
    /// is the authoritative mapping to assign indexes from.
    #[serde(default)]
    pub replicates: Option<super::replicates::ReplicateSpec>,
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
    /// The authoritative replicate column-to-index mapping, ordered by index, when this stream
    /// declares a replicate family. Sync services assign each value's `replicate_index` from it;
    /// a retired entry keeps its index reserved for the readings already stored under it.
    pub replicates: Option<Vec<super::replicates::ColumnAssignment>>,
}

impl From<data_streams::Model> for StreamResponse {
    fn from(m: data_streams::Model) -> Self {
        let replicates = super::replicates::ReplicateSpec::from_metadata(&m.metadata)
            .map(|spec| spec.column_assignments());
        Self {
            replicates,
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
    ProjectScope(scope): ProjectScope,
    Json(mut payload): Json<RegisterStreamRequest>,
) -> AppResult<Json<StreamResponse>> {
    if let Some(mt) = payload.measurement_type.as_deref()
        && !matches!(mt, "continuous" | "spot" | "derived")
    {
        return Err(AppError::BadRequest(format!(
            "invalid measurement_type '{mt}' (expected continuous, spot, or derived)"
        )));
    }
    if let Some(spec) = payload.replicates.as_mut() {
        spec.validate(payload.measurement_type.as_deref())?;
        // The stored column-to-index mapping is authoritative and append-only: readings carry
        // their index for life, so a re-registration keeps every known column's index, appends
        // genuinely new columns, and retires absent ones without reusing their indexes. The
        // caller's own `assignments`, if any, are ignored; only the register path authors them.
        let prior = data_streams::Entity::find()
            .filter(data_streams::Column::SourceSystem.eq(&payload.source_system))
            .filter(data_streams::Column::SourceKey.eq(&payload.source_key))
            .one(&state.db)
            .await?
            .and_then(|s| super::replicates::ReplicateSpec::from_metadata(&s.metadata));
        spec.assignments =
            super::replicates::pin_assignments(prior.as_ref(), &spec.source_columns)?;
        spec.embed(&mut payload.metadata)?;
    }
    if let Some(sensor_id) = payload.sensor_id {
        validate_declared_sensor(&state.db, &scope, sensor_id, &payload.metadata).await?;
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
        sensor_id: Set(payload.sensor_id),
        measurement_type: Set(payload.measurement_type.clone()),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
        last_window_digest: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    // Upsert on (source_system, source_key). A byte-identical re-registration must be a no-op
    // write: the sync services re-run discovery every cycle, and rewriting an unchanged row
    // bumps updated_at and churns WAL for nothing.
    let upsert = data_streams::Entity::insert(model)
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
            .action_and_where(sea_orm::sea_query::Expr::cust(
                "(data_streams.source_name, data_streams.source_path, data_streams.metadata)                  IS DISTINCT FROM                  (excluded.source_name, excluded.source_path, excluded.metadata)",
            ))
            .to_owned(),
        )
        .exec(&state.db)
        .await;
    match upsert {
        Ok(_) | Err(sea_orm::DbErr::RecordNotInserted) => {}
        Err(e) => return Err(e.into()),
    }

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

    // A declared instrument attaches to a feed that has none, which is the discovery case. Moving
    // an already-attached feed to a different instrument changes the attribution of everything it
    // has ever written, so it is refused here and left to the explicit swap and relink paths.
    if let Some(declared) = payload.sensor_id
        && stream.sensor_id != Some(declared)
    {
        if let Some(current) = stream.sensor_id {
            return Err(AppError::Conflict(format!(
                "stream {} already reports instrument {current}; relink it explicitly rather than \
                 on registration",
                stream.id
            )));
        }
        let mut active: data_streams::ActiveModel = stream.clone().into();
        active.sensor_id = Set(Some(declared));
        active.updated_at = Set(now.into());
        stream = active.update(&state.db).await?;
    }

    Ok(Json(stream.into()))
}

/// Confine a caller-declared instrument to one the caller has a relationship to.
///
/// The relationship required of a caller confined to a project set is deployment: the instrument
/// must already be deployed into one of that caller's projects. Inventory that is deployed nowhere
/// belongs to no project, so nothing distinguishes another team's spare instrument from this
/// caller's, and attaching one makes every reading the feed writes resolve that instrument's
/// calibration windows. Wiring an undeployed instrument to its first feed is therefore an
/// unrestricted caller's operation, ie. an administrator or an unscoped sync service.
///
/// Two further conditions hold for every caller: the instrument exists, and it does not contradict
/// the device serial the stream's own metadata carries. That serial check is a cross-check on feeds
/// that describe their device, not the confinement: metadata arrives in the same request, so a
/// caller can always omit it, and the scope guard above is what a restricted caller is held to.
pub async fn validate_declared_sensor(
    db: &DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    sensor_id: Uuid,
    metadata: &serde_json::Value,
) -> AppResult<()> {
    let sensor = sensors::Entity::find_by_id(sensor_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Sensor not found".to_string()))?;

    let project = scope::project_of_sensor(db, sensor_id).await?;
    scope::require_target_in_scope(scope, &project, scope::Unowned::Deny, "instrument")?;

    if let Some(declared_serial) = extract_vaisala_device_serial(metadata)
        && sensor.serial_number.as_deref() != Some(declared_serial.as_str())
    {
        return Err(AppError::BadRequest(format!(
            "stream metadata reports device serial '{declared_serial}', which is not the serial of \
             instrument {sensor_id}"
        )));
    }
    Ok(())
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
/// by (serial, parameter), links it to the stream, and stamps `sensor_id` plus whichever curve
/// covers each reading on the stream's site-less readings (an instrument with no curve leaves them
/// uncorrected; the readings stay un-attributed to any site until an explicit adopt). Idempotent: re-import reuses the same sensor and only fills
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
    //
    // Each reading takes the curve whose window covers its OWN time, not the sensor's newest one,
    // so an import of deep history does not stamp today's curve across all of it.
    let attributed =
        calibrations::resolver::attribute_stream_by_window(db, stream_id, ctx.sensor_id).await?;

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

/// A claim that waited out `lock_timeout` is another request pairing the same stream, which is a
/// conflict the caller can retry, not a server fault.
fn claim_error(e: sea_orm::DbErr) -> AppError {
    let message = e.to_string();
    if message.contains("55P03") || message.contains("lock timeout") {
        return AppError::Conflict("Stream is being paired by another request; retry".to_string());
    }
    AppError::Database(e)
}

/// Pair a stream to a site_parameter. Sets `site_parameter_id`/`paired_at` on the stream,
/// backfills existing unpaired readings with site_id/parameter_id, then re-derives each reading's
/// curve from the window covering its own time and refreshes aggregates, as a tracked job.
/// Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/streams/{id}/pair",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    request_body = PairStreamRequest,
    responses(
        (status = 200, description = "Stream paired, backfill count returned", body = PairStreamResponse),
        (status = 400, description = "Stream already paired"),
        (status = 404, description = "Stream or site parameter not found"),
        (status = 409, description = "Another request is pairing the same stream"),
    ),
    tag = "streams"
)]
pub async fn pair_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
    Json(payload): Json<PairStreamRequest>,
) -> AppResult<Json<PairStreamResponse>> {
    let db = &state.db;
    let now = Utc::now();

    // Claim first, then work, all in one transaction with the decompression cap lifted: the claim
    // is what stops two concurrent pairings of one stream both succeeding, and the transaction is
    // what stops a failed backfill leaving the stream paired with unattributed readings.
    let (sp_site_id, sp_parameter_id, backfilled) = bulk_write::guarded(db, async |txn| {
        // A concurrent claim holds the row lock; wait a few seconds for it rather than either
        // failing instantly or hanging, then re-evaluate the claim predicate against its outcome.
        txn.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SET LOCAL lock_timeout = '5s'".to_owned(),
        ))
        .await?;

        let sp = site_parameters::Entity::find_by_id(payload.site_parameter_id)
            .one(txn)
            .await?
            .ok_or_else(|| AppError::NotFound("Site parameter not found".to_string()))?;

        let claimed = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE data_streams \
                 SET site_parameter_id = $1, paired_at = $2, updated_at = $2 \
                 WHERE id = $3 AND site_parameter_id IS NULL",
                [
                    payload.site_parameter_id.into(),
                    now.into(),
                    stream_id.into(),
                ],
            ))
            .await
            .map_err(claim_error)?
            .rows_affected();

        let stream = data_streams::Entity::find_by_id(stream_id)
            .one(txn)
            .await?
            .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
        if claimed == 0 {
            return Err(AppError::BadRequest(
                "Stream is already paired. Unpair it first.".to_string(),
            ));
        }

        // Create/reuse the sensor, then re-read the stream: it may have gained a sensor_id. A
        // stream carrying no device identity keeps NULL attribution rather than minting one.
        let sensor_ctx =
            create_sensor_for_stream(txn, &stream, sp.parameter_id, sp.site_id).await?;
        let sensor_id = sensor_ctx.as_ref().map(|c| c.sensor_id);
        let deployment_id = sensor_ctx.as_ref().and_then(|c| c.deployment_id);
        let stream = data_streams::Entity::find_by_id(stream_id)
            .one(txn)
            .await?
            .ok_or_else(|| AppError::Internal("Failed to re-fetch stream".to_string()))?;
        let stream_measurement_type = stream.measurement_type.clone();

        // Backfill: update readings with site_id + parameter_id + sensor context, and adopt the
        // stream's declared classification for its history. A per-reading measurement_type set at
        // ingest outranks the stream declaration and must survive pairing.
        //
        // No curve is stamped and no value computed. The sensor context carries the instrument's
        // NEWEST calibration, which is not in general the one covering a given reading's time, nor
        // necessarily one authored for this parameter; applying it across a whole backfilled
        // history would correct every row by whichever curve happens to be latest. Which curve
        // covers a reading is a question the reading's own time answers, and the slot reprocess
        // enqueued post-commit is what asks it, for `calibration_id` and `calibrated_value`
        // together.
        let backfilled = bulk_write::mutation(
            txn,
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE readings
                  SET site_id = $1, parameter_id = $2,
                      sensor_id = $4, deployment_id = $5,
                      measurement_type = COALESCE(measurement_type, $6)
                  WHERE stream_id = $3 AND site_id IS NULL",
                [
                    sp.site_id.into(),
                    sp.parameter_id.into(),
                    stream_id.into(),
                    sensor_id.into(),
                    deployment_id.into(),
                    stream_measurement_type.into(),
                ],
            ),
        )
        .await?
        .rows;

        // Replicate groups on the newly paired stream (2+ spot readings sharing a timestamp, e.g.
        // migrated NOMIS A/B/C rows) form samples at pairing time. The row-level triggers populate
        // the sample statistics.
        crate::routes::private::readings::sample_groups::materialise_backfilled_samples(
            txn,
            "r.stream_id = $1",
            stream_id.into(),
        )
        .await?;

        // Attribution arriving is what makes these spot readings addressable as visits: attach
        // their collection events now, deriving the source from where the stream came from.
        crate::routes::private::collection_events::attach::attach_collection_events(
            txn,
            "r.stream_id = $1",
            vec![stream_id.into()],
            crate::routes::private::collection_events::attach::EventSource::ByStreamOrigin,
        )
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
                sensor_id.into(),
            ],
        ))
        .await?;

        // Audit mismatches recorded while the stream was unpaired become reviewable now that the
        // data serves a slot.
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE replicate_audit_holds SET status = 'pending'
             WHERE stream_id = $1 AND status = 'deferred'",
            [stream_id.into()],
        ))
        .await?;

        Ok((sp.site_id, sp.parameter_id, backfilled))
    })
    .await?;

    // Window-reprocess the slot in the background (tracked): re-attributes the backfilled readings
    // to whichever sensor's deployment window covers each time, so pairing a stream into a slot with
    // a real deployment timeline is attributed by window, not by the single frozen sensor context,
    // resolves each reading's calibration and corrected value from the window covering its own time,
    // and refreshes continuous aggregates + cascades derived params.
    // Gated on the stream holding readings at all, not on the backfill having moved rows: a stream
    // re-paired after an unpair, or one whose readings arrived already attributed, backfills nothing
    // and still needs its window resolved against the slot it now feeds.
    let has_readings = state
        .db
        .query_one(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 AS one FROM readings WHERE stream_id = $1 LIMIT 1",
            [stream_id.into()],
        ))
        .await?
        .is_some();
    if backfilled > 0 || has_readings {
        let slot_site = sp_site_id;
        let slot_param = sp_parameter_id;
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

    let Some(sp_id) = stream.site_parameter_id else {
        return Err(AppError::BadRequest("Stream is not paired".to_string()));
    };

    // Ahead of the teardown, and fatal rather than warn-logged: closing is idempotent (it matches
    // only an open deployment), so a retry after any later failure closes nothing twice.
    if let Some(sensor_id) = stream.sensor_id
        && let Some(sp) = site_parameters::Entity::find_by_id(sp_id).one(db).await?
    {
        close_sensor_deployment(db, sensor_id, sp.site_id).await?;
    }

    let now = Utc::now();

    // Clear pairing on stream (keep sensor_id, sensor persists)
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(None);
    active.paired_at = Set(None);
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Release the stream's rows from the slot: one transaction, cap lifted, rollup rebuild queued
    // as a tracked job. The slot itself survives; only this stream stops feeding it.
    let cleared = retire_slot(db, SlotScope::Stream(stream_id)).await?.rows;

    // Open reviews lose their reviewer along with the slot; they wait as deferred until the
    // stream is paired again.
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE replicate_audit_holds SET status = 'deferred'
         WHERE stream_id = $1 AND status = 'pending'",
        [stream_id.into()],
    ))
    .await?;

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

    // "declared" writes nothing to `data_streams`; it aligns each reading with its own stream's
    // declaration, which for a family stream is already spot.
    let streams_updated = if req.measurement_type == "declared" {
        0
    } else {
        if req.measurement_type != "spot" {
            let families = super::replicates::family_keys_in_streams(
                &state.db,
                &req.stream_ids,
                req.source_system.as_deref(),
            )
            .await?;
            super::replicates::refuse_family_retag(&families, &req.measurement_type)?;
        }

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

// ============================================================================
// The (site, parameter) slot: one declaration, two directions
// ============================================================================

/// What a dying slot does with one table's rows.
#[derive(Debug, Clone, Copy)]
pub enum Release {
    /// The measurement outlives the slot: null these columns and keep the row.
    Unattribute(&'static [&'static str]),
    /// The row only describes a group of readings: delete it once none references it.
    DeleteWhenOrphaned,
    /// Describes the site and the parameter rather than the slot's data, so it outlives the slot.
    Retain,
}

/// A table addressed by the (site, parameter) slot rather than by the stream that wrote its rows.
#[derive(Debug, Clone, Copy)]
pub struct SlotTable {
    pub table: &'static str,
    pub release: Release,
    /// Rows carry a `time` column, so a mutation can report the span it touched.
    pub timed: bool,
    /// Rows feed the continuous aggregates, so a mutation here decides the refresh window.
    pub feeds_rollups: bool,
    /// Column completing a `(site_id, parameter_id, ...)` unique constraint, so moving these rows
    /// onto a surviving slot can collide.
    pub unique_with: Option<&'static str>,
}

/// Every table keyed by `(site_id, parameter_id)`.
///
/// A merge re-points all of them onto the survivor ([`move_slot_rows`]); a slot that dies releases
/// each according to its `release` ([`retire_slot`]). Both directions read this one list, which is
/// what stops a merge stranding rows on a deleted parameter and a slot delete abandoning them.
pub const SLOT_TABLES: [SlotTable; 4] = [
    SlotTable {
        table: "readings",
        release: Release::Unattribute(&["site_id", "parameter_id", "sample_id"]),
        timed: true,
        feeds_rollups: true,
        unique_with: None,
    },
    SlotTable {
        table: "status_events",
        release: Release::Unattribute(&["site_id", "parameter_id"]),
        timed: true,
        feeds_rollups: false,
        unique_with: None,
    },
    SlotTable {
        table: "samples",
        release: Release::DeleteWhenOrphaned,
        timed: false,
        feeds_rollups: false,
        unique_with: Some("collected_at"),
    },
    SlotTable {
        table: "annotations",
        release: Release::Retain,
        timed: false,
        feeds_rollups: false,
        unique_with: None,
    },
];

/// Which rows a slot teardown covers.
#[derive(Debug, Clone, Copy)]
pub enum SlotScope {
    /// One stream's rows. The slot itself survives; this stream stops feeding it (unpair).
    Stream(Uuid),
    /// Everything the slot owns, whatever wrote it, plus the streams pointing at it. The slot is
    /// going away (site_parameter delete).
    SiteParameter(Uuid),
}

/// Which sites a slot move covers.
#[derive(Debug, Clone, Copy)]
pub enum MoveScope {
    /// One site's rows, for a site-level merge.
    Site(Uuid),
    /// Every site carrying the source parameter, for a catalog-level merge.
    EverySite,
}

/// Rows a slot move carried, and the span the moved readings cover.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlotMove {
    pub readings: u64,
    pub status_events: u64,
    /// Feeds the caller's post-commit rollup refresh; the rollups group by `parameter_id`, so both
    /// the source's and the survivor's buckets are recomputed by the same window.
    pub touched: TouchedRange,
}

/// Timestamps at which moving the source's rows onto `target_param` would violate a slot table's
/// unique constraint, at most `LIMIT` of them. Empty when the move is safe.
///
/// Resolving a collision by merging the two rows would rewrite a stored measurement statistic
/// (`samples.mean`/`sd`/`n` are computed over one collection group), so callers refuse instead.
pub async fn slot_move_collisions<C: ConnectionTrait>(
    conn: &C,
    scope: MoveScope,
    source_param: Uuid,
    target_param: Uuid,
) -> AppResult<Vec<String>> {
    let mut collisions = Vec::new();
    for slot in SLOT_TABLES {
        let Some(unique_with) = slot.unique_with else {
            continue;
        };
        let table = slot.table;
        let mut values: Vec<sea_orm::Value> = vec![source_param.into(), target_param.into()];
        let site_filter = match scope {
            MoveScope::EverySite => String::new(),
            MoveScope::Site(site_id) => {
                values.push(site_id.into());
                " AND src.site_id = $3".to_string()
            }
        };
        let sql = format!(
            "SELECT DISTINCT src.{unique_with}::text AS value \
             FROM {table} src JOIN {table} dst \
               ON dst.site_id = src.site_id AND dst.{unique_with} = src.{unique_with} \
              AND dst.parameter_id = $2 \
             WHERE src.parameter_id = $1{site_filter} \
             ORDER BY 1 LIMIT 20"
        );
        for row in conn
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                values,
            ))
            .await?
        {
            let value: String = row.try_get("", "value")?;
            collisions.push(format!("{table} {value}"));
        }
    }
    Ok(collisions)
}

/// Re-point every slot-keyed row from `source_param` onto `target_param`.
///
/// Runs inside the caller's guarded transaction. Call [`slot_move_collisions`] first: a table with
/// a `unique_with` column can refuse the move mid-way otherwise.
pub async fn move_slot_rows<C: ConnectionTrait>(
    conn: &C,
    scope: MoveScope,
    source_param: Uuid,
    target_param: Uuid,
) -> AppResult<SlotMove> {
    // $1 is the target parameter, $2 the source, $3 the site when the scope names one.
    let (predicate, site) = match scope {
        MoveScope::EverySite => ("parameter_id = $2", None),
        MoveScope::Site(site_id) => ("parameter_id = $2 AND site_id = $3", Some(site_id)),
    };
    let mut moved = SlotMove::default();

    for slot in SLOT_TABLES {
        let mut values: Vec<sea_orm::Value> = vec![target_param.into(), source_param.into()];
        if let Some(site_id) = site {
            values.push(site_id.into());
        }
        let sql = format!(
            "UPDATE {} SET parameter_id = $1 WHERE {predicate}",
            slot.table
        );
        let statement =
            Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, values);

        let rows = if slot.timed {
            let touched = bulk_write::mutation(conn, statement).await?;
            if slot.feeds_rollups {
                moved.touched = moved.touched.merge(touched);
            }
            touched.rows
        } else {
            conn.execute(statement).await?.rows_affected()
        };

        match slot.table {
            "readings" => moved.readings = rows,
            "status_events" => moved.status_events = rows,
            _ => {}
        }
    }

    Ok(moved)
}

/// The rows one [`SlotScope`] addresses.
struct RetireTarget {
    /// `WHERE` fragment over the slot tables, binding `$1` (and `$2` for a slot).
    predicate: &'static str,
    values: Vec<sea_orm::Value>,
    /// The slot itself is going away, so the streams pointing at it are unpaired too.
    site_parameter_id: Option<Uuid>,
}

async fn resolve_retire_target<C: ConnectionTrait>(
    conn: &C,
    scope: SlotScope,
) -> AppResult<Option<RetireTarget>> {
    match scope {
        SlotScope::Stream(stream_id) => Ok(Some(RetireTarget {
            predicate: "stream_id = $1",
            values: vec![stream_id.into()],
            site_parameter_id: None,
        })),
        SlotScope::SiteParameter(sp_id) => {
            let Some(row) = conn
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT site_id, parameter_id FROM site_parameters WHERE id = $1",
                    [sp_id.into()],
                ))
                .await?
            else {
                return Ok(None);
            };
            let site_id: Uuid = row.try_get("", "site_id")?;
            let parameter_id: Uuid = row.try_get("", "parameter_id")?;
            Ok(Some(RetireTarget {
                predicate: "site_id = $1 AND parameter_id = $2",
                values: vec![site_id.into(), parameter_id.into()],
                site_parameter_id: Some(sp_id),
            }))
        }
    }
}

/// Release everything a slot owns, in one transaction with the decompression cap lifted, and queue
/// the rollup rebuild that has to follow it.
///
/// This is the whole teardown, in the order that keeps it recoverable: the samples a scope
/// references are collected before the readings lose their `sample_id`, the readings and status
/// events are unattributed rather than deleted (the measurement outlives the slot), the samples
/// nothing references any more are deleted, and a slot that is going away releases the streams
/// pointing at it last. Unpair, and a `site_parameters` delete, are the same operation over
/// different scopes.
///
/// The rollups are rebuilt by a tracked `refresh_aggregates_full` job rather than inline: a
/// teardown can span a stream's whole history, and a refresh that fails then belongs in `/jobs`,
/// where it is visible and rerunnable, not as a 500 on an operation that already committed.
///
/// A slot the scope cannot resolve reports an empty range rather than an error, so retiring a row
/// that is already gone is not a failure.
pub async fn retire_slot(db: &DatabaseConnection, scope: SlotScope) -> AppResult<TouchedRange> {
    let touched = bulk_write::guarded(db, async |txn| {
        let Some(target) = resolve_retire_target(txn, scope).await? else {
            return Ok(TouchedRange::default());
        };
        release_slot_rows(txn, &target).await
    })
    .await?;

    if !touched.is_empty() {
        let trigger_id = match scope {
            SlotScope::Stream(id) | SlotScope::SiteParameter(id) => id,
        };
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "refresh_aggregates_full",
            None,
            Some(trigger_id),
            &serde_json::json!({ "full": true }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }
    Ok(touched)
}

async fn release_slot_rows<C: ConnectionTrait>(
    conn: &C,
    target: &RetireTarget,
) -> AppResult<TouchedRange> {
    let sample_ids = referenced_sample_ids(conn, target).await?;
    let mut touched = TouchedRange::default();

    for slot in SLOT_TABLES {
        match slot.release {
            Release::Retain => {}
            Release::Unattribute(columns) => {
                let assignments = columns
                    .iter()
                    .map(|c| format!("{c} = NULL"))
                    .collect::<Vec<_>>()
                    .join(", ");
                // Narrow to rows that still carry something to release: an UPDATE that rewrites
                // already-null rows maximises what it has to decompress and changes nothing.
                let already = columns
                    .iter()
                    .map(|c| format!("{c} IS NOT NULL"))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let sql = format!(
                    "UPDATE {} SET {assignments} WHERE {} AND ({already})",
                    slot.table, target.predicate
                );
                let statement = Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    &sql,
                    target.values.clone(),
                );
                if slot.timed {
                    let range = bulk_write::mutation(conn, statement).await?;
                    if slot.feeds_rollups {
                        touched = touched.merge(range);
                    }
                } else {
                    conn.execute(statement).await?;
                }
            }
            Release::DeleteWhenOrphaned => {
                if sample_ids.is_empty() {
                    continue;
                }
                conn.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "DELETE FROM {} s WHERE s.id = ANY($1) \
                         AND NOT EXISTS (SELECT 1 FROM readings r WHERE r.sample_id = s.id)",
                        slot.table
                    ),
                    [sample_ids.clone().into()],
                ))
                .await?;
            }
        }
    }

    if let Some(sp_id) = target.site_parameter_id {
        // Load-bearing rather than tidy-up: `data_streams.site_parameter_id` has no ON DELETE
        // clause, so the row cannot be deleted while a stream points at it.
        conn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE data_streams SET site_parameter_id = NULL, paired_at = NULL, updated_at = now() \
             WHERE site_parameter_id = $1",
            [sp_id.into()],
        ))
        .await?;
    }

    Ok(touched)
}

/// Samples the scope's readings point at, read before the readings lose their `sample_id`.
async fn referenced_sample_ids<C: ConnectionTrait>(
    conn: &C,
    target: &RetireTarget,
) -> AppResult<Vec<Uuid>> {
    let sql = format!(
        "SELECT DISTINCT sample_id AS id FROM readings WHERE {} AND sample_id IS NOT NULL",
        target.predicate
    );
    Ok(conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            target.values.clone(),
        ))
        .await?
        .iter()
        .filter_map(|row| row.try_get::<Uuid>("", "id").ok())
        .collect())
}
