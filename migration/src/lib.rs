pub use sea_orm_migration::prelude::*;

mod m20260905_000001_baseline;
mod m20260911_000001_realtime_rollup_head;
mod m20260911_000002_spot_param_sensor_index;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260905_000001_baseline::Migration),
            Box::new(m20260911_000001_realtime_rollup_head::Migration),
            Box::new(m20260911_000002_spot_param_sensor_index::Migration),
        ]
    }
}
