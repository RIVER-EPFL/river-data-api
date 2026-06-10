//! Integration tests for the status_events theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test status_events` or one suite with
//! `cargo test --test status_events <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod csv_quoting_and_formats;
mod pagination;
