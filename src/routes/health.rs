use axum::http::StatusCode;

/// Health check endpoint
///
/// Returns 200 OK if the service is running.
/// This endpoint is not rate-limited and suitable for Kubernetes probes.
#[utoipa::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Service is healthy"),
    ),
    tag = "health"
)]
pub async fn healthz() -> StatusCode {
    StatusCode::OK
}
