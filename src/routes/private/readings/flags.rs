use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use sea_orm::ConnectionTrait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReadingKey {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: DateTime<Utc>,
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

/// Flag a set of individual readings by (site_id, parameter_id, time). Requires `write_data`.
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
    Json(payload): Json<FlagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }

    let mut total_updated = 0u64;

    // Process in batches to avoid overly large SQL statements
    for chunk in payload.readings.chunks(500) {
        let mut conditions = Vec::with_capacity(chunk.len());
        let mut values: Vec<sea_orm::Value> = vec![payload.reason.clone().into()];

        for (i, key) in chunk.iter().enumerate() {
            let base = i * 3 + 2; // $1 is reason, so start at $2
            conditions.push(format!(
                "(site_id = ${} AND parameter_id = ${} AND time = ${})",
                base,
                base + 1,
                base + 2
            ));
            values.push(key.site_id.into());
            values.push(key.parameter_id.into());
            values.push(key.time.into());
        }

        let sql = format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = $1 WHERE {}",
            conditions.join(" OR ")
        );

        let result = state
            .db
            .execute(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                values,
            ))
            .await?;

        total_updated += result.rows_affected();
    }

    // Refresh continuous aggregates for the affected time range
    if total_updated > 0 {
        let min_time = payload.readings.iter().map(|r| r.time).min();
        let max_time = payload.readings.iter().map(|r| r.time).max();
        if let (Some(min_t), Some(max_t)) = (min_time, max_time) {
            for view in ["readings_hourly", "readings_daily", "readings_weekly", "readings_monthly"] {
                let sql = format!(
                    "CALL refresh_continuous_aggregate('{}', $1::timestamptz, $2::timestamptz + INTERVAL '1 second')",
                    view
                );
                if let Err(e) = state.db.execute(
                    sea_orm::Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        &sql,
                        vec![min_t.into(), max_t.into()],
                    )
                ).await {
                    tracing::warn!(view, error = %e, "Failed to refresh continuous aggregate after flagging");
                }
            }
        }
    }

    if total_updated > 0 {
        state.response_cache.invalidate_all();
    }

    tracing::info!(updated = total_updated, reason = %payload.reason, "Flagged readings");
    Ok(Json(FlagReadingsResponse {
        updated: total_updated,
    }))
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
    Json(payload): Json<UnflagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }

    let mut total_updated = 0u64;

    for chunk in payload.readings.chunks(500) {
        let mut conditions = Vec::with_capacity(chunk.len());
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(chunk.len() * 3);

        for (i, key) in chunk.iter().enumerate() {
            let base = i * 3 + 1;
            conditions.push(format!(
                "(site_id = ${} AND parameter_id = ${} AND time = ${})",
                base,
                base + 1,
                base + 2
            ));
            values.push(key.site_id.into());
            values.push(key.parameter_id.into());
            values.push(key.time.into());
        }

        let sql = format!(
            "UPDATE readings SET is_flagged = FALSE, flag_reason = NULL WHERE {}",
            conditions.join(" OR ")
        );

        let result = state
            .db
            .execute(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                values,
            ))
            .await?;

        total_updated += result.rows_affected();
    }

    // Refresh continuous aggregates for the affected time range
    if total_updated > 0 {
        let min_time = payload.readings.iter().map(|r| r.time).min();
        let max_time = payload.readings.iter().map(|r| r.time).max();
        if let (Some(min_t), Some(max_t)) = (min_time, max_time) {
            for view in ["readings_hourly", "readings_daily", "readings_weekly", "readings_monthly"] {
                let sql = format!(
                    "CALL refresh_continuous_aggregate('{}', $1::timestamptz, $2::timestamptz + INTERVAL '1 second')",
                    view
                );
                if let Err(e) = state.db.execute(
                    sea_orm::Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        &sql,
                        vec![min_t.into(), max_t.into()],
                    )
                ).await {
                    tracing::warn!(view, error = %e, "Failed to refresh continuous aggregate after unflagging");
                }
            }
        }
    }

    if total_updated > 0 {
        state.response_cache.invalidate_all();
    }

    tracing::info!(updated = total_updated, "Unflagged readings");
    Ok(Json(FlagReadingsResponse {
        updated: total_updated,
    }))
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

async fn refresh_aggregates_for_range(
    state: &AppState,
    min_t: DateTime<Utc>,
    max_t: DateTime<Utc>,
    op: &str,
) {
    for view in ["readings_hourly", "readings_daily", "readings_weekly", "readings_monthly"] {
        let sql = format!(
            "CALL refresh_continuous_aggregate('{}', $1::timestamptz, $2::timestamptz + INTERVAL '1 second')",
            view
        );
        if let Err(e) = state.db.execute(
            sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                vec![min_t.into(), max_t.into()],
            )
        ).await {
            tracing::warn!(view, error = %e, op, "Failed to refresh continuous aggregate");
        }
    }
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
    Json(payload): Json<FlagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    if payload.end_time < payload.start_time {
        return Err(AppError::BadRequest("end_time must be >= start_time".to_string()));
    }

    let result = state
        .db
        .execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE readings SET is_flagged = TRUE, flag_reason = $1
             WHERE site_id = $2 AND parameter_id = $3 AND time >= $4 AND time <= $5",
            vec![
                payload.reason.clone().into(),
                payload.site_id.into(),
                payload.parameter_id.into(),
                payload.start_time.into(),
                payload.end_time.into(),
            ],
        ))
        .await?;
    let total_updated = result.rows_affected();

    if total_updated > 0 {
        refresh_aggregates_for_range(&state, payload.start_time, payload.end_time, "flag_range").await;
        state.response_cache.invalidate_all();
    }

    tracing::info!(
        updated = total_updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        reason = %payload.reason,
        "Flagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse { updated: total_updated }))
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
    Json(payload): Json<UnflagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.end_time < payload.start_time {
        return Err(AppError::BadRequest("end_time must be >= start_time".to_string()));
    }

    let result = state
        .db
        .execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE readings SET is_flagged = FALSE, flag_reason = NULL
             WHERE site_id = $1 AND parameter_id = $2 AND time >= $3 AND time <= $4
               AND is_flagged = TRUE",
            vec![
                payload.site_id.into(),
                payload.parameter_id.into(),
                payload.start_time.into(),
                payload.end_time.into(),
            ],
        ))
        .await?;
    let total_updated = result.rows_affected();

    if total_updated > 0 {
        refresh_aggregates_for_range(&state, payload.start_time, payload.end_time, "unflag_range").await;
        state.response_cache.invalidate_all();
    }

    tracing::info!(
        updated = total_updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        "Unflagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse { updated: total_updated }))
}
