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
        "TRUNCATE alarm_thresholds, sensor_calibrations, sensor_deployments, \
         readings, status_events, public_exposed_parameters, api_tokens, \
         site_parameters, sensors, derived_parameter_sources, derived_parameter_definitions, \
         parameters, sites, projects, data_streams, sync_services, sync_commands, sync_events, \
         sync_service_credentials, sync_service_tokens, annotations, notes, field_trips, \
         constants, standard_curves, samples, pairing_plans CASCADE",
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
