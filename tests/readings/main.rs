//! Integration tests for the readings theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test readings` or one suite with
//! `cargo test --test readings <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod aggregate_refresh_on_flag;
mod csv_import_column_resolution;
mod csv_import_worker;
mod grab_samples_insertion;
mod ingest_dedup_and_visibility;
mod measurement_type_resolution;
