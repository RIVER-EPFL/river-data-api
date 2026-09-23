//! Integration tests for the public_api theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test public_api` or one suite with
//! `cargo test --test public_api <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod declared_precision;
mod docs_version;
mod export_double_roundtrip;
mod export_parity;
mod exposure_lockdown;
mod measurement_type_filter;
mod read_only_endpoints;
mod replicate_determinism;
mod sample_stats_annotation;
mod served_instant_selection;
mod two_stream_instants;
mod unverified_entries;
