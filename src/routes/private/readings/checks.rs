//! The portal's Check gate, ported: screen entered values against the site's seasonal
//! distribution before they are saved.
//!
//! Portal semantics, kept exactly: same station, months within ±2 of the entry date across ALL
//! years, min/Q10/Q90/max (not mean±sd), replicate values pooled. One deliberate fix: the
//! portal's else-if chain made the 'max' label unreachable (a value above the historical maximum
//! was reported as merely above Q90); here the extremes are classified before the quantiles.
//!
//! The check is advisory — it never blocks a value — but it gates the save workflow: the stored
//! check row is what a save's `check_id` is validated against, and a save whose values are not
//! the checked values is refused, which is the portal's "any edit resets Check" enforced
//! server-side.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeasonalCheckRequest {
    pub site_id: Uuid,
    /// The entry instant; its month anchors the ±2-month seasonal window.
    pub time: chrono::DateTime<chrono::Utc>,
    pub values: Vec<SeasonalCheckValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeasonalCheckValue {
    pub parameter_id: Uuid,
    pub value: f64,
}

/// Where an entered value sits against the seasonal distribution. Only `normal` carries no
/// warning; everything else is advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeasonalClass {
    /// No history to compare against.
    NoHistory,
    BelowMin,
    BelowQ10,
    Normal,
    AboveQ90,
    AboveMax,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonalFinding {
    pub parameter_id: Uuid,
    pub value: f64,
    pub class: SeasonalClass,
    pub warning: bool,
    /// Pooled historical values in the seasonal window (unflagged spot replicates, all years).
    pub n: i64,
    pub min: Option<f64>,
    pub q10: Option<f64>,
    pub q90: Option<f64>,
    pub max: Option<f64>,
    /// A capped sample of the pooled values, for the distribution plot.
    pub distribution: Vec<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonalCheckResponse {
    /// Reference for the save: `/grab_samples` validates its readings against this check's
    /// stored entries when the request names it.
    pub check_id: Uuid,
    pub findings: Vec<SeasonalFinding>,
    pub warnings: usize,
}

/// Classify with the extremes before the quantiles, so a value beyond the recorded range reports
/// as beyond it.
#[must_use]
pub fn classify(
    value: f64,
    min: Option<f64>,
    q10: Option<f64>,
    q90: Option<f64>,
    max: Option<f64>,
) -> SeasonalClass {
    let (Some(min), Some(max)) = (min, max) else {
        return SeasonalClass::NoHistory;
    };
    if value < min {
        return SeasonalClass::BelowMin;
    }
    if value > max {
        return SeasonalClass::AboveMax;
    }
    if let Some(q10) = q10
        && value < q10
    {
        return SeasonalClass::BelowQ10;
    }
    if let Some(q90) = q90
        && value > q90
    {
        return SeasonalClass::AboveQ90;
    }
    SeasonalClass::Normal
}

/// Cap on the per-parameter distribution sample returned for plotting.
const DISTRIBUTION_CAP: i64 = 500;

/// Screen entered values against the site's seasonal distribution (same site, entry month ±2
/// across all years, unflagged spot replicates pooled; min/Q10/Q90/max). Stores the check and
/// returns its id: pass it as `check_id` on `/grab_samples` and the save is validated against
/// exactly these values, so an edit after checking requires a fresh check. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/readings/seasonal_check",
    request_body = SeasonalCheckRequest,
    responses(
        (status = 200, description = "Per-value classification with the distribution payload", body = SeasonalCheckResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "ingestion"
)]
pub async fn seasonal_check(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<SeasonalCheckRequest>,
) -> AppResult<Json<SeasonalCheckResponse>> {
    if req.values.is_empty() {
        return Err(AppError::BadRequest("No values to check".to_string()));
    }
    let site_exists = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 AS one FROM sites WHERE id = $1",
            [req.site_id.into()],
        ))
        .await?
        .is_some();
    if !site_exists {
        return Err(AppError::NotFound(format!("Site {} not found", req.site_id)));
    }

    let mut findings = Vec::with_capacity(req.values.len());
    for v in &req.values {
        // Cyclic month distance: December is two months from February. The window pools every
        // unflagged spot replicate of the slot across all years, which is the portal's regex
        // pooling of replicate columns expressed over rows.
        let row = state
            .db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*) AS n,
                        MIN(v) AS min, MAX(v) AS max,
                        percentile_cont(0.1) WITHIN GROUP (ORDER BY v) AS q10,
                        percentile_cont(0.9) WITHIN GROUP (ORDER BY v) AS q90
                 FROM (
                    SELECT COALESCE(calibrated_value, raw_value) AS v
                    FROM readings
                    WHERE site_id = $1 AND parameter_id = $2
                      AND measurement_type = 'spot'
                      AND is_flagged IS NOT TRUE
                      AND withdrawn_at IS NULL
                      AND LEAST(
                            (EXTRACT(MONTH FROM time)::int - EXTRACT(MONTH FROM $3::timestamptz)::int + 12) % 12,
                            (EXTRACT(MONTH FROM $3::timestamptz)::int - EXTRACT(MONTH FROM time)::int + 12) % 12
                          ) <= 2
                 ) pooled",
                [
                    req.site_id.into(),
                    v.parameter_id.into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(req.time).into(),
                ],
            ))
            .await?
            .ok_or_else(|| AppError::Internal("seasonal stats query returned nothing".into()))?;
        let n: i64 = row.try_get("", "n")?;
        let min: Option<f64> = row.try_get("", "min")?;
        let max: Option<f64> = row.try_get("", "max")?;
        let q10: Option<f64> = row.try_get("", "q10")?;
        let q90: Option<f64> = row.try_get("", "q90")?;

        let distribution = if n > 0 {
            state
                .db
                .query_all(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT COALESCE(calibrated_value, raw_value) AS v
                     FROM readings
                     WHERE site_id = $1 AND parameter_id = $2
                       AND measurement_type = 'spot'
                       AND is_flagged IS NOT TRUE
                       AND withdrawn_at IS NULL
                       AND LEAST(
                             (EXTRACT(MONTH FROM time)::int - EXTRACT(MONTH FROM $3::timestamptz)::int + 12) % 12,
                             (EXTRACT(MONTH FROM $3::timestamptz)::int - EXTRACT(MONTH FROM time)::int + 12) % 12
                           ) <= 2
                     ORDER BY time DESC LIMIT $4",
                    [
                        req.site_id.into(),
                        v.parameter_id.into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(req.time).into(),
                        DISTRIBUTION_CAP.into(),
                    ],
                ))
                .await?
                .iter()
                .filter_map(|r| r.try_get::<f64>("", "v").ok())
                .collect()
        } else {
            Vec::new()
        };

        let class = classify(v.value, min, q10, q90, max);
        findings.push(SeasonalFinding {
            parameter_id: v.parameter_id,
            value: v.value,
            class,
            warning: !matches!(class, SeasonalClass::Normal | SeasonalClass::NoHistory),
            n,
            min,
            q10,
            q90,
            max,
            distribution,
        });
    }

    let check_id = Uuid::new_v4();
    state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO seasonal_checks (id, site_id, checked_time, entries, created_by)
             VALUES ($1, $2, $3, $4, $5)",
            [
                check_id.into(),
                req.site_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(req.time).into(),
                serde_json::to_value(&req.values)
                    .unwrap_or(serde_json::Value::Null)
                    .into(),
                crate::routes::private::tools::scripts::actor_label(&auth).into(),
            ],
        ))
        .await?;

    let warnings = findings.iter().filter(|f| f.warning).count();
    Ok(Json(SeasonalCheckResponse {
        check_id,
        findings,
        warnings,
    }))
}

