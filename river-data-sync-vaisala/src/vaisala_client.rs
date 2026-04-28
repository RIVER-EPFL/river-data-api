use chrono::{DateTime, Utc};
use reqwest::Client;
use std::time::Duration;

use crate::models::{LocationsDataResponse, LocationsHistoryResponse, LocationsResponse};

pub struct VaisalaClient {
    http_client: Client,
    base_url: String,
    bearer_token: String,
}

impl VaisalaClient {
    pub fn new(base_url: &str, bearer_token: &str, skip_tls_verify: bool) -> Result<Self, reqwest::Error> {
        let http_client = Client::builder()
            .danger_accept_invalid_certs(skip_tls_verify)
            .timeout(Duration::from_secs(300))
            .build()?;

        Ok(Self {
            http_client,
            base_url: base_url.to_string(),
            bearer_token: bearer_token.to_string(),
        })
    }

    pub async fn get_locations(&self) -> Result<LocationsResponse, SyncError> {
        let url = format!("{}/locations?flatten=true", self.base_url);
        let response = self
            .http_client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| SyncError::Vaisala(format!("Request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SyncError::Vaisala("Rate limited (429)".to_string()));
        }

        if !response.status().is_success() {
            return Err(SyncError::Vaisala(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| SyncError::Vaisala(format!("Failed to parse response: {e}")))
    }

    pub async fn get_locations_history(
        &self,
        location_ids: &[i32],
        date_from: DateTime<Utc>,
        date_to: Option<DateTime<Utc>>,
    ) -> Result<LocationsHistoryResponse, SyncError> {
        let ids_str = format!(
            "[{}]",
            location_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );

        let date_from_epoch = date_from.timestamp();

        let url = match date_to {
            Some(to) => format!(
                "{}/locations_history?location_ids={}&date_from={}&date_to={}",
                self.base_url, ids_str, date_from_epoch, to.timestamp()
            ),
            None => format!(
                "{}/locations_history?location_ids={}&date_from={}",
                self.base_url, ids_str, date_from_epoch
            ),
        };

        let response = self
            .http_client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| SyncError::Vaisala(format!("Request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SyncError::Vaisala("Rate limited (429)".to_string()));
        }

        if !response.status().is_success() {
            return Err(SyncError::Vaisala(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let text = response
            .text()
            .await
            .map_err(|e| SyncError::Vaisala(format!("Failed to get response text: {e}")))?;

        serde_json::from_str(&text).map_err(|e| {
            tracing::error!(
                error = %e,
                body_preview = %text.chars().take(500).collect::<String>(),
                "Failed to parse locations_history response"
            );
            SyncError::Vaisala(format!("Failed to parse response: {e}"))
        })
    }

    pub async fn get_locations_data(
        &self,
        location_ids: &[i32],
    ) -> Result<LocationsDataResponse, SyncError> {
        let ids_str = format!(
            "[{}]",
            location_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );

        let url = format!("{}/locations_data?location_ids={}", self.base_url, ids_str);

        let response = self
            .http_client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| SyncError::Vaisala(format!("Request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SyncError::Vaisala("Rate limited (429)".to_string()));
        }

        if !response.status().is_success() {
            return Err(SyncError::Vaisala(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| SyncError::Vaisala(format!("Failed to parse response: {e}")))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Vaisala API error: {0}")]
    Vaisala(String),
    #[error(transparent)]
    RiverData(#[from] river_data_sync_common::error::RiverDataClientError),
}
