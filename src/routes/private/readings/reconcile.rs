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
use crate::routes::private::sync::replicate_audit as audit;

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
    /// The client's digest of this payload's source-asserted content. Opaque: persisted on the
    /// stream when the pass applies cleanly, echoed on the stream list, and never computed
    /// server-side. The client skips its next pass when its content digests to the same value.
    #[serde(default)]
    pub content_digest: Option<String>,
}

/// One stored row of the window, as the diff reads it.
struct StoredRow {
    raw_value: f64,
    standard_curve_id: Option<Uuid>,
    withdrawn: bool,
    /// The judgements standing on the row, as `{id, kind}`. Non-empty means a person has ruled
    /// on it, and the hold a collision raises names exactly these.
    judgements: serde_json::Value,
    /// Whether a person has ruled on the row at all: a live judgement decision, or the curation
    /// columns of a row that predates the record (T20 gives those their decisions), or a sample
    /// an operator labelled or annotated.
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
    /// Whether classified-changed rows apply this pass; false under a brake.
    pub apply_changed: bool,
    pub holds_raised: usize,
    pub changed_keys: Vec<(DateTime<Utc>, i16)>,
    /// The keys this pass stamped withdrawn, and the keys it cleared the stamp from. Neither is a
    /// payload row (a withdrawn key is absent from the payload by construction), so a consumer
    /// working from the request alone cannot see the instants whose served value moved.
    pub withdrawn_keys: Vec<Key>,
    pub reinstated_keys: Vec<Key>,
    /// The keys the upsert should write: new rows, plus changed rows when they apply. An
    /// unchanged row re-written with identical values is WAL churn the diff exists to avoid;
    /// under a brake the changed keys are excluded so the upsert cannot correct them.
    pub write_keys: HashSet<Key>,
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
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT r.time, r.replicate_index, r.raw_value, r.standard_curve_id,
                        r.withdrawn_at IS NOT NULL AS withdrawn,
                        {judgements} AS judgements,
                        ({judgements} <> '[]'::jsonb
                         OR r.is_flagged IS TRUE OR r.flag_reason IS NOT NULL
                         OR r.label IS NOT NULL OR r.notes IS NOT NULL) AS touched
                 FROM readings r
                 WHERE r.stream_id = $1 AND r.time >= $2 AND r.time < $3",
                judgements = super::decisions::live_judgements_sql("r")
            ),
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
                judgements: row.try_get("", "judgements")?,
                touched: row.try_get("", "touched")?,
            },
        );
    }
    Ok(out)
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

/// Which scale rule a pass trips, or `None` when it is within both. The floor is consulted first:
/// the fractions were sized for full-history windows, so a small window reshapes freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brake {
    /// The pass would change or withdraw too large a share of the window's stored rows.
    Fraction,
    /// One replicate index is withdrawn from too many of the groups that carry it, the shape a
    /// truncated or mis-mapped member column takes and the row fraction cannot see.
    IndexLoss,
}

/// `stored` is every row the window holds, `would_touch` the rows the pass would change or
/// withdraw, and `per_index` one `(withdrawn, stored)` pair per replicate index the pass withdraws
/// from.
pub fn brake_verdict(
    stored: usize,
    would_touch: usize,
    per_index: &[(usize, usize)],
) -> Option<Brake> {
    if would_touch < RECONCILE_BRAKE_MIN_ROWS {
        return None;
    }
    if stored > 0 && (would_touch as f64) / (stored as f64) > RECONCILE_BRAKE_FRACTION {
        return Some(Brake::Fraction);
    }
    if per_index.iter().any(|(withdrawn, total)| {
        *total > 0 && (*withdrawn as f64) / (*total as f64) > RECONCILE_BRAKE_INDEX_FRACTION
    }) {
        return Some(Brake::IndexLoss);
    }
    None
}

pub(crate) async fn upsert_source_modified_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    group_time: DateTime<Utc>,
    expected: serde_json::Value,
    computed: serde_json::Value,
    status: &str,
) -> AppResult<()> {
    audit::upsert_hold(
        conn,
        &audit::Hold {
            key: audit::HoldKey::Stream {
                stream_id,
                group_time,
            },
            kind: "source_modified",
            expected,
            computed,
            delta: serde_json::json!({}),
            status,
            tool: None,
        },
    )
    .await
}

