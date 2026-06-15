pub use sea_orm_migration::prelude::*;

mod m20260325_000001_init;
mod m20260420_000001_samples;
mod m20260420_000002_seed_constants;
mod m20260504_000001_fk_indexes;
mod m20260504_000002_derived_output_param;
mod m20260504_000003_drop_field_trips;
mod m20260508_000001_exclude_flagged_from_aggregates;
mod m20260509_000001_reprocessing;
mod m20260511_000001_add_parameter_aliases;
mod m20260522_000001_reprocessing_jobs_optional_sensor;
mod m20260601_000001_simplify_public_exposure;
mod m20260603_000001_alarm_events;
mod m20260603_000002_job_retry_count;
mod m20260603_000003_parameter_nomenclature;
mod m20260603_000004_drop_alarm_type;
mod m20260603_000005_capture_sensor_serial;
mod m20260603_000006_deployment_twin;
mod m20260603_000007_sensor_dimension_aggregates;
mod m20260605_000001_public_slug_to_code;
mod m20260606_000001_api_token_security;
mod m20260608_000001_api_token_audit_log;
mod m20260608_000002_alarm_events_history_index;
mod m20260610_000001_reprocessing_jobs_status_index;
mod m20260610_000002_job_detail_and_logs;
mod m20260615_000001_notifications;
mod m20260615_000002_notification_state;

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
            Box::new(m20260508_000001_exclude_flagged_from_aggregates::Migration),
            Box::new(m20260509_000001_reprocessing::Migration),
            Box::new(m20260511_000001_add_parameter_aliases::Migration),
            Box::new(m20260522_000001_reprocessing_jobs_optional_sensor::Migration),
            Box::new(m20260601_000001_simplify_public_exposure::Migration),
            Box::new(m20260603_000001_alarm_events::Migration),
            Box::new(m20260603_000002_job_retry_count::Migration),
            Box::new(m20260603_000003_parameter_nomenclature::Migration),
            Box::new(m20260603_000004_drop_alarm_type::Migration),
            Box::new(m20260603_000005_capture_sensor_serial::Migration),
            Box::new(m20260603_000006_deployment_twin::Migration),
            Box::new(m20260603_000007_sensor_dimension_aggregates::Migration),
            Box::new(m20260605_000001_public_slug_to_code::Migration),
            Box::new(m20260606_000001_api_token_security::Migration),
            Box::new(m20260608_000001_api_token_audit_log::Migration),
            Box::new(m20260608_000002_alarm_events_history_index::Migration),
            Box::new(m20260610_000001_reprocessing_jobs_status_index::Migration),
            Box::new(m20260610_000002_job_detail_and_logs::Migration),
            Box::new(m20260615_000001_notifications::Migration),
            Box::new(m20260615_000002_notification_state::Migration),
        ]
    }
}
