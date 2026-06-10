//! Integration tests for the reprocessing_jobs theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test reprocessing_jobs` or one suite with
//! `cargo test --test reprocessing_jobs <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod job_logs_and_detail;
mod job_tracking_on_actions;
mod retry_backoff;
mod startup_sweep;
