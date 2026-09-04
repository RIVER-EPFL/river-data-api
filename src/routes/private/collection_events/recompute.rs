//! The reactive hook: a write that lands or changes a served value at a visit enqueues that
//! visit's recompute, so a calculation reading the value runs without anyone pressing a button.
//!
//! Visit-scoped and write-triggered, never timed or global (ADR 0007). A `portal_sync` visit is
//! never recomputed (Q41), the chain's own save never re-enqueues, and a visit none of the enabled
//! calculations read is left alone. One queued job per visit coalesces a burst of cell saves; the
//! claim releases the job's dedupe key, so a change landing during a run yields one follow-up.

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::error::AppResult;
use crate::routes::private::reprocessing_jobs::worker;
use crate::routes::private::tools::closure;

/// A visit a write touched, with the parameters it touched there.
#[derive(Debug, Clone)]
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

#[must_use]
pub fn dedupe_key(event_id: Uuid) -> String {
    format!("event_recompute:{event_id}")
}

/// The visits whose readings `row_predicate` selects, with the parameters touched at each.
/// The predicate is SQL over `r` (`readings`) and `ds` (`data_streams`), the same shape the
/// attach helper takes, so a write path passes the predicate it just attached with.
pub async fn touched_events<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<TouchedEvent>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT ce.id, ce.source, array_agg(DISTINCT r.parameter_id) AS parameter_ids
                 FROM readings r
                 JOIN data_streams ds ON ds.id = r.stream_id
                 JOIN collection_events ce ON ce.id = r.collection_event_id
                 WHERE {row_predicate}
                 GROUP BY ce.id, ce.source"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(TouchedEvent {
                id: r.try_get("", "id")?,
                source: r.try_get("", "source")?,
                parameter_ids: r.try_get("", "parameter_ids")?,
            })
        })
        .collect()
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
    let fed = closure::calculations_fed_by(db, &all_parameters).await?;
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
        if let Some(id) = worker::enqueue(
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
/// `event_recompute` job, else `stale` when an open stale-output finding names it, else
/// `current`.
pub async fn status_for(
    db: &DatabaseConnection,
    event_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, String>> {
    let mut out: HashMap<Uuid, String> = HashMap::new();
    if event_ids.is_empty() {
        return Ok(out);
    }
    let ids: Vec<String> = event_ids.iter().map(ToString::to_string).collect();
    let jobs = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (params ->> 'collection_event_id')
                    params ->> 'collection_event_id' AS event_id, status
             FROM reprocessing_jobs
             WHERE trigger_type = 'event_recompute'
               AND params ->> 'collection_event_id' = ANY($1)
             ORDER BY params ->> 'collection_event_id', created_at DESC",
            [ids.into()],
        ))
        .await?;
    let mut latest_job: HashMap<Uuid, String> = HashMap::new();
    for row in &jobs {
        let Ok(id) = row.try_get::<String>("", "event_id")?.parse::<Uuid>() else {
            continue;
        };
        latest_job.insert(id, row.try_get("", "status")?);
    }
    let stale = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ce.id
             FROM replicate_audit_holds h
             JOIN collection_events ce
               ON ce.site_id = h.site_id AND ce.collected_at = h.group_time
             WHERE h.kind = 'stale_output' AND h.status = 'pending' AND h.stream_id IS NULL
               AND ce.id = ANY($1)",
            [event_ids.to_vec().into()],
        ))
        .await?;
    let mut stale_events: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for row in &stale {
        stale_events.insert(row.try_get("", "id")?);
    }
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
mod tests {
    use super::{dedupe_key, visit_status};

    #[test]
    fn an_active_job_is_the_state_whatever_the_findings_say() {
        assert_eq!(visit_status(Some("queued"), true), "queued");
        assert_eq!(visit_status(Some("pending"), false), "queued");
        assert_eq!(visit_status(Some("retrying"), true), "queued");
        assert_eq!(visit_status(Some("running"), true), "running");
    }

    #[test]
    fn a_failed_repair_outranks_a_stale_finding_and_a_finished_one_defers_to_it() {
        assert_eq!(visit_status(Some("failed"), true), "failed");
        assert_eq!(visit_status(Some("failed"), false), "failed");
        assert_eq!(visit_status(Some("completed"), true), "stale");
        assert_eq!(visit_status(Some("cancelled"), true), "stale");
        assert_eq!(visit_status(Some("completed"), false), "current");
        assert_eq!(visit_status(None, false), "current");
        assert_eq!(visit_status(None, true), "stale");
    }

    #[test]
    fn the_dedupe_key_is_one_per_visit() {
        let id = uuid::Uuid::nil();
        assert_eq!(dedupe_key(id), format!("event_recompute:{id}"));
    }
}
