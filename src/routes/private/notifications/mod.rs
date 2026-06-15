//! Email + Telegram notifications layered on the alarm pipeline.
//!
//! The dispatcher consumes `AlarmStateChanged` broadcasts (and polls the outbox columns as a
//! backstop), renders messages, and fans them out to the enabled channels. Telegram is restricted
//! to linked identities whose Keycloak role is resolved live (`authz`), never from a cached value.

use sea_orm::DatabaseConnection;

pub mod authz;
pub mod bot;
pub mod commands;
pub mod dispatcher;
pub mod email;
pub mod identities_model;
pub mod log_model;
pub mod messages;
pub mod mutes_model;
pub mod reconcile;
pub mod telegram;
pub mod views;

pub use identities_model::TelegramIdentity;
pub use log_model::NotificationLog;
pub use mutes_model::NotificationMute;

/// A rendered notification ready to deliver. `kind` matches `notification_log.kind`.
#[derive(Clone, Debug)]
pub struct OutgoingMessage {
    pub kind: &'static str,
    pub subject: String,
    pub body: String,
}

/// Outcome of one delivery to one recipient, recorded in `notification_log`.
pub struct DeliveryResult {
    pub recipient: String,
    pub outcome: Result<(), String>,
}

/// A delivery channel (Telegram, email). Each channel resolves its own recipients, so the
/// dispatcher only renders the message once and hands it to every enabled channel.
#[async_trait::async_trait]
pub trait NotificationChannel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn deliver(&self, db: &DatabaseConnection, msg: &OutgoingMessage) -> Vec<DeliveryResult>;
}
