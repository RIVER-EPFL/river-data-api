mod handlers;
mod types;

pub use handlers::get_site_alarms;
pub use types::{AlarmViolationsResponse, ParameterViolationData, SiteAlarmsQuery};

// Re-export utoipa path struct for OpenAPI documentation
pub use handlers::__path_get_site_alarms;
