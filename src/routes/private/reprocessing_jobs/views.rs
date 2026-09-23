//! The tracked-job endpoints that are not generated CRUD: the per-job timeline, cancel and rerun,
//! and the two schedule actions (fire a job now, read the edit trail).

use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::EntityTrait;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::Select;
use sea_orm::Statement;
use sea_orm::UpdateMany;
use sea_orm::sea_query::Expr;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use super::models::job;
use super::models::job::Column;
use super::models::job::Entity;
use super::models::job_log;
use super::models::schedule;
use super::service;
use super::service::JobRegistry;
use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::ProjectScope;
use crate::common::scope;
use crate::common::scope::RowProject;
use crate::common::scope::Unowned;
use crate::error::AppError;
use crate::error::AppResult;

use super::service::IN_FLIGHT_STATES as CANCELLABLE_STATES;

/// A `status IN (...)` list for the statuses above, quoted for SQL.
fn sql_list(states: &[&str]) -> String {
    let quoted: Vec<String> = states.iter().map(|s| format!("'{s}'")).collect();
    format!("({})", quoted.join(", "))
}

/// Refuse a job whose target lies outside the caller's grants. An out-of-scope job and an absent
/// one answer 404 alike, so the response does not confirm the job exists.
///
/// A job with no project-bearing target (`reprocess_all`) is admitted: any
/// member may trigger one, so withholding its timeline would hide the record of their own run.
/// A job naming a target that resolves to no project is refused.
async fn confine_job(state: &AppState, scope: &AccessScope, job_id: Uuid) -> AppResult<()> {
    let project = scope::project_of_job(&state.db, job_id).await?;
    let unowned = if matches!(project, RowProject::Global) {
        Unowned::Allow
    } else {
        Unowned::Deny
    };
    scope::require_row_in_scope(scope, &project, unowned, "Job")
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct JobLogsQuery {
    /// Only return lines with `seq` strictly greater than this (incremental polling/tailing).
    #[serde(default)]
    pub after_seq: Option<i64>,
    /// Max lines to return (default 1000, capped at 5000).
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct JobLogLine {
    pub seq: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub level: String,
    pub message: String,
    pub context: serde_json::Value,
}

impl From<job_log::Model> for JobLogLine {
    fn from(m: job_log::Model) -> Self {
        Self {
            seq: m.seq,
            ts: m.ts.into(),
            level: m.level,
            message: m.message,
            context: m.context,
        }
    }
}

/// One job's timeline from `after_seq` onwards, oldest first, at most `limit` lines.
fn timeline(job_id: Uuid, after_seq: i64, limit: u64) -> Select<job_log::Entity> {
    job_log::Entity::find()
        .filter(job_log::Column::JobId.eq(job_id))
        .filter(job_log::Column::Seq.gt(after_seq))
        .order_by_asc(job_log::Column::Seq)
        .limit(limit)
}

/// `GET /api/reprocessing_jobs/{id}/logs`, the ordered timeline for one job. Paginated by `seq`
/// so the UI can lazy-load the full record and tail new lines. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/reprocessing_jobs/{id}/logs",
    params(("id" = Uuid, Path, description = "Job UUID"), JobLogsQuery),
    responses(
        (status = 200, description = "The job's timeline, oldest first", body = Vec<JobLogLine>),
        (status = 404, description = "No job of that id"),
    ),
    tag = "jobs"
)]
pub async fn get_job_logs(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    Query(q): Query<JobLogsQuery>,
) -> AppResult<Json<Vec<JobLogLine>>> {
    confine_job(&state, &scope, id).await?;
    let limit = q.limit.unwrap_or(1000).min(5000);
    let after = q.after_seq.unwrap_or(-1);

    let out = timeline(id, after, limit)
        .all(&state.db)
        .await?
        .into_iter()
        .map(JobLogLine::from)
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CancelResponse {
    pub status: String,
}

/// `POST /api/reprocessing_jobs/{id}/cancel`, cooperatively cancel a job. A `queued` job (not yet
/// claimed) is cancelled outright; a `running` worker-pool job is signalled via the `cancel_requested`
/// column, which the owning replica's heartbeat observes (possibly on a different replica) and the job
/// honors at its next checkpoint. 409 if the type isn't cancellable or the job isn't in a cancellable
/// state; 404 if the id is unknown. Requires MANAGER, or a token with `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/reprocessing_jobs/{id}/cancel",
    params(("id" = Uuid, Path, description = "Job UUID")),
    responses(
        (status = 200, description = "The job's status after the request", body = CancelResponse),
        (status = 404, description = "No job of that id"),
        (status = 409, description = "This kind is not cancellable, or the job is not in a cancellable state"),
    ),
    tag = "jobs"
)]
pub async fn cancel_job(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<CancelResponse>> {
    confine_job(&state, &scope, id).await?;
    let row = job::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id} not found")))?;

    let trigger_type = row.trigger_type;
    if !service::is_cancellable(&trigger_type) {
        return Err(AppError::Conflict(format!(
            "jobs of type '{trigger_type}' cannot be cancelled once running"
        )));
    }

    let flagged = cancel_request(id).exec(&state.db).await?.rows_affected;

    if flagged > 0 {
        Ok(Json(CancelResponse {
            status: "cancelling".to_string(),
        }))
    } else {
        Err(AppError::Conflict(
            "job is not in a cancellable state".to_string(),
        ))
    }
}

