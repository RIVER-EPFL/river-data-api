mod handlers;
mod types;
pub mod views;

pub use handlers::list_project_sites;
pub use types::ProjectResponse;

// Re-export utoipa path structs for OpenAPI documentation
pub use handlers::__path_list_project_sites;
