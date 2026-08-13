use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Remove refresh policies before dropping views
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_monthly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_weekly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_daily', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_hourly', if_exists => true)",
        )
        .await?;

        // Drop in reverse dependency order
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_monthly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_weekly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_daily CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_hourly CASCADE")
            .await?;

        // Recreate with is_flagged filter
        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE)
            GROUP BY time_bucket('1 hour', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_daily
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 day', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE)
            GROUP BY time_bucket('1 day', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_weekly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 week', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE)
            GROUP BY time_bucket('1 week', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_monthly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 month', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE)
            GROUP BY time_bucket('1 month', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        // Re-add refresh policies
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_hourly',
                start_offset => INTERVAL '3 hours',
                end_offset => INTERVAL '1 hour',
                schedule_interval => INTERVAL '1 hour')",
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_daily',
                start_offset => INTERVAL '3 days',
                end_offset => INTERVAL '1 day',
                schedule_interval => INTERVAL '1 day')",
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_weekly',
                start_offset => INTERVAL '3 weeks',
                end_offset => INTERVAL '1 week',
                schedule_interval => INTERVAL '1 week')",
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_monthly',
                start_offset => INTERVAL '3 months',
                end_offset => INTERVAL '1 month',
                schedule_interval => INTERVAL '1 month')",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Remove policies, drop, and recreate without is_flagged filter
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_monthly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_weekly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_daily', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_hourly', if_exists => true)",
        )
        .await?;

        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_monthly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_weekly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_daily CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_hourly CASCADE")
            .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 hour', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_daily
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 day', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 day', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_weekly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 week', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 week', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW readings_monthly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 month', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 month', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_hourly',
                start_offset => INTERVAL '3 hours',
                end_offset => INTERVAL '1 hour',
                schedule_interval => INTERVAL '1 hour')",
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_daily',
                start_offset => INTERVAL '3 days',
                end_offset => INTERVAL '1 day',
                schedule_interval => INTERVAL '1 day')",
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_weekly',
                start_offset => INTERVAL '3 weeks',
                end_offset => INTERVAL '1 week',
                schedule_interval => INTERVAL '1 week')",
        )
        .await?;

        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_monthly',
                start_offset => INTERVAL '3 months',
                end_offset => INTERVAL '1 month',
                schedule_interval => INTERVAL '1 month')",
        )
        .await?;

        Ok(())
    }
}
