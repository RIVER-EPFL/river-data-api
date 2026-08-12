//! Integration tests for the test harness itself. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test harness` or one suite with
//! `cargo test --test harness <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod cache_builder;
mod compression;
