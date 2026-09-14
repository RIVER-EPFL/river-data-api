//! The alarm entity, the threshold it is evaluated against, and the response shapes.

use chrono::{DateTime, Utc};
use crudcrate::EntityToModels;
use sea_orm::FromQueryResult;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::service::AlarmThresholdOperations;
use crate::routes::private::sites::models::{ProjectRef, SiteRef};

/// Response for alarm violations endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmViolationsResponse {
    /// Project this data belongs to
    #[schema(required)]
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Start of time range (null if no violations)
    #[schema(required)]
    pub start: Option<DateTime<Utc>>,
    /// End of time range (null if no violations)
    #[schema(required)]
    pub end: Option<DateTime<Utc>>,
    /// Array of timestamps where violations occurred
    pub times: Vec<DateTime<Utc>>,
    /// Array of parameters with their violation data
    pub parameters: Vec<ParameterViolationData>,
}

/// Parameter data with values and severity levels
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterViolationData {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    #[schema(required)]
    pub units: Option<String>,
    /// Values array (same length as times). Null at a timestamp where this parameter did not
    /// violate, matching every other series endpoint; the axis is the union across parameters.
    pub values: Vec<Option<f64>>,
    /// Severity levels (same length as times): 1=warning, 2=alarm, null where no violation.
    pub severities: Vec<Option<i16>>,
}

/// Query parameters for site alarms endpoint
#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct SiteAlarmsQuery {
    /// Start time (required, ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601)
    pub end: DateTime<Utc>,
    /// Filter by minimum severity (1=warning, 2=alarm). Default: include all violations.
    pub severity: Option<i16>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Filter to a specific subset of parameters (comma-separated UUIDs). If omitted, covers every
    /// parameter configured for the site.
    pub parameter_ids: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
}

/// A single active alarm violation
#[derive(Debug, Serialize, ToSchema)]
pub struct ActiveAlarm {
    pub site_id: Uuid,
    pub site_name: String,
    pub parameter_id: Uuid,
    pub parameter_name: String,
    pub current_value: f64,
    /// Cadence of the series that raised this breach: 'continuous' (sensor) or 'spot' (grab).
    pub measurement_type: String,
    /// What raised it: 'threshold', the site or parameter bounds, or 'instrument_range', a value
    /// outside what the instrument that measured it can read.
    pub kind: String,
    /// The bounds that were breached. On an 'instrument_range' breach these are the instrument's
    /// own `range_min`/`range_max`, carried as the alarm bounds because a range breach has no
    /// warning degree to it.
    pub threshold: ResolvedThreshold,
    /// The instrument an 'instrument_range' breach is about; absent on a threshold breach, which
    /// is about the water rather than the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_name: Option<String>,
    /// 1=warning, 2=alarm
    pub severity: i16,
    /// Timestamp of the latest violating reading
    pub since: DateTime<Utc>,
    /// When the breach started (from the persisted alarm event).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub started_at: Option<DateTime<Utc>>,
    /// Persisted alarm-event id (present once the sweeper has recorded this breach).
    /// Acknowledge via `POST /api/alarms/{event_id}/acknowledge`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub event_id: Option<Uuid>,
    /// True when the open event has been acknowledged.
    pub acknowledged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub acknowledged_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub acknowledged_by: Option<String>,
    /// Highest severity seen while this event has been open (1=warning, 2=alarm).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub max_severity: Option<i16>,
}

/// Returned by `POST /api/alarms/{event_id}/acknowledge`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AcknowledgedAlarmResponse {
    pub event_id: Uuid,
    pub acknowledged_at: DateTime<Utc>,
    pub acknowledged_by: String,
}

/// Response for active alarms endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct ActiveAlarmsResponse {
    pub alarms: Vec<ActiveAlarm>,
    pub total: usize,
}

/// Severity counts for alarm summary
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmSeverityCounts {
    pub warning: usize,
    pub alarm: usize,
}

/// Per-site alarm counts for alarm summary
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmSiteSummary {
    pub site_id: Uuid,
    pub site_name: String,
    pub warning_count: usize,
    pub alarm_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub latest_reading_time: Option<DateTime<Utc>>,
    /// Most recent `last_seen_at` of an alarm event whose peak severity was a warning
    /// (`max_severity = 1`). Events that escalated to an alarm contribute to `last_alarm_at`
    /// instead, so the two timestamps are disjoint by peak severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub last_warning_at: Option<DateTime<Utc>>,
    /// Most recent `last_seen_at` of an alarm event that reached alarm severity
    /// (`max_severity = 2`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub last_alarm_at: Option<DateTime<Utc>>,
}

