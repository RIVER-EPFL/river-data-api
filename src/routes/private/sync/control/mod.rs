//! The sync control plane: the endpoints a sync service itself calls, as opposed to the operator
//! surface a human drives. Mounted at `/api/sync`; enrollment authenticates on body credentials
//! and everything else on the session token that enrollment returns.

pub mod commands;
pub mod enroll;
pub mod events;
pub mod heartbeat;
pub mod session;
pub mod tokens;

use axum::Router;
use axum::routing::{patch, post};

use crate::common::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/enroll", post(enroll::enroll))
        .route("/heartbeat", post(heartbeat::heartbeat))
        .route("/commands/{id}", patch(commands::update_command))
        .route("/events", post(events::create_sync_event))
        .route("/events/{id}", patch(events::update_sync_event))
}
