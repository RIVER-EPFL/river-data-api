use reqwest::Client;
use std::time::Duration;
use uuid::Uuid;

use crate::models::{
    CommandUpdateRequest, EnrollRequest, EnrollResponse, HeartbeatRequest, HeartbeatResponse,
};

#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error: {status} from {url}: {body}")]
    Api {
        status: u16,
        url: String,
        body: String,
    },
    #[error("Enrollment failed: credentials revoked or invalid")]
    CredentialsRevoked,
}

pub struct ControlPlaneClient {
    http: Client,
    base_url: String,
    session_token: Option<String>,
}

impl ControlPlaneClient {
    pub fn new(base_url: &str) -> Result<Self, reqwest::Error> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            session_token: None,
        })
    }

    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }

    pub fn set_session_token(&mut self, token: String) {
        self.session_token = Some(token);
    }

    fn service_url(&self, path: &str) -> String {
        format!("{}/api/service/sync{}", self.base_url, path)
    }

    pub async fn enroll(
        &mut self,
        client_id: &str,
        client_secret: &str,
        instance_id: &str,
    ) -> Result<EnrollResponse, ControlPlaneError> {
        let req = EnrollRequest {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            instance_id: instance_id.to_string(),
        };

        let resp = self
            .http
            .post(self.service_url("/enroll"))
            .json(&req)
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ControlPlaneError::CredentialsRevoked);
        }
        if !status.is_success() {
            let url = resp.url().to_string();
            let body = resp.text().await.unwrap_or_default();
            return Err(ControlPlaneError::Api {
                status: status.as_u16(),
                url,
                body,
            });
        }

        let enroll_resp: EnrollResponse = resp.json().await?;
        self.session_token = Some(enroll_resp.session_token.clone());
        Ok(enroll_resp)
    }

    pub async fn heartbeat(
        &mut self,
        service_id: Uuid,
        client_secret: &str,
        status: &str,
        current_operation: Option<&str>,
    ) -> Result<HeartbeatResponse, ControlPlaneError> {
        let req = HeartbeatRequest {
            service_id,
            client_secret: client_secret.to_string(),
            status: status.to_string(),
            current_operation: current_operation.map(String::from),
        };

        let mut builder = self.http.post(self.service_url("/heartbeat"));
        if let Some(token) = &self.session_token {
            builder = builder.bearer_auth(token);
        }

        let resp = builder.json(&req).send().await?;

        let http_status = resp.status();
        if http_status == reqwest::StatusCode::UNAUTHORIZED
            || http_status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(ControlPlaneError::CredentialsRevoked);
        }
        if !http_status.is_success() {
            let url = resp.url().to_string();
            let body = resp.text().await.unwrap_or_default();
            return Err(ControlPlaneError::Api {
                status: http_status.as_u16(),
                url,
                body,
            });
        }

        let hb_resp: HeartbeatResponse = resp.json().await?;
        self.session_token = Some(hb_resp.session_token.clone());
        Ok(hb_resp)
    }

    pub async fn update_command(
        &self,
        command_id: Uuid,
        status: &str,
        result: Option<serde_json::Value>,
    ) -> Result<(), ControlPlaneError> {
        let req = CommandUpdateRequest {
            status: status.to_string(),
            result,
        };

        let mut builder = self
            .http
            .patch(self.service_url(&format!("/commands/{command_id}")));
        if let Some(token) = &self.session_token {
            builder = builder.bearer_auth(token);
        }

        let resp = builder.json(&req).send().await?;

        let http_status = resp.status();
        if !http_status.is_success() {
            let url = resp.url().to_string();
            let body = resp.text().await.unwrap_or_default();
            return Err(ControlPlaneError::Api {
                status: http_status.as_u16(),
                url,
                body,
            });
        }

        Ok(())
    }
}
