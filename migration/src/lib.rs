pub use sea_orm_migration::prelude::*;

mod m20260128_000001_init;
mod m20260311_000002_upgrade_schema;
mod m20260312_000003_sync_control_plane;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260128_000001_init::Migration),
            Box::new(m20260311_000002_upgrade_schema::Migration),
            Box::new(m20260312_000003_sync_control_plane::Migration),
        ]
    }
}
