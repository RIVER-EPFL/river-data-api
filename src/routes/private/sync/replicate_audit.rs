//! Sync-time audit of replicate groups against the source portal's own precomputed statistics.
//!
//! A sync service sending a replicate family attaches, per instant, the mean and standard
//! deviation the portal stored for that group. `/ingest` recomputes both over the values it is
//! about to store (curve-corrected where the readings carry curves, sample stdev to match the
//! portal's `sd()`) and always admits the readings: served statistics are trigger-computed from
//! the stored replicates, so a disagreement questions the portal's aggregate cells, not the data.
//! A disagreeing group records a `replicate_audit_holds` row: `pending` on a paired stream (the
//! review queue), `deferred` on an unpaired one (promoted to pending by pairing). Resolutions
//! never write a statistic: the operator either accepts the recomputed numbers (`acknowledged`)
//! or flags specific replicates so the sample statistics recompute over the rest (`remediated`).
//! Both decisions are recorded in `resolution` and stand against re-detection; `reopen` reverts
//! a remediation's flags and returns the hold to review.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::{AuthContext, ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};

/// Best-effort actor identity for the `acknowledged_by` audit field, as on alarm acknowledge.
fn actor_label(auth: &AuthContext) -> String {
    match auth {
        AuthContext::Keycloak { email: Some(e), .. } => e.clone(),
        AuthContext::Keycloak { .. } => "keycloak".to_string(),
        AuthContext::ApiToken { token_id, .. } => format!("token:{token_id}"),
    }
}

/// Confine a hold action to the caller's projects, resolved through the stream's paired site,
/// as the readings flag handlers do. An unpaired stream's hold (deferred) belongs to no project,
/// so it is actionable only by a caller without project restriction.
async fn enforce_hold_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    hold_id: Uuid,
) -> AppResult<()> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COALESCE(sp.site_id, h.site_id) AS site_id FROM replicate_audit_holds h
             LEFT JOIN data_streams ds ON ds.id = h.stream_id
             LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
             WHERE h.id = $1",
            [hold_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no replicate audit hold {hold_id}")))?;
    match row.try_get::<Option<Uuid>>("", "site_id")? {
        Some(site_id) => enforce_project_scope_for_sites(db, scope, &[site_id]).await,
        None => {
            if scope.is_restricted() {
                return Err(AppError::Forbidden(
                    "This hold's stream is unpaired, so it belongs to no project; only a caller \
                     without project restriction can act on it"
                        .to_string(),
                ));
            }
            Ok(())
        }
    }
}

/// Relative tolerance for the mean comparison. The portals store aggregates in MySQL FLOAT
/// columns, so bit-exact equality is not on the table.
pub const DEFAULT_REL_TOL: f64 = 1e-5;
/// Absolute floor, for values near zero where a relative bound collapses.
pub const DEFAULT_ABS_TOL: f64 = 1e-4;
/// The standard deviation gets a looser bound than the mean: against real portal data the stored
/// sd routinely disagrees with a recompute from its own replicate cells at the 1e-5 relative
/// level (FLOAT storage, historical R rounding chains), and a hold per micro-mismatch would bury
/// the real findings. A genuinely wrong sd (population-vs-sample, stale after an edit) sits at
/// percent level and still trips this.
pub const SD_REL_TOL: f64 = 1e-3;
pub const SD_ABS_TOL: f64 = 1e-3;
/// The portals round aggregate cells to 2 decimals before storing, so a disagreement below half
/// the stored quantum is not auditable: the portal's own cell cannot represent it.
pub const PORTAL_QUANTUM: f64 = 0.005;
/// The floor under every tolerance bound, [`PORTAL_QUANTUM`] plus an epsilon so a delta of
/// exactly half the quantum stays inside. Shared by [`stats_agree_with`] and [`bound_sql`] so the
/// in-process comparison and the SQL one cannot drift apart.
pub const QUANTUM_FLOOR: f64 = PORTAL_QUANTUM + 1e-9;

/// The tolerance bound between two statistics: relative to the larger magnitude, with an absolute
/// floor, never below [`QUANTUM_FLOOR`].
#[must_use]
pub fn tolerance_bound(e: f64, c: f64, rel_tol: f64, abs_tol: f64) -> f64 {
    f64::max(rel_tol * f64::max(e.abs(), c.abs()), abs_tol).max(QUANTUM_FLOOR)
}

/// SQL for the same bound between two value expressions, with the relative tolerance bound as
/// `rel_bind`. The one producer of the tolerance in SQL form; the reconciliation verifier uses it.
#[must_use]
pub fn bound_sql(a: &str, b: &str, rel_bind: &str, abs_tol: f64) -> String {
    format!("GREATEST({rel_bind} * GREATEST(abs({a}), abs({b})), {abs_tol}, {QUANTUM_FLOOR})")
}

/// One replicate group's expectation, as the portal stored it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupAudit {
    pub time: DateTime<Utc>,
    /// The portal's precomputed mean for this instant. NULL portal cell = None: nothing to check.
    #[serde(default)]
    pub expected_mean: Option<f64>,
    /// The portal's precomputed standard deviation (sample, n-1).
    #[serde(default)]
    pub expected_sd: Option<f64>,
    /// The count of non-null replicate cells in the portal row. A count mismatch is a hold reason
    /// even when mean and sd agree: a dropped replicate can leave both inside tolerance.
    #[serde(default)]
    pub expected_n: Option<i64>,
}

/// The recomputed statistics of a group of would-be-stored values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupStats {
    pub n: usize,
    pub mean: Option<f64>,
    /// Sample standard deviation (n-1), matching the portals' R `sd()`. None below n=2.
    pub sd: Option<f64>,
}

#[must_use]
pub fn group_stats(values: &[f64]) -> GroupStats {
    let n = values.len();
    if n == 0 {
        return GroupStats {
            n,
            mean: None,
            sd: None,
        };
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = values.iter().sum::<f64>() / n as f64;
    let sd = if n >= 2 {
        #[allow(clippy::cast_precision_loss)]
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        Some(var.sqrt())
    } else {
        None
    };
    GroupStats {
        n,
        mean: Some(mean),
        sd,
    }
}

/// Whether two statistics agree within tolerance. A missing side is not a mismatch: a portal
/// stores no sd for two of three nutrient families, and n=1 groups have no sd to compare.
#[must_use]
pub fn stats_agree(expected: Option<f64>, computed: Option<f64>, rel_tol: f64) -> bool {
    stats_agree_with(expected, computed, rel_tol, DEFAULT_ABS_TOL)
}

#[must_use]
pub fn stats_agree_with(
    expected: Option<f64>,
    computed: Option<f64>,
    rel_tol: f64,
    abs_tol: f64,
) -> bool {
    match (expected, computed) {
        (Some(e), Some(c)) => (e - c).abs() <= tolerance_bound(e, c, rel_tol, abs_tol),
        _ => true,
    }
}

/// One stored value with the replicate index it is stored at. The index is the source's column
/// position and nothing renumbers it, so it is the only handle a resolution can flag by.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ReplicateValue {
    pub index: i16,
    pub value: f64,
}

