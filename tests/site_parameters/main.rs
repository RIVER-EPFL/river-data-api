//! Integration tests for the site_parameters theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test site_parameters` or one suite with
//! `cargo test --test site_parameters <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod apply_group;
mod create_flags;
mod delete_guard;
mod minimal_create;
mod needs_review;
