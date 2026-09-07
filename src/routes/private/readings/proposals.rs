//! Proposed corrections: a value the source changed after river-data stored it (Q84).
//!
//! New values admit on sync as they always did. A stored value the source has since moved is
//! recorded here and is not written, until a person accepts or rejects it. A decision is recorded
//! against the exact number decided on, so the portal's full re-assert every cycle re-proposes
//! nothing; a different number at source replaces the row with a fresh proposal.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// One proposed correction, as the review surface reads it.
#[derive(Debug, Serialize, ToSchema)]
pub struct Proposal {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub site_id: Option<Uuid>,
    pub site_name: Option<String>,
    pub parameter_id: Option<Uuid>,
    pub parameter_code: Option<String>,
    pub time: DateTime<Utc>,
    pub replicate_index: i16,
    pub stored_raw_value: f64,
    pub proposed_raw_value: f64,
    pub stored_standard_curve_id: Option<Uuid>,
    pub proposed_standard_curve_id: Option<Uuid>,
    pub status: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
}

/// Record what the source now asserts at a key whose stored value differs.
///
/// Returns whether this pass raised a *new* proposal, which is what stops the pass counting as
/// clean: a decision already taken on this exact number stands, and only a number nobody has seen
/// resets the row to `pending`.
pub async fn propose<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    key: (DateTime<Utc>, i16),
    proposed: (f64, Option<Uuid>),
    stored: (f64, Option<Uuid>),
) -> AppResult<bool> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            // A row whose proposed value is unchanged keeps its status and its decision, and only
            // its `last_seen_at` moves: the source is still asserting the number that was ruled on.
            r"INSERT INTO reading_change_proposals
                  (stream_id, time, replicate_index, proposed_raw_value, proposed_standard_curve_id,
                   stored_raw_value, stored_standard_curve_id)
              VALUES ($1, $2, $3, $4, $5, $6, $7)
              ON CONFLICT (stream_id, time, replicate_index) DO UPDATE
                 SET last_seen_at = now(),
                     stored_raw_value = EXCLUDED.stored_raw_value,
                     stored_standard_curve_id = EXCLUDED.stored_standard_curve_id,
                     proposed_raw_value = EXCLUDED.proposed_raw_value,
                     proposed_standard_curve_id = EXCLUDED.proposed_standard_curve_id,
                     status = CASE
                         WHEN reading_change_proposals.proposed_raw_value IS DISTINCT FROM EXCLUDED.proposed_raw_value
                           OR reading_change_proposals.proposed_standard_curve_id IS DISTINCT FROM EXCLUDED.proposed_standard_curve_id
                         THEN 'pending' ELSE reading_change_proposals.status END,
                     decided_by = CASE
                         WHEN reading_change_proposals.proposed_raw_value IS DISTINCT FROM EXCLUDED.proposed_raw_value
                           OR reading_change_proposals.proposed_standard_curve_id IS DISTINCT FROM EXCLUDED.proposed_standard_curve_id
                         THEN NULL ELSE reading_change_proposals.decided_by END,
                     decided_at = CASE
                         WHEN reading_change_proposals.proposed_raw_value IS DISTINCT FROM EXCLUDED.proposed_raw_value
                           OR reading_change_proposals.proposed_standard_curve_id IS DISTINCT FROM EXCLUDED.proposed_standard_curve_id
                         THEN NULL ELSE reading_change_proposals.decided_at END
              RETURNING status = 'pending' AND decided_at IS NULL AS awaiting",
            [
                stream_id.into(),
                key.0.into(),
                key.1.into(),
                proposed.0.into(),
                proposed.1.into(),
                stored.0.into(),
                stored.1.into(),
            ],
        ))
        .await?;
    Ok(row
        .map(|r| r.try_get::<bool>("", "awaiting"))
        .transpose()?
        .unwrap_or(false))
}

const SELECT: &str = r"SELECT p.id, p.stream_id, ds.source_system, ds.source_key,
       sp.site_id, s.name AS site_name, sp.parameter_id, par.code AS parameter_code,
       p.time, p.replicate_index, p.stored_raw_value, p.proposed_raw_value,
       p.stored_standard_curve_id, p.proposed_standard_curve_id,
       p.status, p.first_seen_at, p.last_seen_at, p.decided_by, p.decided_at
  FROM reading_change_proposals p
  JOIN data_streams ds ON ds.id = p.stream_id
  LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
  LEFT JOIN sites s ON s.id = sp.site_id
  LEFT JOIN parameters par ON par.id = sp.parameter_id";