/// Validate a save's claimed check: it must belong to the same site and cover every
/// `(parameter, value)` the save writes. A pair the check did not screen is the "edited after
/// checking" case and is refused, so the gate cannot be satisfied by a stale check.
pub async fn validate_check_claim(
    db: &sea_orm::DatabaseConnection,
    check_id: Uuid,
    site_id: Uuid,
    pairs: &[(Uuid, f64)],
) -> AppResult<()> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT site_id, entries FROM seasonal_checks WHERE id = $1",
            [check_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Check {check_id} does not exist")))?;
    let check_site: Uuid = row.try_get("", "site_id")?;
    if check_site != site_id {
        return Err(AppError::BadRequest(
            "The named check screened values for a different site".to_string(),
        ));
    }
    let entries: serde_json::Value = row.try_get("", "entries")?;
    let checked: Vec<SeasonalCheckValue> =
        serde_json::from_value(entries).map_err(|e| AppError::Internal(e.to_string()))?;
    for (parameter_id, value) in pairs {
        let covered = checked
            .iter()
            .any(|c| c.parameter_id == *parameter_id && c.value == *value);
        if !covered {
            return Err(AppError::Conflict(format!(
                "Value {value} for parameter {parameter_id} was not screened by check \
                 {check_id}; values edited after a check need a fresh check"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SeasonalClass, classify};

    #[test]
    fn extremes_are_classified_before_quantiles() {
        let (min, q10, q90, max) = (Some(1.0), Some(2.0), Some(8.0), Some(10.0));
        assert_eq!(classify(0.5, min, q10, q90, max), SeasonalClass::BelowMin);
        assert_eq!(classify(1.5, min, q10, q90, max), SeasonalClass::BelowQ10);
        assert_eq!(classify(5.0, min, q10, q90, max), SeasonalClass::Normal);
        assert_eq!(classify(9.0, min, q10, q90, max), SeasonalClass::AboveQ90);
        // The portal's unreachable label: above the recorded maximum reports as above it.
        assert_eq!(classify(11.0, min, q10, q90, max), SeasonalClass::AboveMax);
    }

    #[test]
    fn no_history_is_its_own_class() {
        assert_eq!(classify(5.0, None, None, None, None), SeasonalClass::NoHistory);
    }

    #[test]
    fn boundary_values_take_the_inner_class() {
        let (min, q10, q90, max) = (Some(1.0), Some(2.0), Some(8.0), Some(10.0));
        // A recorded extreme is not "beyond" the record, but it still sits outside the quantiles.
        assert_eq!(classify(1.0, min, q10, q90, max), SeasonalClass::BelowQ10);
        assert_eq!(classify(10.0, min, q10, q90, max), SeasonalClass::AboveQ90);
        assert_eq!(classify(2.0, min, q10, q90, max), SeasonalClass::Normal);
        assert_eq!(classify(8.0, min, q10, q90, max), SeasonalClass::Normal);
    }
}
