//! Integration tests for the site_parameters theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test site_parameters` or one suite with
//! `cargo test --test site_parameters <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod minimal_create;