fn read(row: &sea_orm::QueryResult) -> Result<Proposal, sea_orm::DbErr> {
    Ok(Proposal {
        id: row.try_get("", "id")?,
        stream_id: row.try_get("", "stream_id")?,
        source_system: row.try_get("", "source_system")?,
        source_key: row.try_get("", "source_key")?,
        site_id: row.try_get("", "site_id")?,
        site_name: row.try_get("", "site_name")?,
        parameter_id: row.try_get("", "parameter_id")?,
        parameter_code: row.try_get("", "parameter_code")?,
        time: row
            .try_get::<DateTime<chrono::FixedOffset>>("", "time")?
            .with_timezone(&Utc),
        replicate_index: row.try_get("", "replicate_index")?,
        stored_raw_value: row.try_get("", "stored_raw_value")?,
        proposed_raw_value: row.try_get("", "proposed_raw_value")?,
        stored_standard_curve_id: row.try_get("", "stored_standard_curve_id")?,
        proposed_standard_curve_id: row.try_get("", "proposed_standard_curve_id")?,
        status: row.try_get("", "status")?,
        first_seen_at: row
            .try_get::<DateTime<chrono::FixedOffset>>("", "first_seen_at")?
            .with_timezone(&Utc),
        last_seen_at: row
            .try_get::<DateTime<chrono::FixedOffset>>("", "last_seen_at")?
            .with_timezone(&Utc),
        decided_by: row.try_get("", "decided_by")?,
        decided_at: row
            .try_get::<Option<DateTime<chrono::FixedOffset>>>("", "decided_at")?
            .map(|t| t.with_timezone(&Utc)),
    })
}

/// Every proposal matching the filter, newest source assertion first.
pub async fn list(
    db: &DatabaseConnection,
    status: Option<&str>,
    stream_id: Option<Uuid>,
    projects: Option<sea_orm::Value>,
) -> AppResult<Vec<Proposal>> {
    let mut sql = SELECT.to_string();
    let mut binds: Vec<sea_orm::Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if let Some(status) = status {
        binds.push(status.into());
        clauses.push(format!("p.status = ${}", binds.len()));
    }
    if let Some(stream_id) = stream_id {
        binds.push(stream_id.into());
        clauses.push(format!("p.stream_id = ${}", binds.len()));
    }
    if let Some(projects) = projects {
        // A scoped caller sees a proposal only where the pairing places it in one of its projects;
        // an unpaired stream belongs to no project and is not theirs to rule on.
        binds.push(projects);
        clauses.push(format!("s.project_id = ANY(${})", binds.len()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY p.last_seen_at DESC, p.time DESC LIMIT 1000");
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            binds,
        ))
        .await?;
    rows.iter().map(|r| Ok(read(r)?)).collect()
}

/// What a decision did, per proposal.
#[derive(Debug, Serialize, ToSchema)]
pub struct DecideResponse {
    pub accepted: usize,
    pub rejected: usize,
    /// Ids that were not decided, each with why.
    pub refused: Vec<(Uuid, String)>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DecideRequest {
    pub ids: Vec<Uuid>,
    /// `accept` writes the proposed value as a value correction; `reject` records the refusal
    /// against this exact source value.
    pub decision: String,
    pub reason: Option<String>,
}

struct Pending {
    id: Uuid,
    stream_id: Uuid,
    time: DateTime<Utc>,
    replicate_index: i16,
    proposed_raw_value: f64,
    proposed_standard_curve_id: Option<Uuid>,
    stored_standard_curve_id: Option<Uuid>,
}

/// The proposals a caller may decide, out of the ids they named. A restricted caller reaches only
/// what their projects hold, so an id outside them is simply not found and comes back refused: the
/// same confinement the listing applies, at the write.
async fn load_undecided<C: ConnectionTrait>(
    conn: &C,
    ids: &[Uuid],
    projects: Option<sea_orm::Value>,
) -> AppResult<Vec<Pending>> {
    let mut binds: Vec<sea_orm::Value> = vec![ids.to_vec().into()];
    let scope = if let Some(projects) = projects {
        binds.push(projects);
        format!(
            " AND EXISTS (SELECT 1 FROM data_streams ds \
                 JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
                 JOIN sites s ON s.id = sp.site_id \
                WHERE ds.id = p.stream_id AND s.project_id = ANY(${}))",
            binds.len()
        )
    } else {
        String::new()
    };
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT p.id, p.stream_id, p.time, p.replicate_index, p.proposed_raw_value,
                        p.proposed_standard_curve_id, p.stored_standard_curve_id
                   FROM reading_change_proposals p
                  WHERE p.id = ANY($1) AND p.status <> 'accepted'{scope}
                  FOR UPDATE OF p"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(Pending {
                id: r.try_get("", "id")?,
                stream_id: r.try_get("", "stream_id")?,
                time: r
                    .try_get::<DateTime<chrono::FixedOffset>>("", "time")?
                    .with_timezone(&Utc),
                replicate_index: r.try_get("", "replicate_index")?,
                proposed_raw_value: r.try_get("", "proposed_raw_value")?,
                proposed_standard_curve_id: r.try_get("", "proposed_standard_curve_id")?,
                stored_standard_curve_id: r.try_get("", "stored_standard_curve_id")?,
            })
        })
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()
        .map_err(Into::into)
}

