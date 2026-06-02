use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

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
        if let Ok(permit) = semaphore.clone().try_acquire_owned() { Ok(Some(permit)) } else {
            tracing::warn!(format = %format, "bulk_request_rejected");
            Err(AppError::ServiceUnavailable(
                "Too many concurrent bulk requests. Please try again later.".to_string(),
            ))
        }
    } else {
        Ok(None)
    }
}

/// Parameter data needed for CSV/NDJSON streaming (simple value-per-param).
pub trait StreamableParam: Send + Sync {
    fn name(&self) -> &str;
    fn parameter_id(&self) -> Option<Uuid> {
        None
    }
    fn value_at(&self, index: usize) -> Option<f64>;
}

/// Aggregate parameter data needed for CSV/NDJSON streaming (avg/min/max/count).
pub trait StreamableAggregateParam: Send + Sync {
    fn name(&self) -> &str;
    fn parameter_id(&self) -> Option<Uuid> {
        None
    }
    fn avg_at(&self, index: usize) -> Option<f64>;
    fn min_at(&self, index: usize) -> Option<f64>;
    fn max_at(&self, index: usize) -> Option<f64>;
    fn count_at(&self, index: usize) -> Option<i64>;
}

/// Format a `DateTime<Utc>` as a time string for streaming output.
/// By default uses RFC 3339 format.
fn format_time_rfc3339(time: &DateTime<Utc>) -> String {
    time.to_rfc3339()
}

/// Build a streaming CSV response from times + parameter data.
pub fn build_csv_response(
    times: &[DateTime<Utc>],
    params: &[impl StreamableParam + Clone + 'static],
) -> AppResult<Response> {
    let formatted: Vec<String> = times.iter().map(format_time_rfc3339).collect();
    build_csv_response_with_times(formatted, params)
}

/// Build a streaming CSV response from pre-formatted time strings + parameter data.
pub fn build_csv_response_with_times(
    times: Vec<String>,
    params: &[impl StreamableParam + Clone + 'static],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let params: Vec<_> = params.to_vec();

    tokio::spawn(async move {
        let include_pid = params.iter().any(|p| p.parameter_id().is_some());

        // Header row
        let mut header = "time".to_string();
        for param in &params {
            header.push(',');
            header.push_str(param.name());
        }
        if include_pid {
            for param in &params {
                header.push_str(&format!(",{}_parameter_id", param.name()));
            }
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        // Data rows
        for (i, time) in times.iter().enumerate() {
            let mut row = time.clone();
            for param in &params {
                row.push(',');
                if let Some(v) = param.value_at(i) {
                    row.push_str(&v.to_string());
                }
            }
            if include_pid {
                for param in &params {
                    row.push(',');
                    if let Some(pid) = param.parameter_id() {
                        row.push_str(&pid.to_string());
                    }
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
    let formatted: Vec<String> = times.iter().map(format_time_rfc3339).collect();
    build_ndjson_response_with_times(formatted, params)
}

/// Build a streaming NDJSON response from pre-formatted time strings + parameter data.
pub fn build_ndjson_response_with_times(
    times: Vec<String>,
    params: &[impl StreamableParam + Clone + 'static],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let params: Vec<_> = params.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time));

            for param in &params {
                let value = param.value_at(i);
                obj.insert(
                    param.name().to_string(),
                    match value {
                        Some(v) => serde_json::json!(v),
                        None => serde_json::Value::Null,
                    },
                );
                if let Some(pid) = param.parameter_id() {
                    obj.insert(
                        format!("{}_parameter_id", param.name()),
                        serde_json::json!(pid.to_string()),
                    );
                }
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

/// Build a streaming CSV response for aggregate data from times + aggregate params.
pub fn build_aggregates_csv_response(
    times: &[DateTime<Utc>],
    params: &[impl StreamableAggregateParam + Clone + 'static],
) -> AppResult<Response> {
    let formatted: Vec<String> = times.iter().map(format_time_rfc3339).collect();
    build_aggregates_csv_response_with_times(formatted, params)
}

/// Build a streaming CSV response for aggregate data from pre-formatted time strings.
pub fn build_aggregates_csv_response_with_times(
    times: Vec<String>,
    params: &[impl StreamableAggregateParam + Clone + 'static],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let params: Vec<_> = params.to_vec();

    tokio::spawn(async move {
        let include_pid = params.iter().any(|p| p.parameter_id().is_some());

        // Header row
        let mut header = "time".to_string();
        for param in &params {
            header.push_str(&format!(
                ",{}_avg,{}_min,{}_max,{}_count",
                param.name(),
                param.name(),
                param.name(),
                param.name()
            ));
        }
        if include_pid {
            for param in &params {
                header.push_str(&format!(",{}_parameter_id", param.name()));
            }
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        // Data rows
        for (i, time) in times.iter().enumerate() {
            let mut row = time.clone();
            for param in &params {
                row.push(',');
                if let Some(v) = param.avg_at(i) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(v) = param.min_at(i) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(v) = param.max_at(i) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(c) = param.count_at(i) {
                    row.push_str(&c.to_string());
                }
            }
            if include_pid {
                for param in &params {
                    row.push(',');
                    if let Some(pid) = param.parameter_id() {
                        row.push_str(&pid.to_string());
                    }
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

/// Build a streaming NDJSON response for aggregate data from times + aggregate params.
pub fn build_aggregates_ndjson_response(
    times: &[DateTime<Utc>],
    params: &[impl StreamableAggregateParam + Clone + 'static],
) -> AppResult<Response> {
    let formatted: Vec<String> = times.iter().map(format_time_rfc3339).collect();
    build_aggregates_ndjson_response_with_times(formatted, params)
}

/// Build a streaming NDJSON response for aggregate data from pre-formatted time strings.
pub fn build_aggregates_ndjson_response_with_times(
    times: Vec<String>,
    params: &[impl StreamableAggregateParam + Clone + 'static],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let params: Vec<_> = params.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time));

            for param in &params {
                let avg = param.avg_at(i);
                let min = param.min_at(i);
                let max = param.max_at(i);
                let count = param.count_at(i).unwrap_or(0);

                obj.insert(
                    format!("{}_avg", param.name()),
                    avg.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(
                    format!("{}_min", param.name()),
                    min.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(
                    format!("{}_max", param.name()),
                    max.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(format!("{}_count", param.name()), serde_json::json!(count));
                if let Some(pid) = param.parameter_id() {
                    obj.insert(
                        format!("{}_parameter_id", param.name()),
                        serde_json::json!(pid.to_string()),
                    );
                }
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
