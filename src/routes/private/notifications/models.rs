//! The channel table every notification kind is subscribed through, the two notification entities,
//! and the request and response shapes the handlers speak.

use chrono::{DateTime, Utc};
use sea_orm::FromQueryResult;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::Role;

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
        description: "A battery whose voltage trend reaches the cutoff before the next field visit.",
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

/// The live authority for a notification recipient. `Active` carries the user's current highest
/// riverdata role; `Revoked` means the link or subscription must be deactivated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleResolution {
    Active(Role),
    Revoked,
}

impl RoleResolution {
    /// The resolved role, if the user is active.
    #[must_use]
    pub fn role(&self) -> Option<&Role> {
        match self {
            Self::Active(r) => Some(r),
            Self::Revoked => None,
        }
    }

    /// Read commands: any current riverdata user (Intern and up).
    #[must_use]
    pub fn allows_user(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    /// Operational commands (mutes): Administrator only.
    #[must_use]
    pub fn allows_admin(&self) -> bool {
        matches!(self, Self::Active(Role::Administrator))
    }

    /// At least `min`'s access level.
    #[must_use]
    pub fn allows_level(&self, min: &Role) -> bool {
        matches!(self, Self::Active(r) if r.level() >= min.level())
    }
}

/// One breach/resolution to describe in a message, resolved to human-readable names.
#[derive(Clone, Debug)]
pub struct PendingEvent {
    pub site_name: String,
    pub parameter_name: String,
    pub units: Option<String>,
    pub severity: i16,
    pub value: f64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct TestSendRequest {
    pub channel: String,
    pub recipient: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TestResult {
    pub recipient: String,
    pub status: String,
    #[schema(required)]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestSendResponse {
    pub channel: String,
    pub results: Vec<TestResult>,
    pub all_sent: bool,
}

#[derive(Debug, Serialize, ToSchema, FromQueryResult)]
#[serde(rename_all = "camelCase")]
pub struct SubscriberRow {
    pub keycloak_sub: String,
    pub web_push_enabled: bool,
    #[sea_orm(alias = "push_count")]
    pub push_subscription_count: i64,
    #[sea_orm(alias = "overrides")]
    pub subscription_overrides: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelHealth {
    pub name: String,
    pub available: bool,
    /// `None` until a probe has run for this channel.
    #[schema(required)]
    pub healthy: Option<bool>,
    #[schema(required)]
    pub detail: Option<String>,
    #[schema(required)]
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationHealth {
    pub channels: Vec<ChannelHealth>,
    /// No channel resolves from config, so every notification is stamped undeliverable. The API
    /// runs regardless (notifications never block ingestion), which is exactly why this has to be
    /// visible rather than inferred from an empty list.
    pub no_channel_configured: bool,
    /// Deliveries in the last 24 hours that reached nobody: `undeliverable` (no channel at all) and
    /// `failed` (a channel refused the send).
    pub undeliverable_24h: i64,
    pub failed_24h: i64,
}

#[derive(Default)]
pub struct SweepOutcome {
    pub revoked: usize,
}

impl SweepOutcome {
    pub fn total(&self) -> usize {
        self.revoked
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeliveryQuery {
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    /// Keep only messages carrying at least one row of this status.
    pub status: Option<String>,
    /// Keep only messages of this kind (`alarm_opened`, `sync_stale`, `test`, ...).
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryCounts {
    pub total: i64,
    pub sent: i64,
    pub failed: i64,
    pub muted: i64,
    pub undeliverable: i64,
    pub skipped: i64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecipient {
    pub channel: String,
    pub recipient: String,
    pub status: String,
    #[schema(required)]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryMessage {
    #[schema(required)]
    pub alarm_event_id: Option<Uuid>,
    pub kind: String,
    /// The second the message's rows were written in, which is the group key.
    pub at: DateTime<Utc>,
    #[schema(required)]
    pub site_name: Option<String>,
    #[schema(required)]
    pub parameter_name: Option<String>,
    pub counts: DeliveryCounts,
    pub recipients: Vec<DeliveryRecipient>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubscriptionScope {
    /// The channel the row answers for, which is the notification kind (M163). Absent means
    /// `alarm_opened`, the audience every subscriber had before channels were separable.
    #[serde(default = "default_channel", alias = "kind_group")]
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(nullable = false)]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(nullable = false)]
    pub site_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(nullable = false)]
    pub parameter_id: Option<Uuid>,
    pub enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MyNotifications {
    pub web_push_enabled: bool,
    pub push_subscription_count: i64,
    pub subscriptions: Vec<SubscriptionScope>,
}

pub(super) fn default_channel() -> String {
    "alarm_opened".to_string()
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePrefsRequest {
    pub web_push_enabled: Option<bool>,
}

/// One channel as the person choosing it sees it: what it sends, whether they are in its
/// audience, and how often it has fired lately, so the volume is known before the box is ticked.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelView {
    pub kind: String,
    pub label: String,
    pub description: String,
    pub on_by_default: bool,
    /// Whether the caller is in this channel's audience right now, their own rows applied.
    pub subscribed: bool,
    /// Notifications of this kind sent in the last day, week and month, across everyone.
    pub sent_1d: i64,
    pub sent_7d: i64,
    pub sent_30d: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSubscriptionsRequest {
    pub subscriptions: Vec<SubscriptionScope>,
}

// ---------------------------------------------------------------------------
// Web Push subscription CRUD
// ---------------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct RegisterPushRequest {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub user_agent: Option<String>,
}

/// The row the push-subscription queries select, which is also what they answer with, so it
/// decodes itself rather than being filled field by field.
#[derive(Serialize, ToSchema, FromQueryResult)]
pub struct PushSubscriptionRow {
    pub id: Uuid,
    pub endpoint: String,
    #[schema(required)]
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    #[schema(required)]
    pub last_success_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, ToSchema)]
pub struct DeletePushRequest {
    pub endpoint: String,
}

// ---------------------------------------------------------------------------
// Self-service test + timed ping
// ---------------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct PingRequest {
    #[serde(default = "default_ping_seconds")]
    pub seconds: u64,
}

fn default_ping_seconds() -> u64 {
    10
}

/// One device's outcome from a self-service push. `endpoint_tail` is the last 12 characters of the
/// endpoint: enough to tell two devices apart in a log or in the UI, useless as a capability.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushAttempt {
    pub id: Uuid,
    pub endpoint_tail: String,
    #[schema(required)]
    pub user_agent: Option<String>,
    pub status: String,
    #[schema(required)]
    pub error: Option<String>,
    pub pruned: bool,
}

/// The delivery ledger: one row per recipient per message, whatever the outcome.
pub mod log {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "notification_log")]
    #[crudcrate(
        api_struct = "NotificationLog",
        name_singular = "notification_log",
        name_plural = "notification_logs",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub alarm_event_id: Option<Uuid>,
        #[crudcrate(filterable)]
        pub kind: String,
        #[crudcrate(filterable)]
        pub channel: String,
        pub recipient: String,
        #[crudcrate(filterable)]
        pub status: String,
        pub error: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// One scope a subscriber has chosen for a channel: the row that overrides the channel default.
pub mod subscription {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "notification_subscriptions")]
    #[crudcrate(
        api_struct = "NotificationSubscription",
        name_singular = "notification_subscription",
        name_plural = "notification_subscriptions"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub keycloak_sub: String,
        #[crudcrate(filterable)]
        pub project_id: Option<Uuid>,
        #[crudcrate(filterable)]
        pub site_id: Option<Uuid>,
        #[crudcrate(filterable)]
        pub parameter_id: Option<Uuid>,
        pub enabled: bool,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update))]
        pub updated_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(filterable)]
        pub channel: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// A browser push endpoint a subscriber registered, one row per device.
