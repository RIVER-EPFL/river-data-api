//! The reactive hook: a write that lands or changes a served value at a visit enqueues that
//! visit's recompute, so a calculation reading the value runs without anyone pressing a button.
//!
//! Visit-scoped and write-triggered, never timed or global (ADR 0007). A `portal_sync` visit is
//! never recomputed (Q41), the chain's own save never re-enqueues, and a visit none of the enabled
//! calculations read is left alone. One queued job per visit coalesces a burst of cell saves; the
//! claim releases the job's dedupe key, so a change landing during a run yields one follow-up.

use sea_orm::sea_query::{
    Alias, Condition, Expr, JoinType, PostgresQueryBuilder, Query as SeaQuery,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, ExprTrait, FromQueryResult,
    QueryFilter, Statement,
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
    pub source: String,
    pub parameter_ids: Vec<Uuid>,
}

/// Who is writing. The chain's own save is the recompute; it never asks for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    Person,
    Chain,
}

/// A row selection still spelled as SQL over `r` (`readings`) and `ds` (`data_streams`), with the
/// values its `$n` placeholders bind. The write paths build these predicates as text today, so
/// this is how one is handed to [`touched_events`] until they build them.
#[must_use]
pub fn rows_matching(row_predicate: &str, binds: Vec<sea_orm::Value>) -> Condition {
    Condition::all().add(Expr::cust_with_values(row_predicate.to_string(), binds))
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
            id: r.id,
        })
        .collect())
}

/// Enqueue `event_recompute` for every touched visit an enabled calculation reads. Returns the
/// ids of the jobs this call queued; a visit with a job already queued adds none.
pub async fn enqueue_for(
    db: &DatabaseConnection,
    events: &[TouchedEvent],
    actor: &str,
    writer: Writer,
) -> AppResult<Vec<Uuid>> {
    if writer == Writer::Chain {
        return Ok(Vec::new());
    }
    let candidates: Vec<&TouchedEvent> = events
        .iter()
        .filter(|e| e.source != "portal_sync")
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut all_parameters: Vec<Uuid> = candidates
        .iter()
        .flat_map(|e| e.parameter_ids.iter().copied())
        .collect();
    all_parameters.sort_unstable();
    all_parameters.dedup();
    let fed = tool_service::calculations_fed_by(db, &all_parameters).await?;
    let mut queued = Vec::new();
    for event in candidates {
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
            &serde_json::json!({ "collection_event_id": event.id, "actor": actor }),
            Some(&key),
        )
        .await?
        {
            queued.push(id);
        }
    }
    Ok(queued)
}
