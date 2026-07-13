use sea_orm_migration::prelude::*;

/// Rebuild the four continuous aggregates to exclude only `spot` readings, letting `derived` rows
/// back into the rollups.
///
/// m20260711_000002 excluded both 'spot' and 'derived', but that predicate was inert for derived
/// data because the derived writer stamped `'continuous'` (or NULL). Now that derived readings are
/// tagged truthfully, the exclusion would empty every derived parameter's hourly/daily series —
/// and derived parameters are served through `/aggregates` exactly like measured ones. Derived
/// readings live under their own output parameter_id, so they roll up as their own series and
/// cannot contaminate source parameters. Grabs ('spot') stay excluded.
///
/// Same shape/policies as m20260711_000002; recreated WITH NO DATA — the janitor's first-tick full
/// refresh repopulates from history, and tests refresh explicitly after seeding.
#[derive(DeriveMigrationName)]
pub struct Migration;

const VIEWS: &[(&str, &str)] = &[
    ("readings_hourly", "1 hour"),
    ("readings_daily", "1 day"),
    ("readings_weekly", "1 week"),
    ("readings_monthly", "1 month"),
];

const POLICIES: &[(&str, &str, &str, &str)] = &[
    ("readings_hourly", "3 hours", "1 hour", "1 hour"),
    ("readings_daily", "3 days", "1 day", "1 day"),
    ("readings_weekly", "3 weeks", "1 week", "1 week"),
    ("readings_monthly", "3 months", "1 month", "1 month"),
];

fn create_view_sql(view: &str, bucket: &str, exclude_derived: bool) -> String {
    let type_filter = if exclude_derived {
        " AND measurement_type IS DISTINCT FROM 'spot' AND measurement_type IS DISTINCT FROM 'derived'"
    } else {
        " AND measurement_type IS DISTINCT FROM 'spot'"
    };
    format!(
        r#"
        CREATE MATERIALIZED VIEW {view}
        WITH (timescaledb.continuous) AS
        SELECT
            time_bucket('{bucket}', time) AS bucket,
            site_id,
            parameter_id, sensor_id,
            AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
            MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
            MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
            COUNT(*) AS count,
            STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value,
            SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,
            SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value
        FROM readings
        WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE){type_filter}
        GROUP BY time_bucket('{bucket}', time), site_id, parameter_id, sensor_id
        WITH NO DATA
        "#
    )
}

async fn rebuild(manager: &SchemaManager<'_>, exclude_derived: bool) -> Result<(), DbErr> {
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
        db.execute_unprepared(&create_view_sql(view, bucket, exclude_derived))
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
    for (view, _) in VIEWS {
        db.execute_unprepared(&format!(
            "CREATE INDEX IF NOT EXISTS idx_{view}_sensor_bucket ON {view} (sensor_id, bucket DESC)"
        ))
        .await?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild(manager, false).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild(manager, true).await
    }
}
