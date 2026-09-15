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
mod m20260914_000009_meteoswiss_stations;
mod m20260914_000010_shared_steps;
mod m20260915_000001_drop_member_role;
mod m20260915_000002_supersede_synced_visit_findings;
mod m20260915_000003_visit_verification;
mod m20260915_000004_drop_calculation_group;

pub struct Migrator;

/// Describe the rebuild required when recorded migrations are absent.
pub fn startup_error(error: DbErr) -> DbErr {
    if matches!(&error, DbErr::Custom(message) if message.starts_with("Migration file of version '") && message.contains("this migration has been applied but its file is missing"))
    {
        return DbErr::Custom("Database records migrations absent from this checkout; after a baseline flatten, back up anything to preserve for restore_cutover, then rebuild the dev volume from river-data-ui with `docker compose down -v && docker compose up -d` (deletes compose volumes).".into());
    }
    error
}

#[cfg(test)]
#[path = "tests/startup.rs"]
mod tests;

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
            Box::new(m20260914_000009_meteoswiss_stations::Migration),
            Box::new(m20260914_000010_shared_steps::Migration),
            Box::new(m20260915_000001_drop_member_role::Migration),
            Box::new(m20260915_000002_supersede_synced_visit_findings::Migration),
            Box::new(m20260915_000003_visit_verification::Migration),
            Box::new(m20260915_000004_drop_calculation_group::Migration),
        ]
    }
}
