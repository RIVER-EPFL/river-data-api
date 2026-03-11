mod aggregates;
mod handlers;
mod readings;
mod types;

pub use aggregates::{AggregatesResponse, ParameterAggregateData, get_site_aggregates};
pub use handlers::{get_site, list_site_parameters, list_sites};
pub use readings::SiteReadingsQuery;
pub use readings::{ParameterData, ReadingsResponse, get_site_readings};
pub use types::{
    ParameterResponse, ProjectRef, SiteDetailResponse, SiteRef, SiteResponse, SitesQuery,
};

// Re-export utoipa path structs for OpenAPI documentation
pub use aggregates::__path_get_site_aggregates;
pub use handlers::{__path_get_site, __path_list_site_parameters, __path_list_sites};
pub use readings::__path_get_site_readings;
