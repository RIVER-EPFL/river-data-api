//! Integration tests for the notifications theme. Run the whole theme with
//! `cargo test --test notifications` or one suite with `cargo test --test notifications <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod access_test;
mod admin_test;
mod anti_backdoor_test;
mod audit_test;
mod capabilities_test;
mod command_scope_test;
mod dispatcher_test;
mod fanout_test;
mod grab_test;
mod link_expiry_test;
mod link_flow_test;
mod me_test;
mod menu_commands;
mod mute_gate;
mod plot_command;
mod thresholds_command;
mod triggers_test;
