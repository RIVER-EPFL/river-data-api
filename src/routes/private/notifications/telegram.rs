//! Telegram Bot API client and the alert delivery channel.
//!
//! Plain HTTPS calls against `api.telegram.org` — no bot framework. Phase 1 uses `send_message`;
//! the bot poller (getUpdates) and `send_photo` are added in later phases.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use super::{DeliveryResult, NotificationChannel, OutgoingMessage};

const API_BASE: &str = "https://api.telegram.org";

/// A parsed inbound update (only the fields the bot uses).
#[derive(Debug, Clone)]
pub struct Update {
    pub update_id: i64,
    pub chat_id: Option<i64>,
    pub username: Option<String>,
    pub text: Option<String>,
}

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    token: String,
}

impl TelegramClient {
    #[must_use]
    pub fn new(token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { http, token }
    }

    /// Long-poll for updates since `offset`. Uses a request timeout above the long-poll timeout so
    /// the held-open connection isn't cut early.
    pub async fn get_updates(&self, offset: i64, timeout_secs: u32) -> Result<Vec<Update>, String> {
        let url = format!("{API_BASE}/bot{}/getUpdates", self.token);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", timeout_secs.to_string()),
            ])
            .timeout(Duration::from_secs(u64::from(timeout_secs) + 10))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("telegram getUpdates {}", resp.status()));
        }
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let results = json["result"].as_array().cloned().unwrap_or_default();
        let updates = results
            .iter()
            .map(|r| {
                let msg = &r["message"];
                Update {
                    update_id: r["update_id"].as_i64().unwrap_or(0),
                    chat_id: msg["chat"]["id"].as_i64(),
                    username: msg["from"]["username"].as_str().map(String::from),
                    text: msg["text"].as_str().map(String::from),
                }
            })
            .collect();
        Ok(updates)
    }

    /// Send a plain-text message to one chat. Returns the Telegram error description on failure.
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), String> {
        let url = format!("{API_BASE}/bot{}/sendMessage", self.token);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!("telegram {status}: {body}"))
        }
    }
}

/// Delivers alerts to every active, alert-subscribed identity that has claimed a chat.
pub struct TelegramChannel {
    client: TelegramClient,
}

impl TelegramChannel {
    #[must_use]
    pub fn new(client: TelegramClient) -> Self {
        Self { client }
    }
}

async fn alert_chat_ids(db: &DatabaseConnection) -> Result<Vec<i64>, String> {
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT telegram_chat_id FROM telegram_identities \
             WHERE is_active AND receive_alerts AND telegram_chat_id IS NOT NULL"
                .to_string(),
        ))
        .await
        .map_err(|e| e.to_string())?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        if let Ok(id) = row.try_get::<i64>("", "telegram_chat_id") {
            ids.push(id);
        }
    }
    Ok(ids)
}

#[async_trait::async_trait]
impl NotificationChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn deliver(&self, db: &DatabaseConnection, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        let chat_ids = match alert_chat_ids(db).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: failed to load recipients");
                return Vec::new();
            }
        };
        let mut results = Vec::with_capacity(chat_ids.len());
        for chat_id in chat_ids {
            let outcome = self.client.send_message(chat_id, &msg.body).await;
            results.push(DeliveryResult {
                recipient: chat_id.to_string(),
                outcome,
            });
        }
        results
    }
}
