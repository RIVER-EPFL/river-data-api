use reqwest::Client;
use std::time::Duration;
use uuid::Uuid;

use crate::models::{
    CrudListResponse, Parameter, Project, ReadingInput, Site, SiteParameter, SourceMapping,
    SyncState,
};
use crate::vaisala_client::SyncError;

/// HTTP client wrapping calls to /api/service/ endpoints.
pub struct ApiClient {
    http_client: Client,
    base_url: String,
    token: String,
}

impl ApiClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to create API HTTP client");

        Self {
            http_client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/service{}", self.base_url, path)
    }

    // ========================================================================
    // Projects
    // ========================================================================

    pub async fn list_projects(&self) -> Result<Vec<Project>, SyncError> {
        let resp = self
            .http_client
            .get(self.url("/projects"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_projects failed: {e}")))?;
        self.check_response(&resp)?;
        let body: CrudListResponse<Project> = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse projects: {e}")))?;
        Ok(body.data)
    }

    pub async fn create_project(&self, project: &serde_json::Value) -> Result<Project, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/projects"))
            .bearer_auth(&self.token)
            .json(project)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_project failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse project: {e}")))
    }

    // ========================================================================
    // Sites
    // ========================================================================

    #[allow(dead_code)]
    pub async fn list_sites(&self) -> Result<Vec<Site>, SyncError> {
        let resp = self
            .http_client
            .get(self.url("/sites"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_sites failed: {e}")))?;
        self.check_response(&resp)?;
        let body: CrudListResponse<Site> = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sites: {e}")))?;
        Ok(body.data)
    }

    pub async fn create_site(&self, site: &serde_json::Value) -> Result<Site, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/sites"))
            .bearer_auth(&self.token)
            .json(site)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_site failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse site: {e}")))
    }

    // ========================================================================
    // Parameters (global catalog)
    // ========================================================================

    pub async fn list_parameters(&self) -> Result<Vec<Parameter>, SyncError> {
        let resp = self
            .http_client
            .get(self.url("/parameters"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_parameters failed: {e}")))?;
        self.check_response(&resp)?;
        let body: CrudListResponse<Parameter> = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse parameters: {e}")))?;
        Ok(body.data)
    }

    pub async fn create_parameter(
        &self,
        param: &serde_json::Value,
    ) -> Result<Parameter, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/parameters"))
            .bearer_auth(&self.token)
            .json(param)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_parameter failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse parameter: {e}")))
    }

    // ========================================================================
    // Site Parameters
    // ========================================================================

    pub async fn list_site_parameters(&self) -> Result<Vec<SiteParameter>, SyncError> {
        let resp = self
            .http_client
            .get(self.url("/site_parameters"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_site_parameters failed: {e}")))?;
        self.check_response(&resp)?;
        let body: CrudListResponse<SiteParameter> = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse site_parameters: {e}")))?;
        Ok(body.data)
    }

    pub async fn create_site_parameter(
        &self,
        sp: &serde_json::Value,
    ) -> Result<SiteParameter, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/site_parameters"))
            .bearer_auth(&self.token)
            .json(sp)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_site_parameter failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse site_parameter: {e}")))
    }

    // ========================================================================
    // Source Mappings
    // ========================================================================

    pub async fn list_source_mappings(
        &self,
        entity_type: Option<&str>,
    ) -> Result<Vec<SourceMapping>, SyncError> {
        let mut url = self.url("/source_mappings");
        if let Some(et) = entity_type {
            url = format!("{url}?entity_type={et}");
        }
        let resp = self
            .http_client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_source_mappings failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse source_mappings: {e}")))
    }

    pub async fn upsert_source_mapping(
        &self,
        mapping: &serde_json::Value,
    ) -> Result<SourceMapping, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/source_mappings"))
            .bearer_auth(&self.token)
            .json(mapping)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("upsert_source_mapping failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse source_mapping: {e}")))
    }

    // ========================================================================
    // Sync State
    // ========================================================================

    pub async fn list_sync_states(&self) -> Result<Vec<SyncState>, SyncError> {
        let resp = self
            .http_client
            .get(self.url("/sync_states"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_sync_states failed: {e}")))?;
        self.check_response(&resp)?;
        let body: CrudListResponse<SyncState> = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sync_states: {e}")))?;
        Ok(body.data)
    }

    pub async fn update_sync_state(
        &self,
        site_parameter_id: Uuid,
        update: &serde_json::Value,
    ) -> Result<SyncState, SyncError> {
        let resp = self
            .http_client
            .patch(self.url(&format!("/sync_states/{site_parameter_id}")))
            .bearer_auth(&self.token)
            .json(update)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("update_sync_state failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sync_state: {e}")))
    }

    pub async fn create_sync_state(
        &self,
        state: &serde_json::Value,
    ) -> Result<SyncState, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/sync_states"))
            .bearer_auth(&self.token)
            .json(state)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_sync_state failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sync_state: {e}")))
    }

    // ========================================================================
    // Batch Readings
    // ========================================================================

    pub async fn insert_readings_batch(
        &self,
        readings: &[ReadingInput],
    ) -> Result<u64, SyncError> {
        #[derive(serde::Deserialize)]
        struct BatchResponse {
            inserted: u64,
        }

        let body = serde_json::json!({ "readings": readings });
        let resp = self
            .http_client
            .post(self.url("/readings/batch"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("insert_readings_batch failed: {e}")))?;
        self.check_response(&resp)?;
        let result: BatchResponse = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse batch response: {e}")))?;
        Ok(result.inserted)
    }

    // ========================================================================
    // Actions
    // ========================================================================

    pub async fn refresh_aggregates(&self, full: bool) -> Result<(), SyncError> {
        let body = serde_json::json!({ "full": full });
        let resp = self
            .http_client
            .post(self.url("/actions/refresh_aggregates"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("refresh_aggregates failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    pub async fn compute_derived(
        &self,
        site_timestamps: &[(Uuid, Vec<chrono::DateTime<chrono::Utc>>)],
    ) -> Result<(), SyncError> {
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
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("compute_derived failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    // ========================================================================
    // Entity Creation (alarm thresholds, sensors, calibrations, deployments)
    // ========================================================================

    pub async fn create_alarm_threshold(
        &self,
        threshold: &serde_json::Value,
    ) -> Result<serde_json::Value, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/alarm_thresholds"))
            .bearer_auth(&self.token)
            .json(threshold)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_alarm_threshold failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse alarm_threshold: {e}")))
    }

    pub async fn create_sensor(
        &self,
        sensor: &serde_json::Value,
    ) -> Result<serde_json::Value, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/sensors"))
            .bearer_auth(&self.token)
            .json(sensor)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_sensor failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sensor: {e}")))
    }

    pub async fn create_sensor_calibration(
        &self,
        cal: &serde_json::Value,
    ) -> Result<serde_json::Value, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/sensor_calibrations"))
            .bearer_auth(&self.token)
            .json(cal)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_sensor_calibration failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sensor_calibration: {e}")))
    }

    pub async fn create_sensor_deployment(
        &self,
        dep: &serde_json::Value,
    ) -> Result<serde_json::Value, SyncError> {
        let resp = self
            .http_client
            .post(self.url("/sensor_deployments"))
            .bearer_auth(&self.token)
            .json(dep)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("create_sensor_deployment failed: {e}")))?;
        self.check_response(&resp)?;
        resp.json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sensor_deployment: {e}")))
    }

    pub async fn update_last_full_sync(&self) -> Result<(), SyncError> {
        let resp = self
            .http_client
            .post(self.url("/actions/update_last_full_sync"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("update_last_full_sync failed: {e}")))?;
        self.check_response(&resp)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn list_sensors(&self) -> Result<Vec<serde_json::Value>, SyncError> {
        let resp = self
            .http_client
            .get(self.url("/sensors"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Api(format!("list_sensors failed: {e}")))?;
        self.check_response(&resp)?;
        let body: CrudListResponse<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| SyncError::Api(format!("parse sensors: {e}")))?;
        Ok(body.data)
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn check_response(&self, resp: &reqwest::Response) -> Result<(), SyncError> {
        if !resp.status().is_success() {
            return Err(SyncError::Api(format!(
                "HTTP {} from {}",
                resp.status(),
                resp.url()
            )));
        }
        Ok(())
    }
}
