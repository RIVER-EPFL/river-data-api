//! Signal-based alert triggers beyond threshold alarms: stale data (with recovery), battery
//! depletion forecast (with re-notify suppression), and sync-service failures (digest). Each dedups
//! through `notification_state` so a standing condition isn't re-announced every cycle.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};

use crate::config::Config;

use super::dispatcher::deliver;
use super::{NotificationChannel, OutgoingMessage, Slot};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const BATTERY_RENOTIFY_DAYS: i64 = 7;

/// Run all signal triggers. Called from the dispatcher cycle when at least one channel is enabled.
pub async fn run(db: &DatabaseConnection, channels: &[Box<dyn NotificationChannel>], config: &Config) {
    if let Err(e) = stale_data(db, channels, config).await {
        tracing::warn!(error = %e, "stale-data trigger failed");
    }
    if let Err(e) = battery_forecast(db, channels, config).await {
        tracing::warn!(error = %e, "battery-forecast trigger failed");
    }
    if let Err(e) = sync_failures(db, channels).await {
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
        Some(r) => Ok(Some((r.try_get("", "state")?, r.try_get("", "last_notified_at")?))),
        None => Ok(None),
    }
}

async fn state_upsert(db: &DatabaseConnection, kind: &str, key: &str, state: &str) -> Result<(), DbErr> {
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
// at 2-3 replicas exactly one replica sends. The unique (kind, subject_key) key arbitrates the race —
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
/// more than `within_days` ago. Atomically advances the timestamp so a single replica re-notifies.
async fn claim_renotify(db: &DatabaseConnection, kind: &str, key: &str, within_days: i64) -> Result<bool, DbErr> {
    let sql = format!(
        "INSERT INTO notification_state (kind, subject_key, state, last_notified_at) \
         VALUES ($1, $2, 'firing', NOW()) \
         ON CONFLICT (kind, subject_key) DO UPDATE SET last_notified_at = NOW(), state = 'firing' \
         WHERE notification_state.last_notified_at < NOW() - INTERVAL '{within_days} days' \
         RETURNING 1 AS one"
    );
    let row = db
        .query_one(Statement::from_sql_and_values(PG, &sql, [kind.into(), key.into()]))
        .await?;
    Ok(row.is_some())
}

/// Claim by advancing a watermark: win iff the stored timestamp still equals `expected` (the value
/// just read) — or the row is absent. A replica that already advanced it wins the compare-and-swap and
/// the loser skips, so a digest is sent once.
async fn claim_cas(db: &DatabaseConnection, kind: &str, key: &str, expected: DateTime<Utc>) -> Result<bool, DbErr> {
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

async fn stale_data(
    db: &DatabaseConnection,
    channels: &[Box<dyn NotificationChannel>],
    config: &Config,
) -> Result<(), DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            PG,
            "SELECT sp.site_id, sp.parameter_id, s.name AS site_name, p.name AS param_name, \
                    (SELECT MAX(r.time) FROM readings r \
                       WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id \
                         AND r.replicate_index = 0) AS last_time \
             FROM site_parameters sp \
             JOIN sites s ON s.id = sp.site_id \
             JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.is_active"
                .to_string(),
        ))
        .await?;

    let threshold = Duration::hours(config.stale_data_threshold_hours);
    for r in &rows {
        let site_id: uuid::Uuid = r.try_get("", "site_id")?;
        let parameter_id: uuid::Uuid = r.try_get("", "parameter_id")?;
        let site_name: String = r.try_get("", "site_name")?;
        let param_name: String = r.try_get("", "param_name")?;
        let Some(last_time) = r.try_get::<Option<DateTime<Utc>>>("", "last_time")? else {
            continue; // never produced data — not "stale", just unpaired/new
        };
        let key = format!("{site_id}:{parameter_id}");
        let age = Utc::now() - last_time;
        let firing = age > threshold;
        let previously_firing = state_get(db, "stale_data", &key).await?.is_some();

        if firing && !previously_firing {
            // Claim the firing transition before sending so only one replica announces it.
            if claim_insert(db, "stale_data", &key).await? {
                let msg = OutgoingMessage {
                    kind: "stale_data",
                    subject: format!("River Data: no data from {site_name}"),
                    body: format!(
                        "⏳ No data from {site_name} / {param_name} for ~{}h.",
                        age.num_hours()
                    ),
                    slot: Some(Slot { project_id: None, site_id, parameter_id }),
                };
                if !deliver(db, channels, &msg, None).await {
                    state_clear(db, "stale_data", &key).await?; // release so it retries next tick
                }
            }
        } else if !firing && previously_firing {
            // Claim the resolve transition before sending the recovery.
            if claim_clear(db, "stale_data", &key).await? {
                let msg = OutgoingMessage {
                    kind: "stale_data",
                    subject: format!("River Data: data resumed from {site_name}"),
                    body: format!("✅ Data flowing again from {site_name} / {param_name}."),
                    slot: Some(Slot { project_id: None, site_id, parameter_id }),
                };
                if !deliver(db, channels, &msg, None).await {
                    state_upsert(db, "stale_data", &key, "firing").await?; // restore so it retries
                }
            }
        }
    }
    Ok(())
}

