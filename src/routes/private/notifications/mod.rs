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
    /// Instrument status forecast from a trend rather than a threshold breach. Opt-in: the person
    /// who acts on it is whoever goes to the field, not whoever watches the data (Q81).
    Prediction,
}

impl KindGroup {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alarms => "alarms",
            Self::Sync => "sync",
            Self::Prediction => "prediction",
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
            "prediction" => Some(Self::Prediction),
            _ => None,
        }
    }

    pub const ALL: [Self; 3] = [Self::Alarms, Self::Sync, Self::Prediction];
}

/// The group a kind is delivered to. `None` is a kind with no audience of its own: the test send
/// addresses whoever asked for it.
#[must_use]
pub fn kind_group(kind: &str) -> Option<KindGroup> {
    match kind {
        "alarm_opened" | "alarm_resolved" => Some(KindGroup::Alarms),
        "stale_data" | "sync_stale" | "sync_failure" | "streams_unpaired" | "holds_open" => {
            Some(KindGroup::Sync)
        }
        "battery_forecast" => Some(KindGroup::Prediction),
        _ => None,
    }
}

/// Every kind the triggers emit. A kind absent from [`kind_group`] is delivered to every enabled
/// recipient with no way to decline, so the list is here and the test below holds it to the map.
pub const EMITTED_KINDS: [&str; 8] = [
    "alarm_opened",
    "alarm_resolved",
    "battery_forecast",
    "stale_data",
    "sync_stale",
    "sync_failure",
    "streams_unpaired",
    "holds_open",
];

/// The kind a message may carry without belonging to a group: the test send is addressed to
/// whoever asked for it, never fanned out.
pub const UNADDRESSED_KIND: &str = "test";

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

    /// Every kind the triggers emit is delivered to a group. A kind the map does not answer for
    /// reaches every enabled recipient with no way to decline, which is what M89 closed.
    #[test]
    fn test_kind_group_maps_every_emitted_kind() {
        for kind in EMITTED_KINDS {
            assert!(
                kind_group(kind).is_some(),
                "'{kind}' is emitted and belongs to no group, so it is delivered to everyone"
            );
        }
        assert_eq!(kind_group("alarm_opened"), Some(KindGroup::Alarms));
        assert_eq!(kind_group("alarm_resolved"), Some(KindGroup::Alarms));
        assert_eq!(kind_group("battery_forecast"), Some(KindGroup::Prediction));
        assert_eq!(kind_group("stale_data"), Some(KindGroup::Sync));
        assert_eq!(kind_group("sync_stale"), Some(KindGroup::Sync));
        assert_eq!(kind_group("sync_failure"), Some(KindGroup::Sync));
        assert_eq!(kind_group("streams_unpaired"), Some(KindGroup::Sync));
        assert_eq!(kind_group("holds_open"), Some(KindGroup::Sync));
    }

    /// The list above is only as good as its completeness, so it is held to the sources: every
    /// `kind:` a message is built with is either an emitted kind or the unaddressed test send.
    /// A new kind added to a trigger fails here rather than reaching everyone silently.
    #[test]
    fn test_every_kind_a_message_carries_is_listed() {
        const SOURCES: [&str; 3] = [
            include_str!("triggers.rs"),
            include_str!("messages.rs"),
            include_str!("views.rs"),
        ];
        for source in SOURCES {
            for tail in source.split("kind: \"").skip(1) {
                let kind = tail.split('"').next().unwrap_or_default();
                assert!(
                    kind == UNADDRESSED_KIND || EMITTED_KINDS.contains(&kind),
                    "a message carries kind '{kind}', which is in neither EMITTED_KINDS nor the \
                     unaddressed test send, so nothing maps it to an audience"
                );
            }
        }
    }

    #[test]
    fn test_test_send_belongs_to_no_group() {
        assert_eq!(kind_group("test"), None);
    }

    #[test]
    fn test_only_alarms_is_subscribed_without_a_row() {
        assert!(KindGroup::Alarms.subscribed_without_a_row());
        assert!(!KindGroup::Sync.subscribed_without_a_row());
        assert!(
            !KindGroup::Prediction.subscribed_without_a_row(),
            "an instrument status forecast is never on by default"
        );
    }

    #[test]
    fn test_group_names_round_trip() {
        for g in KindGroup::ALL {
            assert_eq!(KindGroup::parse(g.as_str()), Some(g));
        }
        assert_eq!(KindGroup::parse("alarm"), None);
    }
}
