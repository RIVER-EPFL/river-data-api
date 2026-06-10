use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::time::Duration;
use uuid::Uuid;

const MAX_GAPS_PER_RUN: usize = 50_000;

/// Whether a site has any active derived `site_parameter`. The spawn-guard for the ingest/batch
/// derived-compute jobs: when false, a derived recompute at that site would do nothing, so the job
/// is skipped entirely (the dominant source of empty `ingest_derived` jobs). Anything genuinely
/// needed is still caught by the periodic janitor gap scan.
pub async fn site_has_active_derived(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> Result<bool, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM site_parameters \
             WHERE site_id = $1 AND is_derived = true AND COALESCE(is_active, true) = true LIMIT 1",
            [site_id.into()],
        ))
        .await?;
    Ok(row.is_some())
}

async fn update_job(db: &DatabaseConnection, job_id: Uuid, sql: &str, values: Vec<sea_orm::Value>) {
    if let Err(e) = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
    {
        tracing::warn!(error = %e, %job_id, "Derived janitor: failed to update reprocessing_jobs row");
    }
}

/// Find (site_id, time) pairs where a source reading exists but no corresponding
/// derived reading was ever written. Recompute each.
///
/// Catches gaps from crashes during ingest, sync cycles missed while the API
/// was down, derived parameters assigned after historical source data already
/// existed, and any other inconsistency between source and derived readings.
///
/// Each call persists a `reprocessing_jobs` row with `trigger_type='janitor_run'`
/// so operators can see janitor activity on the `/jobs` UI page. After filling
/// any gaps, refreshes continuous aggregates back to the earliest filled
/// timestamp so the hourly/daily/weekly/monthly rollups reflect the newly
/// written derived values.
pub async fn run_once(db: &DatabaseConnection) -> Result<usize, sea_orm::DbErr> {
    let started = std::time::Instant::now();
    let job_id = Uuid::new_v4();
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, status) \
         VALUES ($1, NULL, 'janitor_run', 'running')",
        [job_id.into()],
    ))
    .await?;

    let rows = match db
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
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            let msg = e.to_string();
            update_job(
                db,
                job_id,
                "UPDATE reprocessing_jobs SET status = 'failed', error_message = $1, completed_at = NOW() WHERE id = $2",
                vec![msg.as_str().into(), job_id.into()],
            )
            .await;
            return Err(e);
        }
    };

    let total = rows.len() as i32;
    update_job(
        db,
        job_id,
        "UPDATE reprocessing_jobs SET total = $1, progress = 0 WHERE id = $2",
        vec![total.into(), job_id.into()],
    )
    .await;

    if rows.is_empty() {
        update_job(
            db,
            job_id,
            "UPDATE reprocessing_jobs SET status = 'completed', readings_updated = 0, completed_at = NOW() WHERE id = $1",
            vec![job_id.into()],
        )
        .await;
        tracing::info!(
            %job_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Derived janitor: no gaps found"
        );
        return Ok(0);
    }

    tracing::info!(%job_id, gaps = total, "Derived janitor: filling gaps");

    let mut filled: i32 = 0;
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
            tracing::info!(%job_id, "Derived janitor: filled {}/{}", i + 1, total);
            update_job(
                db,
                job_id,
                "UPDATE reprocessing_jobs SET progress = $1 WHERE id = $2",
                vec![(i as i32 + 1).into(), job_id.into()],
            )
            .await;
        }
    }

    if let Some(since) = min_filled {
        tracing::info!(%job_id, %since, filled, "Derived janitor: refreshing continuous aggregates after backfill");
        crate::common::sync_state::refresh_continuous_aggregates(db, Some(since)).await;
    }

    update_job(
        db,
        job_id,
        "UPDATE reprocessing_jobs \
         SET status = 'completed', progress = total, readings_updated = $1, completed_at = NOW() \
         WHERE id = $2",
        vec![filled.into(), job_id.into()],
    )
    .await;

    tracing::info!(
        %job_id,
        filled,
        total,
        capped_at_limit = total as usize >= MAX_GAPS_PER_RUN,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Derived janitor: complete"
    );
    Ok(filled as usize)
}

