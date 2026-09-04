//! Where a calculation's data lives, and what an edit of a stored value will recompute.
//!
//! The dependency graph existed only inside the chain executor, so nothing could answer the two
//! questions a person actually asks: an operator typing into a field wants to know which script
//! reads it, and an admin defining a calculation wants to know whether its inputs are configured
//! anywhere and where the values already stored under its outputs came from. Both are answered
//! from the manifests plus what the store holds, before any write.

use axum::{
    Json,
    extract::{Query, State},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

use super::closure::{CalculationImpact, calculations_fed_by};
use super::engine;

#[derive(Debug, Deserialize, IntoParams)]
pub struct ClosureQuery {
    /// Global parameter ids, comma-separated: the values whose consequences are being asked about.
    pub parameter_ids: Option<String>,
    /// Confine the coverage counts to one site. Every site when omitted.
    pub site_id: Option<Uuid>,
    /// Include the per-slot coverage of every calculation input and output. Off by default: it is
    /// three aggregate queries and a closure asked for before a write does not need it.
    #[serde(default)]
    pub include_coverage: bool,
}

/// What the store holds for one parameter of a calculation: whether anyone configured the slot,
/// how much is there, and where it came from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SlotCoverage {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    /// Sites with a `site_parameters` row for this parameter. Zero is the state an admin needs to
    /// see: a calculation input nobody has configured anywhere.
    pub sites_configured: i64,
    pub reading_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_reading: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reading: Option<DateTime<Utc>>,
    /// The source systems the stored values arrived on, so a value that came from the portal is
    /// not mistaken for one a run produced.
    pub source_systems: Vec<String>,
    /// The minting paths of the tool runs behind the stored values: `interactive`, `csv_import` or
    /// `chain`. Empty means nothing here was produced by a run.
    pub run_sources: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClosureResponse {
    /// The calculations the named parameters feed, in the order the chain would run them.
    pub calculations: Vec<CalculationImpact>,
    /// Coverage per calculation input and output, when `include_coverage` asked for it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<SlotCoverage>,
}

fn parse_ids(csv: Option<&str>) -> AppResult<Vec<Uuid>> {
    csv.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Uuid::parse_str)
        .collect::<Result<_, _>>()
        .map_err(|_| AppError::BadRequest("parameter_ids must be UUIDs".to_string()))
}

