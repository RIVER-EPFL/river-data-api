//! Collection event queries: the CRUD guard, the attach helper every spot write path lands
//! through, the SQL fragments the visit lists are built from, and the recompute state a visit is
//! listed with.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait, IntoTableRef, JoinType, OnConflict, PostgresQueryBuilder,
    Query as SeaQuery,
};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult,
    PaginatorTrait, QueryFilter, Statement, TransactionTrait,
};
use uuid::Uuid;

use super::models::{CollectionEvent, StagedEvent};
use crate::common::bulk_write;
use crate::common::paging::Window;
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::readings::models as readings;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;

pub(super) const MAX_PAGE_SIZE: u64 = 200;

pub struct CollectionEventOperations;

/// How many readings the event holds. The FK is `ON DELETE SET NULL`, so a delete would leave
/// them attached to no visit with no route to re-attach them.
async fn attached_readings<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<i64, ApiError> {
    let n = readings::Entity::find()
        .filter(readings::Column::CollectionEventId.eq(id))
        .count(db)
        .await
        .map_err(ApiError::database)?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

impl CRUDOperations for CollectionEventOperations {
    type Resource = CollectionEvent;

    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        let n = attached_readings(db, id).await?;
        if n > 0 {
            return Err(ApiError::conflict(format!(
                "Collection event {id} holds {n} reading{} and cannot be deleted: the readings \
                 would be detached from every visit.",
                if n == 1 { "" } else { "s" }
            )));
        }
        Ok(())
    }
}

/// How the event came to exist, decided by the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Manual,
    PortalSync,
    /// Derive per group from the streams feeding it: a stream this system did not create itself
    /// was registered by a sync service. Used by backfills that span sources.
    ByStreamOrigin,
}

/// `bool_or` SQL deciding whether any stream feeding a group was registered by a sync service:
/// a stream this system did not create itself. Shared with the sample backfill, which uses the
/// same distinction to decide whether a group's readings arrived as declared collections.
pub fn any_sync_origin_sql() -> String {
    "bool_or(ds.source_system NOT IN ('api', 'grab_sample', 'csv', 'csv_import'))".to_string()
}

impl EventSource {
    fn sql(self) -> String {
        match self {
            Self::Manual => "'manual'".to_string(),
            Self::PortalSync => "'portal_sync'".to_string(),
            Self::ByStreamOrigin => format!(
                "CASE WHEN {} THEN 'portal_sync' ELSE 'manual' END",
                any_sync_origin_sql()
            ),
        }
    }
}

/// The aliases the attach statements and the predicates they are given share.
fn r() -> Alias {
    Alias::new("r")
}

fn ds() -> Alias {
    Alias::new("ds")
}

fn ce() -> Alias {
    Alias::new("ce")
}

/// Find-or-create the `collection_events` rows for the attributed spot readings a predicate
/// selects, then stamp `collection_event_id` onto them.
///
/// `row_predicate` is SQL over the aliases `r` (`readings`) and `ds` (`data_streams`) with the
/// given binds, exactly as `sample_groups::materialise_samples` takes it. The stamping UPDATE can
/// reach compressed chunks, so callers run inside a `bulk_write::guarded` transaction when the
/// window can be historical.
pub async fn attach_collection_events<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    binds: Vec<sea_orm::Value>,
    source: EventSource,
) -> AppResult<()> {
    let attributed_spot = |predicate: String, binds: Vec<sea_orm::Value>| {
        Condition::all()
            .add(Expr::cust_with_values(predicate, binds))
            .add(Expr::col((r(), readings::Column::CollectionEventId)).is_null())
            .add(Expr::col((r(), readings::Column::SiteId)).is_not_null())
            .add(ExprTrait::eq(
                Expr::col((r(), readings::Column::MeasurementType)),
                "spot",
            ))
    };

    let mut rows = SeaQuery::select();
    rows.column((r(), readings::Column::SiteId))
        .column((r(), readings::Column::Time))
        .expr(Expr::cust(source.sql()))
        .from_as(readings::Entity, r())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds(),
            Expr::col((r(), readings::Column::StreamId)).equals((ds(), data_streams::Column::Id)),
        )
        .cond_where(attributed_spot(row_predicate.to_string(), binds.clone()))
        .add_group_by([
            Expr::col((r(), readings::Column::SiteId)),
            Expr::col((r(), readings::Column::Time)),
        ]);
    let mut insert = SeaQuery::insert();
    insert
        .into_table(super::Entity)
        .columns([
            super::Column::SiteId,
            super::Column::CollectedAt,
            super::Column::Source,
        ])
        .on_conflict(
            OnConflict::columns([super::Column::SiteId, super::Column::CollectedAt])
                .do_nothing()
                .to_owned(),
        );
    insert
        .select_from(rows)
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let (sql, values) = insert.build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    let mut stamp = SeaQuery::update();
    stamp
        .table(IntoTableRef::into_table_ref(readings::Entity).alias(r()))
        .value(
            readings::Column::CollectionEventId,
            Expr::col((ce(), super::Column::Id)),
        )
        .from(IntoTableRef::into_table_ref(data_streams::Entity).alias(ds()))
        .from(IntoTableRef::into_table_ref(super::Entity).alias(ce()))
        .cond_where(
            attributed_spot(row_predicate.to_string(), binds)
                .add(
                    Expr::col((r(), readings::Column::StreamId))
                        .equals((ds(), data_streams::Column::Id)),
                )
                .add(
                    Expr::col((ce(), super::Column::SiteId))
                        .equals((r(), readings::Column::SiteId)),
                )
                .add(
                    Expr::col((ce(), super::Column::CollectedAt))
                        .equals((r(), readings::Column::Time)),
                ),
        );
    let (sql, values) = stamp.build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    Ok(())
}

