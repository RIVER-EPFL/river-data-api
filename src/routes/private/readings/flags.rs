use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::bulk_write;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::recompute;
use crate::routes::private::readings::decisions::{self, Kind, Origin};
use crate::routes::private::readings::tail;
use crate::routes::private::tools::scripts::actor_label;

/// Keys per statement. A statement is one OR-chain, and each term carries `time = $n` equality, so
/// chunk exclusion prunes; the bound is on statement size, not on correctness.
const KEYS_PER_STATEMENT: usize = 500;

/// Curation moves values already served, at instants a bounded query may hold cached anywhere, and
/// the rollups exclude what a flag hides, so the refresh is the write's own span and its failure is
/// the caller's. Nothing arrives here, so no slot is announced and no alarm is re-evaluated.
const CURATION_TAIL: tail::Axes = tail::Axes {
    cache: tail::Cache::All,
    refresh: tail::Refresh::Range { fatal: true },
    announce: false,
    reconcile_alarms: false,
    episodes: tail::Episodes::None,
    writer: recompute::Writer::Person,
};

/// What a recorded curation left behind, in the shape the shared tail reads.
fn written(recorded: &decisions::Recorded) -> tail::Written {
    tail::Written::new(recorded.rows)
        .over(recorded.span)
        .touching(recorded.touched_events.clone())
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReadingKey {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: DateTime<Utc>,
    /// One replicate of a grab group, which scopes the write to spot rows: a sonde reading sharing
    /// the grab's snapped timestamp must not be flagged by a replicate key. Omit to act on every
    /// row at that timestamp.
    #[serde(default)]
    pub replicate_index: Option<i16>,
    /// Restrict the write to one cadence ('continuous' | 'spot' | 'derived'). 'continuous' also
    /// covers legacy NULL-typed rows.
    #[serde(default)]
    pub measurement_type: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FlagReadingsRequest {
    pub readings: Vec<ReadingKey>,
    pub reason: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UnflagReadingsRequest {
    pub readings: Vec<ReadingKey>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FlagReadingsResponse {
    pub updated: u64,
    /// Under `dry_run`, the calculations the flagged parameter feeds and the outputs each would
    /// rewrite. Flagging changes the served value, so it changes what a calculation reads; the
    /// consequence is reported before the write, not discovered after it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calculations: Vec<crate::routes::private::tools::closure::CalculationImpact>,
}

/// `SET` clause of the flag write, and the values it binds ahead of the keys.
enum FlagWrite {
    Set(String),
    Clear,
}

impl FlagWrite {
    fn kind(&self) -> Kind {
        match self {
            FlagWrite::Set(_) => Kind::Flag,
            FlagWrite::Clear => Kind::Unflag,
        }
    }

    fn new_value(&self) -> serde_json::Value {
        match self {
            FlagWrite::Set(reason) => serde_json::json!({ "reason": reason }),
            FlagWrite::Clear => serde_json::json!({}),
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            FlagWrite::Set(reason) => Some(reason.as_str()),
            FlagWrite::Clear => None,
        }
    }

    /// Rows already in the requested state are not decided again.
    fn state_predicate(&self) -> &'static str {
        match self {
            FlagWrite::Set(_) => "r.is_flagged IS NOT TRUE",
            FlagWrite::Clear => "r.is_flagged IS TRUE",
        }
    }
}

/// Flag or unflag an explicit key set, then refresh the rollups over the buckets it changed.
///
/// Each key becomes a decision (ADR 0008); the record's trigger projects it onto the row. The
/// whole key set is one transaction with the decompression cap lifted: a partial flag set is a
/// state no reader can interpret, and any key may land in a chunk the compression policy has
/// reached. The refresh runs after the commit because `refresh_continuous_aggregate` is a
/// procedure with its own transaction control, and so does the reactive hook the decisions
/// reach.
async fn apply_flags(
    state: &AppState,
    scope: &AccessScope,
    actor: &str,
    keys: &[ReadingKey],
    write: FlagWrite,
) -> AppResult<u64> {
    if keys.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    let target_sites: Vec<Uuid> = keys.iter().map(|r| r.site_id).collect();
    enforce_project_scope_for_sites(&state.db, scope, &target_sites).await?;

    let set_id = Uuid::new_v4();
    let recorded = bulk_write::guarded(&state.db, async |txn| {
        let mut all = decisions::Recorded::default();
        for chunk in keys.chunks(KEYS_PER_STATEMENT) {
            let mut values: Vec<sea_orm::Value> = Vec::with_capacity(chunk.len() * 5);
            let mut conditions = Vec::with_capacity(chunk.len());
            for (i, key) in chunk.iter().enumerate() {
                let base = i * 5 + 1;
                conditions.push(format!(
                    "(r.site_id = ${b0} AND r.parameter_id = ${b1} AND r.time = ${b2} \
                      AND (${b3}::smallint IS NULL \
                           OR (r.replicate_index = ${b3} AND r.measurement_type = 'spot')) \
                      AND (${b4}::text IS NULL OR r.measurement_type = ${b4} \
                           OR (${b4} = 'continuous' AND r.measurement_type IS NULL)))",
                    b0 = base,
                    b1 = base + 1,
                    b2 = base + 2,
                    b3 = base + 3,
                    b4 = base + 4
                ));
                values.push(key.site_id.into());
                values.push(key.parameter_id.into());
                values.push(key.time.into());
                values.push(key.replicate_index.into());
                values.push(key.measurement_type.clone().into());
            }
            let predicate = format!(
                "({}) AND {}",
                conditions.join(" OR "),
                write.state_predicate()
            );
            let recorded = decisions::record_many(
                txn,
                write.kind(),
                &predicate,
                values,
                decisions::NewValue::Literal(write.new_value()),
                actor,
                write.reason(),
                Origin::Manual,
                Some(set_id),
            )
            .await?;
            all.rows += recorded.rows;
            all.span = match (all.span, recorded.span) {
                (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
                (x, None) => x,
                (None, y) => y,
            };
            all.touched_events.extend(recorded.touched_events);
        }
        Ok(all)
    })
    .await?;

    tail::run(state, &written(&recorded), &CURATION_TAIL, actor).await?;
    Ok(recorded.rows)
}

/// One slot over a closed time range.
struct SlotRange {
    site_id: Uuid,
    parameter_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
}

impl SlotRange {
    async fn admit(&self, state: &AppState, scope: &AccessScope) -> AppResult<()> {
        if self.end_time < self.start_time {
            return Err(AppError::BadRequest(
                "end_time must be >= start_time".to_string(),
            ));
        }
        enforce_project_scope_for_sites(&state.db, scope, &[self.site_id]).await
    }

    /// The rows the write selects: the slot, the range, and not already in the requested state.
    fn predicate(&self, write: &FlagWrite) -> String {
        format!(
            "r.site_id = $1 AND r.parameter_id = $2 AND r.time >= $3 AND r.time <= $4 AND {}",
            write.state_predicate()
        )
    }

    fn binds(&self) -> Vec<sea_orm::Value> {
        vec![
            self.site_id.into(),
            self.parameter_id.into(),
            self.start_time.into(),
            self.end_time.into(),
        ]
    }
}

/// The count `apply_flags_over_range` would report, with nothing written.
async fn count_flags_over_range(
    state: &AppState,
    scope: &AccessScope,
    range: SlotRange,
    write: &FlagWrite,
) -> AppResult<u64> {
    use sea_orm::{ConnectionTrait, Statement};
    range.admit(state, scope).await?;
    let row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) AS n FROM readings r WHERE {}",
                range.predicate(write)
            ),
            range.binds(),
        ))
        .await?
        .ok_or_else(|| AppError::Internal("count returned no row".to_string()))?;
    let n: i64 = row.try_get("", "n")?;
    Ok(u64::try_from(n).unwrap_or(0))
}

