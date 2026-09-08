//! The schedule control-plane REST surface (Stage D): list/inspect recurring-Service schedules,
//! edit their cadence/policies/tunables, fire one now, and read the edit audit trail. Edits are
//! validated against the owning [`Job`]'s `validate` and recorded in `change_audit` under
//! the subject `schedule:{job_name}`.
//!
//! Raw `Statement` SQL + `AppResult<Json<…>>`, matching the sibling custom job handlers in
//! [`super::routes`].

use axum::Json;
use axum::extract::{Path, State};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::job::{self, JobRegistry};
use super::schedule::{CatchupPolicy, OverlapPolicy};
use super::worker;
use crate::common::AppState;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};

/// One schedule row as the API exposes it. `running` is computed per-request from the live job
/// queue, not stored. Field names/types are the UI contract.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ScheduleView {
    pub job_name: String,
    pub enabled: bool,
    #[schema(required)]
    pub interval_seconds: Option<i64>,
    #[schema(required)]
    pub next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(required)]
    pub last_enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(required)]
    pub overlap_policy: Option<String>,
    #[schema(required)]
    pub catchup_policy: Option<String>,
    pub tunables: serde_json::Value,
    /// What this job accepts under `tunables`, from its own declaration. Empty means none.
    pub tunables_schema: Vec<job::TunableSpec>,
    #[schema(required)]
    pub updated_by: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Whether a non-terminal job of this `job_name` is in flight (queued/pending/running/retrying).
    pub running: bool,
}

/// Editable-field snapshot recorded in `change_audit.old_value` / `new_value`. The exact JSON
/// shape of a before/after pair so an edit is auditable from the columns alone.
#[derive(Debug, Serialize)]
struct AuditSnapshot {
    enabled: bool,
    interval_seconds: Option<i64>,
    overlap_policy: Option<String>,
    catchup_policy: Option<String>,
    tunables: serde_json::Value,
}

/// Build the full job registry (on-demand jobs + the recurring Services with their cadence) for the
/// handlers' tunables validation and run-now existence check. Stateless and cheap; rebuilt per call
/// rather than threaded through `AppState` so the worker/scheduler's registry stays the single
/// source the brief's fallback allows. The scheduled-Service set is what carries `validate`/cadence.
fn full_registry(state: &AppState) -> JobRegistry {
    let mut registry = job::build_registry();
    job::register_scheduled_services(&mut registry, &state.config);
    registry
}

/// Read one schedule row into a [`ScheduleView`], or `None` if the row doesn't exist. `running` is
/// resolved in the same statement via an EXISTS subselect against the job queue.
async fn load_view(
    db: &sea_orm::DatabaseConnection,
    registry: &JobRegistry,
    job_name: &str,
) -> Result<Option<ScheduleView>, sea_orm::DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.job_name, s.enabled, s.interval_seconds, s.next_run_at, s.last_enqueued_at, \
                    s.overlap_policy, s.catchup_policy, s.tunables, s.updated_by, s.updated_at, \
                    EXISTS ( \
                        SELECT 1 FROM reprocessing_jobs j \
                        WHERE j.trigger_type = s.job_name \
                          AND j.status IN ('queued', 'pending', 'running', 'retrying') \
                    ) AS running \
             FROM schedules s \
             WHERE s.job_name = $1",
            [job_name.into()],
        ))
        .await?;
    row.map(|r| view_with_schema(&r, registry)).transpose()
}

/// The stored row plus what the registry says the job accepts, so a form is built from the
/// server's declaration rather than from a copy of it.
fn view_with_schema(
    r: &sea_orm::QueryResult,
    registry: &JobRegistry,
) -> Result<ScheduleView, sea_orm::DbErr> {
    let mut view = view_from_row(r)?;
    if let Some(handler) = registry.get(&view.job_name) {
        view.tunables_schema = handler.tunables();
    }
    Ok(view)
}

/// The stored schedule row. `tunables` is nullable in the table and empty where a job declares
/// none, which is what the view reads it as.
#[derive(sea_orm::FromQueryResult)]
struct ScheduleRow {
    job_name: String,
    enabled: bool,
    interval_seconds: Option<i64>,
    next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    last_enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    overlap_policy: Option<String>,
    catchup_policy: Option<String>,
    tunables: Option<serde_json::Value>,
    updated_by: Option<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
    running: bool,
}

fn view_from_row(r: &sea_orm::QueryResult) -> Result<ScheduleView, sea_orm::DbErr> {
    let r = ScheduleRow::from_query_result(r, "")?;
    Ok(ScheduleView {
        job_name: r.job_name,
        enabled: r.enabled,
        interval_seconds: r.interval_seconds,
        next_run_at: r.next_run_at,
        last_enqueued_at: r.last_enqueued_at,
        overlap_policy: r.overlap_policy,
        catchup_policy: r.catchup_policy,
        tunables: r.tunables.unwrap_or_else(|| serde_json::json!({})),
        tunables_schema: Vec::new(),
        updated_by: r.updated_by,
        updated_at: r.updated_at,
        running: r.running,
    })
}

