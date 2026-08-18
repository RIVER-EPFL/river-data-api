//! Integration tests for the tools theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test tools` or one suite with
//! `cargo test --test tools <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod calculators;
mod constants_parity;
mod scripts_lifecycle;