/// The values a hold was recorded over, in the order the source sent them. Holds written before
/// the index travelled with the value hold bare numbers; their index is unrecoverable, because no
/// position in the array stands for one, so it reads as `None` rather than as the position.
#[must_use]
pub fn stored_values(computed: &serde_json::Value) -> Vec<(Option<i16>, f64)> {
    computed
        .get("values")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::Object(_) => Some((
                        v.get("index")
                            .and_then(serde_json::Value::as_i64)
                            .and_then(|i| i16::try_from(i).ok()),
                        f64_at(v, "value")?,
                    )),
                    _ => Some((None, v.as_f64()?)),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The audit verdict for one group.
#[derive(Debug, Clone, Serialize)]
pub struct GroupMismatch {
    pub time: DateTime<Utc>,
    pub expected_mean: Option<f64>,
    pub expected_sd: Option<f64>,
    pub expected_n: Option<i64>,
    pub computed_mean: Option<f64>,
    pub computed_sd: Option<f64>,
    pub n: usize,
    /// The stored values the statistics were computed over, each at its replicate index.
    pub values: Vec<ReplicateValue>,
}

/// Open holds: the ones still awaiting review, unique per (stream, group_time). Must match the
/// partial index predicate in m20260821_000002 exactly, since the upsert names it as its
/// conflict target. Everything else is a decision or an outcome and is never rewritten by the
/// gate.
const OPEN: &str = "('pending', 'deferred')";

/// Everything past review. `use_portal`, `use_manual` and `consumed` are legacy statuses kept
/// for history; nothing produces them.
const RESOLVED: &str =
    "('acknowledged', 'remediated', 'superseded', 'use_portal', 'use_manual', 'consumed')";

/// The most recent hold for a group, as the ingest gate reads it. Terminal decisions matter to
/// the gate as much as open holds: a re-detected disagreement must not reopen a group an
/// operator already ruled on.
pub struct LatestHold {
    pub time: DateTime<Utc>,
    pub status: String,
    pub id: Uuid,
    /// The portal expectation the hold was recorded against, for [`expected_changed`].
    pub expected: serde_json::Value,
}

/// Whether an incoming audit claim differs from the expectation a hold recorded, under the same
/// tolerances detection uses. A terminal decision stands against re-detection of the SAME
/// disagreement; a cycle whose expected statistics have moved is new evidence and opens a fresh
/// hold.
#[must_use]
pub fn expected_changed(recorded: &serde_json::Value, audit: &GroupAudit) -> bool {
    fn side_changed(a: Option<f64>, b: Option<f64>, rel_tol: f64, abs_tol: f64) -> bool {
        match (a, b) {
            (Some(_), Some(_)) => !stats_agree_with(a, b, rel_tol, abs_tol),
            (None, None) => false,
            _ => true,
        }
    }
    side_changed(
        f64_at(recorded, "mean"),
        audit.expected_mean,
        DEFAULT_REL_TOL,
        DEFAULT_ABS_TOL,
    ) || side_changed(
        f64_at(recorded, "sd"),
        audit.expected_sd,
        SD_REL_TOL,
        SD_ABS_TOL,
    ) || recorded.get("n").and_then(serde_json::Value::as_i64) != audit.expected_n
}

/// The most recent hold per group for a stream at the given instants, any status.
pub async fn latest_holds<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    times: &[DateTime<Utc>],
) -> AppResult<Vec<LatestHold>> {
    let (Some(lo), Some(hi)) = (times.iter().min(), times.iter().max()) else {
        return Ok(Vec::new());
    };
    // Range bind + exact-match filter here: a timestamptz array bind panics in the driver, and
    // one batch's audit instants are contiguous anyway.
    let wanted: std::collections::HashSet<DateTime<Utc>> = times.iter().copied().collect();
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (group_time) id, group_time, status, expected
             FROM replicate_audit_holds
             WHERE stream_id = $1 AND group_time >= $2 AND group_time <= $3
             ORDER BY group_time, created_at DESC, id DESC",
            [
                stream_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(*lo).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(*hi).into(),
            ],
        ))
        .await?;
    rows.iter()
        .filter_map(|r| {
            let entry = (|| -> Result<_, sea_orm::DbErr> {
                Ok(LatestHold {
                    time: r
                        .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "group_time")?
                        .with_timezone(&Utc),
                    status: r.try_get::<String>("", "status")?,
                    id: r.try_get::<Uuid>("", "id")?,
                    expected: r.try_get::<serde_json::Value>("", "expected")?,
                })
            })();
            match entry {
                Ok(e) if wanted.contains(&e.time) => Some(Ok(e)),
                Ok(_) => None,
                Err(e) => Some(Err(e.into())),
            }
        })
        .collect()
}

/// Record (or refresh) a hold for a mismatching group: `pending` on a paired stream (the review
/// queue), `deferred` on an unpaired one (waiting for pairing). The open unique index makes the
/// re-detection on every sync cycle an update of the same row, never a duplicate; a deferred row
/// found by a paired-stream detection is promoted to pending.
pub async fn upsert_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    mismatch: &GroupMismatch,
    status: &str,
) -> AppResult<()> {
    let mut expected = serde_json::json!({
        "mean": mismatch.expected_mean,
        "sd": mismatch.expected_sd,
    });
    let computed = serde_json::json!({
        "mean": mismatch.computed_mean,
        "sd": mismatch.computed_sd,
        "n": mismatch.n,
        "values": mismatch.values,
    });
    let mut delta = serde_json::json!({
        "mean": delta_of(mismatch.expected_mean, mismatch.computed_mean),
        "sd": delta_of(mismatch.expected_sd, mismatch.computed_sd),
    });
    if let Some(expected_n) = mismatch.expected_n {
        expected["n"] = expected_n.into();
        delta["n"] =
            i64::try_from(mismatch.n).map_or(serde_json::Value::Null, |n| (expected_n - n).into());
    }
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds (stream_id, group_time, expected, computed, delta, status)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (stream_id, group_time) WHERE status IN {OPEN}
             DO UPDATE SET expected = EXCLUDED.expected, computed = EXCLUDED.computed,
                           delta = EXCLUDED.delta,
                           status = CASE WHEN replicate_audit_holds.status = 'deferred'
                                              AND EXCLUDED.status = 'pending'
                                         THEN 'pending' ELSE replicate_audit_holds.status END
             WHERE replicate_audit_holds.status IN {OPEN}"
        ),
        [
            stream_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(mismatch.time).into(),
            expected.into(),
            computed.into(),
            delta.into(),
            status.to_string().into(),
        ],
    ))
    .await?;
    Ok(())
}

