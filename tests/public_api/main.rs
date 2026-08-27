//! Integration tests for the public_api theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test public_api` or one suite with
//! `cargo test --test public_api <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod export_parity;
mod exposure_lockdown;
mod measurement_type_filter;
mod read_only_endpoints;
mod replicate_determinism;
mod served_instant_selection;
