//! What a replicate group's statistics become under a change nobody has made yet: without the
//! replicates about to be flagged, with the ones about to be restored, or under the other
//! standard-deviation divisor. Read-only; the write paths recompute for real.

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::sd_estimator;
use crate::routes::private::sync::models::GroupAudit;
use crate::routes::private::sync::service::{self as audit, GroupStats};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SamplePreviewRequest {
    /// Key the group by its stream, or by site and parameter; the instant is required either way.
    #[serde(default)]
    pub stream_id: Option<Uuid>,
    #[serde(default)]
    pub site_id: Option<Uuid>,
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    pub time: DateTime<Utc>,
    /// Replicates to leave out, as a flag would.
    #[serde(default)]
    pub exclude_replicate_indexes: Vec<i16>,
    /// Flagged replicates to bring back, as an unflag would.
    #[serde(default)]
    pub include_replicate_indexes: Vec<i16>,
    /// The divisor to compute the proposed sd under; absent keeps the group's current one.
    #[serde(default)]
    pub estimator: Option<String>,
    /// A replicate audit hold on this group; the response says whether the proposed statistics
    /// meet its recorded expectation under the audit tolerances.
    #[serde(default)]
    pub hold_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct PreviewStats {
    pub n: usize,
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
    /// 'sample' (divisor n-1) or 'population' (divisor n).
    #[schema(value_type = sd_estimator::SdEstimator)]
    pub sd_estimator: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct PreviewDelta {
    pub n: i64,
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewReplicate {
    pub index: i16,
    pub value: f64,
    pub flagged: bool,
    pub withdrawn: bool,
    /// Whether the value counts in the statistics served now.
    pub included_now: bool,
    /// Whether it would count after the change.
    pub included_after: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HoldMatch {
    pub hold_id: Uuid,
    #[schema(required)]
    pub expected_mean: Option<f64>,
    #[schema(required)]
    pub expected_sd: Option<f64>,
    #[schema(required)]
    pub expected_n: Option<i64>,
    /// Whether the current statistics meet the expectation (they do not, or there is no hold).
    pub meets_now: bool,
    /// Whether the proposed statistics meet it under the audit tolerances.
    pub meets_after: bool,
    pub mean_agrees: bool,
    pub sd_agrees: bool,
    pub n_agrees: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SamplePreviewResponse {
    pub current: PreviewStats,
    pub proposed: PreviewStats,
    pub delta: PreviewDelta,
    pub replicates: Vec<PreviewReplicate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub hold: Option<HoldMatch>,
}

/// One stored replicate as the preview reads it.
#[derive(Debug, Clone, Copy)]
pub struct Replicate {
    pub index: i16,
    pub value: f64,
    pub flagged: bool,
    pub withdrawn: bool,
}

/// One replicate as the preview query returns it: the slot it belongs to, and the four columns a
/// [`Replicate`] is made of.
#[derive(sea_orm::FromQueryResult)]
struct PreviewRow {
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    replicate_index: i16,
    value: f64,
    flagged: bool,
    withdrawn: bool,
}

/// The change being previewed.
#[derive(Debug, Clone, Default)]
pub struct Change<'a> {
    pub exclude: &'a [i16],
    pub include: &'a [i16],
    pub estimator: Option<&'static str>,
}

fn stats_over(values: &[f64], estimator: &'static str) -> PreviewStats {
    let s = audit::group_stats(values).under(estimator);
    PreviewStats {
        n: s.n,
        mean: s.mean,
        sd: s.sd,
        sd_estimator: estimator,
    }
}

/// The statistics now and after the change, over the same rule the samples trigger applies:
/// unflagged, unwithdrawn replicates only. A withdrawn replicate is outside every count and no
/// flag change brings it back.
#[must_use]
pub fn preview(
    replicates: &[Replicate],
    current_estimator: &'static str,
    change: &Change<'_>,
) -> (
    PreviewStats,
    PreviewStats,
    PreviewDelta,
    Vec<PreviewReplicate>,
) {
    let proposed_estimator = change.estimator.unwrap_or(current_estimator);
    let mut rows = Vec::with_capacity(replicates.len());
    let mut now = Vec::new();
    let mut after = Vec::new();
    for r in replicates {
        let included_now = !r.flagged && !r.withdrawn;
        let included_after = !r.withdrawn
            && !change.exclude.contains(&r.index)
            && (!r.flagged || change.include.contains(&r.index));
        if included_now {
            now.push(r.value);
        }
        if included_after {
            after.push(r.value);
        }
        rows.push(PreviewReplicate {
            index: r.index,
            value: r.value,
            flagged: r.flagged,
            withdrawn: r.withdrawn,
            included_now,
            included_after,
        });
    }
    let current = stats_over(&now, current_estimator);
    let proposed = stats_over(&after, proposed_estimator);
    let delta = PreviewDelta {
        n: i64::try_from(proposed.n).unwrap_or(0) - i64::try_from(current.n).unwrap_or(0),
        mean: proposed.mean.zip(current.mean).map(|(p, c)| p - c),
        sd: proposed.sd.zip(current.sd).map(|(p, c)| p - c),
    };
    (current, proposed, delta, rows)
}

/// Whether statistics meet a hold's recorded expectation, under the tolerances the audit itself
/// compares with.
#[must_use]
pub fn hold_match(
    hold_id: Uuid,
    expected: &GroupAudit,
    now: &PreviewStats,
    after: &PreviewStats,
) -> HoldMatch {
    let as_group = |s: &PreviewStats| GroupStats {
        n: s.n,
        mean: s.mean,
        sd: s.sd,
    };
    let after_group = as_group(after);
    HoldMatch {
        hold_id,
        expected_mean: expected.expected_mean,
        expected_sd: expected.expected_sd,
        expected_n: expected.expected_n,
        meets_now: audit::agrees(expected, &as_group(now)),
        meets_after: audit::agrees(expected, &after_group),
        mean_agrees: audit::stats_agree(expected.expected_mean, after.mean, audit::DEFAULT_REL_TOL),
        sd_agrees: audit::stats_agree_with(
            expected.expected_sd,
            after.sd,
            audit::SD_REL_TOL,
            audit::SD_ABS_TOL,
        ),
        n_agrees: expected
            .expected_n
            .is_none_or(|n| i64::try_from(after.n) == Ok(n)),
    }
}

/// Preview a replicate group's statistics after flagging, restoring or switching the sd divisor.
/// Nothing is written. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/readings/sample_preview",
    request_body = SamplePreviewRequest,
    responses(
        (status = 200, body = SamplePreviewResponse),
        (status = 400, description = "Neither key form, an unknown estimator, or an index the group does not hold"),
        (status = 404, description = "No spot reading at that instant, or no such hold on it"),
    ),
    tag = "readings"
)]
pub async fn sample_preview(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(q): Json<SamplePreviewRequest>,
) -> AppResult<Json<SamplePreviewResponse>> {
    let estimator = sd_estimator::parse_opt(q.estimator.as_deref())?;
    let time = sea_orm::prelude::DateTimeWithTimeZone::from(q.time);
    let rows = match (q.stream_id, q.site_id, q.parameter_id) {
        (Some(stream_id), _, _) => {
            state
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT site_id, parameter_id, replicate_index,
                            COALESCE(calibrated_value, raw_value) AS value,
                            is_flagged IS TRUE AS flagged, withdrawn_at IS NOT NULL AS withdrawn
                     FROM readings WHERE stream_id = $1 AND time = $2
                     ORDER BY replicate_index",
                    [stream_id.into(), time.into()],
                ))
                .await?
        }
        (None, Some(site_id), Some(parameter_id)) => {
            state
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT site_id, parameter_id, replicate_index,
                            COALESCE(calibrated_value, raw_value) AS value,
                            is_flagged IS TRUE AS flagged, withdrawn_at IS NOT NULL AS withdrawn
                     FROM readings
                     WHERE site_id = $1 AND parameter_id = $2 AND time = $3
                       AND measurement_type = 'spot'
                     ORDER BY replicate_index",
                    [site_id.into(), parameter_id.into(), time.into()],
                ))
                .await?
        }
        _ => {
            return Err(AppError::BadRequest(
                "Provide either stream_id or both site_id and parameter_id".to_string(),
            ));
        }
    };
    if rows.is_empty() {
        return Err(AppError::NotFound("No reading at that instant".to_string()));
    }
    let mut replicates = Vec::with_capacity(rows.len());
    let mut slot: Option<(Uuid, Uuid)> = None;
    for row in &rows {
        let row = PreviewRow::from_query_result(row, "")?;
        if let (Some(s), Some(p)) = (row.site_id, row.parameter_id) {
            slot.get_or_insert((s, p));
        }
        replicates.push(Replicate {
            index: row.replicate_index,
            value: row.value,
            flagged: row.flagged,
            withdrawn: row.withdrawn,
        });
    }
    if let Some((site_id, _)) = slot {
        enforce_project_scope_for_sites(&state.db, &scope, &[site_id]).await?;
    } else if scope.is_restricted() {
        return Err(AppError::NotFound("No reading at that instant".to_string()));
    }
    let known: Vec<i16> = replicates.iter().map(|r| r.index).collect();
    let unknown: Vec<String> = q
        .exclude_replicate_indexes
        .iter()
        .chain(q.include_replicate_indexes.iter())
        .filter(|i| !known.contains(i))
        .map(i16::to_string)
        .collect();
    if !unknown.is_empty() {
        return Err(AppError::BadRequest(format!(
            "no reading at replicate index {} in this group",
            unknown.join(", ")
        )));
    }

    // The divisor the group is served under now: the instant's own choice, else the slot's
    // declaration, else the undeclared fallback.
    let current_estimator = match slot {
        Some((site_id, parameter_id)) => {
            match sd_estimator::instant_declaration(&state.db, site_id, parameter_id, q.time)
                .await?
            {
                Some(e) => e,
                None => {
                    sd_estimator::resolve(&state.db, site_id, parameter_id, None, None)
                        .await?
                        .estimator
                }
            }
        }
        None => sd_estimator::SAMPLE,
    };

    let change = Change {
        exclude: &q.exclude_replicate_indexes,
        include: &q.include_replicate_indexes,
        estimator,
    };
    let (current, proposed, delta, rows) = preview(&replicates, current_estimator, &change);

    let hold = match q.hold_id {
        None => None,
        Some(hold_id) => {
            let row = state
                .db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT expected FROM replicate_audit_holds
                     WHERE id = $1 AND kind = 'replicate_stats' AND group_time = $2",
                    [hold_id.into(), time.into()],
                ))
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!("no replicate audit hold {hold_id} on this instant"))
                })?;
            let expected: serde_json::Value = row.try_get("", "expected")?;
            let expected = GroupAudit {
                time: q.time,
                expected_mean: expected.get("mean").and_then(serde_json::Value::as_f64),
                expected_sd: expected.get("sd").and_then(serde_json::Value::as_f64),
                expected_n: expected.get("n").and_then(serde_json::Value::as_i64),
            };
            Some(hold_match(hold_id, &expected, &current, &proposed))
        }
    };

    Ok(Json(SamplePreviewResponse {
        current,
        proposed,
        delta,
        replicates: rows,
        hold,
    }))
}

#[cfg(test)]
#[path = "tests/sample_preview.rs"]
mod tests;
