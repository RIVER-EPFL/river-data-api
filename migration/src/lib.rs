pub use sea_orm_migration::prelude::*;

mod m20260325_000001_init;
mod m20260420_000001_samples;
mod m20260420_000002_seed_constants;
mod m20260504_000001_fk_indexes;
mod m20260504_000002_derived_output_param;
mod m20260504_000003_drop_field_trips;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260325_000001_init::Migration),
            Box::new(m20260420_000001_samples::Migration),
            Box::new(m20260420_000002_seed_constants::Migration),
            Box::new(m20260504_000001_fk_indexes::Migration),
            Box::new(m20260504_000002_derived_output_param::Migration),
            Box::new(m20260504_000003_drop_field_trips::Migration),
        ]
    }
}
