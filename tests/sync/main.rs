//! Integration tests for the sync theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sync` or one suite with
//! `cargo test --test sync <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod annotations_register;
mod catalog_usage;
mod change_proposals;
mod control_plane;
mod control_plane_client;
mod credential_listing_gate;
mod credentials;
mod curve_proposals;
mod fake_portal_cycle;
mod hold_delta_expressions;
mod hold_kinds;
mod hold_list_statements;
mod notes_register;
mod pairing_backfill_parity;
mod pairing_plan_apply;
mod pairing_plan_hardening;
mod pairing_plan_resolution;
mod plan_instrument_decisions;
mod plan_review_progress;
mod replicate_audit;
mod replicate_flag_indexes;
mod routes_surface;
