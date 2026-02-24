pub mod apptitude;

use axum::Router;

use crate::common::AppState;

/// Router for all partner API integrations.
pub fn partner_router() -> Router<AppState> {
    Router::new().nest("/apptitude", apptitude::apptitude_router())
}
