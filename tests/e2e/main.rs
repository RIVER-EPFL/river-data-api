//! Integration tests for the e2e theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test e2e` or one suite with
//! `cargo test --test e2e <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod aggregation_and_paired_series;
mod alarm_annotation_note_lifecycle;
mod atomicity_and_compression;
mod calibration_spanning_two_sites;
mod collection_event_chain;
mod csv_as_tool_entry;
mod deployment_backdate_two_sites;
mod deployment_slot_and_recall;
mod full_public_data_workflow;
mod grab_aggregation_and_tool_save;
mod ingest_and_pairing_attribution;
mod instrument_grab_alongside_sensor;
mod onboarding_tracks;
mod pairing_plan_lifecycle;
mod pairing_wizard_patch_apply;
mod portal_curve_instrument;
mod portal_migration_wizard;
mod provision_to_public;
mod replicate_sync_flow;
mod seasonal_check_gate;
mod sensor_backfill_attribution;
mod sensor_deploy_move_recall;
mod sensor_ui_lifecycle;
mod site_parameter_merge_flag_alarm;
mod status_search_export_comparison;
mod stream_discovery_pairing;
mod sync_parity;
mod tool_run_provenance;
mod tools_grab_export;
mod windowed_diff;
