//! Integration tests for the readings theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test readings` or one suite with
//! `cargo test --test readings <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod aggregate_refresh_on_flag;
mod batch_standard_curve;
mod csv_import_column_resolution;
mod csv_import_family_guard;
mod csv_import_sessions;
mod csv_import_worker;
mod duplicate_slots;
mod grab_samples_insertion;
mod guarded_bulk_write;
mod ingest_dedup_and_visibility;
mod ingest_forms_samples;
mod ingest_standard_curves;
mod ingest_validation;
mod measurement_type_resolution;
mod replicate_index_resync;
mod sample_row_predicate;
mod write_path_admission;
