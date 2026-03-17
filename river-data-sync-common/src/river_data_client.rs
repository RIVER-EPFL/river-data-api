use reqwest::Client;
use std::time::Duration;
use uuid::Uuid;

use crate::error::RiverDataClientError;
use crate::models::{
    Parameter, Project, ReadingInput, SensorCalibration, SensorDeployment, Site, SiteParameter,
    SourceMapping, StatusEventInput, SyncState,
};

/// HTTP client wrapping calls to /api/service/ endpoints.
pub struct RiverDataClient {
    http_client: Client,
    base_url: String,
    token: std::sync::RwLock<String>,
}

impl RiverDataClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to create API HTTP client");

        Self {
            http_client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: std::sync::RwLock::new(token.to_string()),
        }
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

    /// Build a CrudCrate-compatible list URL with pagination query params.
    /// CrudCrate expects `sort`, `range`, and `filter` query params.
    fn list_url(&self, path: &str) -> String {
        format!(
            "{}/api/service{}?sort=%5B%22id%22%2C%22ASC%22%5D&range=%5B0%2C9999%5D&filter=%7B%7D",
            self.base_url,
            path,
        )
    }

    // ========================================================================
    // Projects
    // ========================================================================

    pub async fn list_projects(&self) -> Result<Vec<Project>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/projects"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_projects failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse projects: {e}")))
    }

    pub async fn create_project(&self, project: &serde_json::Value) -> Result<Project, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/projects"))
            .bearer_auth(&self.current_token())
            .json(project)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_project failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse project: {e}")))
    }

    // ========================================================================
    // Sites
    // ========================================================================

    #[allow(dead_code)]
    pub async fn list_sites(&self) -> Result<Vec<Site>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/sites"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_sites failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sites: {e}")))
    }

    pub async fn create_site(&self, site: &serde_json::Value) -> Result<Site, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/sites"))
            .bearer_auth(&self.current_token())
            .json(site)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_site failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse site: {e}")))
    }

    // ========================================================================
    // Parameters (global catalog)
    // ========================================================================

    pub async fn list_parameters(&self) -> Result<Vec<Parameter>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/parameters"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_parameters failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse parameters: {e}")))
    }

    pub async fn create_parameter(
        &self,
        param: &serde_json::Value,
    ) -> Result<Parameter, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/parameters"))
            .bearer_auth(&self.current_token())
            .json(param)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_parameter failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse parameter: {e}")))
    }

    // ========================================================================
    // Site Parameters
    // ========================================================================

    pub async fn list_site_parameters(&self) -> Result<Vec<SiteParameter>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/site_parameters"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_site_parameters failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse site_parameters: {e}")))
    }

    pub async fn create_site_parameter(
        &self,
        sp: &serde_json::Value,
    ) -> Result<SiteParameter, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/site_parameters"))
            .bearer_auth(&self.current_token())
            .json(sp)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_site_parameter failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse site_parameter: {e}")))
    }

    // ========================================================================
    // Source Mappings
    // ========================================================================

    pub async fn list_source_mappings(
        &self,
        entity_type: Option<&str>,
        source_system: Option<&str>,
    ) -> Result<Vec<SourceMapping>, RiverDataClientError> {
        let mut url = self.url("/source_mappings");
        let mut params: Vec<String> = Vec::new();
        if let Some(et) = entity_type {
            params.push(format!("entity_type={et}"));
        }
        if let Some(ss) = source_system {
            params.push(format!("source_system={ss}"));
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
            .map_err(|e| RiverDataClientError::Api(format!("list_source_mappings failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse source_mappings: {e}")))
    }

    pub async fn upsert_source_mapping(
        &self,
        mapping: &serde_json::Value,
    ) -> Result<SourceMapping, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/source_mappings"))
            .bearer_auth(&self.current_token())
            .json(mapping)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("upsert_source_mapping failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse source_mapping: {e}")))
    }

    // ========================================================================
    // Sync State
    // ========================================================================

    pub async fn list_sync_states(&self) -> Result<Vec<SyncState>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.url("/sync_states"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_sync_states failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sync_states: {e}")))
    }

    pub async fn update_sync_state(
        &self,
        site_parameter_id: Uuid,
        update: &serde_json::Value,
    ) -> Result<SyncState, RiverDataClientError> {
        // CrudCrate PUT requires the primary key in the body
        let mut body = update.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("site_parameter_id".to_string(), serde_json::json!(site_parameter_id));
        }
        let resp = self
            .http_client
            .put(self.url(&format!("/sync_states/{site_parameter_id}")))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("update_sync_state failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sync_state: {e}")))
    }

    pub async fn create_sync_state(
        &self,
        state: &serde_json::Value,
    ) -> Result<SyncState, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/sync_states"))
            .bearer_auth(&self.current_token())
            .json(state)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_sync_state failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sync_state: {e}")))
    }

    // ========================================================================
    // Batch Readings
    // ========================================================================

    pub async fn insert_readings_batch(
        &self,
        readings: &[ReadingInput],
    ) -> Result<u64, RiverDataClientError> {
        #[derive(serde::Deserialize)]
        struct BatchResponse {
            inserted: u64,
        }

        let body = serde_json::json!({ "readings": readings });
        let resp = self
            .http_client
            .post(self.url("/readings/batch"))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("insert_readings_batch failed: {e}")))?;
        self.check_response(&resp)?;
        let result: BatchResponse = resp
            .json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse batch response: {e}")))?;
        Ok(result.inserted)
    }

    // ========================================================================
    // Batch Status Events
    // ========================================================================

    pub async fn insert_status_events_batch(
        &self,
        events: &[StatusEventInput],
    ) -> Result<u64, RiverDataClientError> {
        #[derive(serde::Deserialize)]
        struct BatchResponse {
            inserted: u64,
        }

        let body = serde_json::json!({ "events": events });
        let resp = self
            .http_client
            .post(self.url("/status_events/batch"))
            .bearer_auth(&self.current_token())
            .json(&body)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("insert_status_events_batch failed: {e}")))?;
        self.check_response(&resp)?;
        let result: BatchResponse = resp
            .json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse batch response: {e}")))?;
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
    // Entity Creation (alarm thresholds, sensors, calibrations, deployments)
    // ========================================================================

    pub async fn create_alarm_threshold(
        &self,
        threshold: &serde_json::Value,
    ) -> Result<serde_json::Value, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/alarm_thresholds"))
            .bearer_auth(&self.current_token())
            .json(threshold)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_alarm_threshold failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse alarm_threshold: {e}")))
    }

    pub async fn create_sensor(
        &self,
        sensor: &serde_json::Value,
    ) -> Result<serde_json::Value, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/sensors"))
            .bearer_auth(&self.current_token())
            .json(sensor)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_sensor failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sensor: {e}")))
    }

    pub async fn create_sensor_calibration(
        &self,
        cal: &serde_json::Value,
    ) -> Result<serde_json::Value, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/sensor_calibrations"))
            .bearer_auth(&self.current_token())
            .json(cal)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_sensor_calibration failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sensor_calibration: {e}")))
    }

    pub async fn create_sensor_deployment(
        &self,
        dep: &serde_json::Value,
    ) -> Result<serde_json::Value, RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/sensor_deployments"))
            .bearer_auth(&self.current_token())
            .json(dep)
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("create_sensor_deployment failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sensor_deployment: {e}")))
    }

    pub async fn update_last_full_sync(&self) -> Result<(), RiverDataClientError> {
        let resp = self
            .http_client
            .post(self.url("/actions/update_last_full_sync"))
            .bearer_auth(&self.current_token())
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("update_last_full_sync failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    // ========================================================================
    // Sensor Deployments
    // ========================================================================

    pub async fn list_sensor_deployments(
        &self,
    ) -> Result<Vec<SensorDeployment>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/sensor_deployments"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| {
                RiverDataClientError::Api(format!("list_sensor_deployments failed: {e}"))
            })?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sensor_deployments: {e}")))
    }

    // ========================================================================
    // Sensor Calibrations
    // ========================================================================

    pub async fn list_sensor_calibrations(
        &self,
    ) -> Result<Vec<SensorCalibration>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/sensor_calibrations"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| {
                RiverDataClientError::Api(format!("list_sensor_calibrations failed: {e}"))
            })?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sensor_calibrations: {e}")))
    }

    #[allow(dead_code)]
    pub async fn list_sensors(&self) -> Result<Vec<serde_json::Value>, RiverDataClientError> {
        let resp = self
            .http_client
            .get(self.list_url("/sensors"))
            .bearer_auth(&self.current_token())
            .send()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("list_sensors failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| RiverDataClientError::Api(format!("parse sensors: {e}")))
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
