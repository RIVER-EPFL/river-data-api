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
