pub use sea_orm_migration::prelude::*;

pub mod tool_hash;
pub mod tool_prelude;

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
mod m20260620_000001_notification_subscriptions;
mod m20260620_000002_notification_health;
mod m20260621_000001_job_worker_pool;
mod m20260622_000001_schedules;
mod m20260623_000001_schedule_audit;
mod m20260623_000002_csv_import_staging;
mod m20260709_000001_user_project_grants;
mod m20260710_000001_subprojects;
mod m20260711_000001_unified_curve_columns;
mod m20260711_000002_aggregates_exclude_grabs;
mod m20260711_000003_decouple_sensor_parameter;
mod m20260711_000004_inherit_calibration_parameter;
mod m20260711_000005_drop_standard_curves;
mod m20260711_000006_inherit_windowed_only;
mod m20260711_000007_subproject_move_cascade;
mod m20260713_000001_data_frequency;
mod m20260713_000002_aggregates_include_derived;
mod m20260811_000001_portal_constants;
mod m20260811_100001_samples_unique;
mod m20260811_100002_csv_staging_seq;
mod m20260812_000001_sync_service_paused;
mod m20260812_000003_alarm_event_cadence;
mod m20260812_000004_sync_command_expiry_not_null;
mod m20260813_000001_calibration_valid_until_explicit;
mod m20260813_000003_standard_curves;
mod m20260813_000004_readings_standard_curve_fk;
mod m20260813_000005_sample_trigger_predicate;
mod m20260813_000006_readings_calibration_id_index;
mod m20260813_000007_retire_identity_calibrations;
mod m20260814_000001_reprocessing_jobs_autovacuum;
mod m20260814_000002_sync_event_readings_skipped;
mod m20260814_000003_pair_api_streams;
mod m20260814_000004_telegram_link_expiry;
mod m20260817_000001_telegram_command_audit;
mod m20260817_000002_scrub_telegram_urls;
mod m20260817_000003_telegram_attestation;
mod m20260818_000002_ch4_in_sa_units;
mod m20260818_000003_tool_scripts;
mod m20260818_000004_sample_provenance;
mod m20260818_000005_seed_tool_scripts;
mod m20260818_000006_analyte_catalog;
mod m20260818_000007_tool_script_version_note;
mod m20260818_000008_tool_output_analytes;
mod m20260820_000001_standard_curve_provenance;
mod m20260820_000002_replicate_audit_holds;
mod m20260820_000003_audit_hold_resolutions;
mod m20260821_000001_audit_hold_deferred;
mod m20260821_000002_audit_hold_resolution;
mod m20260827_000001_spot_partial_indexes;
mod m20260827_000002_tool_runs;
mod m20260828_000001_collection_events;
mod m20260828_000002_windowed_diff;

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
            Box::new(m20260620_000001_notification_subscriptions::Migration),
            Box::new(m20260620_000002_notification_health::Migration),
            Box::new(m20260621_000001_job_worker_pool::Migration),
            Box::new(m20260622_000001_schedules::Migration),
            Box::new(m20260623_000001_schedule_audit::Migration),
            Box::new(m20260623_000002_csv_import_staging::Migration),
            Box::new(m20260709_000001_user_project_grants::Migration),
            Box::new(m20260710_000001_subprojects::Migration),
            Box::new(m20260711_000001_unified_curve_columns::Migration),
            Box::new(m20260711_000002_aggregates_exclude_grabs::Migration),
            Box::new(m20260711_000003_decouple_sensor_parameter::Migration),
            Box::new(m20260711_000004_inherit_calibration_parameter::Migration),
            Box::new(m20260711_000005_drop_standard_curves::Migration),
            Box::new(m20260711_000006_inherit_windowed_only::Migration),
            Box::new(m20260711_000007_subproject_move_cascade::Migration),
            Box::new(m20260713_000001_data_frequency::Migration),
            Box::new(m20260713_000002_aggregates_include_derived::Migration),
            Box::new(m20260811_000001_portal_constants::Migration),
            Box::new(m20260811_100001_samples_unique::Migration),
            Box::new(m20260811_100002_csv_staging_seq::Migration),
            Box::new(m20260812_000001_sync_service_paused::Migration),
            Box::new(m20260812_000003_alarm_event_cadence::Migration),
            Box::new(m20260812_000004_sync_command_expiry_not_null::Migration),
            Box::new(m20260813_000001_calibration_valid_until_explicit::Migration),
            Box::new(m20260813_000003_standard_curves::Migration),
            Box::new(m20260813_000004_readings_standard_curve_fk::Migration),
            Box::new(m20260813_000005_sample_trigger_predicate::Migration),
            // The index first: the delete below is what needs it.
            Box::new(m20260813_000006_readings_calibration_id_index::Migration),
            Box::new(m20260813_000007_retire_identity_calibrations::Migration),
            Box::new(m20260814_000001_reprocessing_jobs_autovacuum::Migration),
            Box::new(m20260814_000002_sync_event_readings_skipped::Migration),
            Box::new(m20260814_000003_pair_api_streams::Migration),
            Box::new(m20260814_000004_telegram_link_expiry::Migration),
            Box::new(m20260817_000001_telegram_command_audit::Migration),
            Box::new(m20260817_000002_scrub_telegram_urls::Migration),
            Box::new(m20260817_000003_telegram_attestation::Migration),
            Box::new(m20260818_000002_ch4_in_sa_units::Migration),
            Box::new(m20260818_000003_tool_scripts::Migration),
            Box::new(m20260818_000004_sample_provenance::Migration),
            Box::new(m20260818_000005_seed_tool_scripts::Migration),
            Box::new(m20260818_000006_analyte_catalog::Migration),
            Box::new(m20260818_000007_tool_script_version_note::Migration),
            Box::new(m20260818_000008_tool_output_analytes::Migration),
            Box::new(m20260820_000001_standard_curve_provenance::Migration),
            Box::new(m20260820_000002_replicate_audit_holds::Migration),
            Box::new(m20260820_000003_audit_hold_resolutions::Migration),
            Box::new(m20260821_000001_audit_hold_deferred::Migration),
            Box::new(m20260821_000002_audit_hold_resolution::Migration),
            Box::new(m20260827_000001_spot_partial_indexes::Migration),
            Box::new(m20260827_000002_tool_runs::Migration),
            Box::new(m20260828_000001_collection_events::Migration),
            Box::new(m20260828_000002_windowed_diff::Migration),
        ]
    }
}
