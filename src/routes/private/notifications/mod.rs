//! Email + Telegram notifications layered on the alarm pipeline.
//!
//! The dispatcher consumes `AlarmStateChanged` broadcasts (and polls the outbox columns as a
//! backstop), renders messages, and fans them out to the enabled channels. Telegram is restricted
//! to linked identities whose Keycloak role is resolved live (`authz`), never from a cached value.

use uuid::Uuid;

use crate::common::AppState;

pub mod access;
pub mod attest;
pub mod audit;
pub mod authz;
pub mod bot;
pub mod commands;
pub mod dispatcher;
pub mod email;
pub mod health;
pub mod identities_model;
pub mod keyboard;
pub mod log_model;
pub mod me;
pub mod messages;
pub mod mutes_model;
pub mod plot_args;
pub mod reconcile;
pub mod telegram;
pub mod triggers;
pub mod views;

pub use identities_model::TelegramIdentity;
pub use log_model::NotificationLog;
pub use mutes_model::NotificationMute;

/// The (project, site, parameter) an alert belongs to. Used to fan out only to subscribers who opted
/// in to that scope. `None` on a message means system-wide (e.g. a sync-service failure), every
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

/// What a bot command answers with.
///
/// Every handler but the plot commands returns text, so `From<String>` keeps their call sites
/// unchanged and the routing table's arms all still evaluate to `String`.
pub enum Reply {
    Text(String),
    /// Text plus tappable choices, for picking a site or a parameter.
    Menu {
        text: String,
        keyboard: keyboard::Keyboard,
    },
    Photo {
        png: Vec<u8>,
        caption: String,
        keyboard: Option<keyboard::Keyboard>,
    },
}

impl Reply {
    /// The words of a reply, whatever form it takes.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Reply::Text(t) | Reply::Menu { text: t, .. } | Reply::Photo { caption: t, .. } => t,
        }
    }

    /// The buttons under a reply, if it has any.
    #[must_use]
    pub fn keyboard(&self) -> Option<&keyboard::Keyboard> {
        match self {
            Reply::Text(_) => None,
            Reply::Menu { keyboard, .. } => Some(keyboard),
            Reply::Photo { keyboard, .. } => keyboard.as_ref(),
        }
    }
}

impl From<String> for Reply {
    fn from(s: String) -> Self {
        Reply::Text(s)
    }
}

impl From<&str> for Reply {
    fn from(s: &str) -> Self {
        Reply::Text(s.to_string())
    }
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