/// `GET /api/schedules`, every recurring-Service schedule, ordered by `job_name`. Requires
/// `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/schedules",
    responses((status = 200, description = "Every recurring job's schedule", body = Vec<ScheduleView>)),
    tag = "schedules"
)]
pub async fn list_schedules(State(state): State<AppState>) -> AppResult<Json<Vec<ScheduleView>>> {
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.job_name, s.enabled, s.interval_seconds, s.next_run_at, s.last_enqueued_at, \
                    s.overlap_policy, s.catchup_policy, s.tunables, s.updated_by, s.updated_at, \
                    EXISTS ( \
                        SELECT 1 FROM reprocessing_jobs j \
                        WHERE j.trigger_type = s.job_name \
                          AND j.status IN ('queued', 'pending', 'running', 'retrying') \
                    ) AS running \
             FROM schedules s \
             ORDER BY s.job_name"
                .to_string(),
        ))
        .await?;

    let registry = full_registry(&state);
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(view_with_schema(r, &registry)?);
    }
    Ok(Json(out))
}

/// `GET /api/schedules/{job_name}`, one schedule. 404 if unknown. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/schedules/{job_name}",
    params(("job_name" = String, Path, description = "Registered job name")),
    responses(
        (status = 200, description = "The schedule", body = ScheduleView),
        (status = 404, description = "No schedule of that name"),
    ),
    tag = "schedules"
)]
pub async fn get_schedule(
    State(state): State<AppState>,
    Path(job_name): Path<String>,
) -> AppResult<Json<ScheduleView>> {
    load_view(&state.db, &full_registry(&state), &job_name)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("schedule '{job_name}' not found")))
}

/// PATCH body, every field optional; absent fields are left unchanged.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct UpdateScheduleRequest {
    pub enabled: Option<bool>,
    pub interval_seconds: Option<i64>,
    pub overlap_policy: Option<String>,
    pub catchup_policy: Option<String>,
    pub tunables: Option<serde_json::Value>,
}

/// Whether a string round-trips through the policy enum unchanged (i.e. is a known value, not the
/// silent default substituted for an unrecognised one).
fn known_overlap(s: &str) -> bool {
    OverlapPolicy::from_str_or_default(Some(s)).as_str() == s
}

fn known_catchup(s: &str) -> bool {
    CatchupPolicy::from_str_or_default(Some(s)).as_str() == s
}

