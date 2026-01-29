//! Intelligent response caching for data endpoints.
//!
//! This module provides smart caching that adapts to query types:
//!
//! - **Bounded queries** (with end time): Cache indefinitely until TTL expires.
//!   Historical data within a fixed time range won't change.
//!
//! - **Unbounded queries** (no end time): Check for new data on each request.
//!   If new readings exist beyond the cached max_time, invalidate and refresh.
//!
//! # Usage
//!
//! ```text
//! // In your endpoint handler:
//! let cache_key = cache::cache_key("readings", &[&station_id, &start, &end]);
//!
//! // Check cache (pass query_end for bounded queries, None for unbounded)
//! if let Some(cached) = cache::get_cached(&state, &cache_key, &sensor_ids, query.end).await {
//!     return cache::json_response((*cached).to_vec(), true);
//! }
//!
//! // ... compute response ...
//!
//! // Cache and return
//! cache::cache_and_respond(&state, cache_key, &response, actual_end).await
//! ```
//!
//! # Cache Invalidation Strategy
//!
//! | Query Type | Invalidation |
//! |------------|--------------|
//! | Bounded (end specified) | TTL only - data won't change |
//! | Unbounded (no end) | TTL + freshness check via MAX(time) |
//!
//! The freshness check queries `MAX(time)` for the relevant sensors (~1-2ms)
//! and compares against the cached response's max_time. If new data exists,
//! the cache entry is invalidated and fresh data is fetched.

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

/// Build a cache key from a prefix and components.
///
/// Components are joined with `:` separator. Empty components are included
/// to ensure different queries produce different keys.
pub fn cache_key(prefix: &str, components: &[&str]) -> String {
    let mut key = prefix.to_string();
    for c in components {
        key.push(':');
        key.push_str(c);
    }
    key
}

/// Query the latest reading time for given sensor IDs.
///
/// Used for freshness checking on unbounded queries. Returns the MAX(time)
/// across all readings for the specified sensors.
///
/// This query is optimized and typically completes in ~1-2ms.
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

/// Try to get a cached response with intelligent freshness checking.
///
/// # Arguments
///
/// * `state` - Application state containing the cache
/// * `cache_key` - Unique key for this query
/// * `sensor_ids` - Sensor IDs involved (for freshness check)
/// * `query_end` - The query's end time, or None for unbounded queries
///
/// # Freshness Logic
///
/// - **Bounded queries** (`query_end = Some`): Return cached data directly.
///   Historical data within a fixed time range won't change, so no freshness
///   check is needed. Cache expires naturally via TTL.
///
/// - **Unbounded queries** (`query_end = None`): Check if new data exists
///   beyond the cached max_time. If so, invalidate and return None to trigger
///   a fresh fetch.
///
/// # Returns
///
/// - `Some(data)` - Cached response data (cache hit)
/// - `None` - Cache miss or stale (caller should fetch fresh data)
pub async fn get_cached(
    state: &AppState,
    cache_key: &str,
    sensor_ids: &[uuid::Uuid],
    query_end: Option<DateTime<Utc>>,
) -> Option<Arc<Vec<u8>>> {
    let cached = state.response_cache.get(cache_key).await?;

    // Only do freshness check for unbounded queries (no end time specified)
    // Bounded queries asking for historical data won't change
    if query_end.is_none() {
        if let Ok(Some(latest)) = get_latest_time(state, sensor_ids).await {
            if let Some(cached_max) = cached.max_time {
                if latest > cached_max {
                    // New data exists beyond what we cached
                    tracing::debug!(
                        cache_key = %cache_key,
                        cached_max = %cached_max,
                        latest = %latest,
                        "cache_stale"
                    );
                    state.response_cache.invalidate(cache_key).await;
                    return None;
                }
            }
        }
    }

    tracing::debug!(cache_key = %cache_key, "cache_hit");
    Some(cached.data.clone())
}

/// Store a response in cache with metadata for freshness tracking.
///
/// # Arguments
///
/// * `state` - Application state containing the cache
/// * `cache_key` - Unique key for this query
/// * `data` - Serialized response data
/// * `max_time` - The latest timestamp in the response data (for freshness tracking)
pub async fn store_cached(
    state: &AppState,
    cache_key: String,
    data: Vec<u8>,
    max_time: Option<DateTime<Utc>>,
) {
    let size = data.len();
    state
        .response_cache
        .insert(
            cache_key.clone(),
            CachedResponse {
                data: Arc::new(data),
                max_time,
            },
        )
        .await;

    tracing::debug!(
        cache_key = %cache_key,
        size_bytes = size,
        max_time = ?max_time,
        "cache_stored"
    );
}

/// Build a JSON response with X-Cache header indicating hit/miss status.
///
/// # Arguments
///
/// * `data` - JSON bytes to return
/// * `cache_hit` - Whether this was served from cache
///
/// # Headers
///
/// - `Content-Type: application/json`
/// - `X-Cache: HIT` or `X-Cache: MISS`
pub fn json_response(data: Vec<u8>, cache_hit: bool) -> AppResult<Response> {
    let cache_header = if cache_hit { "HIT" } else { "MISS" };
    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .header("X-Cache", HeaderValue::from_static(cache_header))
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Serialize a response, store in cache, and return it.
///
/// This is a convenience function that combines serialization, caching,
/// and response building into a single call.
///
/// # Arguments
///
/// * `state` - Application state containing the cache
/// * `cache_key` - Unique key for this query
/// * `response` - Response struct to serialize
/// * `max_time` - Latest timestamp in response (for freshness tracking)
///
/// # Returns
///
/// HTTP response with `X-Cache: MISS` header (since we just computed it)
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

/// Manually invalidate a cache entry.
///
/// Use this when you know data has changed and want to force a refresh
/// on the next request.
pub async fn invalidate(state: &AppState, cache_key: &str) {
    state.response_cache.invalidate(cache_key).await;
    tracing::debug!(cache_key = %cache_key, "cache_invalidated");
}

/// Invalidate all cache entries matching a prefix.
///
/// Useful for invalidating all cached data for a specific station
/// or data type.
pub async fn invalidate_prefix(state: &AppState, prefix: &str) {
    let prefix_owned = prefix.to_string();
    let _ = state.response_cache.invalidate_entries_if(move |key, _| {
        key.starts_with(&prefix_owned)
    });
    tracing::debug!(prefix = %prefix, "cache_prefix_invalidated");
}
