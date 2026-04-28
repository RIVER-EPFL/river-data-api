use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::routes::sites::{ProjectRef, SiteRef};

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
    /// Values array (same length as times)
    pub values: Vec<f64>,
    /// Severity levels (same length as times): 1=warning, 2=alarm
    pub severities: Vec<i16>,
}

/// Query parameters for site alarms endpoint
#[derive(Debug, Deserialize, IntoParams)]
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
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "json".to_string()
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
    pub threshold: AlarmThresholdInfo,
    /// 1=warning, 2=alarm
    pub severity: i16,
    /// Timestamp of the latest violating reading
    pub since: DateTime<Utc>,
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
}

/// Response for alarm summary endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmSummaryResponse {
    pub total: usize,
    pub by_severity: AlarmSeverityCounts,
    pub by_site: Vec<AlarmSiteSummary>,
}