pub mod push_subscription {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "web_push_subscriptions")]
    #[crudcrate(
        api_struct = "WebPushSubscription",
        name_singular = "web_push_subscription",
        name_plural = "web_push_subscriptions"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub keycloak_sub: String,
        #[crudcrate(filterable)]
        pub endpoint: String,
        pub p256dh: String,
        pub auth: String,
        pub user_agent: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update))]
        pub last_success_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Slot mutes: a `(site, parameter)` nobody is to be alerted about until the mute expires.
pub mod mutes {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use sea_orm::{Condition, ExprTrait, sea_query::Expr};

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "notification_mutes")]
    #[crudcrate(
        api_struct = "NotificationMute",
        name_singular = "notification_mute",
        name_plural = "notification_mutes",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub site_id: Uuid,
        #[crudcrate(filterable)]
        pub parameter_id: Uuid,
        // NULL means muted until /unmute.
        pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
        pub created_by: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::sites::Entity",
            from = "Column::SiteId",
            to = "crate::routes::private::sites::Column::Id"
        )]
        Site,
        #[sea_orm(
            belongs_to = "crate::routes::private::parameters::Entity",
            from = "Column::ParameterId",
            to = "crate::routes::private::parameters::Column::Id"
        )]
        Parameter,
    }

    impl Related<crate::routes::private::sites::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Site.def()
        }
    }

    impl Related<crate::routes::private::parameters::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Parameter.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}

    /// A mute is in force until it expires; a NULL `expires_at` never expires. The predicate is
    /// defined once here so the delivery gate and the listings cannot disagree about what "muted"
    /// means.
    pub fn in_force() -> Condition {
        Condition::any()
            .add(Column::ExpiresAt.is_null())
            .add(Expr::col(Column::ExpiresAt).gt(Expr::current_timestamp()))
    }
}

/// The dispatcher's dedup and claim rows, keyed `(kind, subject_key)`: one row per condition
/// currently announced, plus the health of each delivery channel under `channel_health`.
///
/// Read-only as an entity (`routes(read)`): every writer is the dispatcher claiming or clearing a
/// transition, never a person. The key is composite, so no single-row route is mounted (Q153).
pub mod state {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "notification_state")]
    #[crudcrate(
        api_struct = "NotificationState",
        name_singular = "notification_state",
        name_plural = "notification_states",
        generate_router,
        routes(read)
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, filterable, sortable, exclude(update, create))]
        pub kind: String,
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, filterable, sortable, exclude(update, create))]
        pub subject_key: String,
        #[crudcrate(filterable, exclude(update, create))]
        pub state: String,
        #[crudcrate(sortable, exclude(update, create))]
        pub last_notified_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(update, create))]
        pub detail: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[cfg(test)]
#[path = "tests/models.rs"]
mod tests;
