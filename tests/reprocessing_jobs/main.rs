//! Integration tests for the reprocessing_jobs theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test reprocessing_jobs` or one suite with
//! `cargo test --test reprocessing_jobs <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod alarm_backfill_slots;
mod cancel;
mod failure_events;
mod job_log_tail;
mod job_logs_and_detail;
mod job_tracking_on_actions;
mod measurement_retag;
mod rerun;
mod retention;
mod retry_backoff;
mod schedule_control;
mod schedule_routes;
mod scheduler;
mod shutdown_drain;
mod sync_full_reassert;
mod sync_maintenance;
mod worker_pool;
mod worker_timeline;