/// The per-visit counts, computed the same way on the site grid and the cross-site list. A
/// hold is keyed on the slot (an event-audit finding) or on the stream that raised it, so the
/// stream's pairing resolves the site.
/// Find or create the visit at `(site_id, collected_at)`: the portal's New Entry, made
/// idempotent. A visit already standing is returned as it is, `created` false, so two tools
/// entering the same visit land on one row instead of racing the unique key.
pub async fn stage_visit<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    collected_at: DateTime<Utc>,
    actor: &str,
    notes: Option<&str>,
) -> AppResult<StagedEvent> {
    let collected_at = sea_orm::prelude::DateTimeWithTimeZone::from(collected_at);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "WITH staged AS (
                 INSERT INTO collection_events (site_id, collected_at, source, created_by, notes)
                 VALUES ($1, $2, 'manual', $3, $4)
                 ON CONFLICT (site_id, collected_at) DO NOTHING
                 RETURNING id, site_id, collected_at, source, created_by, notes, true AS created
             )
             SELECT * FROM staged
             UNION ALL
             SELECT id, site_id, collected_at, source, created_by, notes, false AS created
             FROM collection_events
             WHERE site_id = $1 AND collected_at = $2 AND NOT EXISTS (SELECT 1 FROM staged)",
            vec![
                site_id.into(),
                collected_at.into(),
                actor.into(),
                notes.into(),
            ],
        ))
        .await?
        .ok_or_else(|| AppError::Internal("Staging returned no visit".to_string()))?;
    Ok(StagedEvent::from_query_result(&row, "")?)
}

/// The ids among `site_ids` that name no site, in the order given.
pub async fn missing_sites<C: ConnectionTrait>(
    conn: &C,
    site_ids: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    use crate::routes::private::sites::{Column, Entity};
    let found: Vec<Uuid> = Entity::find()
        .filter(Column::Id.is_in(site_ids.iter().copied()))
        .all(conn)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();
    Ok(site_ids
        .iter()
        .filter(|id| !found.contains(id))
        .copied()
        .collect())
}

pub(super) fn visit_count_columns() -> String {
    let filled = SeaQuery::select()
        .expr(Expr::col((r(), readings::Column::ParameterId)).count_distinct())
        .from_as(readings::Entity, r())
        .and_where(
            Expr::col((r(), readings::Column::CollectionEventId)).equals((ce(), super::Column::Id)),
        )
        .and_where(Expr::col((r(), readings::Column::WithdrawnAt)).is_null())
        .and_where(Expr::cust("r.is_flagged IS NOT TRUE"))
        .and_where(Expr::col((r(), readings::Column::ParameterId)).is_not_null())
        .to_owned()
        .to_string(PostgresQueryBuilder);
    format!(
        "({filled}) AS filled, \
         (SELECT COUNT(*) FROM replicate_audit_holds h \
           LEFT JOIN data_streams ds ON ds.id = h.stream_id \
           LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
           WHERE h.group_time = ce.collected_at AND h.status = '{pending}' \
             AND COALESCE(h.site_id, sp.site_id) = ce.site_id) AS findings_open",
        pending = HoldStatus::Pending.as_str()
    )
}

/// Paging is opt-in: a caller naming neither `page` nor `page_size` gets every row.
pub(super) fn paging(page: Option<u64>, page_size: Option<u64>) -> Option<Window> {
    if page.is_none() && page_size.is_none() {
        return None;
    }
    Some(Window::from_page(
        page,
        page_size,
        MAX_PAGE_SIZE,
        MAX_PAGE_SIZE,
    ))
}

