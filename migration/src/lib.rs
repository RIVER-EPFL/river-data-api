pub use sea_orm_migration::prelude::*;

mod m20260905_000001_baseline;
mod m20260914_000001_member_source_calculation;
mod m20260914_000002_sensor_range;
mod m20260914_000003_alarm_event_kind;
mod m20260914_000004_six_and_twelve_hour_rollups;
mod m20260914_000005_meteoswiss_subscriptions;
mod m20260914_000006_meteoswiss_subscription_parameter;
mod m20260914_000007_meteoswiss_fetch_state;
mod m20260914_000008_csv_import_chunks;

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
            Box::new(m20260914_000005_meteoswiss_subscriptions::Migration),
            Box::new(m20260914_000006_meteoswiss_subscription_parameter::Migration),
            Box::new(m20260914_000007_meteoswiss_fetch_state::Migration),
            Box::new(m20260914_000008_csv_import_chunks::Migration),
        ]
    }
}
