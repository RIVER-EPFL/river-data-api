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
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

use super::closure::{self, CalculationImpact};
use super::engine;

#[derive(Debug, Deserialize, IntoParams)]
pub struct ClosureQuery {
    /// Global parameter ids, comma-separated: the values whose consequences are being asked about.
    pub parameter_ids: Option<String>,
    /// A calibration whose consequences are being asked about, instead of a parameter list.
    pub calibration_id: Option<Uuid>,
    /// A site parameter row, instead of a parameter list.
    pub site_parameter_id: Option<Uuid>,
    /// One reading's stream, instead of a parameter list.
    pub stream_id: Option<Uuid>,
    /// A calculation by name: what its own outputs feed downstream.
    pub calculation: Option<String>,
    /// Confine the coverage counts to one site. Every site when omitted.
    pub site_id: Option<Uuid>,
    /// Include the per-slot coverage of every calculation input and output. Off by default: it is
    /// three aggregate queries and a closure asked for before a write does not need it.
    #[serde(default)]
    pub include_coverage: bool,
}

/// What the store holds for one parameter of a calculation: whether anyone configured the slot,
/// how much is there, and where it came from.
#[derive(Debug, Clone, Serialize, ToSchema, FromQueryResult)]
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
                "SELECT p.id AS parameter_id, p.code AS parameter_code, \
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

    let coverage = rows
        .iter()
        .map(|r| SlotCoverage::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?;
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
    let subject = closure_subject(&query)?;
    let calculations = closure::calculations_fed_by_subject(&state.db, &subject).await?;

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

/// Which subject the query names. One relation, four subjects (M126): the four are mutually
/// exclusive, because a closure answering two questions at once answers neither.
fn closure_subject(query: &ClosureQuery) -> AppResult<closure::Subject> {
    let named = [
        query.calibration_id.is_some(),
        query.site_parameter_id.is_some(),
        query.stream_id.is_some(),
        query.calculation.is_some(),
    ]
    .into_iter()
    .filter(|n| *n)
    .count();
    if named > 1 {
        return Err(AppError::BadRequest(
            "name one subject: calibration_id, site_parameter_id, stream_id or calculation"
                .to_string(),
        ));
    }
    if let Some(id) = query.calibration_id {
        return Ok(closure::Subject::Calibration(id));
    }
    if let Some(id) = query.site_parameter_id {
        return Ok(closure::Subject::Slot(id));
    }
    if let Some(stream_id) = query.stream_id {
        return Ok(closure::Subject::Reading {
            stream_id,
            replicate_index: None,
        });
    }
    if let Some(name) = &query.calculation {
        return Ok(closure::Subject::Calculation(name.clone()));
    }
    Ok(closure::Subject::Parameters(parse_ids(
        query.parameter_ids.as_deref(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::{ClosureQuery, closure_subject, parse_ids};
    use crate::routes::private::tools::closure::Subject;

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

    fn query() -> ClosureQuery {
        ClosureQuery {
            parameter_ids: None,
            calibration_id: None,
            site_parameter_id: None,
            stream_id: None,
            calculation: None,
            site_id: None,
            include_coverage: false,
        }
    }

    #[test]
    fn each_subject_is_recognised_from_the_field_that_names_it() {
        let id = uuid::Uuid::new_v4();
        assert!(matches!(
            closure_subject(&ClosureQuery { calibration_id: Some(id), ..query() }).unwrap(),
            Subject::Calibration(got) if got == id
        ));
        assert!(matches!(
            closure_subject(&ClosureQuery { site_parameter_id: Some(id), ..query() }).unwrap(),
            Subject::Slot(got) if got == id
        ));
        assert!(matches!(
            closure_subject(&ClosureQuery { stream_id: Some(id), ..query() }).unwrap(),
            Subject::Reading { stream_id, .. } if stream_id == id
        ));
        assert!(matches!(
            closure_subject(&ClosureQuery { calculation: Some("doc".into()), ..query() }).unwrap(),
            Subject::Calculation(name) if name == "doc"
        ));
    }

    #[test]
    fn no_subject_is_the_parameter_list_it_has_always_been() {
        let id = uuid::Uuid::new_v4();
        let q = ClosureQuery { parameter_ids: Some(id.to_string()), ..query() };
        assert!(matches!(closure_subject(&q).unwrap(), Subject::Parameters(ids) if ids == vec![id]));
        assert!(matches!(closure_subject(&query()).unwrap(), Subject::Parameters(ids) if ids.is_empty()));
    }

    #[test]
    fn two_subjects_at_once_are_refused_rather_than_ranked() {
        let q = ClosureQuery {
            calibration_id: Some(uuid::Uuid::new_v4()),
            stream_id: Some(uuid::Uuid::new_v4()),
            ..query()
        };
        assert!(closure_subject(&q).is_err());
    }
}
