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
    for (i, row) in rows.iter().enumerate() {
        let site_id: Uuid = row.try_get("", "site_id")?;
        let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        let utc_time = time.with_timezone(&chrono::Utc);
        match crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
            db, site_id, utc_time,
        )
        .await
        {
            Ok(()) => filled += 1,
            Err(e) => tracing::warn!(error = %e, site_id = %site_id, time = %utc_time, "Janitor failed to fill derived gap"),
        }
        if (i + 1) % 1000 == 0 {
            tracing::info!("Derived janitor: filled {}/{}", i + 1, total);
        }
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
/// Spawned as `tokio::spawn(periodic(db, interval))` from main.rs.
pub async fn periodic(db: DatabaseConnection, interval: Duration) {
    tracing::info!(
        interval_secs = interval.as_secs(),
        "Derived janitor: starting"
    );

    if let Err(e) = run_once(&db).await {
        tracing::warn!(error = %e, "Derived janitor: initial run failed");
    }

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Err(e) = run_once(&db).await {
            tracing::warn!(error = %e, "Derived janitor: periodic run failed");
        }
    }
}
