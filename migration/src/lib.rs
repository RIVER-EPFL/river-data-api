pub use sea_orm_migration::prelude::*;

mod m20260905_000001_baseline;
mod m20260905_000002_pairing_plan_version;
pub mod m20260906_000001_attribute_existing_readings;
mod m20260906_000002_instrument_required_untyped;
mod m20260906_000003_note_provenance;
mod m20260907_000001_full_reassert_per_service;
pub mod m20260907_000002_backdate_auto_deployments;
mod m20260907_000003_sample_statistics;
mod m20260907_000004_unverified_entries;
mod m20260907_000005_instrument_kind;
pub mod m20260907_000006_synthesise_curation_record;
mod m20260907_000007_meteoswiss_pressure;
mod m20260908_000001_parameter_groups;
mod m20260908_000002_calculation_engines;
mod m20260908_000003_source_identity_hold_uniq;
mod m20260908_000004_notification_kind_groups;
mod m20260908_000005_curve_fitted_on;
pub mod m20260908_000006_one_doc_parameter;
pub mod m20260908_000007_seed_portal_parameter_groups;
pub mod m20260908_000008_channel_health_into_state;
mod m20260909_000001_seed_metalp_parameter_groups;
mod m20260910_000001_change_audit;
mod m20260910_000002_parameter_default_thresholds;
mod m20260910_000003_generated_sample_stdev;
mod m20260910_000004_rollback_restores_ingested_at;
pub mod m20260910_000005_source_parameter_instrument_names;
pub mod m20260910_000006_provenance_kind;
pub mod m20260910_000007_site_parameter_entry_mode;
pub mod m20260910_000008_formula_curve_slot;
mod m20260910_000009_slot_instrument;
pub mod m20260910_000010_formula_per_replicate;
pub mod portal_seed;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260905_000001_baseline::Migration),
            Box::new(m20260905_000002_pairing_plan_version::Migration),
            Box::new(m20260906_000001_attribute_existing_readings::Migration),
            Box::new(m20260906_000002_instrument_required_untyped::Migration),
            Box::new(m20260906_000003_note_provenance::Migration),
            Box::new(m20260907_000001_full_reassert_per_service::Migration),
            Box::new(m20260907_000002_backdate_auto_deployments::Migration),
            Box::new(m20260907_000003_sample_statistics::Migration),
            Box::new(m20260907_000004_unverified_entries::Migration),
            Box::new(m20260907_000005_instrument_kind::Migration),
            Box::new(m20260907_000006_synthesise_curation_record::Migration),
            Box::new(m20260907_000007_meteoswiss_pressure::Migration),
            Box::new(m20260908_000001_parameter_groups::Migration),
            Box::new(m20260908_000002_calculation_engines::Migration),
            Box::new(m20260908_000003_source_identity_hold_uniq::Migration),
            Box::new(m20260908_000004_notification_kind_groups::Migration),
            Box::new(m20260908_000005_curve_fitted_on::Migration),
            Box::new(m20260908_000006_one_doc_parameter::Migration),
            Box::new(m20260908_000007_seed_portal_parameter_groups::Migration),
            Box::new(m20260908_000008_channel_health_into_state::Migration),
            Box::new(m20260909_000001_seed_metalp_parameter_groups::Migration),
            Box::new(m20260910_000001_change_audit::Migration),
            Box::new(m20260910_000002_parameter_default_thresholds::Migration),
            Box::new(m20260910_000003_generated_sample_stdev::Migration),
            Box::new(m20260910_000004_rollback_restores_ingested_at::Migration),
            Box::new(m20260910_000005_source_parameter_instrument_names::Migration),
            Box::new(m20260910_000006_provenance_kind::Migration),
            Box::new(m20260910_000007_site_parameter_entry_mode::Migration),
            Box::new(m20260910_000008_formula_curve_slot::Migration),
            Box::new(m20260910_000009_slot_instrument::Migration),
            Box::new(m20260910_000010_formula_per_replicate::Migration),
        ]
    }
}
