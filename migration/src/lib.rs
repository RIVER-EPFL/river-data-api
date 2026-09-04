pub use sea_orm_migration::prelude::*;

mod m20260905_000001_baseline;
mod m20260905_000002_pairing_plan_version;
pub mod m20260906_000001_attribute_existing_readings;
mod m20260906_000002_instrument_required_untyped;
mod m20260907_000001_full_reassert_per_service;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260905_000001_baseline::Migration),
            Box::new(m20260905_000002_pairing_plan_version::Migration),
            Box::new(m20260906_000001_attribute_existing_readings::Migration),
            Box::new(m20260906_000002_instrument_required_untyped::Migration),
            Box::new(m20260907_000001_full_reassert_per_service::Migration),
        ]
    }
}
