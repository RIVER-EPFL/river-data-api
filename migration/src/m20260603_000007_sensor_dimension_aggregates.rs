use sea_orm_migration::prelude::*;

/// Rebuild readings_{hourly,daily,weekly,monthly} continuous aggregates with sensor_id added to the
/// grouping key: (bucket, site_id, parameter_id, sensor_id). This lets overlays split a
/// site/parameter series by sensor at any resolution, while the default site read re-aggregates
/// across sensors (sum count, count-weighted avg via sum_value/count, MIN/MAX, pooled variance via
/// sum_value/sum_sq_value). Adds per-sensor aggregate indexes (sensor_id, bucket DESC) per CAGG.
///
/// Semantics preserved verbatim from m20260508 (exclude_flagged):
///   WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE)
///   value = COALESCE(calibrated_value, raw_value)
///
/// Like the existing CAGG migrations, this recreates the views WITH NO DATA and does NOT call
/// refresh_continuous_aggregate (which cannot run inside the migration transaction). The
/// derived-parameter janitor does a full continuous-aggregate refresh on its first tick after
/// startup, which repopulates these from the full history; tests refresh explicitly after seeding.
#[derive(DeriveMigrationName)]
pub struct Migration;

const VIEWS: &[(&str, &str)] = &[
    ("readings_hourly", "1 hour"),
    ("readings_daily", "1 day"),
    ("readings_weekly", "1 week"),
    ("readings_monthly", "1 month"),
];

// (view, start_offset, end_offset, schedule_interval) — verbatim from init/m20260508.
const POLICIES: &[(&str, &str, &str, &str)] = &[
    ("readings_hourly", "3 hours", "1 hour", "1 hour"),
    ("readings_daily", "3 days", "1 day", "1 day"),
    ("readings_weekly", "3 weeks", "1 week", "1 week"),
    ("readings_monthly", "3 months", "1 month", "1 month"),
];

fn create_view_sql(view: &str, bucket: &str, with_sensor: bool) -> String {
    let (sensor_select, sensor_group) = if with_sensor {
        (", sensor_id", ", sensor_id")
    } else {
        ("", "")
    };
    let sums = if with_sensor {
        ",\n            SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,\n            SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value"
    } else {
        ""
    };
    format!(
        r#"
        CREATE MATERIALIZED VIEW {view}
        WITH (timescaledb.continuous) AS
        SELECT
            time_bucket('{bucket}', time) AS bucket,
            site_id,
            parameter_id{sensor_select},
            AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
            MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
            MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
            COUNT(*) AS count,
            STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value{sums}
        FROM readings
        WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE)
        GROUP BY time_bucket('{bucket}', time), site_id, parameter_id{sensor_group}
        WITH NO DATA
        "#
    )
}

async fn rebuild(manager: &SchemaManager<'_>, with_sensor: bool) -> Result<(), DbErr> {
    let db = manager.get_connection();

    for (view, ..) in POLICIES {
        db.execute_unprepared(&format!(
            "SELECT remove_continuous_aggregate_policy('{view}', if_exists => true)"
        ))
        .await?;
    }
    for (view, _) in VIEWS.iter().rev() {
        db.execute_unprepared(&format!("DROP MATERIALIZED VIEW IF EXISTS {view} CASCADE"))
            .await?;
    }
    for (view, bucket) in VIEWS {
        db.execute_unprepared(&create_view_sql(view, bucket, with_sensor))
            .await?;
    }
    for (view, start_off, end_off, sched) in POLICIES {
        db.execute_unprepared(&format!(
            "SELECT add_continuous_aggregate_policy('{view}',
                start_offset => INTERVAL '{start_off}',
                end_offset => INTERVAL '{end_off}',
                schedule_interval => INTERVAL '{sched}')"
        ))
        .await?;
    }
    if with_sensor {
        for (view, _) in VIEWS {
            db.execute_unprepared(&format!(
                "CREATE INDEX IF NOT EXISTS idx_{view}_sensor_bucket ON {view} (sensor_id, bucket DESC)"
            ))
            .await?;
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Restore the pre-Phase-3 shape: (bucket, site_id, parameter_id), no sensor_id/sum columns.
        rebuild(manager, false).await
    }
}
