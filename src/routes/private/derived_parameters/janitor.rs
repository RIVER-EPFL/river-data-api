use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::time::Duration;
use uuid::Uuid;

const MAX_GAPS_PER_RUN: usize = 50_000;

/// Find (site_id, time) pairs where a source reading exists but no corresponding
/// derived reading was ever written. Recompute each.
///
/// Catches gaps from crashes during ingest, sync cycles missed while the API
/// was down, derived parameters assigned after historical source data already
/// existed, and any other inconsistency between source and derived readings.
///
/// After filling gaps, refreshes continuous aggregates back to the earliest
/// filled timestamp so the hourly/daily/weekly/monthly rollups reflect the
/// newly written derived values.
pub async fn run_once(db: &DatabaseConnection) -> Result<usize, sea_orm::DbErr> {
    let started = std::time::Instant::now();
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT r.site_id, r.time
              FROM readings r
              JOIN site_parameters sp
                ON sp.site_id = r.site_id
               AND sp.is_derived = true
               AND COALESCE(sp.is_active, true) = true
              JOIN derived_parameter_sources dps
                ON dps.derived_definition_id = sp.derived_definition_id
               AND dps.parameter_id = r.parameter_id
              WHERE NOT EXISTS (
                  SELECT 1 FROM readings r2
                  WHERE r2.site_id = r.site_id
                    AND r2.parameter_id = sp.parameter_id
                    AND r2.time = r.time
              )
              ORDER BY r.site_id, r.time
              LIMIT $1",
            [(MAX_GAPS_PER_RUN as i64).into()],
        ))
        .await?;

    if rows.is_empty() {
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Derived janitor: no gaps found"
        );
        return Ok(0);
    }

    let total = rows.len();
    tracing::info!(gaps = total, "Derived janitor: filling gaps");

    let mut filled = 0usize;
    let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
    for (i, row) in rows.iter().enumerate() {
        let site_id: Uuid = row.try_get("", "site_id")?;
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        let utc_time = time.with_timezone(&chrono::Utc);
        match crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
            db, site_id, utc_time,
        )
        .await
        {
            Ok(()) => {
                filled += 1;
                min_filled = Some(min_filled.map_or(utc_time, |m| m.min(utc_time)));
            }
            Err(e) => tracing::warn!(error = %e, site_id = %site_id, time = %utc_time, "Janitor failed to fill derived gap"),
        }
        if (i + 1) % 1000 == 0 {
            tracing::info!("Derived janitor: filled {}/{}", i + 1, total);
        }
    }

    if let Some(since) = min_filled {
        tracing::info!(%since, filled, "Derived janitor: refreshing continuous aggregates after backfill");
        crate::common::sync_state::refresh_continuous_aggregates(db, Some(since)).await;
    }

    tracing::info!(
        filled,
        total,
        capped_at_limit = total >= MAX_GAPS_PER_RUN,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Derived janitor: complete"
    );
    Ok(filled)
}

/// Long-running task: run the janitor once on startup, then every `interval`.
///
/// On every tick:
///   1. `run_once` fills any missing derived readings (and refreshes aggregates
///      scoped to the earliest backfilled timestamp).
///   2. An incremental refresh of the last 24h hourly / 7d daily windows runs
///      unconditionally, so aggregates stay in sync with regular ingestion
///      even when no derived gaps were found.
///   3. Once every ~24h, a full refresh of all continuous aggregates runs,
///      catching any historical drift older than 7d without needing a manual
///      `POST /actions/refresh_aggregates {full: true}`.
///
/// Spawned as `tokio::spawn(periodic(db, interval))` from main.rs.
pub async fn periodic(db: DatabaseConnection, interval: Duration) {
    tracing::info!(
        interval_secs = interval.as_secs(),
        "Derived janitor: starting"
    );

    const FULL_REFRESH_EVERY: Duration = Duration::from_secs(86_400);
    let mut last_full_refresh: Option<std::time::Instant> = None;

    let tick = async |db: &DatabaseConnection,
                      last_full_refresh: &mut Option<std::time::Instant>| {
        if let Err(e) = run_once(db).await {
            tracing::warn!(error = %e, "Derived janitor: run failed");
        }

        let now = std::time::Instant::now();
        let due_full = last_full_refresh
            .is_none_or(|t| now.duration_since(t) >= FULL_REFRESH_EVERY);
        if due_full {
            tracing::info!("Derived janitor: running scheduled full continuous aggregate refresh");
            crate::common::sync_state::refresh_continuous_aggregates_full(db).await;
            *last_full_refresh = Some(now);
        } else {
            crate::common::sync_state::refresh_continuous_aggregates(db, None).await;
        }
    };

    tick(&db, &mut last_full_refresh).await;

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        tick(&db, &mut last_full_refresh).await;
    }
}
