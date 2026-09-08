//! What the generated CRUD cannot state about a schedule: which edits are legal, what an edit does
//! to the grid, and the trail it leaves.

use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement, TransactionTrait};

use super::job::{self, JobRegistry};
use super::schedule::{CatchupPolicy, OverlapPolicy};
use super::schedule_model::Schedule;

pub struct ScheduleOperations;

/// The registry a schedule is validated against: the on-demand jobs plus the recurring Services
/// with their cadence. Stateless and cheap, rebuilt per call. The config comes from the process's
/// own `AppState`; only its config is read, never its pool.
fn full_registry() -> JobRegistry {
    let mut registry = job::build_registry();
    if let Some(state) = crate::common::global_app_state() {
        job::register_scheduled_services(&mut registry, &state.config);
    }
    registry
}

/// Whether a string round-trips through the policy enum unchanged, which is how an unknown value is
/// told from one the enum silently defaults.
fn known_overlap(s: &str) -> bool {
    OverlapPolicy::from_str_or_default(Some(s)).as_str() == s
}

fn known_catchup(s: &str) -> bool {
    CatchupPolicy::from_str_or_default(Some(s)).as_str() == s
}

#[derive(FromQueryResult)]
struct Stored {
    enabled: bool,
    interval_seconds: Option<i64>,
    overlap_policy: Option<String>,
    catchup_policy: Option<String>,
    tunables: serde_json::Value,
}

/// The five editable fields, as `change_audit` records them on both sides of an edit.
fn snapshot(
    enabled: bool,
    interval_seconds: Option<i64>,
    overlap_policy: Option<&String>,
    catchup_policy: Option<&String>,
    tunables: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "enabled": enabled,
        "interval_seconds": interval_seconds,
        "overlap_policy": overlap_policy,
        "catchup_policy": catchup_policy,
        "tunables": tunables,
    })
}

/// Whether a job of each name is in flight, for the names given. One statement for a page.
async fn running_names<C: ConnectionTrait>(
    db: &C,
    names: &[String],
) -> Result<std::collections::HashSet<String>, ApiError> {
    if names.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT trigger_type FROM reprocessing_jobs \
              WHERE trigger_type = ANY($1) \
                AND status IN ('queued', 'pending', 'running', 'retrying')",
            [names.to_vec().into()],
        ))
        .await
        .map_err(ApiError::database)?;
    rows.iter()
        .map(|r| {
            r.try_get::<String>("", "trigger_type")
                .map_err(ApiError::database)
        })
        .collect()
}

/// The tunables a job declares, as the form reads them. A row whose name the registry does not know
/// declares none.
fn tunables_schema(registry: &JobRegistry, job_name: &str) -> serde_json::Value {
    registry
        .get(job_name)
        .map(|handler| {
            serde_json::to_value(handler.tunables()).unwrap_or_else(|_| serde_json::json!([]))
        })
        .unwrap_or_else(|| serde_json::json!([]))
}

impl CRUDOperations for ScheduleOperations {
    type Resource = Schedule;

