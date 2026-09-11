//! Integration tests for the migrations theme. Each submodule is one behaviour suite;
//! run the whole theme with `cargo test --test migrations` or one suite with
//! `cargo test --test migrations <module>`.

#[path = "../common/mod.rs"]
#[allow(dead_code, unused_imports)]
mod common;

mod attribute_existing_readings;
mod backdate_auto_deployments;
mod blank_database;
mod change_audit;
mod cutover_restore;
mod channel_health_into_state;
mod derived_definition_versions;
mod formula_name_units_not_null;
mod instrument_kind;
mod provenance_kind;
mod roll_back_live_pins;
mod sample_statistics;
mod site_parameter_entry_mode;
mod source_parameter_instrument_names;
mod stage_unpaired_readings;
mod synthesise_curation_record;