async fn upsert_brake_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    window: &SourceWindow,
    changed: usize,
    withdrawn: usize,
    stored: usize,
    status: &str,
) -> AppResult<()> {
    audit::upsert_hold(
        conn,
        &audit::Hold {
            key: audit::HoldKey::Stream {
                stream_id,
                group_time: window.from,
            },
            kind: "brake_fired",
            expected: serde_json::json!({
                "window": { "from": window.from, "to": window.to },
                "would_change": changed,
                "would_withdraw": withdrawn,
                "stored_in_window": stored,
                "threshold": RECONCILE_BRAKE_FRACTION,
            }),
            computed: serde_json::json!({ "held": "changed and withdrawn; new rows applied" }),
            delta: serde_json::json!({}),
            status,
            tool: None,
        },
    )
    .await
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
    actor: &str,
    paired: bool,
) -> AppResult<DiffOutcome> {
    let status = audit::status_for(paired);
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
        withdrawn_keys: Vec::new(),
        reinstated_keys: Vec::new(),
        write_keys: HashSet::new(),
    };

    let mut admitted_keys: HashSet<Key> = HashSet::with_capacity(admitted.len());
    let mut changed_touched: Vec<(Key, serde_json::Value)> = Vec::new();
    let mut reinstate: Vec<Key> = Vec::new();
    let mut changed_all: Vec<Key> = Vec::new();
    for (key, raw_value, standard_curve_id) in admitted {
        admitted_keys.insert(*key);
        // Keys outside the claimed window are plain appends and classify as new.
        match stored.get(key) {
            None => {
                outcome.new_rows += 1;
                outcome.write_keys.insert(*key);
            }
            Some(row) => {
                let equal =
                    row.raw_value == *raw_value && row.standard_curve_id == *standard_curve_id;
                if equal {
                    outcome.unchanged += 1;
                } else {
                    outcome.changed += 1;
                    changed_all.push(*key);
                    if outcome.changed_keys.len() < 500 {
                        outcome.changed_keys.push(*key);
                    }
                    if row.touched {
                        changed_touched.push((*key, row.judgements.clone()));
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
    let mut withdraw_touched: Vec<(Key, serde_json::Value)> = Vec::new();
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
            withdraw_touched.push((*key, row.judgements.clone()));
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
    let mut groups_with_index: HashMap<i16, usize> = HashMap::new();
    let mut withdrawn_with_index: HashMap<i16, usize> = HashMap::new();
    for (_, i) in stored.keys() {
        *groups_with_index.entry(*i).or_default() += 1;
    }
    for (_, i) in to_withdraw
        .iter()
        .chain(withdraw_touched.iter().map(|(key, _)| key))
    {
        *withdrawn_with_index.entry(*i).or_default() += 1;
    }
    let per_index: Vec<(usize, usize)> = withdrawn_with_index
        .iter()
        .filter_map(|(i, n)| groups_with_index.get(i).map(|total| (*n, *total)))
        .collect();
    if brake_verdict(stored.len(), would_touch, &per_index).is_some() {
        // The release path: an operator who acknowledged this stream's brake_fired hold has
        // ruled that the reshape is legitimate, so exactly one braked-scale pass applies and the
        // ruling is consumed (hold -> remediated). The source re-asserts the same window every
        // cycle, so "acknowledge, then let the next cycle through" is the whole workflow; a
        // later reshape brakes afresh with a new hold.
        let release = conn
            .query_one_raw(Statement::from_sql_and_values(
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
                conn.execute_raw(Statement::from_sql_and_values(
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
                    status,
                )
                .await?;
                outcome.holds_raised += 1;
                return Ok(outcome);
            }
        }
    }

    outcome.write_keys.extend(changed_all);

    // Curated rows never change servedness without a person: the withdrawal is not stamped and
    // the disagreement lands in the review queue. A corrected value on a curated row IS applied
    // (upstream owns the value; the flag still excludes it from serving), with the same hold so
    // the operator re-rules on the new number.
    for (key, judgements) in &withdraw_touched {
        upsert_source_modified_hold(
            conn,
            stream_id,
            key.0,
            serde_json::json!({ "claim": "withdrawn", "replicate_index": key.1,
                                "judgements": judgements,
                                "window": { "from": window.from, "to": window.to } }),
            serde_json::json!({ "kept_served": true }),
            status,
        )
        .await?;
        outcome.holds_raised += 1;
    }
    for (key, judgements) in &changed_touched {
        upsert_source_modified_hold(
            conn,
            stream_id,
            key.0,
            serde_json::json!({ "claim": "value_changed", "replicate_index": key.1,
                                "judgements": judgements }),
            serde_json::json!({ "applied": true, "still_excluded_if_flagged": true }),
            status,
        )
        .await?;
        outcome.holds_raised += 1;
    }

    // A withdrawal and a reinstatement are decisions of sync origin (ADR 0008): the source
    // retracted or re-asserted the row, and the record says so beside any operator's judgement.
    if !to_withdraw.is_empty() {
        let rows: Vec<(DateTime<Utc>, i16, serde_json::Value)> = to_withdraw
            .iter()
            .map(|(t, i)| {
                (
                    *t,
                    *i,
                    serde_json::json!({ "reason": "absent from source window" }),
                )
            })
            .collect();
        super::decisions::record_keyed(
            conn,
            super::decisions::Kind::Withdraw,
            stream_id,
            &rows,
            actor,
            Some("absent from source window"),
            super::decisions::Origin::Sync,
            super::decisions::Keyed::All,
            None,
            None,
        )
        .await?;
        outcome.withdrawn = to_withdraw.len();
        outcome.withdrawn_keys = to_withdraw.iter().copied().collect();
    }

    if !reinstate.is_empty() {
        let rows: Vec<(DateTime<Utc>, i16, serde_json::Value)> = reinstate
            .iter()
            .map(|(t, i)| (*t, *i, serde_json::json!({})))
            .collect();
        super::decisions::record_keyed(
            conn,
            super::decisions::Kind::Reassert,
            stream_id,
            &rows,
            actor,
            Some("re-asserted by the source window"),
            super::decisions::Origin::Sync,
            super::decisions::Keyed::All,
            None,
            None,
        )
        .await?;
        outcome.reinstated = reinstate.len();
        outcome.reinstated_keys = reinstate.iter().copied().collect();
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
    conn.execute_raw(Statement::from_sql_and_values(
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

#[cfg(test)]
mod tests {
    use super::*;

    // 0.15 of the stored rows, and 0.5 of a replicate index's groups. A pass that reshapes fewer
    // than RECONCILE_BRAKE_MIN_ROWS rows is never braked, whatever the fractions say.

    #[test]
    fn test_brake_verdict_below_the_floor_is_never_braked() {
        // 4 of 5 rows is 80%, and every group loses its index, but the floor is not reached.
        assert_eq!(brake_verdict(5, 4, &[(4, 5)]), None);
    }

    #[test]
    fn test_brake_verdict_at_the_floor_the_fractions_apply() {
        assert_eq!(brake_verdict(5, 5, &[]), Some(Brake::Fraction));
    }

    #[test]
    fn test_brake_verdict_at_the_row_fraction_passes() {
        // 15 of 100 is exactly the threshold, which is not over it.
        assert_eq!(brake_verdict(100, 15, &[]), None);
    }

    #[test]
    fn test_brake_verdict_one_row_over_the_fraction_brakes() {
        assert_eq!(brake_verdict(100, 16, &[]), Some(Brake::Fraction));
    }

    #[test]
    fn test_brake_verdict_at_the_index_fraction_passes() {
        // Half of the groups carrying index 1 lose it, which is not more than half.
        assert_eq!(brake_verdict(100, 5, &[(5, 10)]), None);
    }

    #[test]
    fn test_brake_verdict_one_group_over_the_index_fraction_brakes() {
        assert_eq!(brake_verdict(100, 6, &[(6, 10)]), Some(Brake::IndexLoss));
    }

    #[test]
    fn test_brake_verdict_index_loss_is_seen_where_the_row_fraction_is_not() {
        // 6 of 200 rows is 3%, well under the row fraction, but they are every group's index 1.
        assert_eq!(brake_verdict(200, 6, &[(6, 6)]), Some(Brake::IndexLoss));
    }

    #[test]
    fn test_brake_verdict_index_loss_is_per_index_not_pooled() {
        // Neither index loses more than half, though together they are half the withdrawals.
        assert_eq!(brake_verdict(200, 10, &[(5, 10), (5, 10)]), None);
    }

    #[test]
    fn test_brake_verdict_an_empty_window_has_no_row_fraction() {
        // Nothing stored is nothing to reshape; only new rows can be in such a pass.
        assert_eq!(brake_verdict(0, 9, &[]), None);
    }
}
