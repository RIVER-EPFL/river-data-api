pub use sea_orm_migration::prelude::*;

mod m20260325_000001_init;
mod m20260420_000001_samples;
mod m20260420_000002_seed_constants;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260325_000001_init::Migration),
            Box::new(m20260420_000001_samples::Migration),
            Box::new(m20260420_000002_seed_constants::Migration),
        ]
    }
}
