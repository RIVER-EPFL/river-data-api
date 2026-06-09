//! Integration tests for the projects theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test projects` or one suite with
//! `cargo test --test projects <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod metadata_crud;
