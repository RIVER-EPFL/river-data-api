//! The period statistics table the portals rendered under every time series.
//!
//! `getStats` in `cnet-data-portal/app/utils/helper_functions.R` prints Time Points, N, NA's,
//! Median, Mean, SD, Min and Max per parameter over the displayed range, under both the grab and
//! the sensor series. River-data had no server-side equivalent, so the only period summary was
//! computed in the browser with an sd whose divisor was neither chosen nor named. This computes it
//! in SQL over exactly the values the API serves, and labels both divisors rather than one.

use axum::{
    Json,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::{resolve_site_with_project, validate_optional_time_range};

use super::types::SiteRef;

#[derive(Debug, Deserialize, IntoParams)]
pub struct StatisticsQuery {
    /// Start of the range (ISO 8601). Defaults to the configured lookback.
    pub start: Option<DateTime<Utc>>,
    /// End of the range (ISO 8601). Open-ended when omitted.
    pub end: Option<DateTime<Utc>>,
    /// Global parameter ids, comma-separated. Every parameter at the site when omitted.
    pub parameter_ids: Option<String>,
    /// `continuous` (default) or `spot`. A spot period is summarised over the served instant
    /// values, which are sample means, not over the individual replicates.
    pub measurement_type: Option<String>,
}

/// One parameter's row of the summary query. Derived rather than hand-decoded so a column added
/// to the query and not to its reader is a compile error.
#[derive(FromQueryResult)]
struct StatisticsRow {
    parameter_id: Uuid,
    code: String,
    name: String,
    units: Option<String>,
    decimal_places: Option<i16>,
    time_points: i64,
    n: i64,
    median: Option<f64>,
    mean: Option<f64>,
    stdev_sample: Option<f64>,
    stdev_population: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
}

/// The portal's eight rows for one parameter, with both standard deviations rather than one.
#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterStatistics {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimal_places: Option<i16>,
    /// Instants in the range, whether or not each carries a value.
    pub time_points: i64,
    /// Instants carrying a value: what every statistic below is computed over.
    pub n: i64,
    /// `time_points` less `n`, the portal's NA's row.
    pub nulls: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    /// Both divisors, named. Which one a slot declares governs its replicate groups, not a period
    /// summary, so neither is presented as the answer here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev_sample: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev_population: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatisticsResponse {
    pub site: SiteRef,
    pub start: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    /// `continuous` or `spot`: which cadence the rows summarise.
    pub measurement_type: String,
    pub parameters: Vec<ParameterStatistics>,
}

/// The value expression each cadence is summarised over.
///
/// Continuous rows live at `replicate_index = 0` and are the reading itself. A spot instant is
/// summarised at the value the API serves for it, the sample mean over the live replicates, so the
/// period statistics and the plotted series are the same numbers.
fn value_source(measurement_type: &str) -> &'static str {
    if measurement_type == "spot" {
        "SELECT r.parameter_id, r.time, \
                COALESCE(MAX(smp.mean), AVG(COALESCE(r.calibrated_value, r.raw_value))) AS value \
         FROM readings r \
         LEFT JOIN samples smp ON smp.id = r.sample_id \
         WHERE r.site_id = $1 AND r.parameter_id = ANY($2) AND r.measurement_type = 'spot' \
           AND r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL \
         GROUP BY r.parameter_id, r.time"
    } else {
        // Several streams can serve one slot instant; the period counts the instant once.
        "SELECT r.parameter_id, r.time, \
                AVG(COALESCE(r.calibrated_value, r.raw_value)) AS value \
         FROM readings r \
         WHERE r.site_id = $1 AND r.parameter_id = ANY($2) AND r.replicate_index = 0 \
           AND r.measurement_type IS DISTINCT FROM 'spot' AND r.is_flagged IS NOT TRUE \
         GROUP BY r.parameter_id, r.time"
    }
}