/// Accept or reject proposals. An accepted one is written through the curation record, so the
/// value carries who accepted it and that it arrived through this stream's source system; a
/// rejected one keeps its number, so the next pass re-proposes nothing.
pub async fn decide<C: ConnectionTrait>(
    conn: &C,
    ids: &[Uuid],
    accept: bool,
    actor: &str,
    reason: Option<&str>,
    projects: Option<sea_orm::Value>,
) -> AppResult<(DecideResponse, super::decisions::Recorded)> {
    let mut response = DecideResponse {
        accepted: 0,
        rejected: 0,
        refused: Vec::new(),
    };
    let pending = load_undecided(conn, ids, projects).await?;
    let found: Vec<Uuid> = pending.iter().map(|p| p.id).collect();
    for id in ids {
        if !found.contains(id) {
            response.refused.push((
                *id,
                "no undecided proposal with that id in your projects".to_string(),
            ));
        }
    }
    let mut written = super::decisions::Recorded::default();
    if accept {
        let mut by_stream: std::collections::HashMap<Uuid, Vec<&Pending>> =
            std::collections::HashMap::new();
        for p in &pending {
            by_stream.entry(p.stream_id).or_default().push(p);
        }
        for (stream_id, rows) in by_stream {
            let corrections: Vec<(DateTime<Utc>, i16, serde_json::Value)> = rows
                .iter()
                .map(|p| {
                    (
                        p.time,
                        p.replicate_index,
                        serde_json::json!({ "raw_value": p.proposed_raw_value }),
                    )
                })
                .collect();
            written.absorb(
                super::decisions::record_keyed(
                    conn,
                    super::decisions::Kind::ValueCorrection,
                    stream_id,
                    &corrections,
                    actor,
                    Some(reason.unwrap_or("accepted source correction")),
                    super::decisions::Origin::Sync,
                    super::decisions::Keyed::Changed,
                    None,
                    None,
                )
                .await?,
            );
            // A curve the source named travels with the value it produced: correcting one and
            // leaving the other would store a number no curve accounts for.
            let curves: Vec<(DateTime<Utc>, i16, serde_json::Value)> = rows
                .iter()
                .filter(|p| p.proposed_standard_curve_id != p.stored_standard_curve_id)
                .map(|p| {
                    (
                        p.time,
                        p.replicate_index,
                        serde_json::json!({ "standard_curve_id": p.proposed_standard_curve_id }),
                    )
                })
                .collect();
            super::decisions::record_keyed(
                conn,
                super::decisions::Kind::Curve,
                stream_id,
                &curves,
                actor,
                Some("accepted source correction"),
                super::decisions::Origin::Sync,
                super::decisions::Keyed::Changed,
                None,
                None,
            )
            .await?;
        }
        response.accepted = pending.len();
    } else {
        response.rejected = pending.len();
    }
    if !found.is_empty() {
        conn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE reading_change_proposals
                SET status = $2, decided_by = $3, decided_at = now()
              WHERE id = ANY($1)",
            [
                found.clone().into(),
                if accept { "accepted" } else { "rejected" }.into(),
                actor.into(),
            ],
        ))
        .await?;
    }
    Ok((response, written))
}