/// `PATCH /api/schedules/{job_name}`, edit cadence/policies/tunables. 404 unknown row; 400 on a
/// bad interval, unknown policy, or rejected tunables. Applies only provided fields, recomputes
/// `next_run_at` when the interval changes or a disabled schedule is enabled, stamps the actor, and
/// writes a `change_audit` row. Requires `write_metadata` (+ non-scoped token). Returns the
/// updated [`ScheduleView`].
#[utoipa::path(
    patch,
    path = "/api/schedules/{job_name}",
    params(("job_name" = String, Path, description = "Registered job name")),
    request_body = UpdateScheduleRequest,
    responses(
        (status = 200, description = "The updated schedule", body = ScheduleView),
        (status = 400, description = "Bad interval, unknown policy, or rejected tunables"),
        (status = 404, description = "No schedule of that name"),
    ),
    tag = "schedules"
)]
pub async fn update_schedule(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(job_name): Path<String>,
    Json(req): Json<UpdateScheduleRequest>,
) -> AppResult<Json<ScheduleView>> {
    if let Some(interval) = req.interval_seconds
        && interval < 1
    {
        return Err(AppError::BadRequest(
            "interval_seconds must be >= 1".to_string(),
        ));
    }
    if let Some(p) = req.overlap_policy.as_deref()
        && !known_overlap(p)
    {
        return Err(AppError::BadRequest(format!(
            "unknown overlap_policy '{p}' (expected skip_if_running|allow_concurrent)"
        )));
    }
    if let Some(p) = req.catchup_policy.as_deref()
        && !known_catchup(p)
    {
        return Err(AppError::BadRequest(format!(
            "unknown catchup_policy '{p}' (expected run_once|skip)"
        )));
    }

    // Read the pre-image (also the 404 check). Locked nowhere, the single UPDATE below is atomic and
    // we don't need cross-statement consistency for an operator edit.
    let registry = full_registry(&state);
    let before = load_view(&state.db, &registry, &job_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("schedule '{job_name}' not found")))?;

    // Tunables are validated by the owning Job; an unregistered job_name with a row can still be
    // edited (no handler to validate against), so only validate when both a handler and tunables are
    // present.
    if let Some(tunables) = req.tunables.as_ref()
        && let Some(handler) = registry.get(&job_name)
    {
        handler.validate(tunables).map_err(AppError::BadRequest)?;
    }

    let enabled = req.enabled.unwrap_or(before.enabled);
    let interval_seconds = req.interval_seconds.or(before.interval_seconds);
    let overlap_policy = req.overlap_policy.clone().or(before.overlap_policy.clone());
    let catchup_policy = req.catchup_policy.clone().or(before.catchup_policy.clone());
    let tunables = req.tunables.clone().unwrap_or(before.tunables.clone());

    let interval_changed = req
        .interval_seconds
        .is_some_and(|n| Some(n) != before.interval_seconds);
    let being_enabled = enabled && !before.enabled;
    // Apply a lowered interval / a re-enable immediately: next slot is now + the (new) interval,
    // instead of waiting out the stale `next_run_at`. Otherwise leave the grid where it is.
    let reset_next_run = interval_changed || being_enabled;

    let actor = crate::common::actor::label(&auth);

    // Single UPDATE; `next_run_at` is reset in SQL (`now() + interval`) only when needed.
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE schedules SET \
                 enabled = $2, \
                 interval_seconds = $3, \
                 overlap_policy = $4, \
                 catchup_policy = $5, \
                 tunables = $6::jsonb, \
                 next_run_at = CASE WHEN $7 \
                     THEN now() + (interval '1 second' * GREATEST($3, 1)) \
                     ELSE next_run_at END, \
                 updated_by = $8, \
                 updated_at = now() \
             WHERE job_name = $1",
            [
                job_name.clone().into(),
                enabled.into(),
                interval_seconds.into(),
                overlap_policy.clone().into(),
                catchup_policy.clone().into(),
                tunables.to_string().into(),
                reset_next_run.into(),
                actor.clone().into(),
            ],
        ))
        .await?;

    let old_snapshot = AuditSnapshot {
        enabled: before.enabled,
        interval_seconds: before.interval_seconds,
        overlap_policy: before.overlap_policy.clone(),
        catchup_policy: before.catchup_policy.clone(),
        tunables: before.tunables.clone(),
    };
    let new_snapshot = AuditSnapshot {
        enabled,
        interval_seconds,
        overlap_policy: overlap_policy.clone(),
        catchup_policy: catchup_policy.clone(),
        tunables: tunables.clone(),
    };
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO change_audit (subject, change, changed_by, old_value, new_value) \
             VALUES ('schedule:' || $1, 'schedule_update', $2, $3::jsonb, $4::jsonb)",
            [
                job_name.clone().into(),
                actor.into(),
                serde_json::to_value(&old_snapshot)
                    .unwrap_or_default()
                    .to_string()
                    .into(),
                serde_json::to_value(&new_snapshot)
                    .unwrap_or_default()
                    .to_string()
                    .into(),
            ],
        ))
        .await?;

    load_view(&state.db, &registry, &job_name)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("schedule '{job_name}' not found")))
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
/// tunables snapshot. 404 if `job_name` is not a known job. Requires `write_metadata` (+ non-scoped
/// token).
#[utoipa::path(
    post,
    path = "/api/schedules/{job_name}/run_now",
    params(("job_name" = String, Path, description = "Registered job name")),
    responses(
        (status = 200, description = "The enqueued run", body = RunNowResponse),
        (status = 404, description = "No job of that name is registered"),
    ),
    tag = "schedules"
)]
pub async fn run_now(
    State(state): State<AppState>,
    Path(job_name): Path<String>,
) -> AppResult<Json<RunNowResponse>> {
    if full_registry(&state).get(&job_name).is_none() {
        return Err(AppError::NotFound(format!(
            "no job named '{job_name}' is registered"
        )));
    }

    // Snapshot the schedule's tunables so a manual run mirrors a scheduled one; no row → `{}`.
    let tunables: serde_json::Value = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT tunables FROM schedules WHERE job_name = $1",
            [job_name.clone().into()],
        ))
        .await?
        .and_then(|r| {
            r.try_get::<Option<serde_json::Value>>("", "tunables")
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| serde_json::json!({}));

    // Per-second dedupe key so an accidental double-click collapses to one run; a deliberate second
    // run in a later second is allowed.
    let dedupe_key = format!("{job_name}:run_now:{}", chrono::Utc::now().timestamp());
    let job_id = worker::enqueue(
        &state.db,
        &job_name,
        None,
        None,
        &serde_json::json!({ "trigger": "run_now", "tunables": tunables }),
        Some(&dedupe_key),
    )
    .await?;

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
    responses((status = 200, description = "The schedule's edit trail, newest first", body = Vec<crate::routes::private::change_audit::ChangeEntry>)),
    tag = "schedules"
)]
pub async fn get_schedule_audit(
    State(state): State<AppState>,
    Path(job_name): Path<String>,
) -> AppResult<Json<Vec<crate::routes::private::change_audit::ChangeEntry>>> {
    Ok(Json(
        crate::routes::private::change_audit::entries_for(
            &state.db,
            &format!("schedule:{job_name}"),
        )
        .await?,
    ))
}