/// Period statistics for a site's parameters over a range.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/statistics",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        StatisticsQuery
    ),
    responses(
        (status = 200, description = "Period statistics per parameter", body = StatisticsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_statistics(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<StatisticsQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Response> {
    let (site, _project) = resolve_site_with_project(&state.db, &site_id).await?;
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let measurement_type = match query.measurement_type.as_deref() {
        None | Some("continuous") => "continuous",
        Some("spot") => "spot",
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "measurement_type must be 'continuous' or 'spot', not '{other}'"
            )));
        }
    };

    let effective_start = query.start.unwrap_or_else(|| {
        Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    validate_optional_time_range(Some(effective_start), query.end)?;

    let parameter_ids: Vec<Uuid> = match query.parameter_ids.as_deref() {
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Uuid::parse_str)
            .collect::<Result<_, _>>()
            .map_err(|_| AppError::BadRequest("parameter_ids must be UUIDs".to_string()))?,
        None => {
            use crate::routes::private::sites::parameters;
            use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
            parameters::Entity::find()
                .filter(parameters::Column::SiteId.eq(site.id))
                .all(&state.db)
                .await?
                .into_iter()
                .map(|sp| sp.parameter_id)
                .collect()
        }
    };
    if parameter_ids.is_empty() {
        return Ok(Json(StatisticsResponse {
            site: SiteRef {
                id: site.id,
                name: site.name.clone(),
            },
            start: effective_start,
            end: query.end,
            measurement_type: measurement_type.to_string(),
            parameters: Vec::new(),
        })
        .into_response());
    }

    let mut binds: Vec<sea_orm::Value> = vec![
        site.id.into(),
        parameter_ids.clone().into(),
        effective_start.into(),
    ];
    let end_clause = match query.end {
        Some(end) => {
            binds.push(end.into());
            " AND v.time <= $4"
        }
        None => "",
    };

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT p.id AS parameter_id, p.code, p.name, \
                        COALESCE(sp.display_units, p.default_units) AS units, \
                        sp.decimal_places, \
                        COUNT(v.time) AS time_points, \
                        COUNT(v.value) AS n, \
                        PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v.value) AS median, \
                        AVG(v.value) AS mean, \
                        STDDEV_SAMP(v.value) AS stdev_sample, \
                        STDDEV_POP(v.value) AS stdev_population, \
                        MIN(v.value) AS min_value, \
                        MAX(v.value) AS max_value \
                 FROM parameters p \
                 LEFT JOIN site_parameters sp \
                        ON sp.parameter_id = p.id AND sp.site_id = $1 \
                 LEFT JOIN ({source}) v \
                        ON v.parameter_id = p.id AND v.time >= $3{end_clause} \
                 WHERE p.id = ANY($2) \
                 GROUP BY p.id, p.code, p.name, units, sp.decimal_places \
                 ORDER BY p.code",
                source = value_source(measurement_type),
            ),
            binds,
        ))
        .await?;

    let mut parameters = Vec::with_capacity(rows.len());
    for row in &rows {
        let r = StatisticsRow::from_query_result(row, "")?;
        parameters.push(ParameterStatistics {
            parameter_id: r.parameter_id,
            code: r.code,
            name: r.name,
            units: r.units,
            decimal_places: r.decimal_places,
            time_points: r.time_points,
            n: r.n,
            nulls: r.time_points - r.n,
            median: r.median,
            mean: r.mean,
            stdev_sample: r.stdev_sample,
            stdev_population: r.stdev_population,
            min: r.min_value,
            max: r.max_value,
        });
    }

    Ok(Json(StatisticsResponse {
        site: SiteRef {
                id: site.id,
                name: site.name.clone(),
            },
        start: effective_start,
        end: query.end,
        measurement_type: measurement_type.to_string(),
        parameters,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::value_source;

    /// Expected behaviour: a spot period is summarised at the served instant value, a continuous
    /// one at the reading. Reading the wrong side would summarise replicates as if each were a
    /// measurement of its own.
    #[test]
    fn each_cadence_is_summarised_over_what_the_api_serves_for_it() {
        let spot = value_source("spot");
        assert!(spot.contains("smp.mean"), "the spot arm reads the mean");
        assert!(
            spot.contains("withdrawn_at IS NULL"),
            "a retracted replicate is not in the period"
        );

        let continuous = value_source("continuous");
        assert!(
            continuous.contains("r.replicate_index = 0"),
            "continuous rows live at index 0"
        );
        assert!(
            !continuous.contains("samples"),
            "a continuous reading has no sample to average"
        );
    }
}
