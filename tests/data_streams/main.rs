//! Integration tests for the data_streams theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test data_streams` or one suite with
//! `cargo test --test data_streams <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod measurement_classification;
mod pair_replicate_samples;
mod register_declares_instrument;
mod register_pair_stats;
mod replicate_retag_guard;
mod slot_retirement;
