//! The portal's Check gate, ported: screen entered values against the site's seasonal
//! distribution before they are saved.
//!
//! Portal semantics, kept exactly: same station, months within ±2 of the entry date across ALL
//! years, min/Q10/Q90/max (not mean±sd), replicate values pooled. One deliberate fix: the
//! portal's else-if chain made the 'max' label unreachable (a value above the historical maximum
//! was reported as merely above Q90); here the extremes are classified before the quantiles.
//!
//! The check is advisory, it never blocks a value, but it gates the save workflow: the stored
//! check row is what a save's `check_id` is validated against, and a save whose values are not
//! the checked values is refused, which is the portal's "any edit resets Check" enforced
//! server-side.
//!
//! The response carries a `method` object describing what was computed. It is built here, next
//! to the query it describes, so the explanation the UI shows cannot drift from the SQL.

use std::collections::HashMap;

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, EntityTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Half-width of the seasonal window: the entry month plus and minus this many months.
pub const WINDOW_MONTHS: i32 = 2;

/// Cap on the per-parameter distribution sample returned for plotting.
const DISTRIBUTION_CAP: i64 = 500;

/// The rows the window pools: one slot's unflagged, non-withdrawn spot replicates whose month is
/// within `WINDOW_MONTHS` of the entry month, cyclically, across every year. `$1` site, `$2`
/// parameter, `$3` the entry instant, `$4` the half-width.
const POOLED_ROWS_SQL: &str = "SELECT raw_value AS v
    FROM readings
    WHERE site_id = $1 AND parameter_id = $2
      AND measurement_type = 'spot'
      AND is_flagged IS NOT TRUE
      AND withdrawn_at IS NULL
      AND unverified IS NOT TRUE
      AND LEAST(
            (EXTRACT(MONTH FROM time)::int - EXTRACT(MONTH FROM $3::timestamptz)::int + 12) % 12,
            (EXTRACT(MONTH FROM $3::timestamptz)::int - EXTRACT(MONTH FROM time)::int + 12) % 12
          ) <= $4";

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

impl SeasonalClass {
    /// Every class, in the order the method object lists them.
    pub const ALL: [SeasonalClass; 6] = [
        SeasonalClass::NoHistory,
        SeasonalClass::BelowMin,
        SeasonalClass::BelowQ10,
        SeasonalClass::Normal,
        SeasonalClass::AboveQ90,
        SeasonalClass::AboveMax,
    ];

    /// Whether the class is reported as a warning. No history is not a warning: there is
    /// nothing to disagree with.
    #[must_use]
    pub fn is_warning(self) -> bool {
        !matches!(self, SeasonalClass::Normal | SeasonalClass::NoHistory)
    }

    fn meaning(self) -> &'static str {
        match self {
            SeasonalClass::NoHistory => "no pooled history for this parameter in the window",
            SeasonalClass::BelowMin => "below the lowest pooled value",
            SeasonalClass::BelowQ10 => "at or above the minimum but below the 10th percentile",
            SeasonalClass::Normal => "between the 10th and 90th percentiles, inclusive",
            SeasonalClass::AboveQ90 => "above the 90th percentile but at or below the maximum",
            SeasonalClass::AboveMax => "above the highest pooled value",
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonalFinding {
    pub parameter_id: Uuid,
    pub value: f64,
    pub class: SeasonalClass,
    pub warning: bool,
    /// Pooled historical values in the seasonal window (unflagged spot replicates, all years).
    pub n: i64,
    #[schema(required)]
    pub min: Option<f64>,
    #[schema(required)]
    pub q10: Option<f64>,
    #[schema(required)]
    pub q90: Option<f64>,
    #[schema(required)]
    pub max: Option<f64>,
    /// A capped sample of the pooled values, for the distribution plot.
    pub distribution: Vec<f64>,
}

/// One classification label and what it means, for the method description.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeasonalClassDescription {
    pub class: SeasonalClass,
    pub meaning: &'static str,
    pub warning: bool,
}

/// What the check computed, in the terms the query uses. Rendered by the UI as the explanation
/// of the check; never authored client-side.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeasonalMethod {
    /// Half-width of the seasonal window in months.
    pub window_months: i32,
    /// Which rows are pooled.
    pub window: String,
    /// Which rows are excluded and how replicates enter.
    pub pooled: &'static str,
    /// Which stored column is compared, and against what.
    pub value: &'static str,
    /// The statistics computed over the pooled values.
    pub statistics: &'static str,
    /// The classification, extremes first.
    pub classes: Vec<SeasonalClassDescription>,
}

