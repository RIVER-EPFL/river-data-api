//! Integration tests for the notifications theme. Run the whole theme with
//! `cargo test --test notifications` or one suite with `cargo test --test notifications <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod anti_backdoor_test;
mod dispatcher_test;
mod grab_test;
mod link_flow_test;
