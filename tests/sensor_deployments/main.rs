//! Integration tests for the sensor_deployments theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sensor_deployments` or one suite with
//! `cargo test --test sensor_deployments <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod api_serialization_and_filter;
mod lifecycle_rules;
mod rollback_reopens_previous;
