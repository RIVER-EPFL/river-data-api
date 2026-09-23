//! Integration tests for the operator actions. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test admin` or one suite with
//! `cargo test --test admin <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod merge_conflict_handling;
mod merge_parameters;
mod merge_rollup_refresh;
mod merge_site_parameters_job;
mod merge_visit_recompute;
mod reprocess_all_backdate;
mod slot_keyed_merge;
mod users;
