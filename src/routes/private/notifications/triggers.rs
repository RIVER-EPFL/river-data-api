//! Signal-based alert triggers beyond threshold alarms: stale data (with recovery), battery
//! depletion forecast (with re-notify suppression), and sync-service failures (digest). Each dedups
//! through `notification_state` so a standing condition isn't re-announced every cycle.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};

use super::dispatcher::deliver;
use super::{NotificationChannel, OutgoingMessage, Slot};
use crate::common::AppState;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const BATTERY_RENOTIFY_HOURS: i64 = 7 * 24;

/// A wholly failed cycle is rare, so its digest goes out on the next tick. A partial cycle repeats
/// every sync interval for as long as one stream keeps failing (the cursor is forward-only, so the
/// same rows are replayed), which without a window would put a digest on every tick.
const SYNC_PARTIAL_RENOTIFY_HOURS: i64 = 6;

/// Multiple of a slot's own observed grab interval before its spot series counts as stale. The
/// configured hour threshold is a logger cadence; grabs arrive on campaign days, so the expectation
/// has to come from the series itself.
const SPOT_STALE_INTERVAL_FACTOR: i32 = 3;

/// Run all signal triggers. Called from the dispatcher cycle when at least one channel is enabled.
pub async fn run(state: &AppState, channels: &[Box<dyn NotificationChannel>]) {
    if let Err(e) = stale_data(state, channels).await {
        tracing::warn!(error = %e, "stale-data trigger failed");
    }
    if let Err(e) = battery_forecast(state, channels).await {
        tracing::warn!(error = %e, "battery-forecast trigger failed");
    }
    if let Err(e) = sync_failures(state, channels).await {
        tracing::warn!(error = %e, "sync-failure trigger failed");
    }
}

async fn state_get(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
) -> Result<Option<(String, DateTime<Utc>)>, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT state, last_notified_at FROM notification_state \
             WHERE kind = $1 AND subject_key = $2",
            [kind.into(), key.into()],
        ))
        .await?;
    match row {
        Some(r) => Ok(Some((
            r.try_get("", "state")?,
            r.try_get("", "last_notified_at")?,
        ))),
        None => Ok(None),
    }
}

async fn state_upsert(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
    state: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        PG,
        "INSERT INTO notification_state (kind, subject_key, state, last_notified_at) \
         VALUES ($1, $2, $3, NOW()) \
         ON CONFLICT (kind, subject_key) \
         DO UPDATE SET state = EXCLUDED.state, last_notified_at = NOW()",
        [kind.into(), key.into(), state.into()],
    ))
    .await?;
    Ok(())
}

async fn state_clear(db: &DatabaseConnection, kind: &str, key: &str) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        PG,
        "DELETE FROM notification_state WHERE kind = $1 AND subject_key = $2",
        [kind.into(), key.into()],
    ))
    .await?;
    Ok(())
}

// Multi-replica claims: each transition is committed to `notification_state` BEFORE the send so that
// at 2-3 replicas exactly one replica sends. The unique (kind, subject_key) key arbitrates the race,
// the single winner gets a RETURNING row, losers get none and skip.

