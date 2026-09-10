//! The notification sweeps: the outbox dispatch, the signal triggers, the push-subscription
//! reconcile, and the jobs that drive them.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
    Statement,
};

use super::models::*;
use super::service::*;
use crate::common::AppState;
use crate::config::Config;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use crate::routes::private::reprocessing_jobs::schedule::Schedule;
use crate::routes::private::sync::models::services as sync_services;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

pub async fn sweep(state: &AppState) -> Result<SweepOutcome, sea_orm::DbErr> {
    let db = &state.db;
    let mut outcome = SweepOutcome::default();

    let subs: Vec<String> = push_subscription::Entity::find()
        .select_only()
        .column(push_subscription::Column::KeycloakSub)
        .distinct()
        .into_tuple()
        .all(db)
        .await?;

    for sub in subs {
        let resolution = state.authorizer.resolve(state, &sub).await;
        if matches!(resolution, Some(RoleResolution::Revoked)) {
            let res = push_subscription::Entity::delete_many()
                .filter(push_subscription::Column::KeycloakSub.eq(sub.clone()))
                .exec(db)
                .await?;
            let removed = res.rows_affected;
            outcome.revoked += removed as usize;
            if removed > 0 {
                // An access change is the one thing this sweep does that somebody may need to
                // read back, and it belongs in the entity trail rather than the reading ledger
                // (Q57, M164).
                db.execute_raw(Statement::from_sql_and_values(
                    PG,
                    "INSERT INTO change_audit (subject, change, old_value, new_value, changed_by) \
                     VALUES ($1, 'access_revoked', $2::jsonb, NULL, 'system')",
                    [
                        format!("push_subscriptions:{sub}").into(),
                        serde_json::json!({ "removed": removed }).to_string().into(),
                    ],
                ))
                .await?;
            }
            tracing::info!(sub = %sub, "push_reconcile: pruned subscriptions for revoked user");
        }
    }

    Ok(outcome)
}

/// One drain of the outbox (open + resolve passes). Exposed `pub` so integration tests can drive it
/// deterministically with injected channels instead of waiting on the interval. Needs the live
/// `AppState` so the per-recipient project-access guard can resolve grants and roles.
pub async fn dispatch_once(state: &AppState, channels: &[Box<dyn NotificationChannel>]) {
    if let Err(e) = process_pending(state, channels, true).await {
        tracing::warn!(error = %e, "notification dispatcher: open pass failed");
    }
    if let Err(e) = process_pending(state, channels, false).await {
        tracing::warn!(error = %e, "notification dispatcher: resolve pass failed");
    }
    // Signal triggers only run when a channel is configured (they do heavier detection queries).
    if !channels.is_empty() {
        run(state, channels).await;
    }
}

async fn process_pending(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
    opened: bool,
) -> Result<(), DbErr> {
    let db = &state.db;
    let config = state.config.as_ref();
    let rows = fetch_pending(db, opened).await?;
    if rows.is_empty() {
        return Ok(());
    }

    let column = if opened {
        "notified_at"
    } else {
        "resolution_notified_at"
    };

    // A muted slot is suppressed inside `deliver`, which reports success, so the claim below
    // stands and neither this replica nor a peer re-picks the event.
    //
    // One message per event so each carries its own slot, fan-out is per-subscriber by scope, which
    // a single batched message spanning multiple slots couldn't express. Each event is CLAIMED with an
    // atomic stamp before the send, so at 2-3 replicas exactly one replica owns it; a transient send
    // failure releases the claim so the next tick retries (at-least-once). The claim is one autocommit
    // UPDATE, no DB connection is held across the external send.
    let base = config.dashboard_base_url.as_deref();
    for r in &rows {
        if !claim_event(db, column, r.id).await? {
            continue; // a peer replica already claimed this event
        }
        let events = [r.event.clone()];
        let mut msg = if opened {
            render_opened(&events, base)
        } else {
            render_resolved(&events, base)
        };
        msg.slot = Some(Slot {
            project_id: Some(r.project_id),
            site_id: r.slot.0,
            parameter_id: r.slot.1,
        });
        if !deliver(state, channels, &msg, Some(r.id)).await {
            release_claim(db, column, r.id).await?;
        }
    }
    Ok(())
}

