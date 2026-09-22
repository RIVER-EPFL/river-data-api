//! Integration tests for the derived_parameters theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test derived_parameters` or one suite with
//! `cargo test --test derived_parameters <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod continuous_recompute_and_backfill;
mod excluded_inputs;
mod formula_transition;
mod formula_validation_and_crud;
mod formula_versions;
mod janitor_gap_filler;
mod lifecycle_define_assign_publish;
mod output_parameter_code;
mod preview_set;
mod shared_steps;