/// The method description for the query in `POOLED_ROWS_SQL` and the classification in
/// `classify`.
#[must_use]
pub fn method() -> SeasonalMethod {
    SeasonalMethod {
        window_months: WINDOW_MONTHS,
        window: format!(
            "Same site and parameter, entry month ±{WINDOW_MONTHS} months (cyclic, so December \
             is two months from February), across every year of stored history."
        ),
        pooled: "Spot (grab) readings only. Flagged and withdrawn readings are excluded. \
                 Replicates enter as individual values, not as their sample mean.",
        value: "The stored raw value, compared against the entered number as typed; \
                corrections enter on neither side.",
        statistics: "Minimum, maximum, and the 10th and 90th percentiles of the pooled values \
                     (percentile_cont, linear interpolation between ranks).",
        classes: SeasonalClass::ALL
            .iter()
            .map(|c| SeasonalClassDescription {
                class: *c,
                meaning: c.meaning(),
                warning: c.is_warning(),
            })
            .collect(),
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonalCheckResponse {
    /// Reference for the save: `/grab_samples` validates its readings against this check's
    /// stored entries when the request names it.
    pub check_id: Uuid,
    pub findings: Vec<SeasonalFinding>,
    pub warnings: usize,
    pub method: SeasonalMethod,
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

/// One pooled value from the seasonal window, which is the whole row the distribution reads.
#[derive(FromQueryResult)]
struct PooledValue {
    v: f64,
}

#[derive(FromQueryResult)]
struct StoredCheck {
    site_id: Uuid,
    entries: serde_json::Value,
}

/// The pooled distribution of one slot for one entry instant.
#[derive(Debug, Clone, Copy, FromQueryResult)]
pub struct SeasonalStats {
    pub n: i64,
    pub min: Option<f64>,
    pub q10: Option<f64>,
    pub q90: Option<f64>,
    pub max: Option<f64>,
}

impl SeasonalStats {
    #[must_use]
    pub fn classify(&self, value: f64) -> SeasonalClass {
        classify(value, self.min, self.q10, self.q90, self.max)
    }
}

fn window_params(
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> [sea_orm::Value; 4] {
    [
        site_id.into(),
        parameter_id.into(),
        sea_orm::prelude::DateTimeWithTimeZone::from(time).into(),
        WINDOW_MONTHS.into(),
    ]
}

/// min/Q10/Q90/max over the slot's pooled window for `time`.
pub async fn seasonal_stats(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> AppResult<SeasonalStats> {
    SeasonalStats::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS n,
                        MIN(v) AS min, MAX(v) AS max,
                        percentile_cont(0.1) WITHIN GROUP (ORDER BY v) AS q10,
                        percentile_cont(0.9) WITHIN GROUP (ORDER BY v) AS q90
                 FROM ({POOLED_ROWS_SQL}) pooled"
        ),
        window_params(site_id, parameter_id, time),
    ))
    .one(db)
    .await?
    .ok_or_else(|| AppError::Internal("seasonal stats query returned nothing".into()))
}

/// The most recent pooled values, capped, for the distribution plot.
async fn seasonal_distribution(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> AppResult<Vec<f64>> {
    let mut params = window_params(site_id, parameter_id, time).to_vec();
    params.push(DISTRIBUTION_CAP.into());
    Ok(PooledValue::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!("{POOLED_ROWS_SQL} ORDER BY time DESC LIMIT $5"),
        params,
    ))
    .all(db)
    .await?
    .into_iter()
    .map(|r| r.v)
    .collect())
}

/// One screened cell of a wide file: which row and column it came from, and where it sits.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScreenedCell {
    /// 1-based line number in the file (the header is line 1).
    pub row: usize,
    pub parameter_id: Uuid,
    pub value: f64,
    pub class: SeasonalClass,
    pub warning: bool,
    pub n: i64,
    #[schema(required)]
    pub min: Option<f64>,
    #[schema(required)]
    pub max: Option<f64>,
}

/// Screen many cells at once, each against the window its own instant anchors. The statistics
/// are computed once per (parameter, month) rather than once per cell, so a file with thousands
/// of rows costs one query per distinct slot and month.
pub async fn screen_cells(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    cells: &[(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)],
) -> AppResult<Vec<ScreenedCell>> {
    use chrono::Datelike;
    let mut stats: HashMap<(Uuid, u32), SeasonalStats> = HashMap::new();
    let mut out = Vec::with_capacity(cells.len());
    for (row, parameter_id, time, value) in cells {
        let key = (*parameter_id, time.month());
        let s = match stats.get(&key) {
            Some(s) => *s,
            None => {
                let s = seasonal_stats(db, site_id, *parameter_id, *time).await?;
                stats.insert(key, s);
                s
            }
        };
        let class = s.classify(*value);
        out.push(ScreenedCell {
            row: *row,
            parameter_id: *parameter_id,
            value: *value,
            class,
            warning: class.is_warning(),
            n: s.n,
            min: s.min,
            max: s.max,
        });
    }
    Ok(out)
}

