mod handlers;
mod types;

pub use handlers::{get_active_alarms, get_alarm_summary, get_site_alarms};
pub use types::{
    ActiveAlarmsResponse, AlarmSummaryResponse, AlarmViolationsResponse, ParameterViolationData,
    SiteAlarmsQuery,
};

// Re-export utoipa path structs for OpenAPI documentation
pub use handlers::{__path_get_active_alarms, __path_get_alarm_summary, __path_get_site_alarms};
