//! Integration tests for the tools theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test tools` or one suite with
//! `cargo test --test tools <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod apply_calculation;
mod audit_applicability;
mod calculation_closure;
mod calculators;
mod cnet_authoring;
mod concurrent_version_saves;
mod constants_parity;
mod decommission;
mod decommissioned_run;
mod draft_run;
mod formula_calculation;
mod formula_draft_run;
mod group_calculations;
mod na_clears_output;
mod output_parameters;
mod partial_skip;
mod pending_inputs_inherit;
mod replicate_family_inputs;
mod run_contract;
mod run_trace;
mod runner_absent;
mod scripts_authoring;
mod scripts_lifecycle;
mod seeded_cases;
mod seeded_version_hashes;
mod skipped_output;
mod staged_preview;
mod two_stage_calculation;
mod unassigned_site;
mod version_usage;