/// The rows each trigger's query returns. Derived rather than hand-decoded so a column added to a
/// query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct StaleSlot {
    site_id: uuid::Uuid,
    parameter_id: uuid::Uuid,
    project_id: Option<uuid::Uuid>,
    site_name: String,
    param_name: String,
    last_continuous: Option<DateTime<Utc>>,
    last_spot: Option<DateTime<Utc>>,
    spot_max_gap_seconds: Option<f64>,
}

#[derive(FromQueryResult)]
struct BatteryTrend {
    site_id: uuid::Uuid,
    project_id: Option<uuid::Uuid>,
    site_name: String,
    latest: Option<f64>,
    slope: Option<f64>,
}

#[derive(FromQueryResult)]
struct UnpairedStream {
    id: uuid::Uuid,
    source_system: String,
    label: String,
}

#[derive(FromQueryResult)]
struct HoldCount {
    kind: String,
    n: i64,
}

#[derive(FromQueryResult)]
struct FailedJobs {
    trigger_type: String,
    n: i64,
    sample_error: Option<String>,
    scope: Option<serde_json::Value>,
}

#[derive(FromQueryResult)]
struct FailureCounts {
    n_failed: i64,
    n_partial: i64,
    sample_error: Option<String>,
}

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
    if let Err(e) = sync_staleness(state, channels).await {
        tracing::warn!(error = %e, "sync-staleness trigger failed");
    }
    if let Err(e) = streams_unpaired(state, channels).await {
        tracing::warn!(error = %e, "unpaired-streams trigger failed");
    }
    if let Err(e) = holds_open(state, channels).await {
        tracing::warn!(error = %e, "open-holds trigger failed");
    }
    if let Err(e) = jobs_failed(state, channels).await {
        tracing::warn!(error = %e, "failed-job trigger failed");
    }
    if let Err(e) = changes_pending(state, channels).await {
        tracing::warn!(error = %e, "pending-change trigger failed");
    }
    if let Err(e) = curve_drift(state, channels).await {
        tracing::warn!(error = %e, "curve-drift trigger failed");
    }
    if let Err(e) = derived_computed(state, channels).await {
        tracing::warn!(error = %e, "derived-computed trigger failed");
    }
    if let Err(e) = operational_digests(state, channels).await {
        tracing::warn!(error = %e, "operational-digest trigger failed");
    }
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
    db.execute_raw(Statement::from_string(
        PG,
        "DELETE FROM notification_state \
         WHERE kind = 'stale_data' AND subject_key NOT LIKE '%:%:%'"
            .to_string(),
    ))
    .await?;

    // The dispatcher wakes on every alarm-state broadcast, so each lookup is a backward walk of
    // idx_readings_site_param_time stopping at the first match, never an aggregate over the slot.
    let rows = db
        .query_all_raw(Statement::from_string(
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
                       AND r.measurement_type = 'spot' AND r.withdrawn_at IS NULL \
                     ORDER BY r.time DESC LIMIT 1) AS last_spot, \
                   (SELECT EXTRACT(EPOCH FROM MAX(g.gap))::float8 FROM ( \
                      SELECT s.t - LAG(s.t) OVER (ORDER BY s.t) AS gap FROM ( \
                        SELECT DISTINCT r.time AS t FROM readings r \
                        WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id \
                          AND r.measurement_type = 'spot' AND r.withdrawn_at IS NULL \
                        ORDER BY r.time DESC LIMIT 5) s \
                    ) g) AS spot_max_gap_seconds \
             ) agg ON TRUE \
             WHERE sp.is_active"
                .to_string(),
        ))
        .await?;

    let base_threshold = Duration::hours(config.stale_data_threshold_hours);
    for r in &rows {
        let StaleSlot {
            site_id,
            parameter_id,
            project_id,
            site_name,
            param_name,
            last_continuous,
            last_spot,
            spot_max_gap_seconds,
        } = StaleSlot::from_query_result(r, "")?;

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
                        subject: format!("RIVER Data: no {noun} from {site_name}"),
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
                        subject: format!("RIVER Data: {noun} resumed from {site_name}"),
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
        .query_one_raw(Statement::from_string(
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
        .query_all_raw(Statement::from_sql_and_values(
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
        let BatteryTrend {
            site_id,
            project_id,
            site_name,
            latest,
            slope,
        } = BatteryTrend::from_query_result(r, "")?;
        let (Some(latest), Some(slope)) = (latest, slope) else {
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
            subject: format!("RIVER Data: battery low at {site_name}"),
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

/// Hours between repeat alerts while a sync service stays heartbeat-dead.
const SYNC_STALE_RENOTIFY_HOURS: i64 = 12;

/// Hours between repeat alerts while the review queue still holds an open decision.
const HOLDS_RENOTIFY_HOURS: i64 = SYNC_STALE_RENOTIFY_HOURS;

/// A sync service whose heartbeat has stopped. `sync_failures` cannot see this — it counts
/// `sync_events` rows and a dead service writes none (the three portal services were once
/// heartbeat-dead for two days with zero notifications) — so the expectation is judged from the
/// heartbeat itself, with the same thresholds the operator health view uses.
async fn sync_staleness(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let stale_after = state.config.sync_health_warning_secs;
    let services = sync_services::Entity::find()
        .filter(sync_services::Column::Paused.eq(false))
        .all(db)
        .await?;

    for svc in services {
        let (instance, service_type, last_heartbeat) =
            (svc.instance_id, svc.service_type, svc.last_heartbeat);
        let Some(hb) = last_heartbeat else {
            // Never enrolled to the point of a heartbeat: the enrollment path's problem, and
            // alerting on it forever would drown the signal this trigger exists for.
            continue;
        };
        let age = Utc::now() - hb.with_timezone(&Utc);
        if age.num_seconds() <= stale_after {
            continue;
        }
        let key = instance.clone();
        if !claim_renotify(db, "sync_stale", &key, SYNC_STALE_RENOTIFY_HOURS).await? {
            continue;
        }
        let msg = OutgoingMessage {
            kind: "sync_stale",
            subject: format!("RIVER Data: {service_type} sync service is silent"),
            body: format!(
                "🔌 {service_type}/{instance} last heartbeat {} ({} hours ago). Its source is                  not being synced; check the service.",
                hb.to_rfc3339(),
                age.num_hours()
            ),
            // System-wide infrastructure alert, no per-site scope.
            slot: None,
        };
        let _ = deliver(state, channels, &msg, None).await;
    }
    Ok(())
}

/// A stream a sync brought in that nobody has paired yet. Its readings are stored unattributed, so
/// they reach no site view and no aggregate until an operator pairs it; nothing else points at the
/// wizard, so the discovery has to arrive rather than be found.
///
/// A stream is a standing condition, keyed and claimed per stream so each one is announced once and
/// stops being announced when it is paired, and the cycle's new ones are digested per source system
/// rather than sent one message each. Pairing is the operator's own action, so it clears the state
/// silently instead of sending a recovery notice.
async fn streams_unpaired(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;

    // A stream that was paired, deactivated or deleted is no longer waiting: drop its row so that
    // an unpairing later reads as a fresh discovery.
    db.execute_raw(Statement::from_string(
        PG,
        "DELETE FROM notification_state ns WHERE ns.kind = 'streams_unpaired' \
         AND NOT EXISTS (SELECT 1 FROM data_streams ds WHERE ds.id::text = ns.subject_key \
                         AND ds.site_parameter_id IS NULL AND ds.is_active)"
            .to_string(),
    ))
    .await?;

    // `last_data_time` is the stream's cursor, so it is set exactly when readings have landed.
    let rows = db
        .query_all_raw(Statement::from_string(
            PG,
            "SELECT id, source_system, COALESCE(source_name, source_key) AS label \
             FROM data_streams \
             WHERE site_parameter_id IS NULL AND is_active AND last_data_time IS NOT NULL \
             ORDER BY source_system, source_key"
                .to_string(),
        ))
        .await?;

    let mut by_system: std::collections::BTreeMap<String, Vec<(uuid::Uuid, String)>> =
        std::collections::BTreeMap::new();
    for r in &rows {
        let UnpairedStream {
            id,
            source_system,
            label,
        } = UnpairedStream::from_query_result(r, "")?;
        // Claim the firing transition before sending so only one replica announces it.
        if claim_insert(db, "streams_unpaired", &id.to_string()).await? {
            by_system
                .entry(source_system)
                .or_default()
                .push((id, label));
        }
    }

    for (source_system, claimed) in by_system {
        let n = claimed.len();
        let listed = claimed
            .iter()
            .take(5)
            .map(|(_, label)| label.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let more = if n > 5 {
            format!(" and {} more", n - 5)
        } else {
            String::new()
        };
        let msg = OutgoingMessage {
            kind: "streams_unpaired",
            subject: format!("RIVER Data: {n} unpaired stream(s) on {source_system}"),
            body: format!(
                "🔗 {n} {source_system} stream(s) are storing readings with no site parameter, so \
                 nothing is attributed: {listed}{more}. Pair them from Data Streams."
            ),
            // Which slot they belong to is the question being asked, so there is none yet.
            slot: None,
        };
        if !deliver(state, channels, &msg, None).await {
            for (id, _) in &claimed {
                state_clear(db, "streams_unpaired", &id.to_string()).await?; // retry next tick
            }
        }
    }
    Ok(())
}

/// Open review-queue holds. A held source edit or a statistics disagreement waits for a person, and
/// the audits panel is only found by people who already know it exists, so the backlog is announced
/// on the same cadence a silent sync service is.
async fn holds_open(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let rows = db
        .query_all_raw(Statement::from_string(
            PG,
            format!(
                "SELECT kind, COUNT(*)::bigint AS n FROM replicate_audit_holds \
                 WHERE status IN {open} GROUP BY kind ORDER BY kind",
                open = crate::routes::private::sync::service::OPEN
            ),
        ))
        .await?;
    if rows.is_empty() {
        // Nothing is waiting, so a later backlog is a fresh transition rather than a repeat.
        state_clear(db, "holds_open", "all").await?;
        return Ok(());
    }

    let mut total = 0i64;
    let mut parts = Vec::new();
    for r in &rows {
        let HoldCount { kind, n } = HoldCount::from_query_result(r, "")?;
        total += n;
        parts.push(format!("{n} {kind}"));
    }
    if !claim_renotify(db, "holds_open", "all", HOLDS_RENOTIFY_HOURS).await? {
        return Ok(());
    }
    let msg = OutgoingMessage {
        kind: "holds_open",
        subject: format!("RIVER Data: {total} hold(s) awaiting review"),
        body: format!(
            "📋 {total} hold(s) are open in the review queue ({}). Review them under Data \
             Streams, Audits.",
            parts.join(", ")
        ),
        // The queue spans slots and unpaired streams alike, so it carries no single scope.
        slot: None,
    };
    let _ = deliver(state, channels, &msg, None).await;
    Ok(())
}

/// Hours between repeat alerts while proposed corrections wait for a decision.
const PROPOSALS_RENOTIFY_HOURS: i64 = 12;

/// Values a source has changed since river-data stored them, waiting for a person (Q84). Nothing
/// is written until one of them is accepted, so an unread queue is stored history diverging from
/// the portal in silence.
async fn changes_pending(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let counts = crate::routes::private::readings::proposals::pending_by_source(db)
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
    // What arrived is the other half of the sentence Q84 asks for, and usually the larger number:
    // a cycle that adds four thousand readings and holds nothing is an event nobody is told about
    // if only the queue is counted. Read before the claim, which moves the watermark.
    let since = state_get(db, "changes_pending", "all")
        .await?
        .map(|(_, at)| at)
        .unwrap_or_else(|| Utc::now() - Duration::hours(PROPOSALS_RENOTIFY_HOURS));
    let arrivals = arrivals_by_source(db, since).await?;
    if counts.is_empty() && arrivals.is_empty() {
        state_clear(db, "changes_pending", "all").await?;
        return Ok(());
    }
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    let arrived: i64 = arrivals.iter().map(|(_, n)| n).sum();
    if !claim_renotify(db, "changes_pending", "all", PROPOSALS_RENOTIFY_HOURS).await? {
        return Ok(());
    }
    let by_source = |rows: &[(String, i64)]| {
        rows.iter()
            .map(|(source, n)| format!("{n} from {source}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut body = String::new();
    if arrived > 0 {
        body.push_str(&format!(
            "📥 {arrived} new value(s) synced ({}). ",
            by_source(&arrivals)
        ));
    }
    if total > 0 {
        body.push_str(&format!(
            "✏️ {total} stored value(s) have been changed at source and are awaiting a decision \
             ({}). Accept or reject them under Data Streams, Audits.",
            by_source(&counts)
        ));
    }
    let msg = OutgoingMessage {
        kind: "changes_pending",
        subject: if total > 0 {
            format!("RIVER Data: {total} stored value(s) changed at source")
        } else {
            format!("RIVER Data: {arrived} new value(s) synced")
        },
        body: body.trim_end().to_string(),
        // The queue spans streams and sources alike, so it carries no single scope.
        slot: None,
    };
    let _ = deliver(state, channels, &msg, None).await;
    Ok(())
}

/// The longest a recomposed value goes unannounced when the digest cannot be claimed.
const CURVE_DRIFT_WINDOW_HOURS: i64 = 24;

/// Values the janitor recomposed because they had drifted from the curves their own rows name
/// (Q57, Q118). The sweep repairs and records rather than asking, so this says what moved and
/// where to read it, and the ledger carries the rollback.
async fn curve_drift(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let since = state_get(db, "curve_drift", "all")
        .await?
        .map(|(_, at)| at)
        .unwrap_or_else(|| Utc::now() - Duration::hours(CURVE_DRIFT_WINDOW_HOURS));
    let moved = decisions_since(db, "curve_recompose", since).await?;
    if moved == 0 {
        state_clear(db, "curve_drift", "all").await?;
        return Ok(());
    }
    if !claim_cas(db, "curve_drift", "all", since).await? {
        return Ok(());
    }
    let msg = OutgoingMessage {
        kind: "curve_drift",
        subject: format!("RIVER Data: {moved} corrected value(s) recomposed"),
        body: format!(
            "🧮 {moved} stored value(s) no longer matched the curves their readings name and were \
             recomposed from them. Each move is recorded on the reading and can be rolled back \
             from its history."
        ),
        // The sweep spans every slot whose curves moved, so it carries no single scope.
        slot: None,
    };
    let _ = deliver(state, channels, &msg, None).await;
    Ok(())
}

/// Derived values computed where none was stored, announced the way recomposed ones are (Q57).
async fn derived_computed(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let since = state_get(db, "derived_computed", "all")
        .await?
        .map(|(_, at)| at)
        .unwrap_or_else(|| Utc::now() - Duration::hours(CURVE_DRIFT_WINDOW_HOURS));
    let computed = decisions_since(db, "derived_computed", since).await?;
    if computed == 0 {
        state_clear(db, "derived_computed", "all").await?;
        return Ok(());
    }
    if !claim_cas(db, "derived_computed", "all", since).await? {
        return Ok(());
    }
    let msg = OutgoingMessage {
        kind: "derived_computed",
        subject: format!("RIVER Data: {computed} derived value(s) computed"),
        body: format!(
            "🧮 {computed} derived value(s) were computed where none was stored. Each one names \
             the formula version it was made with, on the reading."
        ),
        // The fill spans every slot with a gap, so it carries no single scope.
        slot: None,
    };
    let _ = deliver(state, channels, &msg, None).await;
    Ok(())
}

/// What the arms that keep the system running did, one channel each (Q57). None is on by default:
/// a refresh or a prune is upkeep, and the person who wants to watch it says so. The number comes
/// from the job rows the arm already writes, so nothing here counts anything twice.
///
/// Each entry is the kind, the job that does the work, the `detail.counts` key holding its number
/// (`None` reads the job's own headline `readings_updated`), and how the message says it.
const OPERATIONAL: [(&str, &str, Option<&str>, &str); 5] = [
    (
        "access_revoked",
        "identity_reconcile",
        Some("revoked"),
        "push subscription(s) removed for people who lost their grant",
    ),
    (
        "aggregates_refreshed",
        "janitor_service",
        Some("recomposed"),
        "value(s) recomposed while the rollups were refreshed",
    ),
    (
        "jobs_pruned",
        "janitor_service",
        Some("pruned"),
        "tracked job row(s) aged out of the timeline",
    ),
    (
        "sync_events_swept",
        "sync_event_sweep",
        Some("sync_events_closed"),
        "stale sync event(s) marked failed",
    ),
    (
        "ledger_pruned",
        "sync_ledger_retention",
        None,
        "sync event(s) and ingest receipt(s) deleted by retention",
    ),
];

async fn operational_digests(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    for (kind, trigger_type, key, noun) in OPERATIONAL {
        let since = state_get(db, kind, "all")
            .await?
            .map(|(_, at)| at)
            .unwrap_or_else(|| Utc::now() - Duration::hours(CURVE_DRIFT_WINDOW_HOURS));
        let n = job_total_since(db, trigger_type, key, since).await?;
        if n == 0 {
            state_clear(db, kind, "all").await?;
            continue;
        }
        if !claim_cas(db, kind, "all", since).await? {
            continue;
        }
        let msg = OutgoingMessage {
            kind,
            subject: format!("RIVER Data: {n} {noun}"),
            body: format!("🧹 {n} {noun}. The run that did it is in the job timeline."),
            // Upkeep spans the whole deployment, so it carries no slot.
            slot: None,
        };
        let _ = deliver(state, channels, &msg, None).await;
    }
    Ok(())
}

/// Hours between repeat alerts while jobs of one kind keep failing.
const JOB_FAILED_RENOTIFY_HOURS: i64 = 6;

/// A background job that has spent its retries and ended `failed`. Everything the operator
/// triggers runs as one of these, so a plan apply that violated a constraint or a reprocess that
/// died leaves an `error_message` on a row and nothing else; the digest is per trigger type so a
/// broken kind failing on every run is one alert rather than one per job.
async fn jobs_failed(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
) -> Result<(), DbErr> {
    let db = &state.db;
    let rows = db
        .query_all_raw(Statement::from_string(
            PG,
            "SELECT trigger_type, COUNT(*)::bigint AS n, \
                    (ARRAY_AGG(error_message ORDER BY completed_at DESC) \
                        FILTER (WHERE error_message IS NOT NULL))[1] AS sample_error, \
                    (ARRAY_AGG(detail -> 'scope' ORDER BY completed_at DESC) \
                        FILTER (WHERE detail -> 'scope' IS NOT NULL))[1] AS scope \
               FROM reprocessing_jobs \
              WHERE status = 'failed' AND completed_at > NOW() - INTERVAL '24 hours' \
              GROUP BY trigger_type ORDER BY trigger_type"
                .to_string(),
        ))
        .await?;

    for row in &rows {
        let FailedJobs {
            trigger_type,
            n,
            sample_error,
            scope,
        } = FailedJobs::from_query_result(row, "")?;
        if !claim_renotify(db, "job_failed", &trigger_type, JOB_FAILED_RENOTIFY_HOURS).await? {
            continue;
        }
        let mut body = format!(
            "🛠 {n} {trigger_type} job(s) failed in the last 24 hours and will not be retried again."
        );
        if let Some(scope) = scope.filter(|s| !s.is_null()) {
            body.push_str(&format!("\nScope: {scope}"));
        }
        if let Some(err) = sample_error {
            let err: String = err.chars().take(300).collect();
            body.push_str(&format!("\nLatest error: {err}"));
        }
        body.push_str("\nOpen it under System, Jobs.");
        let msg = OutgoingMessage {
            kind: "job_failed",
            subject: format!("RIVER Data: {trigger_type} job failed"),
            body,
            // A job kind spans whatever it was run over, so the digest carries no single slot.
            slot: None,
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
    let services = sync_services::Entity::find().all(db).await?;

    for svc in services {
        let (service_id, instance, service_type) = (svc.id, svc.instance_id, svc.service_type);
        let key = instance.clone();

        // Count unhealthy cycles since the last time we notified for this service (24h on first run).
        // A driver only returns Err when the whole cycle collapses, so per-stream ingest errors land
        // as 'partial'; counting 'failed' alone leaves a service that fails every stream silent.
        let since = state_get(db, "sync_failure", &key)
            .await?
            .map_or_else(|| Utc::now() - Duration::hours(24), |(_, t)| t);
        let count_row = db
            .query_one_raw(Statement::from_sql_and_values(
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
        let FailureCounts {
            n_failed,
            n_partial,
            sample_error,
        } = FailureCounts::from_query_result(&count_row, "").unwrap_or(FailureCounts {
            n_failed: 0,
            n_partial: 0,
            sample_error: None,
        });
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
            subject: format!("RIVER Data: sync failures on {service_type}"),
            body,
            // System-wide infrastructure alert, no per-site scope, every enabled recipient gets it.
            slot: None,
        };
        let _ = deliver(state, channels, &msg, None).await;
    }
    Ok(())
}

/// Prune Web Push subscriptions for users whose Keycloak account is revoked or disabled.
pub struct PushSubscriptionReconcile {
    interval_seconds: u64,
}

impl PushSubscriptionReconcile {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.identity_reconcile_interval_seconds,
        }
    }
}

#[async_trait]
impl Job for PushSubscriptionReconcile {
    fn name(&self) -> &'static str {
        "identity_reconcile"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("push_subscription_reconcile: no AppState in process; skipping");
            return Ok(0);
        };
        match sweep(&state).await {
            Ok(o) => {
                if o.total() > 0 {
                    tracing::info!(
                        revoked = o.revoked,
                        "Push subscription reconciliation: users pruned"
                    );
                }
                ctx.report(JobReport::new().count("revoked", o.revoked))
                    .await;
                Ok(o.total() as i64)
            }
            Err(e) => Err(e),
        }
    }
}

/// Probe each configured notification channel and upsert
/// The channel health heartbeat. Wraps [`health::probe_once`].
pub struct NotifyHealth {
    interval_seconds: u64,
}

impl NotifyHealth {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.notify_health_interval_seconds.max(30),
        }
    }
}

#[async_trait]
impl Job for NotifyHealth {
    fn name(&self) -> &'static str {
        "notify_health"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("notify_health: no AppState in process; skipping");
            return Ok(0);
        };
        let probed = probe_once(ctx.db(), &state.config).await;
        ctx.report(JobReport::new().count("channels_probed", probed))
            .await;
        Ok(0)
    }
}

/// Drain the `alarm_events` notification outbox (open + resolve passes) and run the signal triggers,
/// the notification dispatcher. Wraps [`dispatcher::dispatch_once`]. The `AlarmStateChanged`
/// broadcast still wakes an immediate enqueue in `main.rs` for low latency; this schedule is the
/// fallback cadence.
pub struct DispatchNotifications {
    interval_seconds: u64,
}

impl DispatchNotifications {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.notify_poll_interval_seconds,
        }
    }
}

#[async_trait]
impl Job for DispatchNotifications {
    fn name(&self) -> &'static str {
        "dispatch_notifications"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("dispatch_notifications: no AppState in process; skipping");
            return Ok(0);
        };
        let channels = build_channels(&state.config);
        ctx.report(JobReport::new().count("channels", channels.len()))
            .await;
        dispatch_once(&state, &channels).await;
        Ok(0)
    }
}
