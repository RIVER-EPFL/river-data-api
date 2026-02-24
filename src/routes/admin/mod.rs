pub mod sync;

use axum::Router;
use crate::common::AppState;
use crate::common::auth::Role;

pub fn admin_router(state: &AppState) -> Router<AppState> {
    let mut router = Router::new()
        .nest("/sync", sync::router());

    // Apply Keycloak auth layer if configured
    if let Some(instance) = state.keycloak_auth_instance.clone() {
        use axum_keycloak_auth::{PassthroughMode, layer::KeycloakAuthLayer};
        router = router.layer(
            KeycloakAuthLayer::<Role>::builder()
                .instance(instance)
                .passthrough_mode(PassthroughMode::Block)
                .persist_raw_claims(false)
                .expected_audiences(vec![String::from("account")])
                .required_roles(vec![Role::Administrator])
                .build(),
        );
    } else {
        tracing::warn!("Admin routes are not protected by authentication");
    }

    router
}
