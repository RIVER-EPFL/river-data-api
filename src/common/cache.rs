//! Response caching for the data endpoints.
//!
//! # What makes an entry stale
//!
//! Writes invalidate; reads do not ask the database whether the stored bytes still hold. Every
//! stored entry carries the site it serves, resolved from its key by [`site_of_key`], so
//! [`invalidate_site`] drops a site's entries in one call across every namespace: the private
//! `readings:{site_id}:…` / `aggregates:{site_id}:…` / `alarms:{site_id}:…` keys and the public
//! `pub_readings:{project_code}:{site_code}:…` / `pub_aggregates:…` keys alike. An entry whose key
//! resolves to no site is dropped by any site invalidation: a missing attribution costs a
//! recomputation, a wrong one would serve stale bytes.
//!
//! Two things reach [`invalidate_site`]:
//!
//! - Writers that call it (through [`invalidate_prefix`] until they are moved over).
//! - [`spawn_write_invalidator`], an in-process subscriber to the `AppEvent` bus the writers
//!   already use for SSE: an `AppEvent::DataIngested` naming a site invalidates that site, so a
//!   writer that announces its write needs no cache call of its own. That announcement is the
//!   contract for anything that changes stored reading values, a reprocess included.
//!
//! # The freshness probe
//!
//! Unbounded queries (no `end`) additionally compare `MAX(time)` for the requested parameters
//! against the cached response's maximum. That is a backstop, not the mechanism: it sees only
//! appends past the cached maximum, so a value corrected in place or a row backfilled below it
//! moves no maximum and is caught by write-side invalidation instead. Bounded queries skip the
//! probe entirely.
//!
//! # Usage
//!
//! ```text
//! // Site first, so the entry can be attributed; see `cache_key::key_for` for the rest.
//! let cache_key = cache_key::key_for(&format!("readings:{}", site.id), &key_struct);
//!
//! if let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, query.end).await {
//!     return cache::json_response((*cached).to_vec(), true);
//! }
//!
//! cache::cache_and_respond(&state, cache_key, &response, actual_end).await
//! ```

use axum::{
    http::{HeaderValue, header},
    response::Response,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use super::state::ResponseCache;
use super::{AppEvent, AppState, CachedResponse};
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
///
/// The first component names the site: its UUID on the private endpoints, the project and site
/// codes on the public ones. [`site_of_key`] reads it back, which is what makes an entry
/// invalidatable.
#[must_use]
pub fn cache_key(prefix: &str, components: &[&str]) -> String {
    let mut key = prefix.to_string();
    for c in components {
        key.push(':');
        key.push_str(c);
    }
    key
}

/// Whether responses are cached at all. Both limits are configurable to zero, and a zero of either
/// kind means every store is thrown away, so the work of preparing one is skipped.
fn caching_enabled(state: &AppState) -> bool {
    state.config.cache_ttl_seconds > 0 && state.config.cache_max_bytes > 0
}

/// How a cache key names its site: by UUID on the private endpoints, by project and site code on
/// the public ones.
#[derive(Debug, PartialEq, Eq)]
enum KeySite<'a> {
    Id(Uuid),
    Codes(&'a str, &'a str),
}

/// Strip the quotes a JSON-rendered string component carries.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// The value of a `name=value` component, or `None` unless exactly one distinct value is present.
///
/// Disagreement means a component's own text spelled a field name, so the key names no site rather
/// than the wrong one.
fn unique_named<'a>(key: &'a str, field: &str) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for component in key.split(':').skip(1) {
        let Some((name, value)) = component.split_once('=') else {
            continue;
        };
        if name != field {
            continue;
        }
        let value = unquote(value);
        match found {
            Some(seen) if seen != value => return None,
            _ => found = Some(value),
        }
    }
    found.filter(|v| !v.is_empty())
}