fn delta_of(expected: Option<f64>, computed: Option<f64>) -> Option<f64> {
    Some(expected? - computed?)
}

/// Close a hold whose group now matches at source.
pub async fn close_hold<C: ConnectionTrait>(
    conn: &C,
    hold_id: Uuid,
    terminal_status: &str,
) -> AppResult<()> {
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE replicate_audit_holds SET status = $2 WHERE id = $1",
        [hold_id.into(), terminal_status.to_string().into()],
    ))
    .await?;
    Ok(())
}

fn f64_at(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(serde_json::Value::as_f64)
}

/// Signature classification of a disagreement, first match wins. The signatures cover the
/// failure classes observed in the real portal data, so the review queue reads as a triage
/// list rather than columns of deltas.
#[must_use]
pub fn classify(expected: &serde_json::Value, computed: &serde_json::Value) -> &'static str {
    let expected_n = expected.get("n").and_then(serde_json::Value::as_i64);
    let computed_n = computed.get("n").and_then(serde_json::Value::as_i64);
    if let Some(en) = expected_n
        && computed_n != Some(en)
    {
        return "n_mismatch";
    }
    let expected_mean = f64_at(expected, "mean");
    let expected_sd = f64_at(expected, "sd");
    let computed_mean = f64_at(computed, "mean");
    let computed_sd = f64_at(computed, "sd");
    // A population-divisor sd relates to the sample one by sqrt((n-1)/n). The signature claims
    // the sd is the ONLY disagreement, so it requires the means to agree: a wrong mean with a
    // coincidentally population-shaped sd is not explained by the divisor.
    if let (Some(esd), Some(csd), Some(n)) = (expected_sd, computed_sd, computed_n)
        && n >= 2
        && stats_agree(expected_mean, computed_mean, DEFAULT_REL_TOL)
    {
        #[allow(clippy::cast_precision_loss)]
        let population = csd * (((n - 1) as f64) / n as f64).sqrt();
        if stats_agree_with(Some(esd), Some(population), SD_REL_TOL, SD_ABS_TOL) {
            return "population_sd";
        }
    }
    // A stale cell frozen over the first k replicates before later ones were entered.
    let values: Vec<f64> = stored_values(computed)
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    if let Some(em) = expected_mean
        && values.len() >= 2
    {
        for k in 1..values.len() {
            #[allow(clippy::cast_precision_loss)]
            let prefix_mean = values[..k].iter().sum::<f64>() / k as f64;
            if (prefix_mean - em).abs() <= PORTAL_QUANTUM + 1e-9 {
                return "stale_subset";
            }
        }
    }
    "unexplained"
}

/// The next resolution object, stamped with the acting identity and time, with the previous one
/// appended under `history` so the decision trail survives reopen and re-resolve cycles.
fn merged_resolution(
    prev: Option<serde_json::Value>,
    mut next: serde_json::Value,
    by: &str,
) -> serde_json::Value {
    next["by"] = by.into();
    next["at"] = Utc::now().to_rfc3339().into();
    if let Some(prev) = prev {
        let mut history = prev
            .get("history")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut stripped = prev;
        if let Some(obj) = stripped.as_object_mut() {
            obj.remove("history");
        }
        history.push(stripped);
        next["history"] = serde_json::Value::Array(history);
    }
    next
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ListHoldsQuery {
    #[serde(default)]
    pub stream_id: Option<Uuid>,
    /// Comma-separated stream UUIDs.
    #[serde(default)]
    pub stream_ids: Option<String>,
    #[serde(default)]
    pub source_system: Option<String>,
    /// One status, or the view `resolved` (every decided or cleared hold); defaults to
    /// `pending`, the review queue.
    #[serde(default)]
    pub status: Option<String>,
    /// Only holds whose relative_delta is at or below this, mirroring the bulk-acknowledge
    /// ceiling so the UI can preview exactly what a threshold would acknowledge.
    #[serde(default)]
    pub max_relative_delta: Option<f64>,
    /// Only holds whose mean_relative_delta is at or below this. ANDs with the other ceilings.
    #[serde(default)]
    pub max_mean_relative_delta: Option<f64>,
    /// Only holds whose sd_relative_delta is at or below this. ANDs with the other ceilings.
    #[serde(default)]
    pub max_sd_relative_delta: Option<f64>,
    /// `relative_delta_desc` | `relative_delta_asc` | `created_at_desc` (default). Sorting by
    /// scale is what lets an operator triage the whole backlog largest-first across pages.
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub page: Option<u64>,
    #[serde(default)]
    pub page_size: Option<u64>,
}

/// The one denominator every relative delta is normalised by: the MEAN magnitude, not each
/// statistic's own, because that is the scale an operator judges significance on (an sd off by 2
/// on a value of 150 is noise; a mean off by 2 is not).
macro_rules! scale_sql {
    () => {
        "GREATEST(
        abs(COALESCE((h.expected->>'mean')::float8, 0)),
        abs(COALESCE((h.computed->>'mean')::float8, 0)),
        1e-9
    )"
    };
}

const MEAN_RELATIVE_DELTA_SQL: &str = concat!(
    "COALESCE(abs((h.delta->>'mean')::float8), 0) / ",
    scale_sql!()
);
const SD_RELATIVE_DELTA_SQL: &str = concat!(
    "COALESCE(abs((h.delta->>'sd')::float8), 0) / ",
    scale_sql!()
);

