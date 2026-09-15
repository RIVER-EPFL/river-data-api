//! Integration tests for the e2e theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test e2e` or one suite with
//! `cargo test --test e2e <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod alarm_annotation_note_lifecycle;
mod atomicity_and_compression;
mod calibration_spanning_two_sites;
mod collection_event_chain;
mod csv_as_tool_entry;
mod deployment_backdate_two_sites;
mod formula_save_arms;
mod full_public_data_workflow;
mod ingest_and_pairing_attribution;
mod instrument_grab_alongside_sensor;
mod onboarding_tracks;
mod pairing_plan_lifecycle;
mod portal_curve_instrument;
mod portal_full_sync_command;
mod portal_import_to_paired;
mod portal_loop;
mod portal_station_pairing;
mod provision_to_public;
mod reactive_recompute;
mod replicate_sync_flow;
mod replicates_as_readings;
mod scoped_recompute;
mod sd_estimator_declaration;
mod seasonal_check_gate;
mod sensor_ui_lifecycle;
mod status_search_export_comparison;
mod sync_parity;
mod tool_run_provenance;
mod tools_grab_export;
mod windowed_diff;