/// The site a key names, in either spelling: as leading components (`{prefix}:{site_id}:…`,
/// `{prefix}:{project_code}:{site_code}:…`), or as named fields when the key was built from a
/// struct (`site_id="…"`, `project_code="…"` with `site_code="…"`).
fn key_site(key: &str) -> Option<KeySite<'_>> {
    if let Some(value) = unique_named(key, "site_id")
        && let Ok(site_id) = Uuid::parse_str(value)
    {
        return Some(KeySite::Id(site_id));
    }
    if let (Some(project_code), Some(site_code)) = (
        unique_named(key, "project_code"),
        unique_named(key, "site_code"),
    ) {
        return Some(KeySite::Codes(project_code, site_code));
    }

    let mut components = key.split(':').skip(1);
    let first = components.next()?;
    if let Ok(site_id) = Uuid::parse_str(first) {
        return Some(KeySite::Id(site_id));
    }
    let second = components.next()?;
    if first.is_empty() || second.is_empty() || first.contains('=') || second.contains('=') {
        return None;
    }
    Some(KeySite::Codes(first, second))
}

/// The site a cache key serves, or `None` when this process cannot resolve one.
///
/// The code pair resolves through the public config cache the public handlers have already
/// populated by the time they store a response, so the common path costs no query.
pub async fn site_of_key(state: &AppState, key: &str) -> Option<Uuid> {
    let (project_code, site_code) = match key_site(key)? {
        KeySite::Id(site_id) => return Some(site_id),
        KeySite::Codes(project_code, site_code) => (project_code, site_code),
    };

    let config = crate::routes::public::service::get_public_config(
        &state.db,
        &state.public_config_cache,
        project_code,
    )
    .await
    .ok()?;

    config
        .sites
        .iter()
        .find(|s| s.code == site_code)
        .map(|s| s.site_id)
}

/// Query the latest reading time for given parameter IDs.
///
/// Backstop for unbounded queries only, see the module docs. Typically completes in ~1-2ms.
pub async fn get_latest_time(
    state: &AppState,
    param_ids: &[uuid::Uuid],
) -> AppResult<Option<DateTime<Utc>>> {
    if param_ids.is_empty() {
        return Ok(None);
    }

    let placeholders: Vec<String> = (1..=param_ids.len()).map(|i| format!("${i}")).collect();
    let values: Vec<sea_orm::Value> = param_ids.iter().map(|id| (*id).into()).collect();

    let sql = format!(
        "SELECT MAX(time) as max_time FROM readings WHERE parameter_id IN ({})",
        placeholders.join(",")
    );

    let result = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    Ok(result
        .and_then(|row| MaxTimeRow::from_query_result(&row, "").ok())
        .and_then(|r| r.max_time))
}

/// Try to get a cached response.
///
/// # Arguments
///
/// * `state` - Application state containing the cache
/// * `cache_key` - Unique key for this query
/// * `param_ids` - Global parameter IDs involved (for the freshness backstop)
/// * `query_end` - The query's end time, or None for unbounded queries
///
/// A hit means the entry survived both TTL and every invalidation registered since it was stored.
/// Unbounded queries additionally run the `MAX(time)` backstop described in the module docs.
///
/// # Returns
///
/// - `Some(data)` - Cached response data (cache hit)
/// - `None` - Cache miss, invalidated, or stale (caller should fetch fresh data)
pub async fn get_cached(
    state: &AppState,
    cache_key: &str,
    param_ids: &[uuid::Uuid],
    query_end: Option<DateTime<Utc>>,
) -> Option<Arc<Vec<u8>>> {
    let cached = state.response_cache.get(cache_key).await?;

    if query_end.is_none()
        && let Ok(Some(latest)) = get_latest_time(state, param_ids).await
        && let Some(cached_max) = cached.max_time
        && latest > cached_max
    {
        tracing::debug!(
            cache_key = %cache_key,
            cached_max = %cached_max,
            latest = %latest,
            "cache_stale"
        );
        invalidate(state, cache_key).await;
        return None;
    }

    tracing::debug!(cache_key = %cache_key, "cache_hit");
    Some(cached.data.clone())
}