/// The cancel of one job still in flight. The durable flag lets the owning replica, which may not be
/// this one, stop the job at its next checkpoint; a still-queued job is cancelled outright since
/// nothing is running it yet, and frees its dedupe key as a claim does.
fn cancel_request(id: Uuid) -> UpdateMany<Entity> {
    let queued = || Column::Status.eq("queued");
    Entity::update_many()
        .col_expr(Column::CancelRequested, Expr::value(true))
        .col_expr(
            Column::Status,
            Expr::case(queued(), "cancelled")
                .finally(Expr::col(Column::Status))
                .into(),
        )
        .col_expr(
            Column::CompletedAt,
            Expr::case(queued(), Expr::current_timestamp())
                .finally(Expr::col(Column::CompletedAt))
                .into(),
        )
        .col_expr(
            Column::DedupeKey,
            Expr::case(queued(), Expr::value(Option::<String>::None))
                .finally(Expr::col(Column::DedupeKey))
                .into(),
        )
        .filter(Column::Id.eq(id))
        .filter(Column::Status.is_in(CANCELLABLE_STATES))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RerunResponse {
    pub job_id: Uuid,
    pub status: String,
}

/// `POST /api/reprocessing_jobs/{id}/rerun`, replay a finished job from the `params` stored on its
/// row. Returns a NEW job (history is preserved). 409 if the type isn't rerunnable or an equivalent
/// job is already in flight; 404 if the job id is unknown. Requires MANAGER, or a token with
/// `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/reprocessing_jobs/{id}/rerun",
    params(("id" = Uuid, Path, description = "Job UUID")),
    responses(
        (status = 200, description = "The new job the replay created", body = RerunResponse),
        (status = 404, description = "No job of that id"),
        (status = 409, description = "This kind is not rerunnable, or an equivalent job is in flight"),
    ),
    tag = "jobs"
)]
pub async fn rerun_job(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RerunResponse>> {
    confine_job(&state, &scope, id).await?;
    let row = job::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id} not found")))?;

    let job::Model {
        trigger_type,
        sensor_id,
        trigger_id,
        params,
        ..
    } = row;

    if !service::is_rerunnable(&trigger_type) {
        return Err(AppError::Conflict(format!(
            "jobs of type '{trigger_type}' cannot be rerun"
        )));
    }

    // Reject if an equivalent job (same type + same target) is already in flight. The slot-scoped
    // jobs carry no `sensor_id` or `trigger_id`, so `params` is the only thing separating one
    // slot's run from another's.
    let in_flight = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT 1 FROM reprocessing_jobs \
                 WHERE status IN {in_flight_states} \
                   AND trigger_type = $1 \
                   AND sensor_id IS NOT DISTINCT FROM $2 \
                   AND trigger_id IS NOT DISTINCT FROM $3 \
                   AND params IS NOT DISTINCT FROM $4::jsonb \
                 LIMIT 1",
                in_flight_states = sql_list(&CANCELLABLE_STATES)
            ),
            [
                trigger_type.as_str().into(),
                sensor_id.map_or(sea_orm::Value::Uuid(None), Into::into),
                trigger_id.map_or(sea_orm::Value::Uuid(None), Into::into),
                params.to_string().into(),
            ],
        ))
        .await?;
    if in_flight.is_some() {
        return Err(AppError::Conflict(
            "an equivalent job is already in flight".to_string(),
        ));
    }

    // Replay the original job from its persisted params: the row already carries the exact
    // `trigger_type`/`sensor_id`/`trigger_id`/`params` the first run used, so first-run and rerun
    // share the single leased `enqueue` path (no separate reconstruction, no inline spawn).
    let new_id = service::enqueue(
        &state.db,
        &trigger_type,
        sensor_id,
        trigger_id,
        &params,
        None,
    )
    .await?
    .ok_or_else(|| AppError::Conflict("an equivalent job is already in flight".to_string()))?;

    Ok(Json(RerunResponse {
        job_id: new_id,
        status: "queued".to_string(),
    }))
}

// --- Schedule actions ---
//
// Listing, inspecting and editing a schedule are the generated entity routes
// (`super::models::schedule`), with the rules an edit must satisfy in `super::service`. These two
// are actions on a schedule rather than reads or writes of one: `run_now` enqueues a job, and the
// trail lives in `change_audit` under the subject `schedule:{job_name}`.

