//! Email delivery: a `Mailer` trait with SMTP (lettre) and Microsoft Graph backends, plus the
//! `EmailChannel` that delivers alerts to the configured `ALERT_EMAIL_TO` address.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tokio::sync::Mutex;

use crate::common::AppState;
use crate::config::{Config, EmailBackend};

use super::{DeliveryResult, NotificationChannel, OutgoingMessage};

#[async_trait::async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String>;
    /// Reachability probe with no message sent: an SMTP connection test, or a Graph token fetch.
    async fn check_health(&self) -> Result<String, String>;
}

/// SMTP submission via lettre. Port 25 uses a plaintext relay (e.g. an on-campus mail gateway);
/// any other port uses STARTTLS (e.g. `smtp.office365.com:587`). Credentials are optional, an
/// unauthenticated campus relay needs none.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
}

impl SmtpMailer {
    pub fn new(
        host: &str,
        port: u16,
        username: Option<&str>,
        password: Option<&str>,
        from: String,
    ) -> Result<Self, String> {
        let mut builder = if port == 25 {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| e.to_string())?
        }
        .port(port);
        if let (Some(user), Some(pass)) = (username, password) {
            builder = builder.credentials(Credentials::new(user.to_string(), pass.to_string()));
        }
        Ok(Self {
            transport: builder.build(),
            from,
        })
    }
}

#[async_trait::async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String> {
        let email = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|e: lettre::address::AddressError| e.to_string())?,
            )
            .to(to
                .parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| e.to_string())?;
        self.transport
            .send(email)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn check_health(&self) -> Result<String, String> {
        match self.transport.test_connection().await {
            Ok(true) => Ok("SMTP connection OK".to_string()),
            Ok(false) => Err("SMTP server did not accept the connection".to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Microsoft Graph `sendMail` via client-credentials. Caches the app token until shortly before
/// expiry. The sender mailbox must grant the app `Mail.Send` (scoped via an application access
/// policy).
pub struct GraphMailer {
    http: reqwest::Client,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    sender: String,
    token_cache: Arc<Mutex<Option<(String, Instant)>>>,
}

impl GraphMailer {
    #[must_use]
    pub fn new(
        tenant_id: String,
        client_id: String,
        client_secret: String,
        sender: String,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            http,
            tenant_id,
            client_id,
            client_secret,
            sender,
            token_cache: Arc::new(Mutex::new(None)),
        }
    }

    async fn access_token(&self) -> Result<String, String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((token, expiry)) = cache.as_ref() {
            if *expiry > Instant::now() {
                return Ok(token.clone());
            }
        }
        let url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", "https://graph.microsoft.com/.default"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("graph token {status}: {body}"));
        }
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let token = json["access_token"]
            .as_str()
            .ok_or("graph token: missing access_token")?
            .to_string();
        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);
        // Refresh a minute early to avoid sending with a just-expired token.
        let ttl = Duration::from_secs(expires_in.saturating_sub(60).max(30));
        *cache = Some((token.clone(), Instant::now() + ttl));
        Ok(token)
    }
}

#[async_trait::async_trait]
impl Mailer for GraphMailer {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String> {
        let token = self.access_token().await?;
        let url = format!(
            "https://graph.microsoft.com/v1.0/users/{}/sendMail",
            self.sender
        );
        let payload = serde_json::json!({
            "message": {
                "subject": subject,
                "body": { "contentType": "Text", "content": body },
                "toRecipients": [ { "emailAddress": { "address": to } } ]
            },
            "saveToSentItems": false
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!("graph sendMail {status}: {body}"))
        }
    }

    async fn check_health(&self) -> Result<String, String> {
        self.access_token()
            .await
            .map(|_| "Graph token acquired".to_string())
    }
}

/// Build the configured mailer, or `None` if email is disabled / not fully configured (logs why).
pub fn build_mailer(config: &Config) -> Option<Box<dyn Mailer>> {
    match config.email_backend {
        EmailBackend::Disabled => None,
        EmailBackend::Smtp => {
            let (Some(host), Some(from)) = (config.smtp_host.as_ref(), config.smtp_from.as_ref())
            else {
                tracing::warn!(
                    "EMAIL_BACKEND=smtp but SMTP_HOST/SMTP_FROM not set, email disabled"
                );
                return None;
            };
            match SmtpMailer::new(
                host,
                config.smtp_port,
                config.smtp_username.as_deref(),
                config.smtp_password.as_deref(),
                from.clone(),
            ) {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to build SMTP mailer, email disabled");
                    None
                }
            }
        }
        EmailBackend::Graph => {
            let (Some(tenant), Some(client_id), Some(secret), Some(sender)) = (
                config.graph_tenant_id.as_ref(),
                config.graph_client_id.as_ref(),
                config.graph_client_secret.as_ref(),
                config.graph_sender.as_ref(),
            ) else {
                tracing::warn!(
                    "EMAIL_BACKEND=graph but GRAPH_TENANT_ID/CLIENT_ID/CLIENT_SECRET/SENDER not all set, email disabled"
                );
                return None;
            };
            Some(Box::new(GraphMailer::new(
                tenant.clone(),
                client_id.clone(),
                secret.clone(),
                sender.clone(),
            )))
        }
    }
}

/// Delivers alerts by email to the single configured recipient (an EPFL group list fans out).
pub struct EmailChannel {
    mailer: Box<dyn Mailer>,
    recipient: String,
}

impl EmailChannel {
    #[must_use]
    pub fn new(mailer: Box<dyn Mailer>, recipient: String) -> Self {
        Self { mailer, recipient }
    }
}

#[async_trait::async_trait]
impl NotificationChannel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn check_health(&self) -> Result<String, String> {
        self.mailer.check_health().await
    }

    async fn deliver(&self, _state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        let outcome = self
            .mailer
            .send(&self.recipient, &msg.subject, &msg.body)
            .await;
        vec![DeliveryResult {
            recipient: self.recipient.clone(),
            outcome,
        }]
    }
}
