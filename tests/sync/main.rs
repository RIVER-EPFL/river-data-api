//! Integration tests for the sync theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test sync` or one suite with
//! `cargo test --test sync <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod control_plane;
mod pairing_plan_apply;
