//! Integration tests for the tools theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test tools` or one suite with
//! `cargo test --test tools <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod calculators;
mod constants_parity;
mod draft_run;
mod output_parameters;
mod prelude_boundary;
mod run_contract;
mod runner_absent;
mod script_inspection;
mod scripts_authoring;
mod scripts_lifecycle;
mod seeded_cases;
