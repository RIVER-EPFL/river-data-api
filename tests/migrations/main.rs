//! Integration tests for the migrations theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test migrations` or one suite with
//! `cargo test --test migrations <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod attribute_existing_readings;
mod backdate_auto_deployments;
mod channel_health_into_state;
mod one_doc_parameter;
mod source_parameter_instrument_names;
mod synthesise_curation_record;
