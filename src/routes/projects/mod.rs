mod handlers;
mod types;

pub use handlers::{get_project, list_project_sites, list_projects};
pub use types::ProjectResponse;

// Re-export utoipa path structs for OpenAPI documentation
pub use handlers::{__path_get_project, __path_list_project_sites, __path_list_projects};
