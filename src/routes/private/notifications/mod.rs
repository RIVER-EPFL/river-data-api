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

/// A channel a subscriber chooses on its own. The kind is the channel (Q57): somebody who wants to
/// hear about values changing under them and not about a stream going unpaired says so per kind,
/// and a channel nobody subscribed to still records everything it would have said, in the ledger
/// and on the alerts panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    pub kind: &'static str,
    /// What the channel is called where somebody chooses it.
    pub label: &'static str,
    /// What it sends, so the choice is made from the alerts and not from the name.
    pub description: &'static str,
    /// Whether a subscriber with no row for this channel is in its audience. The two alarm kinds
    /// are, which is the audience every subscriber already had; everything else is asked for.
    pub on_by_default: bool,
}

/// Every kind the triggers emit, as the channel it is subscribed through. A kind absent from here
/// would reach every enabled recipient with no way to decline, which is what the tests below hold
/// the triggers to.
pub const CHANNELS: [Channel; 17] = [
    Channel {
        kind: "alarm_opened",
        label: "Alarm opened",
        description: "A reading crossing a warning or alarm threshold.",
        on_by_default: true,
    },
    Channel {
        kind: "alarm_resolved",
        label: "Alarm resolved",
        description: "A slot that was breaching returning to range.",
        on_by_default: true,
    },
    Channel {
        kind: "battery_forecast",
        label: "Instrument forecasts",
        description:
            "A battery whose voltage trend reaches the cutoff before the next field visit.",
        on_by_default: false,
    },
    Channel {
        kind: "stale_data",
        label: "Site gone quiet",
        description: "A site that has stopped sending data.",
        on_by_default: false,
    },
    Channel {
        kind: "sync_stale",
        label: "Sync service silent",
        description: "A sync service whose heartbeat has stopped.",
        on_by_default: false,
    },
    Channel {
        kind: "sync_failure",
        label: "Sync failures",
        description: "A sync cycle that ended in an error.",
        on_by_default: false,
    },
    Channel {
        kind: "streams_unpaired",
        label: "Unpaired streams",
        description: "A source sending readings into a stream that is paired to no slot.",
        on_by_default: false,
    },
    Channel {
        kind: "holds_open",
        label: "Review queue",
        description: "Audit holds waiting for somebody to decide them.",
        on_by_default: false,
    },
    Channel {
        kind: "job_failed",
        label: "Failed jobs",
        description: "A background job that has spent its retries.",
        on_by_default: false,
    },
    Channel {
        kind: "changes_pending",
        label: "Source changes",
        description: "Values a source changed after river-data stored them, and what arrived.",
        on_by_default: false,
    },
    Channel {
        kind: "curve_drift",
        label: "Recomposed values",
        description: "Stored values the janitor recomposed from the curves their readings name.",
        on_by_default: false,
    },
    Channel {
        kind: "derived_computed",
        label: "Derived values computed",
        description: "Derived values computed where none was stored.",
        on_by_default: false,
    },
    Channel {
        kind: "access_revoked",
        label: "Access revoked",
        description: "Push subscriptions removed because the person lost their grant.",
        on_by_default: false,
    },
    Channel {
        kind: "aggregates_refreshed",
        label: "Rollups refreshed",
        description: "Continuous aggregate refreshes, which is upkeep rather than a change.",
        on_by_default: false,
    },
    Channel {
        kind: "jobs_pruned",
        label: "Job rows pruned",
        description: "Tracked job rows aged out of the timeline by retention.",
        on_by_default: false,
    },
    Channel {
        kind: "sync_events_swept",
        label: "Stale sync events closed",
        description: "Sync events left running past the staleness threshold and marked failed.",
        on_by_default: false,
    },
    Channel {
        kind: "ledger_pruned",
        label: "Sync ledger pruned",
        description: "Sync events and ingest receipts deleted by their retention horizons.",
        on_by_default: false,
    },
];

/// The channel a kind is subscribed through, or `None` for a kind no channel answers for.
#[must_use]
pub fn channel(kind: &str) -> Option<&'static Channel> {
    CHANNELS.iter().find(|c| c.kind == kind)
}

/// Whether a subscriber holding no row for this kind is in its audience.
#[must_use]
pub fn on_by_default(kind: &str) -> bool {
    channel(kind).is_some_and(|c| c.on_by_default)
}

/// Every kind the triggers emit, which is the channel table read as names.
#[must_use]
pub fn emitted_kinds() -> Vec<&'static str> {
    CHANNELS.iter().map(|c| c.kind).collect()
}

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

    /// Every kind the triggers emit has a channel to decline it on. A kind no channel answers
    /// for reaches every enabled recipient with no way to decline, which is what M89 closed and
    /// M163 kept when the group became the kind.
    #[test]
    fn test_every_emitted_kind_has_a_channel() {
        for kind in emitted_kinds() {
            assert!(
                channel(kind).is_some(),
                "'{kind}' is emitted and belongs to no channel, so it is delivered to everyone"
            );
        }
        assert!(on_by_default("alarm_opened"), "the audience nobody opted into");
        assert!(on_by_default("alarm_resolved"));
        for kind in [
            "battery_forecast",
            "stale_data",
            "sync_stale",
            "sync_failure",
            "streams_unpaired",
            "holds_open",
            "job_failed",
            "changes_pending",
            "curve_drift",
            "derived_computed",
        ] {
            assert!(!on_by_default(kind), "'{kind}' is asked for, not assumed");
        }
        assert!(channel("test").is_none(), "the test send addresses whoever asked");
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
                    kind == UNADDRESSED_KIND || emitted_kinds().contains(&kind),
                    "a message carries kind '{kind}', which is on neither a channel nor the \
                     unaddressed test send, so nothing maps it to an audience"
                );
            }
        }
    }

    #[test]
    fn test_channel_names_are_the_kinds_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in CHANNELS {
            assert!(seen.insert(c.kind), "'{}' is declared twice", c.kind);
            assert_eq!(channel(c.kind), Some(&c));
            assert!(!c.label.is_empty() && !c.description.is_empty());
        }
    }
}
