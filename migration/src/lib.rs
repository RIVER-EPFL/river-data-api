pub use sea_orm_migration::prelude::*;

mod m20260317_000001_init;
mod m20260323_000001_seed_analytical_parameters;
mod m20260323_000002_sensor_wiring;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260317_000001_init::Migration),
            Box::new(m20260323_000001_seed_analytical_parameters::Migration),
            Box::new(m20260323_000002_sensor_wiring::Migration),
        ]
    }
}
