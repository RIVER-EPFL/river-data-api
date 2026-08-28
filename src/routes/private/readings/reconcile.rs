//! The windowed diff: convergence of stored content on source content, under a completeness
//! claim (`window {from, to, source_rows_read}`) from a sync service.
//!
//! With a window present the request is a diff, not an upsert: every stored key in the window is
//! classified `unchanged` / `changed` / `withdrawn` / `retained`, and only new, changed and
//! withdrawn rows are touched. Withdrawal is computed defensively — `stored − (admitted ∪
//! rejected ∪ dropped)` — so a key the admission funnel refused, or a cell the backend could not
//! decode, is never read as a source deletion. Retraction is a stamp (`withdrawn_at`), never a
//! delete, and a later honest window that re-asserts a row clears it.
//!
//! Rows an operator has touched (flagged, hand-curved, or in a labelled sample) never change
//! servedness without a person: a withdrawal leaves them served and raises a `source_modified`
//! hold; a value change is applied (upstream owns the value) and raises the same hold. A pass
//! that would change or withdraw more than `RECONCILE_BRAKE_FRACTION` of the window's stored
//! rows, or lose one replicate index from most of its groups, applies only its new rows and
//! raises a `brake_fired` hold. Every pass commits an `ingest_receipts` row whose arithmetic the
//! database CHECKs.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// Fraction of a window's stored rows a single pass may change or withdraw before the brake
/// holds the corrections and withdrawals (new rows always apply).
pub const RECONCILE_BRAKE_FRACTION: f64 = 0.15;
/// Fraction of a window's replicate groups that may lose one index before the brake fires, the
/// test that catches a truncated or mis-mapped member column the row-fraction test cannot see.
pub const RECONCILE_BRAKE_INDEX_FRACTION: f64 = 0.5;
/// Rows a pass must reshape before the fractions are consulted at all. The fractions were sized
/// for full-history windows (hundreds of rows); without a floor, one legitimate replicate
/// removal in a three-row window reads as a 33% reshape and brakes routine lab corrections.
pub const RECONCILE_BRAKE_MIN_ROWS: usize = 5;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceWindow {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Source rows scanned to produce the payload. Guards the honesty checks: an empty payload
    /// over a window the store holds readings for is refused, never read as a deletion.
    pub source_rows_read: u64,
    /// Instants the backend saw but could not decode; stored rows at these keys are retained.
    #[serde(default)]
    pub dropped_times: Vec<DateTime<Utc>>,
}

/// One stored row of the window, as the diff reads it.
struct StoredRow {
    raw_value: f64,
    standard_curve_id: Option<Uuid>,
    withdrawn: bool,
    /// The operator-touched predicate: flagged, hand-curved, or in a labelled/annotated sample.
    touched: bool,
}

pub struct DiffOutcome {
    pub new_rows: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub withdrawn: usize,
    pub retained: usize,
    pub reinstated: usize,
    pub braked: bool,
    /// Keys the diff decided may be written (new + changed + unchanged); under a brake, changed
    /// keys are removed so the upsert cannot correct them.
    pub apply_changed: bool,
    pub holds_raised: usize,
    pub changed_keys: Vec<(DateTime<Utc>, i16)>,
}

pub type Key = (DateTime<Utc>, i16);
/// One admitted payload row as the diff classifies it: key, raw value, declared curve.
pub type AdmittedRow = (Key, f64, Option<Uuid>);

async fn stored_window<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
) -> AppResult<HashMap<Key, StoredRow>> {
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT r.time, r.replicate_index, r.raw_value, r.standard_curve_id,
                    r.withdrawn_at IS NOT NULL AS withdrawn,
                    (r.is_flagged IS TRUE OR r.flag_reason IS NOT NULL
                     OR EXISTS (SELECT 1 FROM samples s WHERE s.id = r.sample_id
                                  AND (s.label IS NOT NULL OR s.notes IS NOT NULL))) AS touched
             FROM readings r
             WHERE r.stream_id = $1 AND r.time >= $2 AND r.time < $3",
            [
                stream_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(window.from).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(window.to).into(),
            ],
        ))
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let time = row
            .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "time")?
            .with_timezone(&Utc);
        let index: i16 = row.try_get("", "replicate_index")?;
        out.insert(
            (time, index),
            StoredRow {
                raw_value: row.try_get("", "raw_value")?,
                standard_curve_id: row.try_get("", "standard_curve_id")?,
                withdrawn: row.try_get("", "withdrawn")?,
                touched: row.try_get("", "touched")?,
            },
        );
    }
    Ok(out)
}


