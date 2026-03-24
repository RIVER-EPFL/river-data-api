use reqwest::Client;
use std::time::Duration;
use uuid::Uuid;

use crate::error::RiverDataClientError;
use crate::models::{DataStream, IngestReading, IngestStatusEvent, RegisterStreamRequest};

/// HTTP client wrapping calls to /api/service/ endpoints.
///
/// Simplified for stream-based architecture: no entity creation methods.
/// Sync services only register streams and ingest data.
pub struct RiverDataClient {
    http_client: Client,
    base_url: String,
    token: std::sync::RwLock<String>,
}

impl RiverDataClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self, reqwest::Error> {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;

        Ok(Self {
            http_client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: std::sync::RwLock::new(token.to_string()),
        })
    }

    /// Update the bearer token (called by the runner on each heartbeat).
    pub fn set_token(&self, token: &str) {
        if let Ok(mut t) = self.token.write() {
            *t = token.to_string();
        }
    }

    fn current_token(&self) -> String {
        self.token.read().map(|t| t.clone()).unwrap_or_default()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/service{}", self.base_url, path)
    }

    // ========================================================================
    // Stream Registration
    // ========================================================================

    /// Register a data stream (upserts on source_system + source_key).
    pub async fn register_stream(
        &self,
        req: &RegisterStreamRequest,
    ) -> Result<DataStream, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/streams/register"))
            .bearer_auth(&self.current_token())
            .json(req)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("register_stream failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse stream: {e}")))
    }

    /// List streams, optionally filtered by source_system and/or paired status.
    pub async fn list_streams(
        &self,
        source_system: Option<&str>,
        is_active: Option<bool>,
    ) -> Result<Vec<DataStream>, RiverDataClientError> {
        let mut url = self.url("/streams");
        let mut params: Vec<String> = Vec::new();
        if let Some(ss) = source_system {
            params.push(format!("source_system={ss}"));
        }
        if let Some(active) = is_active {
            params.push(format!("is_active={active}"));
        }
        if !params.is_empty() {
            url = format!("{url}?{}", params.join("&"));
        }

        let resp = self
            .http_client
            .get(&url)
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_streams failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse streams: {e}")))
    }

    // ========================================================================
    // Data Ingestion
    // ========================================================================

    /// Ingest readings for a stream.
    pub async fn ingest_readings(
        &self,
        stream_id: Uuid,
        readings: &[IngestReading],
    ) -> Result<u64, RiverDataClientError> {
        #[derive(serde::Deserialize)]
        struct IngestResponse {
            inserted: u64,
        }

        let body = serde_json::json!({
            "stream_id": stream_id,
            "readings": readings,
        });
        let resp = self
            .http_client
            .post(self.url("/ingest"))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("ingest_readings failed: {e}")))?;
        self.check_response(&resp)?;
        let result: IngestResponse = resp
            .json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse ingest response: {e}")))?;
        Ok(result.inserted)
    }

    /// Ingest status events for a stream.
    pub async fn ingest_status_events(
        &self,
        stream_id: Uuid,
        events: &[IngestStatusEvent],
    ) -> Result<u64, RiverDataClientError> {
        #[derive(serde::Deserialize)]
        struct IngestResponse {
            inserted: u64,
        }

        let body = serde_json::json!({
            "stream_id": stream_id,
            "events": events,
        });
        let resp = self
            .http_client
            .post(self.url("/ingest/status_events"))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                RiverDataClientError::Api(format!("ingest_status_events failed: {e}"))
            })?;
        self.check_response(&resp)?;
        let result: IngestResponse = resp
            .json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse ingest response: {e}")))?;
        Ok(result.inserted)
    }

    // ========================================================================
    // Actions
    // ========================================================================

    pub async fn refresh_aggregates(&self, full: bool) -> Result<(), RiverDataClientError> {
        let body = serde_json::json!({ "full": full });
        let resp = self
            .http_client
            .post(self.url("/actions/refresh_aggregates"))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("refresh_aggregates failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    pub async fn compute_derived(
        &self,
        site_timestamps: &[(Uuid, Vec<chrono::DateTime<chrono::Utc>>)],
    ) -> Result<(), RiverDataClientError> {
        let entries: Vec<serde_json::Value> = site_timestamps
            .iter()
            .map(|(site_id, timestamps)| {
                serde_json::json!({
                    "site_id": site_id,
                    "timestamps": timestamps,
                })
            })
            .collect();

        let body = serde_json::json!({ "site_timestamps": entries });
        let resp = self
            .http_client
            .post(self.url("/actions/compute_derived"))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("compute_derived failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    // ========================================================================
    // Command Updates (from sync loop, via service API)
    // ========================================================================

    pub async fn update_command(
        &self,
        command_id: Uuid,
        status: &str,
        result: Option<serde_json::Value>,
    ) -> Result<(), RiverDataClientError> {
        let body = serde_json::json!({ "status": status, "result": result });
        let resp = self
            .http_client
            .patch(self.url(&format!("/sync/commands/{command_id}")))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("update_command failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    // ========================================================================
    // Sync Events
    // ========================================================================

    pub async fn create_sync_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<serde_json::Value, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/sync/events"))
            .bearer_auth(&self.current_token())
            .json(event)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_sync_event failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sync_event: {e}")))
    }

    pub async fn update_sync_event(
        &self,
        event_id: Uuid,
        update: &serde_json::Value,
    ) -> Result<(), RiverDataClientError> {
        let resp = self
            .http_client
            .patch(self.url(&format!("/sync/events/{event_id}")))
            .bearer_auth(&self.current_token())
            .json(update)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("update_sync_event failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn check_response(&self, resp: &reqwest::Response) -> Result<(), RiverDataClientError> {
        if !resp.status().is_success() {
            return Err(RiverDataClientError::Api(format!(
                "HTTP {} from {}",
                resp.status(),
                resp.url()
            )));
        }
        Ok(())
    }
}
