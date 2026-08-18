use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::aggregates::{self, Window};
use crate::common::authz::AccessScope;
use crate::common::bulk_write::{self, TouchedRange};
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};

/// Keys per statement. A statement is one OR-chain, and each term carries `time = $n` equality, so
/// chunk exclusion prunes; the bound is on statement size, not on correctness.
const KEYS_PER_STATEMENT: usize = 500;

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReadingKey {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: DateTime<Utc>,
    /// One replicate of a grab group, which scopes the write to spot rows: a sonde reading sharing
    /// the grab's snapped timestamp must not be flagged by a replicate key. Omit to act on every
    /// row at that timestamp.
    #[serde(default)]
    pub replicate_index: Option<i16>,
    /// Restrict the write to one cadence ('continuous' | 'spot' | 'derived'). 'continuous' also
    /// covers legacy NULL-typed rows.
    #[serde(default)]
    pub measurement_type: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FlagReadingsRequest {
    pub readings: Vec<ReadingKey>,
    pub reason: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UnflagReadingsRequest {
    pub readings: Vec<ReadingKey>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FlagReadingsResponse {
    pub updated: u64,
}

/// `SET` clause of the flag write, and the values it binds ahead of the keys.
enum FlagWrite {
    Set(String),
    Clear,
}

impl FlagWrite {
    fn assignment(&self) -> &'static str {
        match self {
            FlagWrite::Set(_) => "is_flagged = TRUE, flag_reason = $1",
            FlagWrite::Clear => "is_flagged = FALSE, flag_reason = NULL",
        }
    }

    /// Values bound before the key placeholders, and therefore the offset the keys start at.
    fn leading_values(&self) -> Vec<sea_orm::Value> {
        match self {
            FlagWrite::Set(reason) => vec![reason.clone().into()],
            FlagWrite::Clear => Vec::new(),
        }
    }
}

/// Flag or unflag an explicit key set, then refresh the rollups over the buckets it changed.
///
/// The whole key set is one transaction with the decompression cap lifted: a partial flag set is a
/// state no reader can interpret, and any key may land in a chunk the compression policy has
/// reached. The refresh runs after the commit because `refresh_continuous_aggregate` is a procedure
/// with its own transaction control.
async fn apply_flags(
    state: &AppState,
    scope: &AccessScope,
    keys: &[ReadingKey],
    write: FlagWrite,
) -> AppResult<u64> {
    if keys.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    let target_sites: Vec<Uuid> = keys.iter().map(|r| r.site_id).collect();
    enforce_project_scope_for_sites(&state.db, scope, &target_sites).await?;

    let touched = bulk_write::guarded(&state.db, async |txn| {
        let mut touched = TouchedRange::default();
        for chunk in keys.chunks(KEYS_PER_STATEMENT) {
            let mut values = write.leading_values();
            let offset = values.len();
            let mut conditions = Vec::with_capacity(chunk.len());

            for (i, key) in chunk.iter().enumerate() {
                let base = offset + i * 5 + 1;
                conditions.push(format!(
                    "(site_id = ${b0} AND parameter_id = ${b1} AND time = ${b2} \
                      AND (${b3}::smallint IS NULL \
                           OR (replicate_index = ${b3} AND measurement_type = 'spot')) \
                      AND (${b4}::text IS NULL OR measurement_type = ${b4} \
                           OR (${b4} = 'continuous' AND measurement_type IS NULL)))",
                    b0 = base,
                    b1 = base + 1,
                    b2 = base + 2,
                    b3 = base + 3,
                    b4 = base + 4
                ));
                values.push(key.site_id.into());
                values.push(key.parameter_id.into());
                values.push(key.time.into());
                values.push(key.replicate_index.into());
                values.push(key.measurement_type.clone().into());
            }

            let sql = format!(
                "UPDATE readings SET {} WHERE {}",
                write.assignment(),
                conditions.join(" OR ")
            );
            let statement = sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                values,
            );
            touched = touched.merge(bulk_write::mutation(txn, statement).await?);
        }
        Ok(touched)
    })
    .await?;

    if let Some(window) = Window::touched(&touched) {
        aggregates::refresh(&state.db, window).await?;
        state.response_cache.invalidate_all();
    }
    Ok(touched.rows)
}

/// Flag or unflag everything in one (site, parameter, time range), then refresh the rollups over
/// the buckets it changed. Same guarantees as [`apply_flags`]; the predicate is a range rather than
/// a key set.
async fn apply_flags_over_range(
    state: &AppState,
    scope: &AccessScope,
    site_id: Uuid,
    parameter_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    write: &FlagWrite,
) -> AppResult<u64> {
    if end_time < start_time {
        return Err(AppError::BadRequest(
            "end_time must be >= start_time".to_string(),
        ));
    }
    enforce_project_scope_for_sites(&state.db, scope, &[site_id]).await?;

    let mut values = write.leading_values();
    let offset = values.len();
    // Unflagging restricts to already-flagged rows so the statement decompresses only what it
    // changes; flagging has no such restriction, an already-flagged row may carry another reason.
    let extra = match write {
        FlagWrite::Set(_) => "",
        FlagWrite::Clear => " AND is_flagged = TRUE",
    };
    let sql = format!(
        "UPDATE readings SET {} \
         WHERE site_id = ${} AND parameter_id = ${} AND time >= ${} AND time <= ${}{}",
        write.assignment(),
        offset + 1,
        offset + 2,
        offset + 3,
        offset + 4,
        extra
    );
    values.push(site_id.into());
    values.push(parameter_id.into());
    values.push(start_time.into());
    values.push(end_time.into());

    let touched = bulk_write::guarded_mutation(
        &state.db,
        sea_orm::Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, values),
    )
    .await?;

    if let Some(window) = Window::touched(&touched) {
        aggregates::refresh(&state.db, window).await?;
        state.response_cache.invalidate_all();
    }
    Ok(touched.rows)
}