/// One scalar per hold saying how large the disagreement is against the measurement's own scale:
/// `max(|Δmean|, |Δsd|) / max(|portal mean|, |computed mean|)`, i.e. the greater of
/// [`MEAN_RELATIVE_DELTA_SQL`] and [`SD_RELATIVE_DELTA_SQL`]. The same expression drives the
/// list's per-row value, the sort, and the threshold bulk acknowledge, so what the UI shows and
/// what the slider acknowledges can never disagree.
const RELATIVE_DELTA_SQL: &str = concat!(
    "GREATEST(
        COALESCE(abs((h.delta->>'mean')::float8), 0),
        COALESCE(abs((h.delta->>'sd')::float8), 0)
    ) / ",
    scale_sql!()
);

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct HoldRow {
    pub id: Uuid,
    /// NULL on event-audit findings, which are keyed on (site, parameter, instant) instead.
    pub stream_id: Option<Uuid>,
    /// `replicate_stats` | `source_modified` | `brake_fired` | `missing_output` | `stale_output`.
    pub kind: String,
    pub source_system: Option<String>,
    pub source_key: Option<String>,
    /// The stream's human name as registered by the source.
    pub source_name: Option<String>,
    /// Site and parameter of the paired slot (or of the event finding itself); NULL while the
    /// stream is unpaired.
    pub site_name: Option<String>,
    pub parameter_name: Option<String>,
    pub parameter_code: Option<String>,
    /// The tool an event finding names.
    pub tool: Option<String>,
    pub paired: bool,
    pub group_time: DateTime<Utc>,
    #[schema(value_type = Object)]
    pub expected: serde_json::Value,
    #[schema(value_type = Object)]
    pub computed: serde_json::Value,
    #[schema(value_type = Object)]
    pub delta: serde_json::Value,
    pub status: String,
    /// Signature of the disagreement: `n_mismatch` | `population_sd` | `stale_subset` |
    /// `unexplained`. Computed from the stored expectation and recompute, never persisted.
    pub classification: String,
    /// The decision record: latest action plus prior actions under `history`.
    #[schema(value_type = Object)]
    pub resolution: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub acknowledged_by: Option<String>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    /// Disagreement size relative to the measurement scale: the greater of
    /// `mean_relative_delta` and `sd_relative_delta`; see [`RELATIVE_DELTA_SQL`].
    pub relative_delta: f64,
    /// `|Δmean| / max(|portal mean|, |computed mean|)`.
    pub mean_relative_delta: f64,
    /// `|Δsd|` over the same mean-magnitude denominator as `mean_relative_delta`.
    pub sd_relative_delta: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListHoldsResponse {
    pub holds: Vec<HoldRow>,
    pub total: u64,
    /// Pending holds within the non-status filters, regardless of the status view requested.
    pub pending: u64,
    /// Deferred holds (unpaired streams) within the same filters.
    pub deferred: u64,
}

/// List replicate audit holds, newest first. The UI's Audits view reads this.
#[utoipa::path(
    get,
    path = "/sync/replicate_audit_holds",
    params(
        ("stream_id" = Option<Uuid>, Query, description = "Filter to one stream"),
        ("stream_ids" = Option<String>, Query, description = "Comma-separated stream UUIDs"),
        ("status" = Option<String>, Query, description = "pending | deferred | acknowledged | remediated | superseded | resolved; omit for pending"),
        ("source_system" = Option<String>, Query, description = "Filter to one source system"),
        ("max_relative_delta" = Option<f64>, Query, description = "Only holds at or below this relative_delta"),
        ("max_mean_relative_delta" = Option<f64>, Query, description = "Only holds at or below this mean_relative_delta"),
        ("max_sd_relative_delta" = Option<f64>, Query, description = "Only holds at or below this sd_relative_delta"),
        ("sort" = Option<String>, Query, description = "relative_delta_desc | relative_delta_asc | created_at_desc"),
        ("page" = Option<u64>, Query, description = "1-based page"),
        ("page_size" = Option<u64>, Query, description = "Default 50, max 500"),
    ),
    responses((status = 200, body = ListHoldsResponse)),
    tag = "sync"
)]
pub async fn list_holds(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(query): Query<ListHoldsQuery>,
) -> AppResult<Json<ListHoldsResponse>> {
    let mut conditions = vec!["TRUE".to_string()];
    let mut binds: Vec<sea_orm::Value> = Vec::new();
    // A restricted caller sees only holds whose stream is paired into their projects; unpaired
    // (deferred) holds belong to no project and are visible only without project restriction.
    if let Some(projects) = scope.sql_project_array() {
        binds.push(projects);
        conditions.push(format!(
            "(EXISTS (SELECT 1 FROM site_parameters sp JOIN sites st ON st.id = sp.site_id \
              WHERE sp.id = ds.site_parameter_id AND st.project_id = ANY(${n})) \
              OR EXISTS (SELECT 1 FROM sites st WHERE st.id = h.site_id \
              AND st.project_id = ANY(${n})))",
            n = binds.len()
        ));
    }
    if let Some(stream_id) = query.stream_id {
        binds.push(stream_id.into());
        conditions.push(format!("h.stream_id = ${}", binds.len()));
    }
    if let Some(stream_ids) = query.stream_ids.as_deref().filter(|s| !s.is_empty()) {
        let ids: Vec<Uuid> = stream_ids
            .split(',')
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| AppError::BadRequest(format!("invalid stream id '{s}'")))
            })
            .collect::<Result<_, _>>()?;
        binds.push(ids.into());
        conditions.push(format!("h.stream_id = ANY(${})", binds.len()));
    }
    if let Some(source_system) = query.source_system.clone() {
        binds.push(source_system.into());
        conditions.push(format!("ds.source_system = ${}", binds.len()));
    }
    if let Some(ceiling) = query.max_relative_delta {
        binds.push(ceiling.into());
        conditions.push(format!("{RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    if let Some(ceiling) = query.max_mean_relative_delta {
        binds.push(ceiling.into());
        conditions.push(format!("{MEAN_RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    if let Some(ceiling) = query.max_sd_relative_delta {
        binds.push(ceiling.into());
        conditions.push(format!("{SD_RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    // The status view is kept out of the count statement's WHERE so `pending`/`deferred` report
    // the whole backlog under the other filters, whichever view the page shows. Status values
    // come from the allowlist below, so inlining them is safe.
    let base_clause = conditions.join(" AND ");
    let status_sql = match query.status.as_deref() {
        Some(
            s @ ("pending" | "deferred" | "acknowledged" | "remediated" | "superseded"
            | "use_portal" | "use_manual" | "consumed"),
        ) => format!("h.status = '{s}'"),
        Some("resolved") => format!("h.status IN {RESOLVED}"),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "unknown hold status '{other}'"
            )));
        }
        None => "h.status = 'pending'".to_string(),
    };
    let where_clause = format!("{base_clause} AND {status_sql}");
    let order_by = match query.sort.as_deref() {
        None | Some("created_at_desc") => "h.created_at DESC".to_string(),
        Some("relative_delta_desc") => format!("{RELATIVE_DELTA_SQL} DESC, h.created_at DESC"),
        Some("relative_delta_asc") => format!("{RELATIVE_DELTA_SQL} ASC, h.created_at DESC"),
        Some(other) => {
            return Err(AppError::BadRequest(format!("unknown sort '{other}'")));
        }
    };

    let page_size = query.page_size.unwrap_or(50).clamp(1, 500);
    let offset = query.page.unwrap_or(1).max(1).saturating_sub(1) * page_size;

    let count_row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) FILTER (WHERE {status_sql})::bigint AS total,
                        COUNT(*) FILTER (WHERE h.status = 'pending')::bigint AS pending,
                        COUNT(*) FILTER (WHERE h.status = 'deferred')::bigint AS deferred
                 FROM replicate_audit_holds h
                 LEFT JOIN data_streams ds ON ds.id = h.stream_id
                 WHERE {base_clause}"
            ),
            binds.clone(),
        ))
        .await?
        .ok_or_else(|| AppError::Internal("hold count returned no row".to_string()))?;
    let total: i64 = count_row.try_get("", "total")?;
    let pending: i64 = count_row.try_get("", "pending")?;
    let deferred: i64 = count_row.try_get("", "deferred")?;

    let mut rows = HoldRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT h.id, h.stream_id, h.kind, ds.source_system, ds.source_key, ds.source_name,
                    COALESCE(s.name, es.name) AS site_name,
                    COALESCE(p.name, ep.name) AS parameter_name,
                    COALESCE(p.code, ep.code) AS parameter_code,
                    h.tool,
                    COALESCE(ds.site_parameter_id IS NOT NULL, FALSE) AS paired,
                    h.group_time,
                    h.expected, h.computed, h.delta, h.status,
                    ''::text AS classification, h.resolution,
                    h.created_at, h.acknowledged_by, h.acknowledged_at,
                    {RELATIVE_DELTA_SQL} AS relative_delta,
                    {MEAN_RELATIVE_DELTA_SQL} AS mean_relative_delta,
                    {SD_RELATIVE_DELTA_SQL} AS sd_relative_delta
             FROM replicate_audit_holds h
             LEFT JOIN data_streams ds ON ds.id = h.stream_id
             LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
             LEFT JOIN sites s ON s.id = sp.site_id
             LEFT JOIN parameters p ON p.id = sp.parameter_id
             LEFT JOIN sites es ON es.id = h.site_id
             LEFT JOIN parameters ep ON ep.id = h.parameter_id
             WHERE {where_clause}
             ORDER BY {order_by}
             LIMIT {page_size} OFFSET {offset}"
        ),
        binds,
    ))
    .all(&state.db)
    .await?;
    for row in &mut rows {
        // The disagreement signature is a replicate-statistics concept; other kinds carry their
        // meaning in `kind` itself.
        if row.kind == "replicate_stats" {
            row.classification = classify(&row.expected, &row.computed).to_string();
        }
    }

    Ok(Json(ListHoldsResponse {
        holds: rows,
        total: u64::try_from(total).unwrap_or(0),
        pending: u64::try_from(pending).unwrap_or(0),
        deferred: u64::try_from(deferred).unwrap_or(0),
    }))
}

