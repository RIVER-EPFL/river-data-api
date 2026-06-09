//! Integration tests for the events theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test events` or one suite with
//! `cargo test --test events <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod sse_event_stream;
