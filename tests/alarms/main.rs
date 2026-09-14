//! Integration tests for the alarms theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test alarms` or one suite with
//! `cargo test --test alarms <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod backfill_reconstruction;
mod curation_and_evaluation;
mod event_driven_reconcile;
mod events_feed_and_summary;
mod export_summary_counts;
mod global_threshold_breach_consistency;
mod instrument_range_episodes;
mod threshold_lifecycle_and_state;