/// How many proposals are awaiting a decision, per source system. The notification's subject.
pub async fn pending_by_source(db: &DatabaseConnection) -> AppResult<Vec<(String, i64)>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.source_system AS source_system, count(*) AS n
               FROM reading_change_proposals p
               JOIN data_streams ds ON ds.id = p.stream_id
              WHERE p.status = 'pending'
              GROUP BY ds.source_system"
                .to_string(),
        ))
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("", "source_system")?, r.try_get("", "n")?)))
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()
        .map_err(Into::into)
}

/// The decision vocabulary, refused rather than defaulted: an unrecognised word is a bug in the
/// caller, not a licence to guess which way a value went.
pub fn parse_decision(decision: &str) -> AppResult<bool> {
    match decision {
        "accept" => Ok(true),
        "reject" => Ok(false),
        other => Err(AppError::BadRequest(format!(
            "decision must be 'accept' or 'reject', not '{other}'"
        ))),
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ListQuery {
    /// `pending` (the default view of the queue), `accepted` or `rejected`.
    pub status: Option<String>,
    pub stream_id: Option<Uuid>,
}

/// The proposed corrections a person has still to decide. Requires `manage_sensors`, the same
/// review layer the audit holds use.
#[utoipa::path(
    get,
    path = "/api/sync/change_proposals",
    params(ListQuery),
    responses((status = 200, description = "Proposed corrections", body = Vec<Proposal>)),
    tag = "sync"
)]
pub async fn list_proposals(
    axum::extract::State(state): axum::extract::State<crate::common::AppState>,
    crate::common::middleware::ProjectScope(scope): crate::common::middleware::ProjectScope,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> AppResult<axum::Json<Vec<Proposal>>> {
    let rows = list(
        &state.db,
        query.status.as_deref(),
        query.stream_id,
        scope.sql_project_array(),
    )
    .await?;
    Ok(axum::Json(rows))
}

/// Accept or reject proposed corrections. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/sync/change_proposals/decide",
    request_body = DecideRequest,
    responses(
        (status = 200, description = "What the decision did", body = DecideResponse),
        (status = 400, description = "An unrecognised decision"),
    ),
    tag = "sync"
)]
pub async fn decide_proposals(
    axum::extract::State(state): axum::extract::State<crate::common::AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    crate::common::middleware::ProjectScope(scope): crate::common::middleware::ProjectScope,
    axum::Json(req): axum::Json<DecideRequest>,
) -> AppResult<axum::Json<DecideResponse>> {
    let accept = parse_decision(&req.decision)?;
    let actor = crate::common::actor::label(&auth);
    let (response, written) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        decide(
            txn,
            &req.ids,
            accept,
            &actor,
            req.reason.as_deref(),
            scope.sql_project_array(),
        )
        .await
    })
    .await?;
    // An accepted correction rewrites a served value, so it takes the same tail every other
    // curation write takes: the rollups over the span it moved, the cache, and the visits whose
    // calculations read it.
    super::tail::run(
        &state,
        &super::tail::Written::new(written.rows)
            .over(written.span)
            .touching(written.touched_events.clone()),
        &ACCEPT_TAIL,
        &actor,
    )
    .await?;
    Ok(axum::Json(response))
}

/// The tail an accepted correction takes. Identical in shape to the flag routes' curation tail:
/// the value moved, so everything computed from it moves with it.
const ACCEPT_TAIL: super::tail::Axes = super::tail::Axes {
    cache: super::tail::Cache::All,
    refresh: super::tail::Refresh::Range { fatal: true },
    announce: false,
    reconcile_alarms: false,
    episodes: super::tail::Episodes::None,
    recompute_derived: true,
    writer: crate::routes::private::collection_events::recompute::Writer::Person,
};

#[cfg(test)]
mod tests {
    #[test]
    fn only_accept_and_reject_are_decisions() {
        assert!(super::parse_decision("accept").unwrap());
        assert!(!super::parse_decision("reject").unwrap());
        assert!(super::parse_decision("apply").is_err());
        assert!(super::parse_decision("").is_err());
    }
}