/// Claim a fresh firing transition: insert the dedup row iff absent. Winner sends.
async fn claim_insert(db: &DatabaseConnection, kind: &str, key: &str) -> Result<bool, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "INSERT INTO notification_state (kind, subject_key, state, last_notified_at) \
             VALUES ($1, $2, 'firing', NOW()) \
             ON CONFLICT (kind, subject_key) DO NOTHING RETURNING 1 AS one",
            [kind.into(), key.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// Claim a resolve transition: delete the dedup row. Winner sends the recovery message.
async fn claim_clear(db: &DatabaseConnection, kind: &str, key: &str) -> Result<bool, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "DELETE FROM notification_state WHERE kind = $1 AND subject_key = $2 RETURNING 1 AS one",
            [kind.into(), key.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// Claim a (re-)notify with a suppression window: win iff there is no prior alert or the last one was
/// more than `within_hours` ago. Atomically advances the timestamp so a single replica re-notifies.
async fn claim_renotify(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
    within_hours: i64,
) -> Result<bool, DbErr> {
    let sql = format!(
        "INSERT INTO notification_state (kind, subject_key, state, last_notified_at) \
         VALUES ($1, $2, 'firing', NOW()) \
         ON CONFLICT (kind, subject_key) DO UPDATE SET last_notified_at = NOW(), state = 'firing' \
         WHERE notification_state.last_notified_at < NOW() - INTERVAL '{within_hours} hours' \
         RETURNING 1 AS one"
    );
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            &sql,
            [kind.into(), key.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// Claim by advancing a watermark: win iff the stored timestamp still equals `expected` (the value
/// just read), or the row is absent. A replica that already advanced it wins the compare-and-swap and
/// the loser skips, so a digest is sent once.
async fn claim_cas(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
    expected: DateTime<Utc>,
) -> Result<bool, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "INSERT INTO notification_state (kind, subject_key, state, last_notified_at) \
             VALUES ($1, $2, 'firing', NOW()) \
             ON CONFLICT (kind, subject_key) DO UPDATE SET last_notified_at = NOW(), state = 'firing' \
             WHERE notification_state.last_notified_at = $3 \
             RETURNING 1 AS one",
            [kind.into(), key.into(), expected.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// A slot carries one series per cadence and each goes stale on its own: a continuous series that
/// keeps flowing says nothing about the grab series beside it, and a spot-only slot has no
/// continuous series at all. Evaluated separately and keyed separately in `notification_state`, the
/// same partition `alarm_events` uses.
async fn stale_data(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let config = state.config.as_ref();

    // Subject keys gained a cadence suffix; the pre-suffix rows are unreachable, so drop them
    // rather than leave a firing state nothing can ever resolve.
    db.execute(Statement::from_string(
        PG,
        "DELETE FROM notification_state \
         WHERE kind = 'stale_data' AND subject_key NOT LIKE '%:%:%'"
            .to_string(),
    ))
    .await?;

    // The dispatcher wakes on every alarm-state broadcast, so each lookup is a backward walk of
    // idx_readings_site_param_time stopping at the first match, never an aggregate over the slot.
    let rows = db
        .query_all(Statement::from_string(
            PG,
            "SELECT sp.site_id, sp.parameter_id, s.project_id, s.name AS site_name, \
                    p.name AS param_name, \
                    agg.last_continuous, agg.last_spot, agg.spot_max_gap_seconds \
             FROM site_parameters sp \
             JOIN sites s ON s.id = sp.site_id \
             JOIN parameters p ON p.id = sp.parameter_id \
             LEFT JOIN LATERAL ( \
                 SELECT \
                   (SELECT r.time FROM readings r \
                     WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id \
                       AND r.replicate_index = 0 \
                       AND r.measurement_type IS DISTINCT FROM 'spot' \
                     ORDER BY r.time DESC LIMIT 1) AS last_continuous, \
                   (SELECT r.time FROM readings r \
                     WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id \
                       AND r.measurement_type = 'spot' \
                     ORDER BY r.time DESC LIMIT 1) AS last_spot, \
                   (SELECT EXTRACT(EPOCH FROM MAX(g.gap))::float8 FROM ( \
                      SELECT s.t - LAG(s.t) OVER (ORDER BY s.t) AS gap FROM ( \
                        SELECT DISTINCT r.time AS t FROM readings r \
                        WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id \
                          AND r.measurement_type = 'spot' \
                        ORDER BY r.time DESC LIMIT 5) s \
                    ) g) AS spot_max_gap_seconds \
             ) agg ON TRUE \
             WHERE sp.is_active"
                .to_string(),
        ))
        .await?;

    let base_threshold = Duration::hours(config.stale_data_threshold_hours);
    for r in &rows {
        let site_id: uuid::Uuid = r.try_get("", "site_id")?;
        let parameter_id: uuid::Uuid = r.try_get("", "parameter_id")?;
        let project_id: Option<uuid::Uuid> = r.try_get("", "project_id")?;
        let site_name: String = r.try_get("", "site_name")?;
        let param_name: String = r.try_get("", "param_name")?;
        let last_continuous: Option<DateTime<Utc>> = r.try_get("", "last_continuous")?;
        let last_spot: Option<DateTime<Utc>> = r.try_get("", "last_spot")?;
        let spot_max_gap_seconds: Option<f64> = r.try_get("", "spot_max_gap_seconds")?;

        for spot in [false, true] {
            let Some(last_time) = (if spot { last_spot } else { last_continuous }) else {
                continue; // this cadence never produced data, not "stale", just unpaired/new
            };
            let threshold = if spot {
                // Grabs arrive in campaigns, so the expected gap is the widest of the recent ones,
                // not the last one: two samples on one field day describe the campaign, not the
                // cadence. A gap under the logger threshold means no multi-day cadence is visible
                // yet, and there is nothing to be late against.
                let gap = Duration::seconds(spot_max_gap_seconds.unwrap_or(0.0) as i64);
                if gap < base_threshold {
                    continue;
                }
                gap * SPOT_STALE_INTERVAL_FACTOR
            } else {
                base_threshold
            };

            let cadence = if spot { "spot" } else { "continuous" };
            let noun = if spot { "grab samples" } else { "data" };
            let key = format!("{site_id}:{parameter_id}:{cadence}");
            let age = Utc::now() - last_time;
            let firing = age > threshold;
            let previously_firing = state_get(db, "stale_data", &key).await?.is_some();
            let slot = Some(Slot {
                project_id,
                site_id,
                parameter_id,
            });

            if firing && !previously_firing {
                // Claim the firing transition before sending so only one replica announces it.
                if claim_insert(db, "stale_data", &key).await? {
                    let msg = OutgoingMessage {
                        kind: "stale_data",
                        subject: format!("River Data: no {noun} from {site_name}"),
                        body: format!(
                            "⏳ No {noun} from {site_name} / {param_name} for ~{}h (expected within ~{}h).",
                            age.num_hours(),
                            threshold.num_hours()
                        ),
                        slot,
                    };
                    if !deliver(state, channels, &msg, None).await {
                        state_clear(db, "stale_data", &key).await?; // release so it retries next tick
                    }
                }
            } else if !firing && previously_firing {
                // Claim the resolve transition before sending the recovery.
                if claim_clear(db, "stale_data", &key).await? {
                    let msg = OutgoingMessage {
                        kind: "stale_data",
                        subject: format!("River Data: {noun} resumed from {site_name}"),
                        body: format!("✅ {noun} flowing again from {site_name} / {param_name}."),
                        slot,
                    };
                    if !deliver(state, channels, &msg, None).await {
                        state_upsert(db, "stale_data", &key, "firing").await?; // restore so it retries
                    }
                }
            }
        }
    }
    Ok(())
}

async fn battery_forecast(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let config = state.config.as_ref();
    let Some(battery_param) = db
        .query_one(Statement::from_string(
            PG,
            "SELECT id FROM parameters \
             WHERE category = 'device_health' AND (code ILIKE '%batt%' OR name ILIKE '%batt%') \
             ORDER BY (name ILIKE 'battery') DESC LIMIT 1"
                .to_string(),
        ))
        .await?
        .map(|r| r.try_get::<uuid::Uuid>("", "id"))
        .transpose()?
    else {
        return Ok(());
    };

    let rows = db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT s.id AS site_id, s.project_id, s.name AS site_name, \
                (SELECT COALESCE(r2.calibrated_value, r2.raw_value) FROM readings r2 \
                   WHERE r2.site_id = s.id AND r2.parameter_id = $1 AND r2.replicate_index = 0 \
                     AND r2.measurement_type IS DISTINCT FROM 'spot' \
                   ORDER BY r2.time DESC LIMIT 1) AS latest, \
                (SELECT regr_slope(COALESCE(r3.calibrated_value, r3.raw_value), \
                                   EXTRACT(EPOCH FROM r3.time) / 86400.0) FROM readings r3 \
                   WHERE r3.site_id = s.id AND r3.parameter_id = $1 AND r3.replicate_index = 0 \
                     AND r3.measurement_type IS DISTINCT FROM 'spot' \
                     AND r3.time > NOW() - INTERVAL '7 days' \
                     AND EXTRACT(HOUR FROM r3.time) BETWEEN 2 AND 4) AS slope \
             FROM sites s",
            [battery_param.into()],
        ))
        .await?;

    let cutoff = config.battery_cutoff_volts;
    for r in &rows {
        let site_id: uuid::Uuid = r.try_get("", "site_id")?;
        let project_id: Option<uuid::Uuid> = r.try_get("", "project_id")?;
        let site_name: String = r.try_get("", "site_name")?;
        let (Some(latest), Some(slope)) = (
            r.try_get::<Option<f64>>("", "latest")?,
            r.try_get::<Option<f64>>("", "slope")?,
        ) else {
            continue;
        };
        if slope >= -1e-6 || latest <= cutoff {
            continue; // not declining, or already below cutoff (a normal threshold alarm covers that)
        }
        let days = (latest - cutoff) / -slope;
        #[allow(clippy::cast_precision_loss)]
        if days >= config.battery_forecast_alert_days as f64 {
            continue;
        }

        let key = site_id.to_string();
        // Claim the (re-)notify atomically: suppresses re-alert within the window AND ensures a single
        // replica sends. On a rare send failure the advisory is suppressed until the window lapses,
        // acceptable for a forecast (threshold alarms keep full at-least-once via the dispatcher).
        if !claim_renotify(db, "battery_forecast", &key, BATTERY_RENOTIFY_HOURS).await? {
            continue;
        }
        let msg = OutgoingMessage {
            kind: "battery_forecast",
            subject: format!("River Data: battery low at {site_name}"),
            body: format!(
                "🔋 {site_name}: {latest:.2}V, trend {slope:+.3}V/day, ~{days:.0}d to {cutoff:.1}V."
            ),
            slot: Some(Slot {
                project_id,
                site_id,
                parameter_id: battery_param,
            }),
        };
        let _ = deliver(state, channels, &msg, None).await;
    }
    Ok(())
}

async fn sync_failures(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let services = db
        .query_all(Statement::from_string(
            PG,
            "SELECT id, instance_id, service_type FROM sync_services".to_string(),
        ))
        .await?;

    for svc in &services {
        let service_id: uuid::Uuid = svc.try_get("", "id")?;
        let instance: String = svc.try_get("", "instance_id")?;
        let service_type: String = svc.try_get("", "service_type")?;
        let key = instance.clone();

        // Count unhealthy cycles since the last time we notified for this service (24h on first run).
        // A driver only returns Err when the whole cycle collapses, so per-stream ingest errors land
        // as 'partial'; counting 'failed' alone leaves a service that fails every stream silent.
        let since = state_get(db, "sync_failure", &key)
            .await?
            .map_or_else(|| Utc::now() - Duration::hours(24), |(_, t)| t);
        let count_row = db
            .query_one(Statement::from_sql_and_values(
                PG,
                "SELECT COUNT(*) FILTER (WHERE status = 'failed') AS n_failed, \
                        COUNT(*) FILTER (WHERE status = 'partial') AS n_partial, \
                        (ARRAY_AGG(errors ->> 0 ORDER BY started_at DESC) \
                            FILTER (WHERE jsonb_typeof(errors) = 'array' \
                                      AND jsonb_array_length(errors) > 0))[1] AS sample_error \
                 FROM sync_events \
                 WHERE service_id = $1 AND status IN ('failed', 'partial') AND started_at > $2",
                [service_id.into(), since.into()],
            ))
            .await?;
        let Some(count_row) = count_row else { continue };
        let n_failed: i64 = count_row.try_get("", "n_failed").unwrap_or(0);
        let n_partial: i64 = count_row.try_get("", "n_partial").unwrap_or(0);
        let sample_error: Option<String> = count_row.try_get("", "sample_error").unwrap_or(None);
        if n_failed == 0 && n_partial == 0 {
            continue;
        }

        // A failed cycle claims by advancing the watermark, so a replica losing the CAS skips and
        // the digest sends once. Partials go through the suppression window instead: they recur
        // every sync interval while a stream is stuck, and holding the watermark keeps the counts
        // cumulative across the stretch.
        let claimed = if n_failed > 0 {
            claim_cas(db, "sync_failure", &key, since).await?
        } else {
            claim_renotify(db, "sync_failure", &key, SYNC_PARTIAL_RENOTIFY_HOURS).await?
        };
        if !claimed {
            continue;
        }

        let mut body = format!(
            "⚠️ {n_failed} failed and {n_partial} partial sync cycle(s) on \
             {service_type}/{instance} since the last alert."
        );
        if let Some(err) = sample_error {
            let err: String = err.chars().take(300).collect();
            body.push_str(&format!("\nLatest error: {err}"));
        }
        let msg = OutgoingMessage {
            kind: "sync_failure",
            subject: format!("River Data: sync failures on {service_type}"),
            body,
            // System-wide infrastructure alert, no per-site scope, every enabled recipient gets it.
            slot: None,
        };
        let _ = deliver(state, channels, &msg, None).await;
    }
    Ok(())
}
