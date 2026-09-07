//! Integration tests for the sync theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sync` or one suite with
//! `cargo test --test sync <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod annotations_register;
mod bulk_pair;
mod control_plane;
mod credential_listing_gate;
mod credentials;
mod duplicate_slots;
mod hold_kinds;
mod notes_register;
mod pagination;
mod pagination_bounds;
mod pairing_plan_apply;
mod pairing_plan_hardening;
mod plan_instrument_decisions;
mod plan_review_progress;
mod replicate_audit;
mod replicate_flag_indexes;
mod routes_surface;
