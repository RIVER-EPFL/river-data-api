//! Integration tests for the readings theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test readings` or one suite with
//! `cargo test --test readings <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod csv_import_column_resolution;
mod grab_samples_insertion;
mod ingest_dedup_and_visibility;
