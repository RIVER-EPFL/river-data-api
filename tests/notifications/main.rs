//! Integration tests for the notifications theme. Run the whole theme with
//! `cargo test --test notifications` or one suite with `cargo test --test notifications <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod admin_test;
mod capabilities_test;
mod dispatcher_test;
mod fanout_test;
mod me_test;
mod mute_gate;
mod triggers_test;
