use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

pub async fn setup_test_db() -> DatabaseConnection {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let db = Database::connect(&url)
        .await
        .expect("Failed to connect to test database");

    migration::Migrator::up(&db, None)
        .await
        .expect("Failed to run migrations");

    db
}

pub async fn cleanup_test_db(db: &DatabaseConnection) {
    let stmts = [
        "SELECT remove_continuous_aggregate_policy('readings_monthly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_weekly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_daily', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_hourly', if_not_exists => true)",
        "TRUNCATE readings, status_events, samples, \
         sync_service_tokens, sync_events, sync_commands, sync_services, sync_service_credentials, \
         pairing_plans, data_streams, \
         reprocessing_jobs, schedules, schedule_audit, csv_import_staging, \
         annotations, notes, \
         alarm_thresholds, alarm_events, api_tokens, \
         telegram_identities, notification_mutes, notification_log, notification_state, \
         notification_subscribers, notification_subscriptions, notification_channel_health, \
         sensor_calibrations, sensor_deployments, sensors, \
         derived_parameter_sources, derived_parameter_definitions, \
         constants, \
         user_project_grants, \
         site_parameters, parameters, sites, subprojects, projects CASCADE",
    ];

    for sql in &stmts {
        let _ = db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                sql.to_string(),
            ))
            .await;
    }
}

pub async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}
