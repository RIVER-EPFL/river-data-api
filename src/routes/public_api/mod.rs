pub mod mount_resilience;

use axum::Router;

use crate::common::AppState;

/// Router for all public API integrations.
pub fn public_router() -> Router<AppState> {
    Router::new().nest("/mountresilience", mount_resilience::mount_resilience_router())
}
