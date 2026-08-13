use axum::http::header::{self, HeaderMap};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{AppError, AppResult};

/// The default response format for readings/aggregates/status-events/alarms query params.
/// Shared serde `#[serde(default = "crate::common::bulk::default_format")]` helper.
#[must_use]
pub fn default_format() -> String {
    "json".to_string()
}

/// Semaphore limiting concurrent bulk (CSV/NDJSON) requests.
/// Created from the config value at startup via `new_bulk_semaphore`.
#[must_use]
pub fn new_bulk_semaphore(limit: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(limit))
}

/// Type alias for the bulk semaphore shared via `AppState`.
pub type BulkSemaphore = Arc<Semaphore>;

/// Determine response format from query param and Accept header.
pub fn determine_format(query_format: &str, headers: &HeaderMap) -> String {
    if query_format != "json" {
        return query_format.to_lowercase();
    }

    if let Some(accept) = headers.get(header::ACCEPT)
        && let Ok(accept_str) = accept.to_str()
    {
        if accept_str.contains("application/x-ndjson") {
            return "ndjson".to_string();
        }
        if accept_str.contains("text/csv") {
            return "csv".to_string();
        }
    }

    "json".to_string()
}

/// Try to acquire a bulk semaphore permit for CSV/NDJSON requests.
/// Returns None for JSON format, Some(permit) for bulk formats, or error if too many concurrent.
pub fn acquire_bulk_permit(
    format: &str,
    semaphore: &BulkSemaphore,
) -> AppResult<Option<OwnedSemaphorePermit>> {
    if format == "csv" || format == "ndjson" {
        if let Ok(permit) = semaphore.clone().try_acquire_owned() {
            Ok(Some(permit))
        } else {
            tracing::warn!(format = %format, "bulk_request_rejected");
            Err(AppError::ServiceUnavailable(
                "Too many concurrent bulk requests. Please try again later.".to_string(),
            ))
        }
    } else {
        Ok(None)
    }
}
