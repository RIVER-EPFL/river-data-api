//! Notification delivery layered on the alarm pipeline.
//!
//! The dispatcher consumes `AlarmStateChanged` broadcasts (and polls the outbox columns as a
//! backstop), renders messages, and fans them out to the enabled Web Push channel.

use uuid::Uuid;

use crate::common::AppState;

pub mod access;
pub mod deliveries;
pub mod dispatcher;
pub mod health;
pub mod log_model;
pub mod me;
pub mod messages;
pub mod mutes_model;
pub mod reconcile;
pub mod triggers;
pub mod views;
pub mod web_push;

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

/// The audience a notification kind is addressed to. A subscriber chooses groups, not kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindGroup {
    Alarms,
    Sync,
}

impl KindGroup {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alarms => "alarms",
            Self::Sync => "sync",
        }
    }

    /// Whether a subscriber with no row for this group is in its audience.
    #[must_use]
    pub fn subscribed_without_a_row(self) -> bool {
        matches!(self, Self::Alarms)
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "alarms" => Some(Self::Alarms),
            "sync" => Some(Self::Sync),
            _ => None,
        }
    }

    pub const ALL: [Self; 2] = [Self::Alarms, Self::Sync];
}

/// The group a kind is delivered to. `None` is a kind with no audience of its own: the test send
/// addresses whoever asked for it.
#[must_use]
pub fn kind_group(kind: &str) -> Option<KindGroup> {
    match kind {
        "alarm_opened" | "alarm_resolved" | "battery_forecast" => Some(KindGroup::Alarms),
        "stale_data" | "sync_stale" | "sync_failure" => Some(KindGroup::Sync),
        _ => None,
    }
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

/// A delivery channel. Each channel resolves its own recipients, so the dispatcher only renders
/// the message once and hands it to every enabled channel.
#[async_trait::async_trait]
pub trait NotificationChannel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn deliver(&self, state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult>;
    async fn check_health(&self) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_group_maps_every_emitted_kind() {
        assert_eq!(kind_group("alarm_opened"), Some(KindGroup::Alarms));
        assert_eq!(kind_group("alarm_resolved"), Some(KindGroup::Alarms));
        assert_eq!(kind_group("battery_forecast"), Some(KindGroup::Alarms));
        assert_eq!(kind_group("stale_data"), Some(KindGroup::Sync));
        assert_eq!(kind_group("sync_stale"), Some(KindGroup::Sync));
        assert_eq!(kind_group("sync_failure"), Some(KindGroup::Sync));
    }

    #[test]
    fn test_test_send_belongs_to_no_group() {
        assert_eq!(kind_group("test"), None);
    }

    #[test]
    fn test_only_alarms_is_subscribed_without_a_row() {
        assert!(KindGroup::Alarms.subscribed_without_a_row());
        assert!(!KindGroup::Sync.subscribed_without_a_row());
    }

    #[test]
    fn test_group_names_round_trip() {
        for g in KindGroup::ALL {
            assert_eq!(KindGroup::parse(g.as_str()), Some(g));
        }
        assert_eq!(KindGroup::parse("alarm"), None);
    }
}
