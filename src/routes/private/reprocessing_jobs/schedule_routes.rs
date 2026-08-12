//! The schedule control-plane REST surface (Stage D): list/inspect recurring-Service schedules,
//! edit their cadence/policies/tunables, fire one now, and read the edit audit trail. Edits are
//! validated against the owning [`Job`]'s `validate` and recorded in `schedule_audit`.
//!
//! Raw `Statement` SQL + `AppResult<Json<…>>`, matching the sibling custom job handlers in
//! [`super::routes`].

use axum::Json;
use axum::extract::{Path, State};
use sea_orm::{ConnectionTrait, Statement};
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
#[derive(Debug, Serialize)]
pub struct ScheduleView {
    pub job_name: String,
    pub enabled: bool,
    pub interval_seconds: Option<i64>,
    pub next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    pub overlap_policy: Option<String>,
    pub catchup_policy: Option<String>,
    pub tunables: serde_json::Value,
    pub updated_by: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Whether a non-terminal job of this `job_name` is in flight (queued/pending/running/retrying).
    pub running: bool,
}

/// Editable-field snapshot recorded in `schedule_audit.old_value` / `new_value`. The exact JSON
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

/// Best-effort actor identity stamped on edits (`updated_by`/`changed_by`). Keycloak email when
/// present, else `keycloak`; an API token is `token:<id>`. Mirrors the alarm-ack audit label.
fn actor_label(auth: &AuthContext) -> String {
    match auth {
        AuthContext::Keycloak { email: Some(e), .. } => e.clone(),
        AuthContext::Keycloak { .. } => "keycloak".to_string(),
        AuthContext::ApiToken { token_id, .. } => format!("token:{token_id}"),
    }
}

/// Read one schedule row into a [`ScheduleView`], or `None` if the row doesn't exist. `running` is
/// resolved in the same statement via an EXISTS subselect against the job queue.
async fn load_view(
    db: &sea_orm::DatabaseConnection,
    job_name: &str,
) -> Result<Option<ScheduleView>, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
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
    row.map(|r| view_from_row(&r)).transpose()
}

fn view_from_row(r: &sea_orm::QueryResult) -> Result<ScheduleView, sea_orm::DbErr> {
    Ok(ScheduleView {
        job_name: r.try_get("", "job_name")?,
        enabled: r.try_get("", "enabled")?,
        interval_seconds: r.try_get("", "interval_seconds")?,
        next_run_at: r.try_get("", "next_run_at")?,
        last_enqueued_at: r.try_get("", "last_enqueued_at")?,
        overlap_policy: r.try_get("", "overlap_policy")?,
        catchup_policy: r.try_get("", "catchup_policy")?,
        tunables: r
            .try_get::<Option<serde_json::Value>>("", "tunables")?
            .unwrap_or_else(|| serde_json::json!({})),
        updated_by: r.try_get("", "updated_by")?,
        updated_at: r.try_get("", "updated_at")?,
        running: r.try_get("", "running")?,
    })
}

/// `GET /api/schedules`, every recurring-Service schedule, ordered by `job_name`. Requires
/// `read_metadata`.
pub async fn list_schedules(State(state): State<AppState>) -> AppResult<Json<Vec<ScheduleView>>> {
    let rows = state
        .db
        .query_all(Statement::from_string(
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

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(view_from_row(r)?);
    }
    Ok(Json(out))
}

/// `GET /api/schedules/{job_name}`, one schedule. 404 if unknown. Requires `read_metadata`.
pub async fn get_schedule(
    State(state): State<AppState>,
    Path(job_name): Path<String>,
) -> AppResult<Json<ScheduleView>> {
    load_view(&state.db, &job_name)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("schedule '{job_name}' not found")))
}

