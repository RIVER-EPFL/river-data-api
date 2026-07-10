//! Integration tests for the e2e theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test e2e` or one suite with
//! `cargo test --test e2e <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod aggregation_and_paired_series;
mod alarm_annotation_note_lifecycle;
mod deployment_slot_and_recall;
mod full_public_data_workflow;
mod grab_aggregation_and_tool_save;
mod ingest_and_pairing_attribution;
mod instrument_grab_alongside_sensor;
mod pairing_plan_lifecycle;
mod provision_to_public;
mod sensor_backfill_attribution;
mod sensor_deploy_move_recall;
mod site_parameter_merge_flag_alarm;
mod status_search_export_comparison;
mod stream_discovery_pairing;
