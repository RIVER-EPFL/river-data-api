//! Integration tests for the sites theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sites` or one suite with
//! `cargo test --test sites <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod aggregate_and_readings_query_correctness;
mod aggregate_propagation;
mod aggregate_refresh_windows;
mod aggregate_sensor_split;
mod data_endpoints;
mod parameter_extents;
mod parameter_frequency;
mod refresh_gaps;
mod sensor_vs_grab_filters;
mod series_export_edges;
mod status_event_page_order;
mod subprojects;
