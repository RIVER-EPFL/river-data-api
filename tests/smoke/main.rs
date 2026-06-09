//! Integration tests for the smoke theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test smoke` or one suite with
//! `cargo test --test smoke <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod infra_seed_and_healthz;
