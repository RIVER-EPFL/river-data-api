mod handlers;
mod types;

pub use handlers::list_sensors;
pub use types::{SensorResponse, SensorsQuery};

// Re-export utoipa path struct for OpenAPI documentation
pub use handlers::__path_list_sensors;
