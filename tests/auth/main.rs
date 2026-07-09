//! Integration tests for the auth theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test auth` or one suite with
//! `cargo test --test auth <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod access_gate;
mod capability_levels;
mod keycloak_jwt_capability;
mod malformed_and_revoked_rejection;
mod middleware_and_permissions;
mod permission_matrix;
mod project_scope_isolation;
mod token_capability_roundtrip;
mod token_expiry;
mod token_lifecycle;
mod token_permission_combinations;
mod token_rate_limit;
mod token_scope_and_audit;
mod token_scope_confinement;
