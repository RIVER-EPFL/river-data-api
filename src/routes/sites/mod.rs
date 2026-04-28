mod aggregates;
pub mod annotations;
mod handlers;
mod readings;
mod status_events;
mod types;
pub mod views;

pub use aggregates::{AggregatesResponse, ParameterAggregateData, get_site_aggregates};
pub use handlers::{get_site_detail, list_site_parameters};
pub use readings::SiteReadingsQuery;
pub use readings::{ParameterData, ReadingsResponse, get_site_readings};
pub use status_events::{StatusEventsResponse, get_site_status_events};
pub use types::{
    ParameterResponse, ProjectRef, SiteDetailResponse, SiteRef, SiteResponse, SitesQuery,
};

pub use annotations::{AnnotationResponse, get_site_annotations};

// Re-export utoipa path structs for OpenAPI documentation
pub use aggregates::__path_get_site_aggregates;
pub use annotations::__path_get_site_annotations;
pub use handlers::__path_get_site_detail;
pub use handlers::__path_list_site_parameters;
pub use readings::__path_get_site_readings;
pub use status_events::__path_get_site_status_events;