/// Store a response, attributed to the site its key names so a write to that site can drop it.
///
/// # Arguments
///
/// * `state` - Application state containing the cache
/// * `cache_key` - Unique key for this query
/// * `data` - Serialized response data
/// * `max_time` - The latest timestamp in the response data (for the freshness backstop)
pub async fn store_cached(
    state: &AppState,
    cache_key: String,
    data: Vec<u8>,
    max_time: Option<DateTime<Utc>>,
) {
    if !caching_enabled(state) {
        return;
    }

    let site = site_of_key(state, &cache_key).await;
    if site.is_none() {
        // Kept, but only until the next invalidation of any site: see `invalidate_site`.
        tracing::debug!(cache_key = %cache_key, "cache_key_names_no_site");
    }

    let size = data.len();
    state
        .response_cache
        .insert(
            cache_key.clone(),
            CachedResponse {
                data: Arc::new(data),
                max_time,
                site,
            },
        )
        .await;

    tracing::debug!(
        cache_key = %cache_key,
        size_bytes = size,
        max_time = ?max_time,
        site = ?site,
        "cache_stored"
    );
}

/// Build a JSON response with X-Cache header indicating hit/miss status.
///
/// # Headers
///
/// - `Content-Type: application/json`
/// - `X-Cache: HIT` or `X-Cache: MISS`
pub fn json_response(data: Vec<u8>, cache_hit: bool) -> AppResult<Response> {
    let cache_header = if cache_hit { "HIT" } else { "MISS" };
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .header("X-Cache", HeaderValue::from_static(cache_header))
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Serialize a response, store it, and return it with `X-Cache: MISS`.
pub async fn cache_and_respond<T: Serialize>(
    state: &AppState,
    cache_key: String,
    response: &T,
    max_time: Option<DateTime<Utc>>,
) -> AppResult<Response> {
    let json_bytes = serde_json::to_vec(response).map_err(|e| AppError::Internal(e.to_string()))?;

    store_cached(state, cache_key, json_bytes.clone(), max_time).await;

    json_response(json_bytes, false)
}

/// Drop one cached entry by its exact key.
pub async fn invalidate(state: &AppState, cache_key: &str) {
    state.response_cache.invalidate(cache_key).await;
    tracing::debug!(cache_key = %cache_key, "cache_invalidated");
}

/// Drop every cached response that serves `site_id`, in every namespace.
///
/// The entry, not the key, carries the site, so one call reaches the private and the public
/// namespace without either being named here. Entries whose key resolved to no site go too: they
/// cannot be shown to be unaffected.
///
/// Takes the cache rather than the whole `AppState` so the bus subscriber can hold it without
/// holding an event sender, which would keep its own channel open forever.
pub fn invalidate_site(cache: &ResponseCache, site_id: Uuid) {
    match cache.invalidate_entries_if(move |_key, entry| {
        entry.site.is_none_or(|entry_site| entry_site == site_id)
    }) {
        Ok(_) => tracing::debug!(site = %site_id, "cache_site_invalidated"),
        Err(e) => {
            // Reachable only if the cache is rebuilt without `support_invalidation_closures`.
            // Blunt rather than silent: what this module shipped with was a discarded `Err`.
            tracing::warn!(error = %e, site = %site_id, "cache_predicate_unavailable");
            invalidate_all(cache, "site invalidation unavailable");
        }
    }
}

/// Drop every cached response. For writes whose effect genuinely spans sites.
pub fn invalidate_all(cache: &ResponseCache, reason: &str) {
    cache.invalidate_all();
    tracing::debug!(reason = %reason, "cache_all_invalidated");
}

/// Drop a site's cached responses, named by a `{namespace}:{site}` prefix.
///
/// Kept for the writers that still spell invalidation as a pair of namespace prefixes; the
/// namespace half is ignored, because a site's entries are dropped together. Call
/// [`invalidate_site`] directly instead. A prefix that names no resolvable site drops everything,
/// so a mistyped prefix cannot pass silently.
pub async fn invalidate_prefix(state: &AppState, prefix: &str) {
    match site_of_key(state, prefix).await {
        Some(site_id) => invalidate_site(&state.response_cache, site_id),
        None => invalidate_all(
            &state.response_cache,
            &format!("prefix names no site: {prefix}"),
        ),
    }
}

/// Subscribe cache invalidation to the event bus the writers already publish to for SSE.
///
/// In-process only, no database trigger: an `AppEvent::DataIngested` naming a site drops that
/// site's entries, so a writer that announces its write is invalidated without calling this module
/// at all. `DataIngested` is the whole contract, which is why anything that changes stored reading
/// values (a reprocess rewriting `calibrated_value`, a retag) has to announce it the same way.
/// `JobCompleted` is deliberately not a signal: its `readings_updated` counts whatever the job
/// returned, alarm events included, so it says nothing about the served bytes.
///
/// A lagged receiver means writes were missed, so it drops everything rather than guess. Started
/// by `AppState::new` when caching is on; ends when the last event sender is dropped.
pub fn spawn_write_invalidator(state: &AppState) {
    if !caching_enabled(state) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };

    let cache = state.response_cache.clone();
    let mut events = state.events.subscribe();

    handle.spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match events.recv().await {
                Ok(AppEvent::DataIngested {
                    site_id: Some(site_id),
                    ..
                }) => invalidate_site(&cache, site_id),
                Ok(_) => {}
                Err(RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "cache_invalidator_lagged");
                    invalidate_all(&cache, "event bus lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "00000000-0000-4000-a000-000000000010";
    const OTHER_SITE: &str = "00000000-0000-4000-a000-000000000020";

    fn site() -> Uuid {
        Uuid::parse_str(SITE).unwrap()
    }

    #[test]
    fn a_positional_key_names_its_site_uuid() {
        let key = cache_key("readings", &[SITE, "2025-01-01T00:00:00Z", "json"]);
        assert_eq!(key_site(&key), Some(KeySite::Id(site())));
        assert_eq!(
            key_site(&cache_key("aggregates", &[SITE])),
            Some(KeySite::Id(site())),
            "the site alone is still a site"
        );
    }

    #[test]
    fn a_positional_public_key_names_its_project_and_site_codes() {
        let key = cache_key("pub_readings", &["test-river", "upstream", "", "json"]);
        assert_eq!(
            key_site(&key),
            Some(KeySite::Codes("test-river", "upstream"))
        );
    }

    #[test]
    fn a_struct_built_key_names_its_site_by_field() {
        let key = crate::common::cache_key::key_for(
            "readings",
            &serde_json::json!({
                "format": "json",
                "effective_start": "2025-01-01T00:00:00Z",
                "site_id": SITE,
            }),
        );
        assert_eq!(
            key_site(&key),
            Some(KeySite::Id(site())),
            "field order and JSON quoting do not hide the site: {key}"
        );
    }

    #[test]
    fn a_struct_built_public_key_names_its_codes_by_field() {
        let key = crate::common::cache_key::key_for(
            "pub_readings",
            &serde_json::json!({
                "project_code": "test-river",
                "site_code": "upstream",
                "start": "2025-01-01T00:00:00Z",
            }),
        );
        assert_eq!(
            key_site(&key),
            Some(KeySite::Codes("test-river", "upstream"))
        );
    }

    #[test]
    fn a_field_named_twice_with_different_values_names_no_site() {
        let forged = format!("readings:sensor_types=\"x:site_id={OTHER_SITE}\":site_id=\"{SITE}\"");
        assert_eq!(
            key_site(&forged),
            None,
            "a component's own text cannot claim the entry for another site"
        );
    }

    #[test]
    fn a_key_with_no_site_names_nothing() {
        assert_eq!(key_site(""), None);
        assert_eq!(key_site("readings"), None);
        assert_eq!(key_site("pub_readings:test-river"), None);
        assert_eq!(key_site("pub_readings::upstream"), None);
        assert_eq!(key_site("pub_readings:test-river:"), None);
        assert_eq!(key_site("readings:format=\"json\""), None);
    }
}
