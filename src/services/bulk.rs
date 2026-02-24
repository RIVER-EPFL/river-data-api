use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{AppError, AppResult};

/// Global semaphore limiting concurrent bulk (CSV/NDJSON) requests.
static BULK_SEMAPHORE: std::sync::LazyLock<Arc<Semaphore>> = std::sync::LazyLock::new(|| {
    let limit = std::env::var("BULK_CONCURRENT_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    Arc::new(Semaphore::new(limit))
});

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
pub fn acquire_bulk_permit(format: &str) -> AppResult<Option<OwnedSemaphorePermit>> {
    if format == "csv" || format == "ndjson" {
        match BULK_SEMAPHORE.clone().try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            Err(_) => {
                tracing::warn!(format = %format, "bulk_request_rejected");
                Err(AppError::ServiceUnavailable(
                    "Too many concurrent bulk requests. Please try again later.".to_string(),
                ))
            }
        }
    } else {
        Ok(None)
    }
}

/// Parameter data needed for CSV/NDJSON streaming.
pub trait StreamableParam: Send + Sync {
    fn name(&self) -> &str;
    fn value_at(&self, index: usize) -> Option<f64>;
}

/// Build a streaming CSV response from times + parameter data.
pub fn build_csv_response(
    times: &[DateTime<Utc>],
    params: &[impl StreamableParam + Clone + 'static],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params: Vec<_> = params.to_vec();

    tokio::spawn(async move {
        // Header row
        let mut header = "time".to_string();
        for param in &params {
            header.push(',');
            header.push_str(param.name());
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        // Data rows
        for (i, time) in times.iter().enumerate() {
            let mut row = time.to_rfc3339();
            for param in &params {
                row.push(',');
                if let Some(v) = param.value_at(i) {
                    row.push_str(&v.to_string());
                }
            }
            row.push('\n');
            if tx.send(Ok(row)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Build a streaming NDJSON response from times + parameter data.
pub fn build_ndjson_response(
    times: &[DateTime<Utc>],
    params: &[impl StreamableParam + Clone + 'static],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params: Vec<_> = params.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time.to_rfc3339()));

            for param in &params {
                let value = param.value_at(i);
                obj.insert(
                    param.name().to_string(),
                    match value {
                        Some(v) => serde_json::json!(v),
                        None => serde_json::Value::Null,
                    },
                );
            }

            let line = format!("{}\n", serde_json::Value::Object(obj));
            if tx.send(Ok(line)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson"),
        )
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))
}