/// Bind a set of keys as parallel arrays for an `unnest` join. Timestamps travel as RFC 3339
/// text and are cast in SQL (`::text[]::timestamptz[]`) — the same convention as the calibration
/// resolver, because the driver cannot bind a timestamptz array directly.
fn key_arrays(keys: &[Key]) -> (sea_orm::Value, sea_orm::Value) {
    use sea_orm::sea_query::ArrayType;
    let times: Vec<sea_orm::Value> = keys.iter().map(|(t, _)| t.to_rfc3339().into()).collect();
    let indices: Vec<sea_orm::Value> = keys
        .iter()
        .map(|(_, i)| i32::from(*i).into())
        .collect();
    (
        sea_orm::Value::Array(ArrayType::String, Some(Box::new(times))),
        sea_orm::Value::Array(ArrayType::Int, Some(Box::new(indices))),
    )
}

/// The two shapes with no state in which acknowledging them is correct: refused, nothing applied.
pub fn refuse_dishonest_window(
    window: &SourceWindow,
    admitted: usize,
    stored_in_window: usize,
) -> AppResult<()> {
    if stored_in_window == 0 {
        return Ok(());
    }
    if admitted == 0 && window.source_rows_read > 0 {
        return Err(AppError::BadRequest(format!(
            "The window claims {} source rows but the payload is empty while the store holds \
             {stored_in_window} readings in it; an empty payload is never read as a deletion",
            window.source_rows_read
        )));
    }
    if window.source_rows_read == 0 {
        return Err(AppError::BadRequest(format!(
            "The window claims zero source rows over a period the store holds {stored_in_window} \
             readings for; assert the real source content or narrow the window"
        )));
    }
    Ok(())
}

async fn upsert_source_modified_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    group_time: DateTime<Utc>,
    expected: serde_json::Value,
    computed: serde_json::Value,
) -> AppResult<()> {
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO replicate_audit_holds
             (stream_id, group_time, kind, expected, computed, delta, status)
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, 'pending')
         ON CONFLICT (stream_id, group_time) WHERE status IN ('pending', 'deferred')
         DO UPDATE SET expected = EXCLUDED.expected, computed = EXCLUDED.computed,
                       kind = EXCLUDED.kind, created_at = NOW()",
        [
            stream_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(group_time).into(),
            "source_modified".into(),
            expected.into(),
            computed.into(),
        ],
    ))
    .await?;
    Ok(())
}

