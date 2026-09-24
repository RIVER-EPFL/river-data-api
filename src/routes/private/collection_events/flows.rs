//! The reactive hook: a write that lands or changes a served value at a visit enqueues that
//! visit's recompute, so a calculation reading the value runs without anyone pressing a button.
//!
//! Visit-scoped and write-triggered, never timed or global (ADR 0007). A `portal_sync` visit is
//! recomputed like any other (Q259), the chain's own save never re-enqueues, and a visit none of
//! the enabled calculations read is left alone. One queued job per visit coalesces a burst of cell saves; the
//! claim releases the job's dedupe key, so a change landing during a run yields one follow-up.

use sea_orm::sea_query::{
    Alias, Condition, Expr, JoinType, PostgresQueryBuilder, Query as SeaQuery,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, QueryFilter, QuerySelect,
    Statement,
};
use uuid::Uuid;

use super::service::dedupe_key;
use crate::error::AppResult;
use crate::routes::private::collection_events::models as events;
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::readings::models as readings;
use crate::routes::private::reprocessing_jobs::service as jobs;
use crate::routes::private::tools::service as tool_service;

/// A visit a write touched, with the parameters it touched there.
#[derive(Debug, Clone, FromQueryResult)]
pub struct TouchedEvent {
    pub id: Uuid,
    pub site_id: Uuid,
    pub source: String,
    pub parameter_ids: Vec<Uuid>,
}

/// Who is writing. The chain's own save is the recompute; it never asks for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    Person,
    Chain,
}

/// A `readings` column as the visit lookup, the event attachment and the sample materialiser all
/// name it: the reading row is `r` in each of their statements, so a caller selecting rows for one
/// of them writes its predicate over this.
#[must_use]
pub fn row(column: readings::Column) -> Expr {
    Expr::col((Alias::new("r"), column))
}

/// The same column tested three-valued: `IS TRUE`, or `IS NOT TRUE` (which a NULL satisfies).
/// sea-query has no operator for either, and `= TRUE` is not the same test.
#[must_use]
pub fn row_is_true(column: readings::Column, expected: bool) -> Expr {
    let not = if expected { "" } else { "NOT " };
    let name = sea_orm::Iden::to_string(&column);
    Expr::cust(format!(r#""r"."{name}" IS {not}TRUE"#))
}

/// The visits whose readings `rows` selects, with the parameters touched at each.
pub async fn touched_events<C: ConnectionTrait>(
    conn: &C,
    rows: Condition,
) -> AppResult<Vec<TouchedEvent>> {
    let r = Alias::new("r");
    let ds = Alias::new("ds");
    let ce = Alias::new("ce");
    let (sql, values) = SeaQuery::select()
        .column((ce.clone(), events::Column::Id))
        .column((ce.clone(), events::Column::SiteId))
        .column((ce.clone(), events::Column::Source))
        .expr_as(
            Expr::cust("array_agg(DISTINCT r.parameter_id)"),
            Alias::new("parameter_ids"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds.clone(), data_streams::Column::Id))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .join_as(
            JoinType::InnerJoin,
            events::Entity,
            ce.clone(),
            Expr::col((ce.clone(), events::Column::Id))
                .equals((r.clone(), readings::Column::CollectionEventId)),
        )
        .cond_where(rows)
        .add_group_by([
            Expr::col((ce.clone(), events::Column::Id)),
            Expr::col((ce.clone(), events::Column::SiteId)),
            Expr::col((ce.clone(), events::Column::Source)),
        ])
        .take()
        .build(PostgresQueryBuilder);
    Ok(
        TouchedEvent::find_by_statement(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .all(conn)
        .await?,
    )
}

/// The same visits, from `(collection_event_id, parameter_id)` pairs a bulk write already knows.
/// A sweep that rewrites values in one statement has the pairs and no predicate to hand back, so
/// this is the other door into [`enqueue_for`].
pub async fn events_from_pairs<C: ConnectionTrait>(
    conn: &C,
    pairs: &[(Uuid, Uuid)],
) -> AppResult<Vec<TouchedEvent>> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_event: std::collections::HashMap<Uuid, Vec<Uuid>> = std::collections::HashMap::new();
    for (event_id, parameter_id) in pairs {
        by_event.entry(*event_id).or_default().push(*parameter_id);
    }
    let ids: Vec<Uuid> = by_event.keys().copied().collect();
    let rows = super::Entity::find()
        .filter(super::Column::Id.is_in(ids))
        .all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| TouchedEvent {
            parameter_ids: by_event.remove(&r.id).unwrap_or_default(),
            source: r.source,
            site_id: r.site_id,
            id: r.id,
        })
        .collect())
}

/// Enqueue `event_recompute` for every touched visit an enabled calculation reads. Returns the
/// ids of the jobs this call queued; a visit with a job already queued adds none.
pub async fn enqueue_for<C: ConnectionTrait>(
    db: &C,
    events: &[TouchedEvent],
    actor: &str,
    writer: Writer,
) -> AppResult<Vec<Uuid>> {
    if writer == Writer::Chain {
        return Ok(Vec::new());
    }
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let mut all_parameters: Vec<Uuid> = events
        .iter()
        .flat_map(|e| e.parameter_ids.iter().copied())
        .collect();
    all_parameters.sort_unstable();
    all_parameters.dedup();
    let fed = tool_service::calculations_fed_by(db, &all_parameters).await?;
    let mut queued = Vec::new();
    for event in events {
        let read_here = fed.iter().any(|c| {
            c.reads
                .iter()
                .any(|p| event.parameter_ids.contains(&p.parameter_id))
        });
        if !read_here {
            continue;
        }
        let key = dedupe_key(event.id);
        if let Some(id) = jobs::enqueue(
            db,
            "event_recompute",
            None,
            Some(event.id),
            &serde_json::json!({
                "collection_event_id": event.id,
                "site_id": event.site_id,
                "actor": actor,
            }),
            Some(&key),
        )
        .await?
        {
            queued.push(id);
        }
    }
    Ok(queued)
}

/// Enqueue the recompute of the visit an output slot sits at, the way a returned slot catches up
/// with the inputs that moved while it was detached. Returns the job queued, if any.
pub async fn enqueue_at_slot<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
    actor: &str,
) -> AppResult<Option<Uuid>> {
    let event_id: Option<Option<Uuid>> = readings::Entity::find()
        .select_only()
        .column(readings::Column::CollectionEventId)
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.eq(parameter_id))
        .filter(readings::Column::Time.eq(at))
        .filter(readings::Column::CollectionEventId.is_not_null())
        .into_tuple()
        .one(db)
        .await?;
    let Some(event_id) = event_id.flatten() else {
        return Ok(None);
    };
    let Some(event) = events::Entity::find_by_id(event_id).one(db).await? else {
        return Ok(None);
    };
    Ok(jobs::enqueue(
        db,
        "event_recompute",
        None,
        Some(event.id),
        &serde_json::json!({
            "collection_event_id": event.id,
            "site_id": event.site_id,
            "actor": actor,
        }),
        Some(&dedupe_key(event.id)),
    )
    .await?)
}
