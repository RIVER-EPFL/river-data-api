//! Integration tests for the cache theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test cache` or one suite with
//! `cargo test --test cache <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod key_and_invalidation;
mod key_generation;
