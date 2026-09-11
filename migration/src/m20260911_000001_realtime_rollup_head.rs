use sea_orm_migration::prelude::*;

/// Serve the head of each rollup from raw rows, and let the policies heal the whole history.
///
/// The four rollups were materialized-only with a policy refreshing three buckets ending one
/// bucket ago, so the newest bucket was never materialised and a read served that absence as data.
/// `materialized_only = false` unions the open bucket from the raw rows instead, measured at 0 to
/// 11 ms per serving query, and a `start_offset => NULL` policy covers the whole history, so a
/// correction to an old reading is healed by the next tick rather than by an application sweep.
///
/// `buckets_per_batch => 0` refreshes the window in one batch. The 2.23 default batches it, which
/// took 34.7 s against 7.15 s to fill an empty hourly rollup and never converged at all on the
/// monthly one: its invalidation entry marched one month per run under every `end_offset` tried.
///
/// The monthly tick costs 0.55 s because the window end sits inside the open month, so that month
/// is recomputed every hour. The other three are 3 to 4 ms.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// The rollups, with the bucket each policy ends one of before now, and the window the baseline
/// policy this replaces covered.
const VIEWS: [(&str, &str, &str); 4] = [
    ("readings_hourly", "1 hour", "3 hours"),
    ("readings_daily", "1 day", "3 days"),
    ("readings_weekly", "1 week", "3 weeks"),
    ("readings_monthly", "1 month", "3 months"),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for (view, bucket, _) in VIEWS {
            db.execute_unprepared(&format!(
                "ALTER MATERIALIZED VIEW {view} SET (timescaledb.materialized_only = false);
                 SELECT remove_continuous_aggregate_policy('{view}', if_exists => TRUE);
                 SELECT add_continuous_aggregate_policy('{view}',
                     start_offset => NULL,
                     end_offset => INTERVAL '{bucket}',
                     schedule_interval => INTERVAL '1 hour',
                     buckets_per_batch => 0,
                     if_not_exists => TRUE);"
            ))
            .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for (view, bucket, lookback) in VIEWS {
            db.execute_unprepared(&format!(
                "ALTER MATERIALIZED VIEW {view} SET (timescaledb.materialized_only = true);
                 SELECT remove_continuous_aggregate_policy('{view}', if_exists => TRUE);
                 SELECT add_continuous_aggregate_policy('{view}',
                     start_offset => INTERVAL '{lookback}',
                     end_offset => INTERVAL '{bucket}',
                     schedule_interval => INTERVAL '{bucket}',
                     if_not_exists => TRUE);"
            ))
            .await?;
        }
        Ok(())
    }
}
