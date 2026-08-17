//! Telegram Bot API client and the alert delivery channel.
//!
//! Plain HTTPS calls against `api.telegram.org`, no bot framework: `getUpdates` for the inbound
//! poller, `sendMessage` for text and `sendPhoto` for rendered charts.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use super::access::{accessible_project_ids, project_allowed};
use super::{DeliveryResult, NotificationChannel, OutgoingMessage, Slot};
use crate::common::AppState;

const API_BASE: &str = "https://api.telegram.org";

/// A parsed inbound update (only the fields the bot uses).
#[derive(Debug, Clone)]
pub struct Update {
    pub update_id: i64,
    pub chat_id: Option<i64>,
    /// Telegram chat type: `private`, `group`, `supergroup`, or `channel`. Write/privileged commands
    /// are refused outside a 1:1 `private` chat, where the sender's identity can't be established.
    pub chat_type: Option<String>,
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
                    chat_type: msg["chat"]["type"].as_str().map(String::from),
                    username: msg["from"]["username"].as_str().map(String::from),
                    text: msg["text"].as_str().map(String::from),
                }
            })
            .collect();
        Ok(updates)
    }

    /// Liveness + token validity check. `getMe` returns the bot's identity when the token is valid.
    pub async fn get_me(&self) -> Result<String, String> {
        let url = format!("{API_BASE}/bot{}/getMe", self.token);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("telegram getMe {status}: {body}"));
        }
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let username = json["result"]["username"].as_str().unwrap_or("?");
        Ok(format!("bot @{username} reachable"))
    }

    /// Send a PNG with a caption.
    ///
    /// `multipart` is already enabled on reqwest for this crate, so the upload needs no new
    /// dependency. Telegram caps a caption at 1024 UTF-16 code units and a photo at 10MB, and
    /// downscales anything wider than ~1280px.
    pub async fn send_photo(
        &self,
        chat_id: i64,
        png: Vec<u8>,
        caption: &str,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/bot{}/sendPhoto", self.token);
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("plot.png")
            .mime_str("image/png")
            .map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", truncate_caption(caption))
            .part("photo", part);
        let resp = self
            .http
            .post(&url)
            .multipart(form)
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


/// Mark a chat as active because an alert reached it.
///
/// Best-effort: a failure here only costs a link some idle headroom, never a delivery.
async fn stamp_delivered(db: &DatabaseConnection, chat_id: i64) {
    let res = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE telegram_identities SET last_verified_at = NOW(), updated_at = NOW() \
             WHERE telegram_chat_id = $1 AND is_active",
            [chat_id.into()],
        ))
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "telegram: failed to stamp delivery activity");
    }
}

/// Telegram counts a caption in UTF-16 code units, capped at 1024. Truncating at 900 *characters*
/// stays under that for any realistic text and cannot split a code point.
fn truncate_caption(s: &str) -> String {
    if s.chars().count() <= 900 {
        return s.to_string();
    }
    s.chars().take(899).collect::<String>() + "\u{2026}"
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


impl TelegramChannel {
    /// A chart of the breaching slot, when `TELEGRAM_ALARM_PLOTS` is on.
    ///
    /// Every failure degrades to `None`: an alert must go out even if the chart cannot be drawn.
    async fn alarm_plot(&self, state: &AppState, msg: &OutgoingMessage) -> Option<Vec<u8>> {
        if !state.config.telegram_alarm_plots {
            return None;
        }
        let slot = msg.slot.as_ref()?;
        let hours = state.config.telegram_alarm_plot_hours.max(1);
        crate::routes::private::notifications::commands::slot_plot_png(
            state,
            slot.site_id,
            slot.parameter_id,
            chrono::Duration::hours(hours),
        )
        .await
    }
}

#[async_trait::async_trait]
impl NotificationChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn check_health(&self) -> Result<String, String> {
        self.client.get_me().await
    }

    async fn deliver(&self, state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        let db = &state.db;
        let recipients = match slot_recipients(db, &msg.slot).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: failed to load recipients");
                return Vec::new();
            }
        };
        // Project-access guard: a member only receives alerts for projects in their grant set.
        // Routing fan-out through the same seam as subscription writes keeps the leak guard in one
        // place. A `None` project (system-wide alert) passes for everyone.
        let project = msg.slot.as_ref().and_then(|s| s.project_id);

        // Rendered once for the whole fan-out rather than per recipient. That is only sound
        // because every recipient reached below has already passed `project_allowed` for THIS
        // slot's project, so one image cannot leak across projects. If a chart ever spans more
        // than one slot, this has to move inside the loop.
        let plot = self.alarm_plot(state, msg).await;

        let mut results = Vec::with_capacity(recipients.len());
        for (sub, chat_id) in recipients {
            if let Some(p) = project
                && !project_allowed(&accessible_project_ids(state, &sub).await, p)
            {
                continue;
            }
            let outcome = self.client.send_message(chat_id, &msg.body).await;
            if outcome.is_ok() {
                // Receiving an alert counts as using the link. Without this, idle expiry would cut
                // off precisely the people who linked once and only ever receive alarms.
                stamp_delivered(db, chat_id).await;
            }
            // The chart is a follow-up message, never a replacement: the alert text is not
            // truncated to a caption, not delayed by rendering, and a failed upload cannot mark
            // the alert undelivered.
            if outcome.is_ok()
                && let Some(png) = plot.as_ref()
                && let Err(e) = self.client.send_photo(chat_id, png.clone(), "").await
            {
                tracing::warn!(error = %e, "telegram: alarm plot upload failed");
            }
            results.push(DeliveryResult {
                recipient: chat_id.to_string(),
                outcome,
            });
        }
        results
    }
}
