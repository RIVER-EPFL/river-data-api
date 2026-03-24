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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_client() {
        let client = ControlPlaneClient::new("http://localhost:3000").unwrap();
        assert_eq!(client.session_token(), None);
    }

    #[test]
    fn test_base_url_strips_trailing_slash() {
        let client = ControlPlaneClient::new("http://localhost:3000/").unwrap();
        assert_eq!(client.service_url("/enroll"), "http://localhost:3000/api/service/sync/enroll");
    }

    #[test]
    fn test_service_url_construction() {
        let client = ControlPlaneClient::new("http://api:3000").unwrap();
        assert_eq!(client.service_url("/enroll"), "http://api:3000/api/service/sync/enroll");
        assert_eq!(client.service_url("/heartbeat"), "http://api:3000/api/service/sync/heartbeat");
        assert_eq!(
            client.service_url("/commands/some-uuid"),
            "http://api:3000/api/service/sync/commands/some-uuid"
        );
    }

    #[test]
    fn test_session_token_management() {
        let mut client = ControlPlaneClient::new("http://localhost:3000").unwrap();
        assert_eq!(client.session_token(), None);

        client.set_session_token("tok-123".to_string());
        assert_eq!(client.session_token(), Some("tok-123"));

        client.set_session_token("tok-456".to_string());
        assert_eq!(client.session_token(), Some("tok-456"));
    }
}