async fn apply_flags_over_range(
    state: &AppState,
    scope: &AccessScope,
    actor: &str,
    range: SlotRange,
    write: &FlagWrite,
) -> AppResult<u64> {
    range.admit(state, scope).await?;
    let predicate = range.predicate(write);
    let binds = range.binds();
    let recorded = bulk_write::guarded(&state.db, async |txn| {
        decisions::record_many(
            txn,
            write.kind(),
            &predicate,
            binds.clone(),
            decisions::NewValue::Literal(write.new_value()),
            actor,
            write.reason(),
            Origin::Manual,
            Some(Uuid::new_v4()),
        )
        .await
    })
    .await?;

    tail::run(state, &written(&recorded), &CURATION_TAIL, actor).await?;
    Ok(recorded.rows)
}

#[utoipa::path(
    patch,
    path = "/api/readings/flag",
    request_body = FlagReadingsRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "Missing readings or reason"),
    ),
    tag = "ingestion"
)]
pub async fn flag_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<FlagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    let updated = apply_flags(
        &state,
        &scope,
        &actor_label(&auth),
        &payload.readings,
        FlagWrite::Set(payload.reason.clone()),
    )
    .await?;

    tracing::info!(updated, reason = %payload.reason, "Flagged readings");
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

/// Unflag a set of previously-flagged readings. Requires `write_data`.
#[utoipa::path(
    patch,
    path = "/api/readings/unflag",
    request_body = UnflagReadingsRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "No readings specified"),
    ),
    tag = "ingestion"
)]
pub async fn unflag_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<UnflagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let updated = apply_flags(
        &state,
        &scope,
        &actor_label(&auth),
        &payload.readings,
        FlagWrite::Clear,
    )
    .await?;

    tracing::info!(updated, "Unflagged readings");
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FlagRangeRequest {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    /// Required unless `dry_run`.
    #[serde(default)]
    pub reason: String,
    /// Report the count the write would return and change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UnflagRangeRequest {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    /// Report the count the write would return and change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// Flag every reading in a (site_id, parameter_id, time range). Requires `write_data`.
/// Refreshes continuous aggregates for the affected window on success. `dry_run` returns the
/// count alone.
#[utoipa::path(
    patch,
    path = "/api/readings/flag_range",
    request_body = FlagRangeRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "Missing reason or end_time < start_time"),
    ),
    tag = "ingestion"
)]
pub async fn flag_range(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<FlagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let range = SlotRange {
        site_id: payload.site_id,
        parameter_id: payload.parameter_id,
        start_time: payload.start_time,
        end_time: payload.end_time,
    };
    if payload.dry_run {
        let updated =
            count_flags_over_range(&state, &scope, range, &FlagWrite::Set(String::new())).await?;
        let calculations = crate::routes::private::tools::closure::calculations_fed_by(
            &state.db,
            &[payload.parameter_id],
        )
        .await?;
        return Ok(Json(FlagReadingsResponse {
            updated,
            calculations,
        }));
    }
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    let updated = apply_flags_over_range(
        &state,
        &scope,
        &actor_label(&auth),
        range,
        &FlagWrite::Set(payload.reason.clone()),
    )
    .await?;

    tracing::info!(
        updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        reason = %payload.reason,
        "Flagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

/// Unflag every reading in a (site_id, parameter_id, time range). Requires `write_data`.
/// Refreshes continuous aggregates for the affected window on success. `dry_run` returns the
/// count alone.
#[utoipa::path(
    patch,
    path = "/api/readings/unflag_range",
    request_body = UnflagRangeRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "end_time < start_time"),
    ),
    tag = "ingestion"
)]
pub async fn unflag_range(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<UnflagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let range = SlotRange {
        site_id: payload.site_id,
        parameter_id: payload.parameter_id,
        start_time: payload.start_time,
        end_time: payload.end_time,
    };
    if payload.dry_run {
        let updated = count_flags_over_range(&state, &scope, range, &FlagWrite::Clear).await?;
        let calculations = crate::routes::private::tools::closure::calculations_fed_by(
            &state.db,
            &[payload.parameter_id],
        )
        .await?;
        return Ok(Json(FlagReadingsResponse {
            updated,
            calculations,
        }));
    }
    let updated = apply_flags_over_range(
        &state,
        &scope,
        &actor_label(&auth),
        range,
        &FlagWrite::Clear,
    )
    .await?;

    tracing::info!(
        updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        "Unflagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}
