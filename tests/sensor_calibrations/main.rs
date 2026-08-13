//! Integration tests for the sensor_calibrations theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sensor_calibrations` or one suite with
//! `cargo test --test sensor_calibrations <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod application_gaps;
mod formula_evaluation;
mod forward_new_data;
mod grab_recomposition;
mod historical_edit;
mod reprocessing;
mod standard_curves;
mod window_boundaries;
mod window_invariants;
