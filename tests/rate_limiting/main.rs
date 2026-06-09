//! Integration tests for the rate_limiting theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test rate_limiting` or one suite with
//! `cargo test --test rate_limiting <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod governor_enforcement;
