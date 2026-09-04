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
mod m20260908_000001_parameter_groups;

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
            Box::new(m20260908_000001_parameter_groups::Migration),
        ]
    }
}
