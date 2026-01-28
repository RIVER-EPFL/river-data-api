use chrono::{DateTime, Utc};
use reqwest::Client;
use std::time::Duration;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::vaisala::models::{LocationsDataResponse, LocationsHistoryResponse, LocationsResponse};

pub struct VaisalaClient {
    http_client: Client,
    base_url: String,
    bearer_token: String,
}

impl VaisalaClient {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let http_client = Client::builder()
            .danger_accept_invalid_certs(config.vaisala_skip_tls_verify)
            .timeout(Duration::from_secs(300)) // 5 minutes for large history requests
            .build()
            .expect("Failed to create HTTP client");

        Self {
            http_client,
            base_url: config.vaisala_base_url.clone(),
            bearer_token: config.vaisala_bearer_token.clone(),
        }
    }

    /// Get all locations (zones and sensors) visible to the authenticated user.
    ///
    /// # Errors
    ///
    /// Returns `AppError::VaisalaApi` if the request fails or returns an error status.
    pub async fn get_locations(&self) -> AppResult<LocationsResponse> {
        let url = format!("{}/locations?flatten=true", self.base_url);

        let response = self
            .http_client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| AppError::VaisalaApi(format!("Request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AppError::VaisalaApi("Rate limited (429)".to_string()));
        }

        if !response.status().is_success() {
            return Err(AppError::VaisalaApi(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| AppError::VaisalaApi(format!("Failed to parse response: {e}")))
    }

    /// Get historical readings for specified location IDs.
    ///
    /// # Errors
    ///
    /// Returns `AppError::VaisalaApi` if the request fails or returns an error status.
    pub async fn get_locations_history(
        &self,
        location_ids: &[i32],
        date_from: DateTime<Utc>,
        date_to: Option<DateTime<Utc>>,
    ) -> AppResult<LocationsHistoryResponse> {
        // Format location_ids as array with brackets: [1270,1272,...]
        // Build URL manually to avoid URL-encoding the brackets
        let ids_str = format!(
            "[{}]",
            location_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );

        // Convert dates to epoch timestamps (seconds)
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
            .map_err(|e| AppError::VaisalaApi(format!("Request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AppError::VaisalaApi("Rate limited (429)".to_string()));
        }

        if !response.status().is_success() {
            return Err(AppError::VaisalaApi(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let text = response
            .text()
            .await
            .map_err(|e| AppError::VaisalaApi(format!("Failed to get response text: {e}")))?;

        serde_json::from_str(&text).map_err(|e| {
            tracing::error!(
                error = %e,
                body_preview = %text.chars().take(500).collect::<String>(),
                "Failed to parse locations_history response"
            );
            AppError::VaisalaApi(format!("Failed to parse response: {e}"))
        })
    }

    /// Get current readings and device status for specified location IDs.
    ///
    /// # Errors
    ///
    /// Returns `AppError::VaisalaApi` if the request fails or returns an error status.
    pub async fn get_locations_data(
        &self,
        location_ids: &[i32],
    ) -> AppResult<LocationsDataResponse> {
        // Format location_ids as array with brackets: [1270,1272,...]
        // Build URL manually to avoid URL-encoding the brackets
        let ids_str = format!(
            "[{}]",
            location_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );

        let url = format!(
            "{}/locations_data?location_ids={}",
            self.base_url, ids_str
        );

        let response = self
            .http_client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| AppError::VaisalaApi(format!("Request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AppError::VaisalaApi("Rate limited (429)".to_string()));
        }

        if !response.status().is_success() {
            return Err(AppError::VaisalaApi(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| AppError::VaisalaApi(format!("Failed to parse response: {e}")))
    }
}
