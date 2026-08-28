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

/// The credential-authenticated entry point, the only route worth brute-forcing.
pub fn enroll_routes() -> Router<AppState> {
    Router::new().route("/enroll", post(enroll::enroll))
}

/// Session-token routes. Deliberately unthrottled: the callers are vetted internal services and
/// the events route is their observability record — a 429 here once lost METALP's cycle record
/// while its data synced fully.
pub fn session_routes() -> Router<AppState> {
    Router::new()
        .route("/heartbeat", post(heartbeat::heartbeat))
        .route("/commands/{id}", patch(commands::update_command))
        .route("/events", post(events::create_sync_event))
        .route("/events/{id}", patch(events::update_sync_event))
}

pub fn routes() -> Router<AppState> {
    enroll_routes().merge(session_routes())
}