/// Response for alarm summary endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmSummaryResponse {
    pub total: usize,
    pub by_severity: AlarmSeverityCounts,
    pub by_site: Vec<AlarmSiteSummary>,
}

/// One resolved row of the resolved-thresholds query: the winning threshold per active
/// `(site, parameter)` slot, with the tier it came from.
#[derive(Debug, Clone, FromQueryResult, serde::Serialize, utoipa::ToSchema)]
pub struct ThresholdRow {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    #[schema(required)]
    pub warning_min: Option<f64>,
    #[schema(required)]
    pub warning_max: Option<f64>,
    #[schema(required)]
    pub alarm_min: Option<f64>,
    #[schema(required)]
    pub alarm_max: Option<f64>,
    /// Which tier supplied this threshold.
    #[schema(value_type = ThresholdSource)]
    pub source: String,
}

/// The tiers a resolved threshold can come from: the slot's own `alarm_thresholds` row, else the
/// parameter's. Named for the document; the column itself travels as the string the SQL builds.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdSource {
    Site,
    Global,
}

/// Query parameters for the persisted alarm-events feed
#[derive(Debug, Deserialize, IntoParams)]
pub struct AlarmEventsQuery {
    /// Filter to a single site
    pub site_id: Option<Uuid>,
    /// Filter on `max_severity` (1=warning, 2=alarm), exact match
    pub severity: Option<i16>,
    /// Lifecycle filter: `open` | `resolved` | `all` (default `all`)
    pub status: Option<String>,
    /// Only events whose active span overlaps [start, end] (ISO 8601)
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    /// Filter to a single parameter
    pub parameter_id: Option<uuid::Uuid>,
    /// Max rows to return (default 200, capped at 1000)
    pub limit: Option<u64>,
    /// Pagination offset (default 0)
    pub offset: Option<u64>,
}

/// A single persisted alarm event
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmEventResponse {
    pub id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub parameter_id: Uuid,
    pub parameter_name: String,
    /// Cadence of the series that raised this event: 'continuous' (sensor) or 'spot' (grab).
    pub measurement_type: String,
    /// What raised it: 'threshold' or 'instrument_range'.
    pub kind: String,
    /// The instrument an 'instrument_range' event is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_id: Option<Uuid>,
    /// Current severity (1=warning, 2=alarm)
    pub severity: i16,
    /// Highest severity seen while the event has been open (1=warning, 2=alarm)
    pub max_severity: i16,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub value_at_start: f64,
    pub last_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub resolved_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub resolved_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub acknowledged_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub acknowledged_by: Option<String>,
}

/// Response for the alarm-events feed
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmEventsResponse {
    pub events: Vec<AlarmEventResponse>,
    pub total: usize,
}

/// The four numeric bounds that define a breach for one (parameter, site) slot.
#[derive(Debug, Clone, Copy, FromQueryResult, serde::Serialize, utoipa::ToSchema)]
pub struct ResolvedThreshold {
    #[schema(required)]
    pub warning_min: Option<f64>,
    #[schema(required)]
    pub warning_max: Option<f64>,
    #[schema(required)]
    pub alarm_min: Option<f64>,
    #[schema(required)]
    pub alarm_max: Option<f64>,
}

impl ResolvedThreshold {
    /// A threshold with every bound NULL never fires; this is the "Disabled" state written by the
    /// UI (a null-valued site row that blocks the global fallback).
    pub fn is_disabled(&self) -> bool {
        self.warning_min.is_none()
            && self.warning_max.is_none()
            && self.alarm_min.is_none()
            && self.alarm_max.is_none()
    }
}