pub(super) fn range_clause(
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    binds: &mut Vec<sea_orm::Value>,
) -> String {
    let mut range = String::new();
    if let Some(start) = start {
        binds.push(start.into());
        range.push_str(&format!(" AND ce.collected_at >= ${}", binds.len()));
    }
    if let Some(end) = end {
        binds.push(end.into());
        range.push_str(&format!(" AND ce.collected_at <= ${}", binds.len()));
    }
    range
}

pub(super) fn limit_clause(paging: Option<Window>, binds: &mut Vec<sea_orm::Value>) -> String {
    let Some(window) = paging else {
        return String::new();
    };
    binds.push((window.limit as i64).into());
    let limit_ref = binds.len();
    binds.push((window.offset as i64).into());
    format!(" LIMIT ${limit_ref} OFFSET ${}", binds.len())
}

/// The `ORDER BY` a sort name resolves to; the secondary key keeps ties stable.
pub(super) fn visit_list_order(sort: Option<&str>, order: Option<&str>) -> AppResult<String> {
    let direction = match order.unwrap_or("desc") {
        "asc" => "ASC",
        "desc" => "DESC",
        other => {
            return Err(AppError::BadRequest(format!(
                "order must be asc or desc, not {other}"
            )));
        }
    };
    let column = match sort.unwrap_or("collected_at") {
        "collected_at" => "ce.collected_at",
        "parameters_filled" => "filled",
        "findings_open" => "findings_open",
        "site_name" => "s.name",
        other => {
            return Err(AppError::BadRequest(format!(
                "sort must be collected_at, parameters_filled, findings_open or site_name, \
                 not {other}"
            )));
        }
    };
    Ok(format!("{column} {direction}, ce.collected_at DESC, ce.id"))
}

#[derive(FromQueryResult)]
struct LatestJobRow {
    event_id: String,
    status: String,
}

#[derive(FromQueryResult)]
struct EventIdRow {
    id: Uuid,
}

#[must_use]
pub fn dedupe_key(event_id: Uuid) -> String {
    format!("event_recompute:{event_id}")
}

/// The visit's recompute state from what its latest job and its open findings say: an active
/// job is the state whatever the findings (it is being repaired), a failed job outranks a stale
/// finding (the repair itself needs attention), a stale finding outranks nothing else.
#[must_use]
pub fn visit_status(latest_job_status: Option<&str>, has_stale_finding: bool) -> &'static str {
    match latest_job_status {
        Some("queued" | "pending" | "retrying") => "queued",
        Some("running") => "running",
        Some("failed") => "failed",
        _ if has_stale_finding => "stale",
        _ => "current",
    }
}

/// The recompute state of each visit: `queued` | `running` | `failed` from its latest
/// `event_recompute` job, else `stale` when an open stale-output or skipped-step finding names
/// it, else `current`.
pub async fn status_for(
    db: &DatabaseConnection,
    event_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, String>> {
    let mut out: HashMap<Uuid, String> = HashMap::new();
    if event_ids.is_empty() {
        return Ok(out);
    }
    let ids: Vec<String> = event_ids.iter().map(ToString::to_string).collect();
    let jobs = LatestJobRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT DISTINCT ON (params ->> 'collection_event_id')
                    params ->> 'collection_event_id' AS event_id, status
             FROM reprocessing_jobs
             WHERE trigger_type = 'event_recompute'
               AND params ->> 'collection_event_id' = ANY($1)
             ORDER BY params ->> 'collection_event_id', created_at DESC",
        [ids.into()],
    ))
    .all(db)
    .await?;
    let mut latest_job: HashMap<Uuid, String> = HashMap::new();
    for row in jobs {
        // `params ->> ...` is text, so the id is parsed rather than decoded; a row whose params
        // carry something that is not a uuid belongs to no visit here.
        let Ok(id) = row.event_id.parse::<Uuid>() else {
            continue;
        };
        latest_job.insert(id, row.status);
    }
    let stale = EventIdRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT DISTINCT ce.id
                 FROM replicate_audit_holds h
                 JOIN collection_events ce
                   ON ce.site_id = h.site_id AND ce.collected_at = h.group_time
                 WHERE h.kind IN {kinds}
                   AND h.status = '{pending}' AND h.stream_id IS NULL
                   AND ce.id = ANY($1)",
            pending = HoldStatus::Pending.as_str(),
            kinds = HoldKind::sql_list(&[HoldKind::StaleOutput, HoldKind::SkippedOutput])
        ),
        [event_ids.to_vec().into()],
    ))
    .all(db)
    .await?;
    let stale_events: std::collections::HashSet<Uuid> = stale.into_iter().map(|r| r.id).collect();
    for id in event_ids {
        out.insert(
            *id,
            visit_status(
                latest_job.get(id).map(String::as_str),
                stale_events.contains(id),
            )
            .to_string(),
        );
    }
    Ok(out)
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
