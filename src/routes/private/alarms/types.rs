use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::routes::private::sites::types::{ProjectRef, SiteRef};

/// Response for alarm violations endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmViolationsResponse {
    /// Project this data belongs to
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Start of time range (null if no violations)
    pub start: Option<DateTime<Utc>>,
    /// End of time range (null if no violations)
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
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
}

/// Information about an alarm threshold configuration
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmThresholdInfo {
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
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
    pub threshold: AlarmThresholdInfo,
    /// 1=warning, 2=alarm
    pub severity: i16,
    /// Timestamp of the latest violating reading
    pub since: DateTime<Utc>,
    /// When the breach started (from the persisted alarm event).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Persisted alarm-event id (present once the sweeper has recorded this breach).
    /// Acknowledge via `POST /api/alarms/{event_id}/acknowledge`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<Uuid>,
    /// True when the open event has been acknowledged.
    pub acknowledged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_by: Option<String>,
    /// Highest severity seen while this event has been open (1=warning, 2=alarm).
    #[serde(skip_serializing_if = "Option::is_none")]
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
    pub latest_reading_time: Option<DateTime<Utc>>,
    /// Most recent `last_seen_at` of an alarm event whose peak severity was a warning
    /// (`max_severity = 1`). Events that escalated to an alarm contribute to `last_alarm_at`
    /// instead, so the two timestamps are disjoint by peak severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_warning_at: Option<DateTime<Utc>>,
    /// Most recent `last_seen_at` of an alarm event that reached alarm severity
    /// (`max_severity = 2`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_alarm_at: Option<DateTime<Utc>>,
}

/// Response for alarm summary endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmSummaryResponse {
    pub total: usize,
    pub by_severity: AlarmSeverityCounts,
    pub by_site: Vec<AlarmSiteSummary>,
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
    /// Current severity (1=warning, 2=alarm)
    pub severity: i16,
    /// Highest severity seen while the event has been open (1=warning, 2=alarm)
    pub max_severity: i16,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub value_at_start: f64,
    pub last_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_by: Option<String>,
}

/// Response for the alarm-events feed
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmEventsResponse {
    pub events: Vec<AlarmEventResponse>,
    pub total: usize,
}
