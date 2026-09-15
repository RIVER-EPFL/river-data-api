//! Integration tests for the sensors theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sensors` or one suite with
//! `cargo test --test sensors <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod adopt_swap_lifecycle;
mod instrument_kinds;
mod instrument_proposals;
mod instruments_overview;
mod lab_instrument_row;
mod last_used_by_parameter;
mod list_latest_reading;
mod manufacturer_range;
mod multi_parameter_channel;
mod read_endpoints;
mod standard_curve_register;
mod swap_reattributes;