/// Delete old janitor-run rows from `reprocessing_jobs`. Only `janitor_run` rows
/// are pruned — calibration/deployment/derived-recompute rows are operator
/// history and preserved indefinitely.
async fn prune_janitor_rows(db: &DatabaseConnection, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    let cutoff_expr = format!("INTERVAL '{retention_days} days'");
    let sql = format!(
        "DELETE FROM reprocessing_jobs \
         WHERE trigger_type = 'janitor_run' \
           AND created_at < NOW() - {cutoff_expr}"
    );
    match db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await
    {
        Ok(res) => tracing::info!(
            deleted = res.rows_affected(),
            retention_days,
            "Derived janitor: pruned old janitor_run rows"
        ),
        Err(e) => tracing::warn!(error = %e, "Derived janitor: failed to prune janitor_run rows"),
    }
}

/// Long-running task: run the janitor once on startup, then every `interval`.
///
/// On every tick:
///   1. `run_once` fills any missing derived readings (and refreshes aggregates
///      scoped to the earliest backfilled timestamp), persisting a
///      `reprocessing_jobs` row visible on the `/jobs` UI page.
///   2. An incremental refresh of the last 24h hourly / 7d daily windows runs
///      unconditionally, so aggregates stay in sync with regular ingestion
///      even when no derived gaps were found.
///   3. Once every `full_refresh_interval`, a full refresh of all continuous
///      aggregates runs, catching any historical drift older than 7d without
///      needing a manual `POST /actions/refresh_aggregates {full: true}`.
///   4. Once every ~24h, old `janitor_run` rows older than `retention_days`
///      are pruned. Set `retention_days = 0` to disable.
///
/// Spawned from main.rs with durations sourced from `Config`
/// (`JANITOR_INTERVAL_SECONDS`, `JANITOR_FULL_REFRESH_SECONDS`,
/// `JANITOR_RETENTION_DAYS`).
pub async fn periodic(
    db: DatabaseConnection,
    interval: Duration,
    full_refresh_interval: Duration,
    retention_days: u32,
) {
    tracing::info!(
        interval_secs = interval.as_secs(),
        full_refresh_secs = full_refresh_interval.as_secs(),
        retention_days,
        "Derived janitor: starting"
    );

    const PRUNE_EVERY: Duration = Duration::from_secs(86_400);
    let mut last_full_refresh: Option<std::time::Instant> = None;
    let mut last_prune: Option<std::time::Instant> = None;

    let tick = async |db: &DatabaseConnection,
                      last_full_refresh: &mut Option<std::time::Instant>,
                      last_prune: &mut Option<std::time::Instant>| {
        if let Err(e) = run_once(db).await {
            tracing::warn!(error = %e, "Derived janitor: run failed");
        }

        let now = std::time::Instant::now();
        let due_full = last_full_refresh.is_none_or(|t| now.duration_since(t) >= full_refresh_interval);
        if due_full {
            tracing::info!("Derived janitor: running scheduled full continuous aggregate refresh");
            crate::common::sync_state::refresh_continuous_aggregates_full(db).await;
            *last_full_refresh = Some(now);
        } else {
            crate::common::sync_state::refresh_continuous_aggregates(db, None).await;
        }

        let due_prune = last_prune.is_none_or(|t| now.duration_since(t) >= PRUNE_EVERY);
        if due_prune {
            prune_janitor_rows(db, retention_days).await;
            *last_prune = Some(now);
        }
    };

    tick(&db, &mut last_full_refresh, &mut last_prune).await;

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        tick(&db, &mut last_full_refresh, &mut last_prune).await;
    }
}
