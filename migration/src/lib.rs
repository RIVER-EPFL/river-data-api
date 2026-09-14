pub use sea_orm_migration::prelude::*;

mod m20260905_000001_baseline;
mod m20260914_000001_member_source_calculation;
mod m20260914_000002_sensor_range;
mod m20260914_000003_alarm_event_kind;
mod m20260914_000004_six_and_twelve_hour_rollups;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260905_000001_baseline::Migration),
            Box::new(m20260914_000001_member_source_calculation::Migration),
            Box::new(m20260914_000002_sensor_range::Migration),
            Box::new(m20260914_000003_alarm_event_kind::Migration),
            Box::new(m20260914_000004_six_and_twelve_hour_rollups::Migration),
        ]
    }
}