/// Flag readings by (site_id, parameter_id, time), optionally one replicate. Requires `write_data`.
#[utoipa::path(
    patch,
    path = "/readings/flag",
    request_body = FlagReadingsRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "Missing readings or reason"),
    ),
    tag = "ingestion"
)]
pub async fn flag_readings(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<FlagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    let updated = apply_flags(
        &state,
        &scope,
        &payload.readings,
        FlagWrite::Set(payload.reason.clone()),
    )
    .await?;

    tracing::info!(updated, reason = %payload.reason, "Flagged readings");
    Ok(Json(FlagReadingsResponse { updated }))
}

/// Unflag a set of previously-flagged readings. Requires `write_data`.
#[utoipa::path(
    patch,
    path = "/readings/unflag",
    request_body = UnflagReadingsRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "No readings specified"),
    ),
    tag = "ingestion"
)]
pub async fn unflag_readings(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<UnflagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let updated = apply_flags(&state, &scope, &payload.readings, FlagWrite::Clear).await?;

    tracing::info!(updated, "Unflagged readings");
    Ok(Json(FlagReadingsResponse { updated }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FlagRangeRequest {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub reason: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UnflagRangeRequest {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
}

/// Flag every reading in a (site_id, parameter_id, time range). Requires `write_data`.
/// Refreshes continuous aggregates for the affected window on success.
#[utoipa::path(
    patch,
    path = "/readings/flag_range",
    request_body = FlagRangeRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "Missing reason or end_time < start_time"),
    ),
    tag = "ingestion"
)]
pub async fn flag_range(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<FlagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    let updated = apply_flags_over_range(
        &state,
        &scope,
        payload.site_id,
        payload.parameter_id,
        payload.start_time,
        payload.end_time,
        &FlagWrite::Set(payload.reason.clone()),
    )
    .await?;

    tracing::info!(
        updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        reason = %payload.reason,
        "Flagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse { updated }))
}

/// Unflag every reading in a (site_id, parameter_id, time range). Requires `write_data`.
/// Refreshes continuous aggregates for the affected window on success.
#[utoipa::path(
    patch,
    path = "/readings/unflag_range",
    request_body = UnflagRangeRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "end_time < start_time"),
    ),
    tag = "ingestion"
)]
pub async fn unflag_range(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<UnflagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let updated = apply_flags_over_range(
        &state,
        &scope,
        payload.site_id,
        payload.parameter_id,
        payload.start_time,
        payload.end_time,
        &FlagWrite::Clear,
    )
    .await?;

    tracing::info!(
        updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        "Unflagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse { updated }))
}
