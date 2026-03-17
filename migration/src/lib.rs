pub use sea_orm_migration::prelude::*;

mod m20260128_000001_init;
mod m20260311_000002_upgrade_schema;
mod m20260316_000003_sync_events;
mod m20260316_000004_sync_events_log;
mod m20260316_000005_audit_schema;
mod m20260316_000006_source_mappings_pk;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260128_000001_init::Migration),
            Box::new(m20260311_000002_upgrade_schema::Migration),
            Box::new(m20260316_000003_sync_events::Migration),
            Box::new(m20260316_000004_sync_events_log::Migration),
            Box::new(m20260316_000005_audit_schema::Migration),
            Box::new(m20260316_000006_source_mappings_pk::Migration),
        ]
    }
}