/// Store a check row and return its id: the reference a save is later held to.
pub async fn store_check(
    db: &impl ConnectionTrait,
    site_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    entries: &[SeasonalCheckValue],
    actor: String,
) -> AppResult<Uuid> {
    let check_id = Uuid::new_v4();
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO seasonal_checks (id, site_id, checked_time, entries, created_by)
         VALUES ($1, $2, $3, $4, $5)",
        [
            check_id.into(),
            site_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(time).into(),
            serde_json::to_value(entries)
                .unwrap_or(serde_json::Value::Null)
                .into(),
            actor.into(),
        ],
    ))
    .await?;
    Ok(check_id)
}

/// Screen entered values against the site's seasonal distribution (same site, entry month ±2
/// across all years, unflagged spot replicates pooled; min/Q10/Q90/max). Stores the check and
/// returns its id: pass it as `check_id` on `/grab_samples` and the save is validated against
/// exactly these values, so an edit after checking requires a fresh check. Requires `read_data`.
///
/// Raw against raw: the window pools `raw_value` and the screened value is the number as entered,
/// so a correction applied to the history cannot move the distribution out from under it. The
/// caveat this does not solve: a CSV imported with `values: "corrected"` stores a processed number
/// in `raw_value`, so such rows pool on a different basis than a typed entry.
#[utoipa::path(
    post,
    path = "/api/readings/seasonal_check",
    request_body = SeasonalCheckRequest,
    responses(
        (status = 200, description = "Per-value classification with the distribution payload and the method", body = SeasonalCheckResponse),
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
    let site_exists = crate::routes::private::sites::Entity::find_by_id(req.site_id)
        .one(&state.db)
        .await?
        .is_some();
    if !site_exists {
        return Err(AppError::NotFound(format!(
            "Site {} not found",
            req.site_id
        )));
    }

    let mut findings = Vec::with_capacity(req.values.len());
    for v in &req.values {
        let stats = seasonal_stats(&state.db, req.site_id, v.parameter_id, req.time).await?;
        let distribution = if stats.n > 0 {
            seasonal_distribution(&state.db, req.site_id, v.parameter_id, req.time).await?
        } else {
            Vec::new()
        };
        let class = stats.classify(v.value);
        findings.push(SeasonalFinding {
            parameter_id: v.parameter_id,
            value: v.value,
            class,
            warning: class.is_warning(),
            n: stats.n,
            min: stats.min,
            q10: stats.q10,
            q90: stats.q90,
            max: stats.max,
            distribution,
        });
    }

    let check_id = store_check(
        &state.db,
        req.site_id,
        req.time,
        &req.values,
        crate::common::actor::label(&auth),
    )
    .await?;

    let warnings = findings.iter().filter(|f| f.warning).count();
    Ok(Json(SeasonalCheckResponse {
        check_id,
        findings,
        warnings,
        method: method(),
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
    let row = StoredCheck::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT site_id, entries FROM seasonal_checks WHERE id = $1",
        [check_id.into()],
    ))
    .one(db)
    .await?
    .ok_or_else(|| AppError::BadRequest(format!("Check {check_id} does not exist")))?;
    if row.site_id != site_id {
        return Err(AppError::BadRequest(
            "The named check screened values for a different site".to_string(),
        ));
    }
    let checked: Vec<SeasonalCheckValue> =
        serde_json::from_value(row.entries).map_err(|e| AppError::Internal(e.to_string()))?;
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
    use super::{SeasonalClass, WINDOW_MONTHS, classify, method};

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
        assert_eq!(
            classify(5.0, None, None, None, None),
            SeasonalClass::NoHistory
        );
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

    #[test]
    fn the_method_describes_every_class_and_the_window_it_queries() {
        let m = method();
        assert_eq!(m.window_months, WINDOW_MONTHS);
        assert!(m.window.contains(&format!("±{WINDOW_MONTHS}")));
        assert_eq!(m.classes.len(), SeasonalClass::ALL.len());
        for (d, c) in m.classes.iter().zip(SeasonalClass::ALL) {
            assert_eq!(d.class, c);
            assert_eq!(d.warning, c.is_warning(), "{c:?}");
            assert!(!d.meaning.is_empty());
        }
        // The exclusions the query applies are the ones the text names.
        assert!(m.pooled.contains("Flagged") && m.pooled.contains("withdrawn"));
        assert!(m.value.contains("raw"));
    }
}