/// Every parameter any active calculation reads or writes, which is what coverage is reported over.
///
/// The outputs resolve through the tool catalog, which is the same resolution a run uses. The
/// `event_inputs` are looked up by code directly: the catalog is loaded from output and param
/// codes only, so an input parameter named nowhere else is absent from it.
async fn calculation_slots(state: &AppState) -> AppResult<Vec<Uuid>> {
    let tools = engine::list_active_tools(&state.db).await?;
    let catalog =
        engine::load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest)).await?;
    let mut ids: Vec<Uuid> = Vec::new();
    let mut input_codes: Vec<String> = Vec::new();
    for tool in &tools {
        for e in &tool.manifest.event_inputs {
            input_codes.push(e.parameter_code.to_lowercase());
        }
        for o in &tool.manifest.outputs {
            if let Some(p) = catalog.resolve(o)
                && !ids.contains(&p.id)
            {
                ids.push(p.id);
            }
        }
    }
    if !input_codes.is_empty() {
        let rows = state
            .db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id FROM parameters WHERE LOWER(code) = ANY($1)",
                [input_codes.into()],
            ))
            .await?;
        for row in &rows {
            let id: Uuid = row.try_get("", "id")?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Coverage for a set of parameters, one query each over configuration, readings and provenance.
pub async fn coverage_for(
    state: &AppState,
    parameter_ids: &[Uuid],
    site_id: Option<Uuid>,
) -> AppResult<Vec<SlotCoverage>> {
    if parameter_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut binds: Vec<sea_orm::Value> = vec![parameter_ids.to_vec().into()];
    let site_clause = match site_id {
        Some(id) => {
            binds.push(id.into());
            " AND r.site_id = $2"
        }
        None => "",
    };
    let sp_clause = if site_id.is_some() {
        " AND sp.site_id = $2"
    } else {
        ""
    };

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT p.id AS parameter_id, p.code, \
                        COALESCE(cfg.sites_configured, 0) AS sites_configured, \
                        COALESCE(obs.reading_count, 0) AS reading_count, \
                        obs.first_reading, obs.last_reading, \
                        COALESCE(obs.source_systems, ARRAY[]::text[]) AS source_systems, \
                        COALESCE(obs.run_sources, ARRAY[]::text[]) AS run_sources \
                 FROM parameters p \
                 LEFT JOIN LATERAL ( \
                     SELECT COUNT(*)::bigint AS sites_configured \
                       FROM site_parameters sp \
                      WHERE sp.parameter_id = p.id{sp_clause} \
                 ) cfg ON true \
                 LEFT JOIN LATERAL ( \
                     SELECT COUNT(*)::bigint AS reading_count, \
                            MIN(r.time) AS first_reading, \
                            MAX(r.time) AS last_reading, \
                            ARRAY_AGG(DISTINCT ds.source_system) \
                                FILTER (WHERE ds.source_system IS NOT NULL) AS source_systems, \
                            ARRAY_AGG(DISTINCT r.provenance ->> 'source') \
                                FILTER (WHERE r.provenance ->> 'source' IS NOT NULL) AS run_sources \
                       FROM readings r \
                       LEFT JOIN data_streams ds ON ds.id = r.stream_id \
                      WHERE r.parameter_id = p.id{site_clause} \
                 ) obs ON true \
                 WHERE p.id = ANY($1) \
                 ORDER BY p.code"
            ),
            binds,
        ))
        .await?;

    let mut coverage = Vec::with_capacity(rows.len());
    for r in &rows {
        coverage.push(SlotCoverage {
            parameter_id: r.try_get("", "parameter_id")?,
            parameter_code: r.try_get("", "code")?,
            sites_configured: r.try_get("", "sites_configured")?,
            reading_count: r.try_get("", "reading_count")?,
            first_reading: r
                .try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "first_reading")?
                .map(|t| t.with_timezone(&Utc)),
            last_reading: r
                .try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "last_reading")?
                .map(|t| t.with_timezone(&Utc)),
            source_systems: r
                .try_get::<Vec<String>>("", "source_systems")
                .unwrap_or_default(),
            run_sources: r
                .try_get::<Vec<String>>("", "run_sources")
                .unwrap_or_default(),
        });
    }
    Ok(coverage)
}

/// The calculations a set of parameters feeds, and where each calculation's data lives.
#[utoipa::path(
    get,
    path = "/api/calculations/closure",
    params(ClosureQuery),
    responses(
        (status = 200, description = "Calculations fed, with optional slot coverage", body = ClosureResponse),
        (status = 400, description = "Invalid query parameters"),
    ),
    tag = "tools"
)]
pub async fn get_calculation_closure(
    State(state): State<AppState>,
    Query(query): Query<ClosureQuery>,
) -> AppResult<Response> {
    let parameter_ids = parse_ids(query.parameter_ids.as_deref())?;
    let calculations = calculations_fed_by(&state.db, &parameter_ids).await?;

    // Coverage covers every slot an active calculation touches, not only the ones asked about: an
    // admin's question is "is this calculation wired up anywhere", which the named set cannot answer.
    let coverage = if query.include_coverage {
        let slots = calculation_slots(&state).await?;
        coverage_for(&state, &slots, query.site_id).await?
    } else {
        Vec::new()
    };

    Ok(Json(ClosureResponse {
        calculations,
        coverage,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::parse_ids;

    #[test]
    fn an_empty_or_absent_list_is_no_parameters_rather_than_an_error() {
        assert!(parse_ids(None).expect("absent").is_empty());
        assert!(parse_ids(Some("")).expect("empty").is_empty());
        assert!(parse_ids(Some(" , ")).expect("separators only").is_empty());
    }

    #[test]
    fn a_value_that_is_not_a_uuid_is_refused_rather_than_dropped() {
        assert!(parse_ids(Some("not-a-uuid")).is_err());
    }
}
