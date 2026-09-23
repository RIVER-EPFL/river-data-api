//! Integration tests for the migrations theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test migrations` or one suite with
//! `cargo test --test migrations <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod blank_database;
mod cutover_restore;
mod entity_revisions;
mod manifest_sd_estimator_strip;
mod reraise_unverified_entries;
mod rollup_policies;
mod slot_cadence_backfill;
