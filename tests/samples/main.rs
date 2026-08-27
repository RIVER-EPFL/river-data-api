//! Integration tests for the samples theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test samples` or one suite with
//! `cargo test --test samples <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod label_notes;
mod provenance;
mod replicate_lifecycle;
mod tool_statistics_parity;
mod trigger_aggregates;
