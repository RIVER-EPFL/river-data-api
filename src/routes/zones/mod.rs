mod handlers;
mod types;

pub use handlers::{get_zone, list_zone_stations, list_zones};
pub use types::ZoneResponse;

// Re-export utoipa path structs for OpenAPI documentation
pub use handlers::{__path_get_zone, __path_list_zone_stations, __path_list_zones};
