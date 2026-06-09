//! Integration tests for the alarm_thresholds theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test alarm_thresholds` or one suite with
//! `cargo test --test alarm_thresholds <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod dedup_and_null_bounds;
mod default_fallback_chain;
mod no_autocreate_shadowing;
