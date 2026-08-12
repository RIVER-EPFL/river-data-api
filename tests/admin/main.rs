//! Integration tests for the admin theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test admin` or one suite with
//! `cargo test --test admin <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod merge_conflict_handling;
mod merge_parameters;
mod merge_site_parameters_job;
mod reprocess_all_backdate;
mod slot_keyed_merge;
mod users;