/// SQL fragment producing the accept-ours resolution object (actor and time stamped on the
/// entry) while preserving any prior actions under `history` (a reopened hold can be
/// re-resolved). Shared by single and bulk acknowledge; `by_bind` is the placeholder carrying
/// the actor label.
fn accept_ours_resolution_sql(by_bind: &str) -> String {
    format!(
        "CASE
    WHEN h.resolution IS NULL
        THEN jsonb_build_object('action', 'accept_ours', 'by', {by_bind}::text, 'at', NOW())
    ELSE jsonb_build_object('action', 'accept_ours', 'by', {by_bind}::text, 'at', NOW(),
         'history',
         COALESCE(h.resolution->'history', '[]'::jsonb)
             || jsonb_build_array(h.resolution - 'history'))
    END"
    )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AcknowledgeResponse {
    pub acknowledged: u64,
}

/// Acknowledge one pending hold: the operator confirms the statistics recomputed from the stored
/// replicates. Terminal; re-detection of the same disagreement leaves the decision standing.
/// The acting identity is taken from the caller's authentication, never from the request.
#[utoipa::path(
    post,
    path = "/sync/replicate_audit_holds/{id}/acknowledge",
    responses(
        (status = 200, body = AcknowledgeResponse),
        (status = 404, description = "No pending hold with this id"),
    ),
    tag = "sync"
)]
pub async fn acknowledge_hold(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> AppResult<Json<AcknowledgeResponse>> {
    enforce_hold_scope(&state.db, &scope, id).await?;
    let by = actor_label(&auth);
    let resolution_sql = accept_ours_resolution_sql("$2");
    let updated = state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "UPDATE replicate_audit_holds AS h
                 SET status = 'acknowledged', resolution = {resolution_sql},
                     acknowledged_by = $2, acknowledged_at = NOW()
                 WHERE id = $1 AND status = 'pending'"
            ),
            [id.into(), by.into()],
        ))
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound(format!(
            "no pending replicate audit hold {id}"
        )));
    }
    Ok(Json(AcknowledgeResponse { acknowledged: 1 }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ResolveHoldRequest {
    /// `ours` (accept the recomputed statistics; identical to acknowledge) | `flag` (flag the
    /// named replicates so the sample statistics recompute over the rest).
    pub mode: String,
    /// The replicate indexes to flag; required for `flag`. Each must be among the values the
    /// hold recorded and unflagged, and at least one unflagged replicate must remain after.
    #[serde(default)]
    pub replicate_indexes: Option<Vec<i16>>,
    /// Recorded as the readings' flag_reason; defaults to a reference to this hold.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResolveHoldResponse {
    /// The status the hold moved to: `acknowledged` | `remediated`.
    pub status: String,
}

/// Resolve one pending hold. Statistics are never written directly: `ours` accepts the
/// recomputed numbers, `flag` marks the named replicates so the trigger recomputes the sample
/// over the rest. Both record the decision on the hold.
#[utoipa::path(
    post,
    path = "/sync/replicate_audit_holds/{id}/resolve",
    request_body = ResolveHoldRequest,
    responses(
        (status = 200, body = ResolveHoldResponse),
        (status = 400, description = "Unknown mode, no replicate indexes, an index the hold does \
                                      not name or the group does not hold, an already-flagged \
                                      index, a flag that would leave no unflagged replicate, or \
                                      a legacy hold recorded without indexes"),
        (status = 404, description = "No pending hold with this id"),
    ),
    tag = "sync"
)]
pub async fn resolve_hold(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<ResolveHoldRequest>,
) -> AppResult<Json<ResolveHoldResponse>> {
    enforce_hold_scope(&state.db, &scope, id).await?;
    let by = actor_label(&auth);
    match payload.mode.as_str() {
        "ours" => {
            let resolution_sql = accept_ours_resolution_sql("$2");
            let updated = state
                .db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "UPDATE replicate_audit_holds AS h
                         SET status = 'acknowledged', resolution = {resolution_sql},
                             acknowledged_by = $2, acknowledged_at = NOW()
                         WHERE id = $1 AND status = 'pending'"
                    ),
                    [id.into(), by.into()],
                ))
                .await?
                .rows_affected();
            if updated == 0 {
                return Err(AppError::NotFound(format!(
                    "no pending replicate audit hold {id}"
                )));
            }
            Ok(Json(ResolveHoldResponse {
                status: "acknowledged".to_string(),
            }))
        }
        "flag" => {
            let mut indexes = payload
                .replicate_indexes
                .clone()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    AppError::BadRequest("flag resolution requires replicate_indexes".to_string())
                })?;
            indexes.sort_unstable();
            indexes.dedup();

            let reason = payload
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map_or_else(|| format!("replicate audit hold {id}"), String::from);
            let index_list = indexes
                .iter()
                .map(i16::to_string)
                .collect::<Vec<_>>()
                .join(", ");

            crate::common::bulk_write::guarded(&state.db, async |txn| {
                // The hold is locked before anything is flagged: the flags and the decision record
                // that explains them must land together or not at all.
                let hold = txn
                    .query_one(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT stream_id, group_time, resolution, computed
                         FROM replicate_audit_holds
                         WHERE id = $1 AND status = 'pending' FOR UPDATE",
                        [id.into()],
                    ))
                    .await?
                    .ok_or_else(|| {
                        AppError::NotFound(format!("no pending replicate audit hold {id}"))
                    })?;
                let stream_id: Uuid = hold.try_get("", "stream_id")?;
                let group_time =
                    hold.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "group_time")?;
                let prev: Option<serde_json::Value> = hold.try_get("", "resolution")?;
                let computed: serde_json::Value = hold.try_get("", "computed")?;

                // A flag resolution may only touch the replicates the hold was recorded over: the
                // operator's decision is about those values, and any other index in the group is
                // evidence this hold never showed them.
                let recorded = stored_values(&computed);
                let recorded_indexes: Vec<i16> = recorded.iter().filter_map(|(i, _)| *i).collect();
                if recorded.is_empty() || recorded_indexes.len() != recorded.len() {
                    return Err(AppError::BadRequest(format!(
                        "replicate audit hold {id} predates index recording, so its values cannot \
                         be addressed by replicate index; flag the readings through the readings \
                         flag endpoints instead"
                    )));
                }
                let out_of_hold: Vec<String> = indexes
                    .iter()
                    .filter(|i| !recorded_indexes.contains(i))
                    .map(i16::to_string)
                    .collect();
                if !out_of_hold.is_empty() {
                    return Err(AppError::BadRequest(format!(
                        "replicate index {} is not named by this hold (it records indexes {}); \
                         nothing was flagged",
                        out_of_hold.join(", "),
                        recorded_indexes
                            .iter()
                            .map(i16::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }

                let group_rows = txn
                    .query_all(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT replicate_index, is_flagged IS TRUE AS flagged FROM readings
                         WHERE stream_id = $1 AND time = $2",
                        [stream_id.into(), group_time.into()],
                    ))
                    .await?;
                let mut existing: Vec<i16> = Vec::with_capacity(group_rows.len());
                let mut already_flagged: Vec<i16> = Vec::new();
                for row in &group_rows {
                    let index: i16 = row.try_get("", "replicate_index")?;
                    existing.push(index);
                    if row.try_get::<bool>("", "flagged")? {
                        already_flagged.push(index);
                    }
                }
                let absent: Vec<String> = indexes
                    .iter()
                    .filter(|i| !existing.contains(i))
                    .map(i16::to_string)
                    .collect();
                if !absent.is_empty() {
                    return Err(AppError::BadRequest(format!(
                        "no reading at replicate index {} in this group; nothing was flagged",
                        absent.join(", ")
                    )));
                }
                let re_flagged: Vec<String> = indexes
                    .iter()
                    .filter(|i| already_flagged.contains(i))
                    .map(i16::to_string)
                    .collect();
                if !re_flagged.is_empty() {
                    return Err(AppError::BadRequest(format!(
                        "replicate index {} is already flagged; nothing was flagged",
                        re_flagged.join(", ")
                    )));
                }
                // Flagging the whole group would leave the sample trigger with n = 0 and the
                // instant would vanish from serving; a group that bad is retracted at source or
                // through the readings endpoints, not resolved here.
                let survivors = existing
                    .iter()
                    .filter(|i| !already_flagged.contains(i) && !indexes.contains(i))
                    .count();
                if survivors == 0 {
                    return Err(AppError::BadRequest(
                        "at least one unflagged replicate must remain in the group; nothing was \
                         flagged"
                            .to_string(),
                    ));
                }

                let flagged: Vec<i16> = txn
                    .query_all(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        format!(
                            "UPDATE readings SET is_flagged = TRUE, flag_reason = $3
                             WHERE stream_id = $1 AND time = $2
                               AND replicate_index IN ({index_list})
                               AND is_flagged IS NOT TRUE
                             RETURNING replicate_index"
                        ),
                        [stream_id.into(), group_time.into(), reason.clone().into()],
                    ))
                    .await?
                    .iter()
                    .map(|row| row.try_get::<i16>("", "replicate_index"))
                    .collect::<Result<_, _>>()?;
                if flagged.len() != indexes.len() {
                    return Err(AppError::Conflict(format!(
                        "the replicate group changed under this request; nothing was flagged \
                         (hold {id})"
                    )));
                }

                let resolution = merged_resolution(
                    prev,
                    serde_json::json!({
                        "action": "flag_replicates",
                        "replicate_indexes": &indexes,
                        "reason": reason,
                    }),
                    &by,
                );
                let decided = txn
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE replicate_audit_holds
                         SET status = 'remediated', resolution = $2,
                             acknowledged_by = $3, acknowledged_at = NOW()
                         WHERE id = $1 AND status = 'pending'",
                        [id.into(), resolution.into(), by.clone().into()],
                    ))
                    .await?
                    .rows_affected();
                if decided != 1 {
                    return Err(AppError::Conflict(format!(
                        "replicate audit hold {id} was resolved by another request; nothing was \
                         flagged"
                    )));
                }
                Ok(())
            })
            .await?;
            // The sample trigger recomputed the served statistics for this instant.
            state.response_cache.invalidate_all();
            Ok(Json(ResolveHoldResponse {
                status: "remediated".to_string(),
            }))
        }
        other => Err(AppError::BadRequest(format!(
            "unknown resolve mode '{other}'"
        ))),
    }
}