async fn upsert_brake_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    changed: usize,
    withdrawn: usize,
    stored: usize,
) -> AppResult<()> {
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO replicate_audit_holds
             (stream_id, group_time, kind, expected, computed, delta, status)
         VALUES ($1, $2, 'brake_fired', $3, $4, '{}'::jsonb, 'pending')
         ON CONFLICT (stream_id, group_time) WHERE status IN ('pending', 'deferred')
         DO UPDATE SET expected = EXCLUDED.expected, computed = EXCLUDED.computed,
                       kind = 'brake_fired', created_at = NOW()",
        [
            stream_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(window.from).into(),
            serde_json::json!({
                "window": { "from": window.from, "to": window.to },
                "would_change": changed,
                "would_withdraw": withdrawn,
                "stored_in_window": stored,
                "threshold": RECONCILE_BRAKE_FRACTION,
            })
            .into(),
            serde_json::json!({ "held": "changed and withdrawn; new rows applied" }).into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Classify the window and apply the withdrawal side. Runs inside the guarded transaction,
/// before the insert/upsert of the admitted rows; returns what the upsert may do (`apply_changed`
/// is false under a brake, in which case the caller inserts with `Replace::Nothing`).
#[allow(clippy::too_many_lines)]
pub async fn run_windowed_diff<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    admitted: &[AdmittedRow],
    rejected_keys: &HashSet<Key>,
) -> AppResult<DiffOutcome> {
    let stored = stored_window(conn, stream_id, window).await?;
    refuse_dishonest_window(window, admitted.len(), stored.len())?;

    let dropped_times: HashSet<DateTime<Utc>> = window.dropped_times.iter().copied().collect();

    let mut outcome = DiffOutcome {
        new_rows: 0,
        changed: 0,
        unchanged: 0,
        withdrawn: 0,
        retained: 0,
        reinstated: 0,
        braked: false,
        apply_changed: true,
        holds_raised: 0,
        changed_keys: Vec::new(),
    };

    let mut admitted_keys: HashSet<Key> = HashSet::with_capacity(admitted.len());
    let mut changed_touched: Vec<Key> = Vec::new();
    let mut reinstate: Vec<Key> = Vec::new();
    for (key, raw_value, standard_curve_id) in admitted {
        admitted_keys.insert(*key);
        // Keys outside the claimed window are plain appends and classify as new.
        match stored.get(key) {
            None => outcome.new_rows += 1,
            Some(row) => {
                let equal =
                    row.raw_value == *raw_value && row.standard_curve_id == *standard_curve_id;
                if equal {
                    outcome.unchanged += 1;
                } else {
                    outcome.changed += 1;
                    if outcome.changed_keys.len() < 500 {
                        outcome.changed_keys.push(*key);
                    }
                    if row.touched {
                        changed_touched.push(*key);
                    }
                }
                // An honest window re-asserting a row clears its retraction, equal or corrected.
                if row.withdrawn {
                    reinstate.push(*key);
                }
            }
        }
    }

    // Withdrawal, computed defensively: absent from the payload, not refused by the funnel, not
    // dropped by the backend, not already withdrawn.
    let mut to_withdraw: Vec<Key> = Vec::new();
    let mut withdraw_touched: Vec<Key> = Vec::new();
    for (key, row) in &stored {
        if admitted_keys.contains(key) || rejected_keys.contains(key) {
            continue;
        }
        if dropped_times.contains(&key.0) {
            outcome.retained += 1;
            continue;
        }
        if row.withdrawn {
            continue;
        }
        if row.touched {
            withdraw_touched.push(*key);
        } else {
            to_withdraw.push(*key);
        }
    }
    outcome.retained += rejected_keys
        .iter()
        .filter(|k| stored.contains_key(*k))
        .count();

    // The brake: a pass reshaping the stored window at scale holds its corrections and
    // withdrawals for review; new rows still apply so ingestion never stops.
    let would_touch = outcome.changed + to_withdraw.len() + withdraw_touched.len();
    let over_floor = would_touch >= RECONCILE_BRAKE_MIN_ROWS;
    let over_fraction = over_floor
        && !stored.is_empty()
        && (would_touch as f64) / (stored.len() as f64) > RECONCILE_BRAKE_FRACTION;
    let over_index_fraction = {
        let mut groups_with_index: HashMap<i16, usize> = HashMap::new();
        let mut withdrawn_with_index: HashMap<i16, usize> = HashMap::new();
        for (t, i) in stored.keys() {
            let _ = t;
            *groups_with_index.entry(*i).or_default() += 1;
        }
        for (_, i) in to_withdraw.iter().chain(withdraw_touched.iter()) {
            *withdrawn_with_index.entry(*i).or_default() += 1;
        }
        over_floor
            && withdrawn_with_index.iter().any(|(i, n)| {
                groups_with_index.get(i).is_some_and(|total| {
                    (*n as f64) / (*total as f64) > RECONCILE_BRAKE_INDEX_FRACTION
                })
            })
    };
    if over_fraction || over_index_fraction {
        // The release path: an operator who acknowledged this stream's brake_fired hold has
        // ruled that the reshape is legitimate, so exactly one braked-scale pass applies and the
        // ruling is consumed (hold -> remediated). The source re-asserts the same window every
        // cycle, so "acknowledge, then let the next cycle through" is the whole workflow; a
        // later reshape brakes afresh with a new hold.
        let release = conn
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id FROM replicate_audit_holds
                 WHERE stream_id = $1 AND kind = 'brake_fired' AND status = 'acknowledged'
                 ORDER BY created_at DESC LIMIT 1",
                [stream_id.into()],
            ))
            .await?;
        match release {
            Some(row) => {
                let hold_id: Uuid = row.try_get("", "id")?;
                conn.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE replicate_audit_holds SET status = 'remediated'
                     WHERE id = $1 AND status = 'acknowledged'",
                    [hold_id.into()],
                ))
                .await?;
                tracing::info!(%stream_id, changed = outcome.changed,
                    withdrawn = to_withdraw.len() + withdraw_touched.len(),
                    "acknowledged brake released; reshape applies once");
            }
            None => {
                outcome.braked = true;
                outcome.apply_changed = false;
                upsert_brake_hold(
                    conn,
                    stream_id,
                    window,
                    outcome.changed,
                    to_withdraw.len() + withdraw_touched.len(),
                    stored.len(),
                )
                .await?;
                outcome.holds_raised += 1;
                return Ok(outcome);
            }
        }
    }

    // Curated rows never change servedness without a person: the withdrawal is not stamped and
    // the disagreement lands in the review queue. A corrected value on a curated row IS applied
    // (upstream owns the value; the flag still excludes it from serving), with the same hold so
    // the operator re-rules on the new number.
    for key in &withdraw_touched {
        upsert_source_modified_hold(
            conn,
            stream_id,
            key.0,
            serde_json::json!({ "claim": "withdrawn", "replicate_index": key.1,
                                "window": { "from": window.from, "to": window.to } }),
            serde_json::json!({ "kept_served": true }),
        )
        .await?;
        outcome.holds_raised += 1;
    }
    for key in &changed_touched {
        upsert_source_modified_hold(
            conn,
            stream_id,
            key.0,
            serde_json::json!({ "claim": "value_changed", "replicate_index": key.1 }),
            serde_json::json!({ "applied": true, "still_excluded_if_flagged": true }),
        )
        .await?;
        outcome.holds_raised += 1;
    }

    if !to_withdraw.is_empty() {
        let (times, indices) = key_arrays(&to_withdraw);
        conn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE readings r
             SET withdrawn_at = NOW(), withdrawn_reason = 'absent from source window'
             FROM unnest($2::text[]::timestamptz[], $3::int[]::smallint[]) AS k(t, ri)
             WHERE r.stream_id = $1 AND r.time = k.t AND r.replicate_index = k.ri
               AND r.withdrawn_at IS NULL",
            [stream_id.into(), times, indices],
        ))
        .await?;
        outcome.withdrawn = to_withdraw.len();
    }

    if !reinstate.is_empty() {
        let (times, indices) = key_arrays(&reinstate);
        conn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE readings r
             SET withdrawn_at = NULL, withdrawn_reason = NULL
             FROM unnest($2::text[]::timestamptz[], $3::int[]::smallint[]) AS k(t, ri)
             WHERE r.stream_id = $1 AND r.time = k.t AND r.replicate_index = k.ri
               AND r.withdrawn_at IS NOT NULL",
            [stream_id.into(), times, indices],
        ))
        .await?;
        outcome.reinstated = reinstate.len();
    }

    Ok(outcome)
}

