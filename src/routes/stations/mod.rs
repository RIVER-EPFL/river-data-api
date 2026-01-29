mod aggregates;
mod handlers;
mod readings;
mod types;

pub use aggregates::{get_station_aggregates, AggregatesResponse, SensorAggregateData};
pub use handlers::{get_station, list_station_sensors, list_stations};
pub use readings::StationReadingsQuery;
pub use readings::{get_station_readings, ReadingsResponse, SensorData};
pub use types::{SensorResponse, StationDetailResponse, StationRef, StationResponse, StationsQuery, ZoneRef};

// Re-export utoipa path structs for OpenAPI documentation
pub use aggregates::__path_get_station_aggregates;
pub use handlers::{__path_get_station, __path_list_station_sensors, __path_list_stations};
pub use readings::__path_get_station_readings;
