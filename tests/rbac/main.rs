//! Integration tests for the RBAC theme: project-visibility grants layered on top of the four
//! Keycloak access levels. Roles (capability) come from Keycloak; grants (which projects a member
//! sees and acts in) live in `user_project_grants` keyed by the Keycloak `sub`. These suites prove
//! the grant axis: fail-closed with no grant, cross-project isolation, the `/api/me` identity
//! endpoint, and the admin grant-management endpoints with cache invalidation.
//!
//! All suites use REAL dev-Keycloak JWTs and auto-skip when Keycloak is unreachable. Run with the
//! dev stack up: `cargo test --test rbac -- --test-threads=1`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod grants_api;
mod me_sites;
mod project_isolation;