    /// `running` and `tunables_schema` are resolved per request: the first from the live queue, the
    /// second from the job's own declaration. Neither is a column.
    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut Schedule,
    ) -> Result<(), ApiError> {
        let registry = full_registry();
        entity.tunables_schema = tunables_schema(&registry, &entity.job_name);
        entity.running = !running_names(db, std::slice::from_ref(&entity.job_name))
            .await?
            .is_empty();
        Ok(())
    }

    /// An edit answers with the same row a read would, computed fields included: the form that
    /// saved it renders the response.
    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut Schedule,
    ) -> Result<(), ApiError> {
        self.after_get_one(db, entity).await
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<<Schedule as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        let registry = full_registry();
        let names: Vec<String> = entities.iter().map(|e| e.job_name.clone()).collect();
        let running = running_names(db, &names).await?;
        for entity in entities.iter_mut() {
            entity.tunables_schema = tunables_schema(&registry, &entity.job_name);
            entity.running = running.contains(&entity.job_name);
        }
        Ok(())
    }

    /// The change-audit trail records who edited a schedule, and the writer is only known to the
    /// request.
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// Refuse an edit the scheduler could not act on, bring the grid forward when the edit implies
    /// it, and record what moved. All of it on the transaction the update runs in, so a refused or
    /// failed edit leaves neither a moved slot nor a trail entry.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        job_name: String,
        data: &<Schedule as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        let interval = data.interval_seconds.flatten();
        if let Some(seconds) = interval
            && seconds < 1
        {
            return Err(ApiError::bad_request("interval_seconds must be >= 1"));
        }
        if let Some(policy) = data.overlap_policy.clone().flatten()
            && !known_overlap(&policy)
        {
            return Err(ApiError::bad_request(format!(
                "unknown overlap_policy '{policy}' (expected skip_if_running|allow_concurrent)"
            )));
        }
        if let Some(policy) = data.catchup_policy.clone().flatten()
            && !known_catchup(&policy)
        {
            return Err(ApiError::bad_request(format!(
                "unknown catchup_policy '{policy}' (expected run_once|skip)"
            )));
        }

        let registry = full_registry();
        // Tunables are validated by the owning job. A row whose name the registry does not know has
        // nothing to validate against and is still editable.
        if let Some(tunables) = data.tunables.clone().flatten()
            && let Some(handler) = registry.get(&job_name)
        {
            handler.validate(&tunables).map_err(ApiError::bad_request)?;
        }

        let Some(before) = Stored::find_by_statement(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT enabled, interval_seconds, overlap_policy, catchup_policy, tunables \
             FROM schedules WHERE job_name = $1",
            [job_name.clone().into()],
        ))
        .one(db)
        .await
        .map_err(ApiError::database)?
        else {
            return Ok(());
        };

        let enabled = data.enabled.flatten().unwrap_or(before.enabled);
        let interval_seconds = interval.or(before.interval_seconds);
        let overlap_policy = data
            .overlap_policy
            .clone()
            .flatten()
            .or_else(|| before.overlap_policy.clone());
        let catchup_policy = data
            .catchup_policy
            .clone()
            .flatten()
            .or_else(|| before.catchup_policy.clone());
        let tunables = data
            .tunables
            .clone()
            .flatten()
            .unwrap_or_else(|| before.tunables.clone());

        // A lowered interval or a re-enable takes effect now rather than waiting out the slot the
        // old cadence left behind. `next_run_at` is not an editable field, so this is the only
        // writer of it here and the update that follows does not touch it.
        let interval_changed = interval.is_some_and(|n| Some(n) != before.interval_seconds);
        let being_enabled = enabled && !before.enabled;
        if interval_changed || being_enabled {
            db.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE schedules \
                    SET next_run_at = now() + (interval '1 second' * GREATEST($2, 1)) \
                  WHERE job_name = $1",
                [
                    job_name.clone().into(),
                    interval_seconds.unwrap_or(1).into(),
                ],
            ))
            .await
            .map_err(ApiError::database)?;
        }

        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO change_audit (subject, change, changed_by, old_value, new_value) \
             VALUES ('schedule:' || $1, 'schedule_update', $2, $3::jsonb, $4::jsonb)",
            [
                job_name.clone().into(),
                crate::common::actor::current().into(),
                snapshot(
                    before.enabled,
                    before.interval_seconds,
                    before.overlap_policy.as_ref(),
                    before.catchup_policy.as_ref(),
                    &before.tunables,
                )
                .to_string()
                .into(),
                snapshot(
                    enabled,
                    interval_seconds,
                    overlap_policy.as_ref(),
                    catchup_policy.as_ref(),
                    &tunables,
                )
                .to_string()
                .into(),
            ],
        ))
        .await
        .map_err(ApiError::database)?;
        Ok(())
    }
}
