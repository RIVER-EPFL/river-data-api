//! Integration tests for the sensors theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sensors` or one suite with
//! `cargo test --test sensors <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod import_adopt_swap_lifecycle;
mod multi_parameter_channel;
mod read_endpoints;
mod swap_reattributes;
