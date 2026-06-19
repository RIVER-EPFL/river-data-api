//! Telegram Bot API client and the alert delivery channel.
//!
//! Plain HTTPS calls against `api.telegram.org` — no bot framework. Phase 1 uses `send_message`;
//! the bot poller (getUpdates) and `send_photo` are added in later phases.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use super::access::{accessible_project_ids, project_allowed};
use super::{DeliveryResult, NotificationChannel, OutgoingMessage, Slot};

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

/// `(linked_keycloak_sub, chat_id)` for every linked, active, telegram-enabled chat that is
/// subscribed to `slot`. A chat with no subscriber/subscription row defaults to enabled + subscribed
/// (so chats linked before opting in still receive alerts). `slot = None` → every enabled chat (a
/// system-wide alert). Subscription precedence: parameter override > site override > project override
/// > default on.
pub async fn slot_recipients(
    db: &DatabaseConnection,
    slot: &Option<Slot>,
) -> Result<Vec<(String, i64)>, String> {
    let rows = match slot {
        Some(s) => {
            db.query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT ti.linked_keycloak_sub AS sub, ti.telegram_chat_id AS chat_id \
                 FROM telegram_identities ti \
                 LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = ti.linked_keycloak_sub \
                 WHERE ti.is_active AND ti.telegram_chat_id IS NOT NULL \
                   AND COALESCE(ns.is_active, true) AND COALESCE(ns.telegram_enabled, true) \
                   AND COALESCE(( \
                     SELECT subq.enabled FROM notification_subscriptions subq \
                     WHERE subq.keycloak_sub = ti.linked_keycloak_sub \
                       AND ( (subq.site_id = $1 AND subq.parameter_id = $2) \
                          OR (subq.site_id = $1 AND subq.parameter_id IS NULL) \
                          OR ($3::uuid IS NOT NULL AND subq.project_id = $3 \
                              AND subq.site_id IS NULL AND subq.parameter_id IS NULL) ) \
                     ORDER BY (subq.parameter_id IS NOT NULL) DESC, (subq.site_id IS NOT NULL) DESC \
                     LIMIT 1 \
                   ), true)",
                [s.site_id.into(), s.parameter_id.into(), s.project_id.into()],
            ))
            .await
        }
        None => {
            db.query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT ti.linked_keycloak_sub AS sub, ti.telegram_chat_id AS chat_id \
                 FROM telegram_identities ti \
                 LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = ti.linked_keycloak_sub \
                 WHERE ti.is_active AND ti.telegram_chat_id IS NOT NULL \
                   AND COALESCE(ns.is_active, true) AND COALESCE(ns.telegram_enabled, true)"
                    .to_string(),
            ))
            .await
        }
    }
    .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let sub: String = row.try_get("", "sub").map_err(|e| e.to_string())?;
        let chat_id: i64 = row.try_get("", "chat_id").map_err(|e| e.to_string())?;
        out.push((sub, chat_id));
    }
    Ok(out)
}

#[async_trait::async_trait]
impl NotificationChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn deliver(&self, db: &DatabaseConnection, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        let recipients = match slot_recipients(db, &msg.slot).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: failed to load recipients");
                return Vec::new();
            }
        };
        // Project-access guard (no-op until project access is role-scoped — see access.rs). Routing
        // fan-out through the same seam as subscription writes keeps the leak guard in one place.
        let project = msg.slot.as_ref().and_then(|s| s.project_id);

        let mut results = Vec::with_capacity(recipients.len());
        for (sub, chat_id) in recipients {
            if let Some(p) = project {
                if !project_allowed(&accessible_project_ids(db, &sub).await, p) {
                    continue;
                }
            }
            let outcome = self.client.send_message(chat_id, &msg.body).await;
            results.push(DeliveryResult {
                recipient: chat_id.to_string(),
                outcome,
            });
        }
        results
    }
}