/// PATCH body, every field optional; absent fields are left unchanged.
#[derive(Debug, Default, Deserialize)]
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
/// writes a `schedule_audit` row. Requires `write_metadata` (+ non-scoped token). Returns the
/// updated [`ScheduleView`].
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
    let before = load_view(&state.db, &job_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("schedule '{job_name}' not found")))?;

    // Tunables are validated by the owning Job; an unregistered job_name with a row can still be
    // edited (no handler to validate against), so only validate when both a handler and tunables are
    // present.
    if let Some(tunables) = req.tunables.as_ref()
        && let Some(handler) = full_registry(&state).get(&job_name)
    {
        handler.validate(tunables).map_err(AppError::BadRequest)?;
    }

    let enabled = req.enabled.unwrap_or(before.enabled);
    let interval_seconds = req.interval_seconds.or(before.interval_seconds);
    let overlap_policy = req.overlap_policy.clone().or(before.overlap_policy.clone());
    let catchup_policy = req.catchup_policy.clone().or(before.catchup_policy.clone());
    let tunables = req.tunables.clone().unwrap_or(before.tunables.clone());

    let interval_changed = req.interval_seconds.is_some_and(|n| Some(n) != before.interval_seconds);
    let being_enabled = enabled && !before.enabled;
    // Apply a lowered interval / a re-enable immediately: next slot is now + the (new) interval,
    // instead of waiting out the stale `next_run_at`. Otherwise leave the grid where it is.
    let reset_next_run = interval_changed || being_enabled;

    let actor = actor_label(&auth);

    // Single UPDATE; `next_run_at` is reset in SQL (`now() + interval`) only when needed.
    state
        .db
        .execute(Statement::from_sql_and_values(
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
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO schedule_audit (job_name, changed_by, old_value, new_value) \
             VALUES ($1, $2, $3::jsonb, $4::jsonb)",
            [
                job_name.clone().into(),
                actor.into(),
                serde_json::to_value(&old_snapshot).unwrap_or_default().to_string().into(),
                serde_json::to_value(&new_snapshot).unwrap_or_default().to_string().into(),
            ],
        ))
        .await?;

    load_view(&state.db, &job_name)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("schedule '{job_name}' not found")))
}

/// `POST /api/schedules/{job_name}/run_now` response: the enqueued job id (None on a dedupe
/// collision, an identical run_now in the same second) and whether one was created.
#[derive(Debug, Serialize)]
pub struct RunNowResponse {
    pub job_id: Option<Uuid>,
    pub enqueued: bool,
}

/// `POST /api/schedules/{job_name}/run_now`, fire one off-cadence run with the schedule's current
/// tunables snapshot. 404 if `job_name` is not a known job. Requires `write_metadata` (+ non-scoped
/// token).
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
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT tunables FROM schedules WHERE job_name = $1",
            [job_name.clone().into()],
        ))
        .await?
        .and_then(|r| r.try_get::<Option<serde_json::Value>>("", "tunables").ok().flatten())
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

/// One edit-audit entry as the API exposes it.
#[derive(Debug, Serialize)]
pub struct ScheduleAuditEntry {
    pub changed_at: chrono::DateTime<chrono::Utc>,
    pub changed_by: Option<String>,
    pub old_value: Option<serde_json::Value>,
    pub new_value: Option<serde_json::Value>,
}

/// `GET /api/schedules/{job_name}/audit`, up to the 100 newest edits for one schedule, newest
/// first. Returns an empty list for an unknown/never-edited job. Requires `read_metadata`.
pub async fn get_schedule_audit(
    State(state): State<AppState>,
    Path(job_name): Path<String>,
) -> AppResult<Json<Vec<ScheduleAuditEntry>>> {
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT changed_at, changed_by, old_value, new_value \
             FROM schedule_audit \
             WHERE job_name = $1 \
             ORDER BY changed_at DESC \
             LIMIT 100",
            [job_name.into()],
        ))
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ScheduleAuditEntry {
            changed_at: r.try_get("", "changed_at")?,
            changed_by: r.try_get("", "changed_by")?,
            old_value: r.try_get("", "old_value")?,
            new_value: r.try_get("", "new_value")?,
        });
    }
    Ok(Json(out))
}