/// SQL `CASE` mapping a value to a severity (`2`=alarm, `1`=warning, `0`=ok). Callers pass the value
/// expression and the four bound expressions, column refs for the live queries (`t.alarm_min`,
/// `rt.alarm_min`), bind-param refs for the episode query (`$7::double precision`), so the severity
/// ladder is defined in exactly one place. Result is a bare integer; cast to `smallint` at the call
/// Query for the resolved-thresholds feed.
#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct ThresholdsQuery {
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
}
/// One resolved threshold plus the slot's latest reading value, for the UI thresholds table.
#[derive(FromQueryResult, serde::Serialize, utoipa::ToSchema)]
pub struct ThresholdWithValue {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    #[schema(required)]
    pub warning_min: Option<f64>,
    #[schema(required)]
    pub warning_max: Option<f64>,
    #[schema(required)]
    pub alarm_min: Option<f64>,
    #[schema(required)]
    pub alarm_max: Option<f64>,
    #[schema(value_type = ThresholdSource)]
    pub source: String,
    /// Latest reading (last 30 days) for this slot, or null if none, display only.
    #[schema(required)]
    pub current_value: Option<f64>,
}

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "alarm_thresholds")]
#[crudcrate(
    api_struct = "AlarmThreshold",
    name_singular = "alarm_threshold",
    name_plural = "alarm_thresholds",
    generate_router,
    operations = AlarmThresholdOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Option<Uuid>,
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
    pub description: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// The alarm episode as an entity.
///
/// `alarm_events` is written by the sweeper and by the acknowledge routes, never by a client, so
/// the entity mounts read only (`routes(read)`): the list, filter and pagination the alarm history
/// needs come from the derive, and nothing may post an episode that no breach produced.
pub mod alarm_event {
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
    #[sea_orm(table_name = "alarm_events")]
    #[crudcrate(
        api_struct = "AlarmEvent",
        name_singular = "alarm_event",
        name_plural = "alarm_events",
        generate_router,
        routes(read),
        upsert_key(site_id, parameter_id, measurement_type, kind),
        upsert_where = Column::ResolvedAt.is_null()
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub site_id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub parameter_id: Uuid,
        /// 1 = warning, 2 = alarm; what the episode reads as now.
        #[crudcrate(filterable, sortable)]
        pub severity: i16,
        /// The worst it has been, which is what the history is ranked by. Set when the episode
        /// opens and advanced by the sweeper, never overwritten by a registration.
        #[crudcrate(filterable, sortable, exclude(create, update))]
        pub max_severity: i16,
        /// Fixed when the episode opens: a later breach of the same open episode leaves it.
        #[crudcrate(sortable, exclude(create, update))]
        pub started_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update))]
        pub value_at_start: f64,
        #[crudcrate(sortable)]
        pub last_seen_at: chrono::DateTime<chrono::Utc>,
        pub last_value: f64,
        #[crudcrate(filterable, sortable)]
        pub acknowledged_at: Option<chrono::DateTime<chrono::Utc>>,
        #[crudcrate(filterable)]
        pub acknowledged_by: Option<String>,
        /// NULL while the breach stands; the sweeper stamps it when the value returns to range.
        #[crudcrate(filterable, sortable)]
        pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
        pub resolved_value: Option<f64>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update), on_update = chrono::Utc::now())]
        pub updated_at: chrono::DateTime<chrono::Utc>,
        /// When the subscribers were told the episode opened.
        pub notified_at: Option<chrono::DateTime<chrono::Utc>>,
        /// When they were told it closed.
        pub resolution_notified_at: Option<chrono::DateTime<chrono::Utc>>,
        /// The cadence the episode belongs to: a grab series and a sensor series alarm apart.
        #[crudcrate(filterable, sortable)]
        pub measurement_type: String,
        /// What raised it: `threshold`, the site or parameter bounds, or `instrument_range`, a
        /// value outside what the instrument that measured it can read. The two stand open on the
        /// same slot at once, so a failing instrument is not read as an unusual river.
        #[crudcrate(filterable, sortable, on_create = "threshold".to_string())]
        pub kind: String,
        /// The instrument a range episode is about; NULL on a threshold episode, which is about
        /// the water rather than the device.
        #[crudcrate(filterable)]
        pub sensor_id: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::parameters::Entity",
            from = "Column::ParameterId",
            to = "crate::routes::private::parameters::Column::Id"
        )]
        Parameter,
        #[sea_orm(
            belongs_to = "crate::routes::private::sites::Entity",
            from = "Column::SiteId",
            to = "crate::routes::private::sites::Column::Id"
        )]
        Site,
    }

    impl Related<crate::routes::private::parameters::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Parameter.def()
        }
    }

    impl Related<crate::routes::private::sites::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Site.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
