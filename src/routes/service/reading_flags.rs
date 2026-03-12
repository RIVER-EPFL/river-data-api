use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use sea_orm::ConnectionTrait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct ReadingKey {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct FlagReadingsRequest {
    pub readings: Vec<ReadingKey>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct UnflagReadingsRequest {
    pub readings: Vec<ReadingKey>,
}

#[derive(Debug, Serialize)]
pub struct FlagReadingsResponse {
    pub updated: u64,
}

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

    tracing::info!(updated = total_updated, reason = %payload.reason, "Flagged readings");
    Ok(Json(FlagReadingsResponse {
        updated: total_updated,
    }))
}

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

    tracing::info!(updated = total_updated, "Unflagged readings");
    Ok(Json(FlagReadingsResponse {
        updated: total_updated,
    }))
}
