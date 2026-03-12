mod aggregates;
mod handlers;
mod readings;
mod status_events;
mod types;
pub mod views;

pub use aggregates::{AggregatesResponse, ParameterAggregateData, get_site_aggregates};
pub use handlers::list_site_parameters;
pub use readings::SiteReadingsQuery;
pub use readings::{ParameterData, ReadingsResponse, get_site_readings};
pub use status_events::{StatusEventsResponse, get_site_status_events};
pub use types::{
    ParameterResponse, ProjectRef, SiteDetailResponse, SiteRef, SiteResponse, SitesQuery,
};

// Re-export utoipa path structs for OpenAPI documentation
pub use aggregates::__path_get_site_aggregates;
pub use handlers::__path_list_site_parameters;
pub use readings::__path_get_site_readings;
pub use status_events::__path_get_site_status_events;