/// Build the full job registry (on-demand jobs + the recurring Services with their cadence) for the
/// handlers' tunables validation and run-now existence check. Stateless and cheap; rebuilt per call
/// rather than threaded through `AppState` so the worker/scheduler's registry stays the single
/// source the brief's fallback allows. The scheduled-Service set is what carries `validate`/cadence.
fn full_registry(state: &AppState) -> JobRegistry {
    let mut registry = service::build_registry();
    service::register_scheduled_services(&mut registry, &state.config);
    registry
}

/// `GET /api/schedules/runnable`, every job kind a person may run off-cadence, by name, with what
/// each one needs and the cadence it also runs on where it has one. Requires `read_metadata`.
///
/// The `schedules` table holds a row only for a kind with a default cadence, so it is not the list
/// of what can be run: the registry is.
#[utoipa::path(
    get,
    path = "/api/schedules/runnable",
    responses((status = 200, description = "The kinds a person may run, by name", body = Vec<service::RunnableJob>)),
    tag = "schedules"
)]
pub async fn list_runnable(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<service::RunnableJob>>> {
    let registry = full_registry(&state);
    let cadence = schedule::Entity::find()
        .all(&state.db)
        .await?
        .into_iter()
        .map(|row| (row.job_name, (row.interval_seconds, row.enabled)))
        .collect();
    Ok(Json(service::runnable_jobs(registry.names(), &cadence)))
}

/// `POST /api/schedules/{job_name}/run_now` response: the enqueued job id (None on a dedupe
/// collision, an identical run_now in the same second) and whether one was created.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RunNowResponse {
    #[schema(required)]
    pub job_id: Option<Uuid>,
    pub enqueued: bool,
}

/// `POST /api/schedules/{job_name}/run_now`, fire one off-cadence run with the schedule's current
/// tunables snapshot and the inputs the kind declares, recorded under the caller. 404 if `job_name`
/// is not a known job, 400 if it is not run by hand, a key is not an input it declares, or an input
/// it requires is missing. Requires `write_metadata` (+ non-scoped token).
#[utoipa::path(
    post,
    path = "/api/schedules/{job_name}/run_now",
    params(("job_name" = String, Path, description = "Registered job name")),
    responses(
        (status = 200, description = "The enqueued run", body = RunNowResponse),
        (status = 400, description = "The job is not run by hand, a key is not an input it declares, or a required input is missing"),
        (status = 404, description = "No job of that name is registered"),
    ),
    tag = "schedules"
)]
pub async fn run_now(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Path(job_name): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> AppResult<Json<RunNowResponse>> {
    let Some(job) = full_registry(&state).get(&job_name) else {
        return Err(AppError::NotFound(format!(
            "no job named '{job_name}' is registered"
        )));
    };
    let supplied = body.map_or(serde_json::Value::Null, |Json(v)| v);
    let offer = job.manual_run();
    if offer == service::ManualRun::NotOffered {
        return Err(AppError::BadRequest(format!(
            "'{job_name}' is not run by hand: its inputs come from the route that enqueues it"
        )));
    }

    // Snapshot the schedule's tunables so a manual run mirrors a scheduled one; no row → `{}`.
    let tunables = schedule::Entity::find()
        .filter(schedule::Column::JobName.eq(job_name.clone()))
        .one(&state.db)
        .await?
        .map_or_else(|| serde_json::json!({}), |row| row.tunables);
    let params = service::manual_params(&offer, &supplied, tunables, &auth)
        .map_err(|reason| AppError::BadRequest(format!("'{job_name}' {reason}")))?;

    // Per-second dedupe key so an accidental double-click collapses to one run; a deliberate second
    // run in a later second is allowed.
    let dedupe_key = format!("{job_name}:run_now:{}", chrono::Utc::now().timestamp());
    let job_id =
        service::enqueue(&state.db, &job_name, None, None, &params, Some(&dedupe_key)).await?;

    Ok(Json(RunNowResponse {
        enqueued: job_id.is_some(),
        job_id,
    }))
}

/// `GET /api/schedules/{job_name}/audit`, up to the 100 newest edits for one schedule, newest
/// first. Returns an empty list for an unknown/never-edited job. Requires `read_metadata`.
///
/// A schedule's trail is one subject in `change_audit`, so this is the general reader keyed for
/// this caller rather than a second query over the same table.
#[utoipa::path(
    get,
    path = "/api/schedules/{job_name}/audit",
    params(("job_name" = String, Path, description = "Registered job name")),
    responses((status = 200, description = "The schedule's edit trail, newest first", body = Vec<crate::routes::private::change_audit::models::ChangeEntry>)),
    tag = "schedules"
)]
pub async fn get_schedule_audit(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(job_name): Path<String>,
) -> AppResult<Json<Vec<crate::routes::private::change_audit::models::ChangeEntry>>> {
    Ok(Json(
        crate::routes::private::change_audit::service::entries_for(
            &state.db,
            &scope,
            &format!("schedule:{job_name}"),
        )
        .await?,
    ))
}

#[cfg(test)]
#[path = "tests/job_timeline.rs"]
mod tests;