/// Revert a decision: a remediation's flags are removed (only the readings that resolution
/// flagged, identified by the recorded reason) and the hold returns to review, `pending` or
/// `deferred` per the stream's current pairing.
#[utoipa::path(
    post,
    path = "/sync/replicate_audit_holds/{id}/reopen",
    responses(
        (status = 200, body = ResolveHoldResponse),
        (status = 404, description = "No decided hold with this id"),
    ),
    tag = "sync"
)]
pub async fn reopen_hold(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> AppResult<Json<ResolveHoldResponse>> {
    enforce_hold_scope(&state.db, &scope, id).await?;
    let by = actor_label(&auth);
    let reopened = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let hold = txn
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT h.stream_id, h.group_time, h.status, h.resolution,
                        (ds.site_parameter_id IS NOT NULL) AS paired
                 FROM replicate_audit_holds h
                 JOIN data_streams ds ON ds.id = h.stream_id
                 WHERE h.id = $1 AND h.status IN ('acknowledged', 'remediated')
                 FOR UPDATE OF h",
                [id.into()],
            ))
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no decided replicate audit hold {id}")))?;
        let stream_id: Uuid = hold.try_get("", "stream_id")?;
        let group_time =
            hold.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "group_time")?;
        let status: String = hold.try_get("", "status")?;
        let paired: bool = hold.try_get("", "paired")?;
        let prev: Option<serde_json::Value> = hold.try_get("", "resolution")?;

        let flagged = prev.as_ref().and_then(|r| {
            (r.get("action")? == "flag_replicates").then(|| {
                (
                    r.get("replicate_indexes")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(serde_json::Value::as_i64)
                                .map(|i| i.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    r.get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
        });

        let reopened = if paired { "pending" } else { "deferred" };
        let resolution = merged_resolution(prev, serde_json::json!({"action": "reopened"}), &by);
        if status == "remediated"
            && let Some((index_list, reason)) = &flagged
            && !index_list.is_empty()
        {
            // Only the rows this resolution flagged: a flag someone set since, or with another
            // reason, stays.
            txn.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "UPDATE readings SET is_flagged = FALSE, flag_reason = NULL
                     WHERE stream_id = $1 AND time = $2
                       AND replicate_index IN ({index_list})
                       AND is_flagged = TRUE AND flag_reason = $3"
                ),
                [stream_id.into(), group_time.into(), reason.clone().into()],
            ))
            .await?;
        }
        let restored = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE replicate_audit_holds
                 SET status = $2, resolution = $3, acknowledged_by = NULL, acknowledged_at = NULL
                 WHERE id = $1 AND status IN ('acknowledged', 'remediated')",
                [id.into(), reopened.to_string().into(), resolution.into()],
            ))
            .await?
            .rows_affected();
        if restored != 1 {
            return Err(AppError::Conflict(format!(
                "replicate audit hold {id} changed under this request; no flag was reverted"
            )));
        }
        Ok(reopened.to_string())
    })
    .await?;
    state.response_cache.invalidate_all();
    Ok(Json(ResolveHoldResponse { status: reopened }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BulkAcknowledgeRequest {
    /// One stream, or omit to scope by source_system (or, with both omitted, every pending hold).
    #[serde(default)]
    pub stream_id: Option<Uuid>,
    /// All of one source's streams, e.g. "cnet".
    #[serde(default)]
    pub source_system: Option<String>,
    /// Restrict to holds whose group_time falls in [start, end]; omit either to leave it open.
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// Only acknowledge holds whose `relative_delta` (as reported by the list endpoint) is at or
    /// below this. The knob behind "accept everything under N%": systematic small offsets are
    /// waved through in one action while the large disagreements stay pending for review.
    #[serde(default)]
    pub max_relative_delta: Option<f64>,
    /// Ceiling on `mean_relative_delta`. ANDs with the other ceilings.
    #[serde(default)]
    pub max_mean_relative_delta: Option<f64>,
    /// Ceiling on `sd_relative_delta`. ANDs with the other ceilings.
    #[serde(default)]
    pub max_sd_relative_delta: Option<f64>,
}

/// Acknowledge pending holds in bulk: one stream or a whole source, optionally bounded by a time
/// window and by a `relative_delta` ceiling, for systematic offsets that would otherwise take one
/// acknowledgement per instant.
#[utoipa::path(
    post,
    path = "/sync/replicate_audit_holds/acknowledge_bulk",
    request_body = BulkAcknowledgeRequest,
    responses((status = 200, body = AcknowledgeResponse)),
    tag = "sync"
)]
pub async fn acknowledge_holds_bulk(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<BulkAcknowledgeRequest>,
) -> AppResult<Json<AcknowledgeResponse>> {
    let by = actor_label(&auth);
    let mut binds: Vec<sea_orm::Value> = vec![by.into()];
    let mut bounds = String::new();
    // A restricted caller acknowledges only holds whose stream is paired to a site in their
    // projects; unpaired (deferred) holds belong to no project and stay out of their reach.
    if let Some(projects) = scope.sql_project_array() {
        binds.push(projects);
        bounds.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM site_parameters sp JOIN sites st ON st.id = sp.site_id \
             WHERE sp.id = ds.site_parameter_id AND st.project_id = ANY(${}))",
            binds.len()
        ));
    }
    if let Some(stream_id) = payload.stream_id {
        binds.push(stream_id.into());
        bounds.push_str(&format!(" AND h.stream_id = ${}", binds.len()));
    }
    if let Some(source_system) = payload.source_system {
        binds.push(source_system.into());
        bounds.push_str(&format!(" AND ds.source_system = ${}", binds.len()));
    }
    if let Some(start) = payload.start {
        binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(start).into());
        bounds.push_str(&format!(" AND h.group_time >= ${}", binds.len()));
    }
    if let Some(end) = payload.end {
        binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(end).into());
        bounds.push_str(&format!(" AND h.group_time <= ${}", binds.len()));
    }
    if let Some(ceiling) = payload.max_relative_delta {
        binds.push(ceiling.into());
        bounds.push_str(&format!(" AND {RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    if let Some(ceiling) = payload.max_mean_relative_delta {
        binds.push(ceiling.into());
        bounds.push_str(&format!(
            " AND {MEAN_RELATIVE_DELTA_SQL} <= ${}",
            binds.len()
        ));
    }
    if let Some(ceiling) = payload.max_sd_relative_delta {
        binds.push(ceiling.into());
        bounds.push_str(&format!(" AND {SD_RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    let resolution_sql = accept_ours_resolution_sql("$1");
    let acknowledged = state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "UPDATE replicate_audit_holds AS h
                 SET status = 'acknowledged', resolution = {resolution_sql},
                     acknowledged_by = $1, acknowledged_at = NOW()
                 FROM data_streams ds
                 WHERE ds.id = h.stream_id AND h.status = 'pending'{bounds}"
            ),
            binds,
        ))
        .await?
        .rows_affected();
    Ok(Json(AcknowledgeResponse { acknowledged }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_of_a_triplicate() {
        let s = group_stats(&[1.0, 2.0, 3.0]);
        assert_eq!(s.n, 3);
        assert!((s.mean.unwrap() - 2.0).abs() < 1e-12);
        assert!((s.sd.unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_singleton_has_no_sd() {
        let s = group_stats(&[5.0]);
        assert_eq!(s.n, 1);
        assert_eq!(s.mean, Some(5.0));
        assert_eq!(s.sd, None);
    }

    #[test]
    fn agreement_is_relative_with_an_absolute_floor() {
        assert!(stats_agree(Some(1000.0), Some(1000.005), DEFAULT_REL_TOL));
        assert!(!stats_agree(Some(1000.0), Some(1001.0), DEFAULT_REL_TOL));
        assert!(stats_agree(Some(0.0), Some(5e-5), DEFAULT_REL_TOL));
        assert!(!stats_agree(Some(0.0), Some(0.5), DEFAULT_REL_TOL));
    }

    #[test]
    fn sd_tolerates_portal_float_noise_but_not_percent_level_drift() {
        // Real cnet example: stored sd 12.1798 vs recomputed 12.17994 (FLOAT noise, passes).
        assert!(stats_agree_with(
            Some(12.179800033569336),
            Some(12.179940200962006),
            SD_REL_TOL,
            SD_ABS_TOL
        ));
        // Population-vs-sample sd (sqrt(2/3) off) is a real finding and still holds.
        assert!(!stats_agree_with(
            Some(68.22),
            Some(83.55),
            SD_REL_TOL,
            SD_ABS_TOL
        ));
    }

    #[test]
    fn quantization_of_the_portal_cell_is_not_a_mismatch() {
        // A 2dp-stored portal mean differs from the true mean by up to half the quantum.
        assert!(stats_agree(
            Some(147.33),
            Some(147.333_333_333_333_3),
            DEFAULT_REL_TOL
        ));
        // A real disagreement sits far above the quantum and still holds.
        assert!(!stats_agree_with(
            Some(11.09),
            Some(13.5769),
            SD_REL_TOL,
            SD_ABS_TOL
        ));
    }

    #[test]
    fn a_missing_side_is_not_a_mismatch() {
        assert!(stats_agree(None, Some(1.0), DEFAULT_REL_TOL));
        assert!(stats_agree(Some(1.0), None, DEFAULT_REL_TOL));
        assert!(stats_agree(None, None, DEFAULT_REL_TOL));
    }

    fn classify_case(expected: serde_json::Value, computed: serde_json::Value) -> &'static str {
        classify(&expected, &computed)
    }

    #[test]
    fn classify_n_mismatch_first() {
        assert_eq!(
            classify_case(
                serde_json::json!({"mean": 20.0, "sd": 10.0, "n": 3}),
                serde_json::json!({"mean": 20.0, "sd": 10.0, "n": 2, "values": [10.0, 30.0]}),
            ),
            "n_mismatch"
        );
    }

    #[test]
    fn classify_population_sd_signature() {
        // Real cnet GLT row: stored sd 4.99 is the population divisor of the recomputed 6.1101.
        assert_eq!(
            classify_case(
                serde_json::json!({"mean": 80.33, "sd": 4.99, "n": 3}),
                serde_json::json!({"mean": 80.3333, "sd": 6.1101, "n": 3,
                                   "values": [75.0, 79.0, 87.0]}),
            ),
            "population_sd"
        );
    }

    #[test]
    fn classify_stale_subset_signature() {
        // Real cnet VAD row: the stored 220.5 is the mean of the first two replicates only.
        assert_eq!(
            classify_case(
                serde_json::json!({"mean": 220.5, "sd": 19.5, "n": 3}),
                serde_json::json!({"mean": 291.6667, "sd": 125.9, "n": 3,
                                   "values": [201.0, 240.0, 434.0]}),
            ),
            "stale_subset"
        );
    }

    #[test]
    fn classify_stale_subset_reads_indexed_values() {
        assert_eq!(
            classify_case(
                serde_json::json!({"mean": 220.5, "sd": 19.5, "n": 3}),
                serde_json::json!({"mean": 291.6667, "sd": 125.9, "n": 3,
                                   "values": [{"index": 1, "value": 201.0},
                                              {"index": 3, "value": 240.0},
                                              {"index": 4, "value": 434.0}]}),
            ),
            "stale_subset"
        );
    }

    #[test]
    fn a_legacy_hold_has_values_without_indexes() {
        let legacy = stored_values(&serde_json::json!({"values": [1.0, 2.5]}));
        assert_eq!(legacy, vec![(None, 1.0), (None, 2.5)]);
        let indexed = stored_values(&serde_json::json!({
            "values": [{"index": 2, "value": 1.0}, {"index": 5, "value": 2.5}]
        }));
        assert_eq!(indexed, vec![(Some(2), 1.0), (Some(5), 2.5)]);
    }

    #[test]
    fn classify_unexplained() {
        assert_eq!(
            classify_case(
                serde_json::json!({"mean": 50.0, "n": 3}),
                serde_json::json!({"mean": 49.0, "n": 3, "values": [25.0, 49.0, 73.0]}),
            ),
            "unexplained"
        );
    }

    #[test]
    fn a_population_shaped_sd_with_a_disagreeing_mean_is_not_population_sd() {
        assert_eq!(
            classify_case(
                serde_json::json!({"mean": 60.0, "sd": 4.99, "n": 3}),
                serde_json::json!({"mean": 80.3333, "sd": 6.1101, "n": 3,
                                   "values": [75.0, 79.0, 87.0]}),
            ),
            "unexplained"
        );
    }

    #[test]
    fn resolution_history_is_preserved() {
        let first = merged_resolution(None, serde_json::json!({"action": "accept_ours"}), "a");
        assert!(first.get("history").is_none());
        assert_eq!(first["by"], "a");
        assert!(first.get("at").is_some());
        let second = merged_resolution(Some(first), serde_json::json!({"action": "reopened"}), "x");
        assert_eq!(second["history"][0]["action"], "accept_ours");
        assert_eq!(second["by"], "x");
        let third = merged_resolution(
            Some(second),
            serde_json::json!({"action": "flag_replicates", "replicate_indexes": [2]}),
            "y",
        );
        assert_eq!(third["action"], "flag_replicates");
        assert_eq!(third["by"], "y");
        assert_eq!(third["history"][0]["action"], "accept_ours");
        assert_eq!(third["history"][1]["action"], "reopened");
        assert_eq!(third["history"][1]["by"], "x");
    }

    #[test]
    fn changed_expectations_reopen_terminal_holds() {
        let recorded = serde_json::json!({"mean": 25.0, "sd": 10.0});
        let same = GroupAudit {
            time: chrono::Utc::now(),
            expected_mean: Some(25.0),
            expected_sd: Some(10.0),
            expected_n: None,
        };
        assert!(!expected_changed(&recorded, &same));
        let moved = GroupAudit {
            expected_mean: Some(26.0),
            ..same.clone()
        };
        assert!(expected_changed(&recorded, &moved));
        let gained_n = GroupAudit {
            expected_n: Some(3),
            ..same.clone()
        };
        assert!(expected_changed(&recorded, &gained_n));
        let lost_sd = GroupAudit {
            expected_sd: None,
            ..same
        };
        assert!(expected_changed(&recorded, &lost_sd));
    }

    #[test]
    fn bound_sql_carries_the_quantum_floor() {
        let sql = bound_sql("a.v", "b.v", "$3", DEFAULT_ABS_TOL);
        assert!(sql.contains(&QUANTUM_FLOOR.to_string()), "{sql}");
        assert!(sql.contains(&DEFAULT_ABS_TOL.to_string()), "{sql}");
    }
}
