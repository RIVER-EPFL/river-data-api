//! Historical alarm-event reconstruction.
//!
//! The 60s [`sweeper`](super::sweeper) only ever inspects the *latest* reading per slot, so breaches
//! in backfilled or historical data never become `alarm_events`. This module fills that gap: given a
//! (site, parameter) slot and a time window, it walks the actual readings and collapses consecutive
//! out-of-range readings into breach *episodes* (gaps-and-islands), writing one row per episode.
//!
//! Only **resolved** episodes (a run with a following in-range reading inside the window) are
//! written here. A run that reaches the last reading in the window is left for the live sweeper to
//! open and manage, so this never races the `uq_alarm_events_open` partial unique index. Writes are
//! idempotent: a rebuild deletes the resolved episodes it previously produced in the window, then
//! reinserts, re-running yields identical rows.

use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use uuid::Uuid;

use super::thresholds::{resolve_threshold, severity_case};

#[derive(Debug, FromQueryResult)]
struct EpisodeRow {
    started_at: DateTime<FixedOffset>,
    value_at_start: f64,
    last_seen_at: DateTime<FixedOffset>,
    last_value: f64,
    max_severity: i16,
    severity: i16,
    resolved_at: Option<DateTime<FixedOffset>>,
    resolved_value: Option<f64>,
}

/// Reconstruct resolved breach episodes for one slot over `[start, end]` and persist them
/// idempotently. Returns the number of episode rows written.
pub async fn evaluate_alarm_episodes(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<i64, sea_orm::DbErr> {
    let Some(threshold) = resolve_threshold(db, site_id, parameter_id).await? else {
        return Ok(0);
    };
    if threshold.is_disabled() {
        // No bounds → nothing can breach. Leave any pre-existing history untouched; disabling a
        // parameter shouldn't erase episodes that genuinely occurred before it was disabled.
        return Ok(0);
    }

    let sev_case = severity_case(
        "COALESCE(smp.mean, r.calibrated_value, r.raw_value)",
        "$5::double precision",
        "$6::double precision",
        "$7::double precision",
        "$8::double precision",
    );

    // Sensor and grab series form separate episode streams: a grab breach must not be
    // "resolved" by the next in-range sonde point (or vice versa).
    let mut written = 0i64;
    let mut all_episodes: Vec<(&'static str, Vec<EpisodeRow>)> = Vec::new();
    for spot in [false, true] {
        let episodes = fetch_episodes(
            db,
            site_id,
            parameter_id,
            start,
            end,
            &threshold,
            &sev_case,
            spot,
        )
        .await?;
        all_episodes.push((super::views::cadence_label(spot), episodes));
    }

    // Idempotent: clear the resolved episodes previously written for this slot+window, then reinsert
    // the freshly computed set. Open rows (`resolved_at IS NULL`) are owned by the sweeper and left
    // alone.
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM alarm_events \
         WHERE site_id = $1 AND parameter_id = $2 AND resolved_at IS NOT NULL \
           AND started_at >= $3 AND started_at <= $4",
        [
            site_id.into(),
            parameter_id.into(),
            start.into(),
            end.into(),
        ],
    ))
    .await?;

    for (cadence, episodes) in &all_episodes {
        for ep in episodes {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO alarm_events \
                    (site_id, parameter_id, measurement_type, severity, max_severity, started_at, \
                     value_at_start, last_seen_at, last_value, resolved_at, resolved_value) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                [
                    site_id.into(),
                    parameter_id.into(),
                    (*cadence).into(),
                    ep.severity.into(),
                    ep.max_severity.into(),
                    ep.started_at.with_timezone(&Utc).into(),
                    ep.value_at_start.into(),
                    ep.last_seen_at.with_timezone(&Utc).into(),
                    ep.last_value.into(),
                    ep.resolved_at.map(|t| t.with_timezone(&Utc)).into(),
                    ep.resolved_value.into(),
                ],
            ))
            .await?;
            written += 1;
        }
    }

    Ok(written)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_episodes(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    threshold: &super::thresholds::ResolvedThreshold,
    sev_case: &str,
    spot: bool,
) -> Result<Vec<EpisodeRow>, sea_orm::DbErr> {
    let cadence_pred = super::views::cadence_predicate(spot);

    // Per-reading severity, then gaps-and-islands. `marked` computes the LAG/LEAD neighbours;
    // `runs` then cumulatively sums the run-start flag (a window function can't be nested inside
    // another, so these must be separate CTEs). `run_id` increments at each breach that follows a
    // non-breach, so all consecutive breaching readings share one id. `next_t`/`next_v` from the
    // run's last row is the following in-range reading (NULL when the run reaches the window edge).
    let sql = format!(
        r"
        WITH ordered AS (
            SELECT r.time AS t,
                   COALESCE(smp.mean, r.calibrated_value, r.raw_value) AS v,
                   {sev_case} AS sev
            FROM readings r
            LEFT JOIN samples smp ON smp.id = r.sample_id
            WHERE r.site_id = $1 AND r.parameter_id = $2 AND r.replicate_index = 0
              AND r.time >= $3 AND r.time <= $4
              AND {cadence_pred}
        ),
        marked AS (
            SELECT t, v, sev,
                   (sev > 0) AS breach,
                   CASE WHEN sev > 0 AND COALESCE(LAG(sev) OVER w, 0) = 0 THEN 1 ELSE 0 END AS run_start,
                   LEAD(t) OVER w AS next_t,
                   LEAD(v) OVER w AS next_v
            FROM ordered
            WINDOW w AS (ORDER BY t)
        ),
        runs AS (
            SELECT t, v, sev, breach, next_t, next_v,
                   SUM(run_start) OVER (ORDER BY t ROWS UNBOUNDED PRECEDING) AS run_id
            FROM marked
        )
        SELECT
            MIN(t) AS started_at,
            (ARRAY_AGG(v ORDER BY t ASC))[1] AS value_at_start,
            MAX(t) AS last_seen_at,
            (ARRAY_AGG(v ORDER BY t DESC))[1] AS last_value,
            MAX(sev)::smallint AS max_severity,
            (ARRAY_AGG(sev ORDER BY t DESC))[1]::smallint AS severity,
            (ARRAY_AGG(next_t ORDER BY t DESC))[1] AS resolved_at,
            (ARRAY_AGG(next_v ORDER BY t DESC))[1] AS resolved_value
        FROM runs
        WHERE breach
        GROUP BY run_id
        HAVING (ARRAY_AGG(next_t ORDER BY t DESC))[1] IS NOT NULL
        ORDER BY started_at
        "
    );

    let values: Vec<sea_orm::Value> = vec![
        site_id.into(),
        parameter_id.into(),
        start.into(),
        end.into(),
        threshold.warning_min.into(),
        threshold.warning_max.into(),
        threshold.alarm_min.into(),
        threshold.alarm_max.into(),
    ];

    let episodes: Vec<EpisodeRow> = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|r| EpisodeRow::from_query_result(&r, "").ok())
        .collect();

    Ok(episodes)
}

