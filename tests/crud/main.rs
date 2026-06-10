//! Integration tests for the crud theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test crud` or one suite with
//! `cargo test --test crud <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod delete_cascade_and_constraints;
