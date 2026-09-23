//! Integration tests for the data_streams theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test data_streams` or one suite with
//! `cargo test --test data_streams <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod measurement_classification;
mod meteoswiss_provisioning;
mod nomis_pairing_refused;
mod pair_backfill_memory;
mod pair_opens_at_history;
mod pair_replicate_samples;
mod pair_visit_recompute;
mod register_declares_instrument;
mod register_pair_stats;
mod replicate_retag_guard;
mod replicate_spec_pinning;
mod slot_retirement;
mod unpair_deployment_scope;
