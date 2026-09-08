//! Slot-level sd-estimator declaration outside the audit-hold flow.
//!
//! `sd_estimator` is excluded from CRUD update because changing the declaration must also
//! recompute the slot's stored samples; this endpoint is the one path, writing the column and
//! enqueueing the tracked `sd_estimator_retag` in the same breath, exactly as the audit
//! resolution's slot scope does.

use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareSdEstimatorRequest {
    /// 'sample' (divisor n-1), 'population' (divisor n), or null to clear the declaration.
    /// Clearing leaves the slot undeclared: new statistics fall back to sample recorded as
    /// 'default', and stored samples keep the estimator they were computed with.
    pub estimator: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeclareSdEstimatorResponse {
    pub site_parameter_id: Uuid,
    #[schema(required)]
    pub estimator: Option<String>,
    #[schema(required)]
    pub previous: Option<String>,
    /// Samples the retag will recompute; 0 when clearing or nothing disagrees.
    pub samples_affected: i64,
    /// The tracked `sd_estimator_retag` job, present when a recompute was enqueued.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub job_id: Option<Uuid>,
}

/// The slot an estimator declaration is written onto, as it stands before the write.
#[derive(FromQueryResult)]
struct SlotDeclaration {
    site_id: Uuid,
    parameter_id: Uuid,
    sd_estimator: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/site_parameters/{id}/declare_sd_estimator",
    request_body = DeclareSdEstimatorRequest,
    responses(
        (status = 200, body = DeclareSdEstimatorResponse),
        (status = 400, description = "Estimator is not 'sample', 'population' or null"),
        (status = 404, description = "No site parameter with this id"),
    ),
    tag = "site_parameters"
)]
pub async fn declare_sd_estimator(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<DeclareSdEstimatorRequest>,
) -> AppResult<Json<DeclareSdEstimatorResponse>> {
    let estimator = match payload.estimator.as_deref() {
        None | Some("sample" | "population") => payload.estimator.clone(),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "estimator must be 'sample', 'population' or null, not '{other}'"
            )));
        }
    };

    let (previous, affected) = state
        .db
        .transaction::<_, (Option<String>, i64), sea_orm::DbErr>(|txn| {
            let estimator = estimator.clone();
            Box::pin(async move {
                crate::common::actor::declare(txn).await?;
                let row = txn
                    .query_one_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT site_id, parameter_id, sd_estimator FROM site_parameters
                         WHERE id = $1 FOR UPDATE",
                        [id.into()],
                    ))
                    .await?
                    .ok_or_else(|| sea_orm::DbErr::RecordNotFound(id.to_string()))?;
                let SlotDeclaration {
                    site_id,
                    parameter_id,
                    sd_estimator: previous,
                } = SlotDeclaration::from_query_result(&row, "")?;

                txn.execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE site_parameters SET sd_estimator = $2 WHERE id = $1",
                    [id.into(), estimator.clone().into()],
                ))
                .await?;

                // Counted inside the transaction the declaration lands in, so the number
                // reported is the one the retag will act on. A cleared declaration recomputes
                // nothing: stored samples keep the estimator they were computed with.
                let affected = if let Some(est) = &estimator {
                    txn.query_one_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT COUNT(*)::bigint AS n FROM samples
                         WHERE site_id = $1 AND parameter_id = $2
                           AND sd_estimator IS DISTINCT FROM $3
                           AND sd_estimator_source <> 'sample'",
                        [site_id.into(), parameter_id.into(), est.clone().into()],
                    ))
                    .await?
                    .map_or(Ok(0_i64), |row| row.try_get::<i64>("", "n"))?
                } else {
                    0
                };
                Ok((previous, affected))
            })
        })
        .await
        .map_err(|e| match e {
            sea_orm::TransactionError::Transaction(sea_orm::DbErr::RecordNotFound(_)) => {
                AppError::NotFound(format!("site parameter {id} not found"))
            }
            sea_orm::TransactionError::Connection(db) => AppError::from(db),
            sea_orm::TransactionError::Transaction(db) => AppError::from(db),
        })?;

    let job_id = if let Some(est) = &estimator
        && affected > 0
    {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "sd_estimator_retag",
            None,
            None,
            &serde_json::json!({
                "estimator": est,
                "site_parameter_ids": [id],
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(DeclareSdEstimatorResponse {
        site_parameter_id: id,
        estimator,
        previous,
        samples_affected: affected,
        job_id,
    }))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetagSdEstimatorRequest {
    /// 'sample' (divisor n-1) or 'population' (divisor n). Every slot in scope must already
    /// declare it: the retag applies a declaration to the stored samples, it does not make one.
    pub estimator: String,
    #[serde(default)]
    pub site_parameter_ids: Vec<Uuid>,
    /// Streams reach their slot through their pairing; an unpaired stream reaches none.
    #[serde(default)]
    pub stream_ids: Vec<Uuid>,
    /// Inclusive bounds on `samples.collected_at`.
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// Also retag samples whose estimator a person chose for that one instant
    /// (`sd_estimator_source = 'sample'`). Off by default, as the declaration's own retag is.
    #[serde(default)]
    pub override_instants: bool,
    /// Count what the retag would touch and enqueue nothing. The declaration check is skipped,
    /// so a slot can be previewed under the divisor it is about to declare.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RetagSdEstimatorResponse {
    pub estimator: String,
    /// Samples the retag will recompute.
    pub samples_affected: i64,
    /// Samples in scope carrying an instant-chosen estimator that differs from the target:
    /// counted inside `samples_affected` with `override_instants`, skipped without.
    pub instant_decisions: i64,
    /// The tracked `sd_estimator_retag` job, present when something needed recomputing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub job_id: Option<Uuid>,
}

/// SQL selecting the slots the request names, by id or through a stream's pairing.
const SLOT_SCOPE: &str = "EXISTS (SELECT 1 FROM site_parameters sp \
     WHERE sp.site_id = s.site_id AND sp.parameter_id = s.parameter_id \
       AND (sp.id = ANY($2) \
            OR EXISTS (SELECT 1 FROM data_streams ds \
                       WHERE ds.id = ANY($3) AND ds.site_parameter_id = sp.id)))";

/// Bring stored samples into line with their slot's declared sd estimator, with the job's own
/// options exposed: a window, a stream scope, and `override_instants`.
/// How many samples a declaration change would recompute, by whether the instant declared for
/// itself.
#[derive(FromQueryResult)]
struct RetagCounts {
    slot_rows: i64,
    instant_rows: i64,
}

#[utoipa::path(
    post,
    path = "/api/actions/retag_sd_estimator",
    request_body = RetagSdEstimatorRequest,
    responses(
        (status = 200, body = RetagSdEstimatorResponse),
        (status = 400, description = "Unknown estimator, no slot or stream named, a window \
                                      that ends before it starts, or a slot in scope that does \
                                      not declare the estimator"),
    ),
    tag = "actions"
)]
pub async fn retag_sd_estimator(
    State(state): State<AppState>,
    Json(payload): Json<RetagSdEstimatorRequest>,
) -> AppResult<Json<RetagSdEstimatorResponse>> {
    use crate::routes::private::readings::sd_estimator;

    let estimator = sd_estimator::parse(&payload.estimator)?;
    if payload.site_parameter_ids.is_empty() && payload.stream_ids.is_empty() {
        return Err(AppError::BadRequest(
            "name at least one site_parameter_id or stream_id".to_string(),
        ));
    }
    if let (Some(start), Some(end)) = (payload.start, payload.end)
        && end < start
    {
        return Err(AppError::BadRequest(
            "the window ends before it starts".to_string(),
        ));
    }

    let db = &state.db;
    let undeclared = if payload.dry_run {
        Vec::new()
    } else {
        db.query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT st.name AS site_name, p.name AS parameter_name, sp.sd_estimator
             FROM site_parameters sp
             JOIN sites st ON st.id = sp.site_id
             JOIN parameters p ON p.id = sp.parameter_id
             WHERE (sp.id = ANY($1)
                    OR EXISTS (SELECT 1 FROM data_streams ds
                               WHERE ds.id = ANY($2) AND ds.site_parameter_id = sp.id))
               AND sp.sd_estimator IS DISTINCT FROM $3
             ORDER BY st.name, p.name",
            [
                payload.site_parameter_ids.clone().into(),
                payload.stream_ids.clone().into(),
                estimator.into(),
            ],
        ))
        .await?
    };
    if !undeclared.is_empty() {
        let named: Vec<String> = undeclared
            .iter()
            .map(|row| {
                let row = UndeclaredRow::from_query_result(row, "")?;
                Ok(format!(
                    "{} / {} ({})",
                    row.site_name,
                    row.parameter_name,
                    row.sd_estimator.as_deref().unwrap_or("not declared")
                ))
            })
            .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
        return Err(AppError::BadRequest(format!(
            "declare '{estimator}' on the slot first; it is not what {} declares",
            named.join(", ")
        )));
    }

    let mut binds: Vec<sea_orm::Value> = vec![
        estimator.into(),
        payload.site_parameter_ids.clone().into(),
        payload.stream_ids.clone().into(),
    ];
    let mut window = String::new();
    if let Some(start) = payload.start {
        binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(start).into());
        window.push_str(&format!(" AND s.collected_at >= ${}", binds.len()));
    }
    if let Some(end) = payload.end {
        binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(end).into());
        window.push_str(&format!(" AND s.collected_at <= ${}", binds.len()));
    }
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) FILTER (WHERE s.sd_estimator_source <> 'sample')::bigint AS slot_rows,
                        COUNT(*) FILTER (WHERE s.sd_estimator_source = 'sample')::bigint AS instant_rows
                 FROM samples s
                 WHERE {SLOT_SCOPE} AND s.sd_estimator IS DISTINCT FROM $1{window}"
            ),
            binds,
        ))
        .await?;
    let counts = row.map(|row| RetagCounts::from_query_result(&row, "")).transpose()?;
    let (slot_rows, instant_rows) =
        counts.map_or((0, 0), |c| (c.slot_rows, c.instant_rows));
    let affected = if payload.override_instants {
        slot_rows + instant_rows
    } else {
        slot_rows
    };

    let job_id = if affected > 0 && !payload.dry_run {
        let mut params = serde_json::json!({
            "estimator": estimator,
            "site_parameter_ids": payload.site_parameter_ids,
            "stream_ids": payload.stream_ids,
            "override_instants": payload.override_instants,
        });
        if let Some(start) = payload.start {
            params["start"] = start.to_rfc3339().into();
        }
        if let Some(end) = payload.end {
            params["end"] = end.to_rfc3339().into();
        }
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "sd_estimator_retag",
            None,
            None,
            &params,
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(RetagSdEstimatorResponse {
        estimator: estimator.to_string(),
        samples_affected: affected,
        instant_decisions: instant_rows,
        job_id,
    }))
}

/// One slot named in the refusal: the site, the parameter and whatever it declares, which is the
/// thing the caller has to change.
#[derive(sea_orm::FromQueryResult)]
struct UndeclaredRow {
    site_name: String,
    parameter_name: String,
    sd_estimator: Option<String>,
}
