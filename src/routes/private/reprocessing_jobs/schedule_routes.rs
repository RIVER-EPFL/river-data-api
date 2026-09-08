//! The two schedule routes that are not CRUD: fire a job now, and read the edit trail.
//!
//! Listing, inspecting and editing a schedule are the generated entity routes
//! (`super::schedule_model`), with the rules an edit must satisfy in `super::schedule_operations`.
//! These two are actions on a schedule rather than reads or writes of one: `run_now` enqueues a
//! job, and the trail lives in `change_audit` under the subject `schedule:{job_name}`.

use axum::Json;
use axum::extract::{Path, State};
use sea_orm::{ConnectionTrait, Statement};
use serde::Serialize;
use uuid::Uuid;

use super::job::{self, JobRegistry};
use super::worker;
use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Build the full job registry (on-demand jobs + the recurring Services with their cadence) for the
/// handlers' tunables validation and run-now existence check. Stateless and cheap; rebuilt per call
/// rather than threaded through `AppState` so the worker/scheduler's registry stays the single
/// source the brief's fallback allows. The scheduled-Service set is what carries `validate`/cadence.
fn full_registry(state: &AppState) -> JobRegistry {
    let mut registry = job::build_registry();
    job::register_scheduled_services(&mut registry, &state.config);
    registry
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
