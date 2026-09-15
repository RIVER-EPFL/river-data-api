//! Integration tests for the parameters theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test parameters` or one suite with
//! `cargo test --test parameters <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod change_audit;
mod given_up_output;
mod groups;