/// Commit the pass's receipt. The arithmetic CHECK (`submitted = new + changed + unchanged +
/// rejected_total`) makes a write path that cannot account for a submitted row unable to commit.
#[allow(clippy::too_many_arguments)]
pub async fn write_receipt<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    submitted: usize,
    outcome: &DiffOutcome,
    rejected_total: usize,
    rejected: &serde_json::Value,
) -> AppResult<()> {
    let changed_keys = if outcome.changed_keys.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(
            outcome
                .changed_keys
                .iter()
                .map(|(t, i)| serde_json::json!([t, i]))
                .collect::<Vec<_>>()
        )
    };
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO ingest_receipts
             (stream_id, window_from, window_to, submitted, new_rows, changed, unchanged,
              retained, rejected_total, rejected, dropped, withdrawn, changed_keys, braked,
              brake_threshold)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        [
            stream_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(window.from).into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(window.to).into(),
            i32::try_from(submitted).unwrap_or(i32::MAX).into(),
            i32::try_from(outcome.new_rows).unwrap_or(i32::MAX).into(),
            i32::try_from(outcome.changed).unwrap_or(i32::MAX).into(),
            i32::try_from(outcome.unchanged).unwrap_or(i32::MAX).into(),
            i32::try_from(outcome.retained).unwrap_or(i32::MAX).into(),
            i32::try_from(rejected_total).unwrap_or(i32::MAX).into(),
            rejected.clone().into(),
            i32::try_from(window.dropped_times.len())
                .unwrap_or(i32::MAX)
                .into(),
            i32::try_from(outcome.withdrawn).unwrap_or(i32::MAX).into(),
            changed_keys.into(),
            outcome.braked.into(),
            (RECONCILE_BRAKE_FRACTION as f32).into(),
        ],
    ))
    .await?;
    Ok(())
}