/// Rebuild resolved alarm episodes across every active slot matching the optional `site_id` /
/// `parameter_id` filter. When `start`/`end` are omitted they default per-slot to the slot's reading
/// range (`MIN`/`MAX(time)`). Returns the total number of episode rows written. Slot-level failures
/// are logged and skipped so one bad slot can't abort the whole rebuild.
pub async fn rebuild_alarm_events(
    db: &DatabaseConnection,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> Result<i64, sea_orm::DbErr> {
    let mut conditions = vec!["sp.is_active = true".to_string()];
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(s) = site_id {
        values.push(s.into());
        conditions.push(format!("sp.site_id = ${}", values.len()));
    }
    if let Some(p) = parameter_id {
        values.push(p.into());
        conditions.push(format!("sp.parameter_id = ${}", values.len()));
    }
    let slot_sql = format!(
        "SELECT DISTINCT sp.site_id, sp.parameter_id FROM site_parameters sp WHERE {}",
        conditions.join(" AND ")
    );
    let slots: Vec<(Uuid, Uuid)> = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &slot_sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|r| {
            let s: Uuid = r.try_get("", "site_id").ok()?;
            let p: Uuid = r.try_get("", "parameter_id").ok()?;
            Some((s, p))
        })
        .collect();

    let mut total = 0i64;
    for (s, p) in slots {
        let (slot_start, slot_end) = if let (Some(a), Some(b)) = (start, end) {
            (a, b)
        } else {
            let row = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT MIN(time) AS lo, MAX(time) AS hi FROM readings \
                     WHERE site_id = $1 AND parameter_id = $2 AND replicate_index = 0",
                    [s.into(), p.into()],
                ))
                .await?;
            let lo = row
                .as_ref()
                .and_then(|r| r.try_get::<DateTime<Utc>>("", "lo").ok());
            let hi = row
                .as_ref()
                .and_then(|r| r.try_get::<DateTime<Utc>>("", "hi").ok());
            match (start.or(lo), end.or(hi)) {
                (Some(a), Some(b)) => (a, b),
                _ => continue, // no readings for this slot
            }
        };

        match evaluate_alarm_episodes(db, s, p, slot_start, slot_end).await {
            Ok(n) => total += n,
            Err(e) => tracing::warn!(
                error = %e,
                site_id = %s,
                parameter_id = %p,
                "rebuild_alarm_events: slot failed"
            ),
        }
    }

    Ok(total)
}
