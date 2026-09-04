//! Integration tests for the tools theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test tools` or one suite with
//! `cargo test --test tools <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod calculation_closure;
mod calculators;
mod constants_parity;
mod draft_run;
mod na_clears_output;
mod output_parameters;
mod run_contract;
mod runner_absent;
mod scripts_authoring;
mod scripts_lifecycle;
mod seeded_cases;
