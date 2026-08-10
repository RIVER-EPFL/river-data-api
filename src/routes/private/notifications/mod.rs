//! Email + Telegram notifications layered on the alarm pipeline.
//!
//! The dispatcher consumes `AlarmStateChanged` broadcasts (and polls the outbox columns as a
//! backstop), renders messages, and fans them out to the enabled channels. Telegram is restricted
//! to linked identities whose Keycloak role is resolved live (`authz`), never from a cached value.

use uuid::Uuid;

use crate::common::AppState;

pub mod access;
pub mod authz;
pub mod bot;
pub mod commands;
pub mod dispatcher;
pub mod email;
pub mod health;
pub mod identities_model;
pub mod log_model;
pub mod me;
pub mod messages;
pub mod mutes_model;
pub mod reconcile;
pub mod telegram;
pub mod triggers;
pub mod views;

pub use identities_model::TelegramIdentity;
pub use log_model::NotificationLog;
pub use mutes_model::NotificationMute;

/// The (project, site, parameter) an alert belongs to. Used to fan out only to subscribers who opted
/// in to that scope. `None` on a message means system-wide (e.g. a sync-service failure) — every
/// enabled recipient gets it. `project_id` is `None` when the trigger didn't resolve it (site- and
/// parameter-level subscription overrides still apply; only project-level overrides are skipped).
#[derive(Clone, Debug)]
pub struct Slot {
    pub project_id: Option<Uuid>,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
}

/// A rendered notification ready to deliver. `kind` matches `notification_log.kind`.
#[derive(Clone, Debug)]
pub struct OutgoingMessage {
    pub kind: &'static str,
    pub subject: String,
    pub body: String,
    /// The alert's scope, for per-subscriber fan-out. `None` = system-wide.
    pub slot: Option<Slot>,
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
    async fn deliver(&self, state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult>;
    /// Live reachability probe (no message sent): `getMe` for Telegram, an SMTP connection test or a
    /// Graph token fetch for email. `Ok(detail)` is healthy; `Err(detail)` carries the failure reason.
    async fn check_health(&self) -> Result<String, String>;
}
