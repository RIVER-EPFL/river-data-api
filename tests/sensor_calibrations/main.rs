//! Integration tests for the sensor_calibrations theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sensor_calibrations` or one suite with
//! `cargo test --test sensor_calibrations <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod formula_evaluation;
mod reprocessing;
