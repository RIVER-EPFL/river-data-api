//! Integration tests for the alarms theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test alarms` or one suite with
//! `cargo test --test alarms <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod backfill_reconstruction;
mod event_driven_reconcile;
mod export_summary_counts;
mod events_feed_and_summary;
mod parameter_default_breach_consistency;
mod threshold_lifecycle_and_state;