async fn battery_forecast(
    db: &DatabaseConnection,
    channels: &[Box<dyn NotificationChannel>],
    config: &Config,
) -> Result<(), DbErr> {
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
            "SELECT s.id AS site_id, s.name AS site_name, \
                (SELECT COALESCE(r2.calibrated_value, r2.raw_value) FROM readings r2 \
                   WHERE r2.site_id = s.id AND r2.parameter_id = $1 AND r2.replicate_index = 0 \
                   ORDER BY r2.time DESC LIMIT 1) AS latest, \
                (SELECT regr_slope(COALESCE(r3.calibrated_value, r3.raw_value), \
                                   EXTRACT(EPOCH FROM r3.time) / 86400.0) FROM readings r3 \
                   WHERE r3.site_id = s.id AND r3.parameter_id = $1 AND r3.replicate_index = 0 \
                     AND r3.time > NOW() - INTERVAL '7 days' \
                     AND EXTRACT(HOUR FROM r3.time) BETWEEN 2 AND 4) AS slope \
             FROM sites s",
            [battery_param.into()],
        ))
        .await?;

    let cutoff = config.battery_cutoff_volts;
    for r in &rows {
        let site_id: uuid::Uuid = r.try_get("", "site_id")?;
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
        // replica sends. On a rare send failure the advisory is suppressed until the window lapses —
        // acceptable for a forecast (threshold alarms keep full at-least-once via the dispatcher).
        if !claim_renotify(db, "battery_forecast", &key, BATTERY_RENOTIFY_DAYS).await? {
            continue;
        }
        let msg = OutgoingMessage {
            kind: "battery_forecast",
            subject: format!("River Data: battery low at {site_name}"),
            body: format!(
                "🔋 {site_name}: {latest:.2}V, trend {slope:+.3}V/day — ~{days:.0}d to {cutoff:.1}V."
            ),
            slot: Some(Slot { project_id: None, site_id, parameter_id: battery_param }),
        };
        let _ = deliver(db, channels, &msg, None).await;
    }
    Ok(())
}

async fn sync_failures(
    db: &DatabaseConnection,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
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

        // Count failures since the last time we notified for this service (24h on first run).
        let since = state_get(db, "sync_failure", &key)
            .await?
            .map_or_else(|| Utc::now() - Duration::hours(24), |(_, t)| t);
        let count_row = db
            .query_one(Statement::from_sql_and_values(
                PG,
                "SELECT COUNT(*) AS n FROM sync_events \
                 WHERE service_id = $1 AND status = 'failed' AND started_at > $2",
                [service_id.into(), since.into()],
            ))
            .await?;
        let n: i64 = count_row.map_or(0, |r| r.try_get("", "n").unwrap_or(0));
        if n == 0 {
            continue;
        }

        // Claim by advancing the watermark from the value we just read; a replica that already
        // advanced it loses the CAS and skips, so the digest is sent once.
        if !claim_cas(db, "sync_failure", &key, since).await? {
            continue;
        }
        let msg = OutgoingMessage {
            kind: "sync_failure",
            subject: format!("River Data: sync failures on {service_type}"),
            body: format!("⚠️ {n} sync failure(s) on {service_type}/{instance} since the last alert."),
            // System-wide infrastructure alert — no per-site scope, every enabled recipient gets it.
            slot: None,
        };
        let _ = deliver(db, channels, &msg, None).await;
    }
    Ok(())
}
