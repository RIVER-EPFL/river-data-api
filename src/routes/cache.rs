use axum::{
    http::{header, HeaderValue},
    response::Response,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::Serialize;
use std::sync::Arc;

use crate::common::{AppState, CachedResponse};
use crate::error::{AppError, AppResult};

/// Result of checking the latest data time in the database
#[derive(Debug, FromQueryResult)]
struct MaxTimeRow {
    max_time: Option<DateTime<Utc>>,
}

/// Build a cache key from components
pub fn cache_key(prefix: &str, components: &[&str]) -> String {
    let mut key = prefix.to_string();
    for c in components {
        key.push(':');
        key.push_str(c);
    }
    key
}

/// Check the latest reading time for given sensor IDs
pub async fn get_latest_time(
    state: &AppState,
    sensor_ids: &[uuid::Uuid],
) -> AppResult<Option<DateTime<Utc>>> {
    if sensor_ids.is_empty() {
        return Ok(None);
    }

    let ids_str = sensor_ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(",");

    let sql = format!(
        "SELECT MAX(time) as max_time FROM readings WHERE sensor_id IN ({})",
        ids_str
    );

    let result = state
        .db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await?;

    Ok(result
        .and_then(|row| MaxTimeRow::from_query_result(&row, "").ok())
        .and_then(|r| r.max_time))
}

/// Try to get a cached response, checking freshness against latest data
pub async fn get_cached(
    state: &AppState,
    cache_key: &str,
    sensor_ids: &[uuid::Uuid],
) -> Option<Arc<Vec<u8>>> {
    let cached = state.response_cache.get(cache_key).await?;

    // Quick freshness check: is there newer data than when we cached?
    if let Ok(Some(latest)) = get_latest_time(state, sensor_ids).await {
        if let Some(cached_max) = cached.max_time {
            if latest > cached_max {
                // New data exists, invalidate cache
                tracing::debug!(cache_key = %cache_key, "cache_stale");
                state.response_cache.invalidate(cache_key).await;
                return None;
            }
        }
    }

    tracing::debug!(cache_key = %cache_key, "cache_hit");
    Some(cached.data.clone())
}

/// Store a response in cache with the max time for freshness tracking
pub async fn store_cached(
    state: &AppState,
    cache_key: String,
    data: Vec<u8>,
    max_time: Option<DateTime<Utc>>,
) {
    state
        .response_cache
        .insert(
            cache_key,
            CachedResponse {
                data: Arc::new(data),
                max_time,
            },
        )
        .await;
}

/// Build a cached JSON response with X-Cache header
pub fn json_response(data: Vec<u8>, cache_hit: bool) -> AppResult<Response> {
    let cache_header = if cache_hit { "HIT" } else { "MISS" };
    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .header("X-Cache", HeaderValue::from_static(cache_header))
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Serialize and cache a response, then return it
pub async fn cache_and_respond<T: Serialize>(
    state: &AppState,
    cache_key: String,
    response: &T,
    max_time: Option<DateTime<Utc>>,
) -> AppResult<Response> {
    let json_bytes = serde_json::to_vec(response)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    store_cached(state, cache_key, json_bytes.clone(), max_time).await;

    json_response(json_bytes, false)
}
