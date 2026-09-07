//! Integration tests for the readings theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test readings` or one suite with
//! `cargo test --test readings <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod aggregate_refresh_on_flag;
mod attribution_order;
mod batch_overwrite_count;
mod batch_standard_curve;
mod collection_events;
mod csv_import_column_resolution;
mod csv_import_recompute;
mod csv_import_family_guard;
mod csv_import_overwrite_tail;
mod csv_import_seasonal_gate;
mod csv_import_sessions;
mod csv_import_tool_curves;
mod csv_import_worker;
mod decisions;
mod edits;
mod flag_range_dry_run;
mod grab_replace_scope;
mod grab_samples_insertion;
mod guarded_bulk_write;
mod ingest_dedup_and_visibility;
mod ingest_forms_samples;
mod ingested_at_restamp;
mod ingest_standard_curves;
mod ingest_validation;
mod instrument_required;
mod measurement_type_resolution;
mod provenance;
mod replicate_index_resync;
mod rollback_propagation;
mod sample_preview;
mod sample_row_predicate;
mod seasonal_check;
mod slot_instrument_declaration;
mod spot_instant_shape;
mod stream_receipts;
mod visits;
mod write_path_admission;
