//! Sync queries and the machinery they share: the session lookup, the pairing-plan engine, and
//! the replicate audit's classification and holds.

use axum::Json;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use moka::future::Cache;
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait as _, Func, JoinType, OnConflict, Order, PostgresQueryBuilder,
    Query as SeaQuery, SelectStatement,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use uuid::Uuid;

use river_data_core::commands as core_commands;

use super::hold_model;
use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::bulk_write;
use crate::common::middleware::enforce_project_scope_for_sites;
use crate::error::{AppError, AppResult};
use crate::routes::private::api_tokens::service::hash_token;
use crate::routes::private::parameter_groups::group_model as parameter_groups;
use crate::routes::private::parameter_groups::member_model;
use crate::routes::private::projects::source_links as project_source_links;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::readings::status_events::models as status_events;
use crate::routes::private::sensors;
use crate::routes::private::sensors::models::{InstrumentKind, proposal};
use crate::routes::private::sensors::service::{
    create_sensor_for_stream, upsert_source_instrument,
};
use crate::routes::private::sites::source_links as site_source_links;
use crate::routes::private::{
    annotations, data_streams, data_streams::pairing_plans, parameters, projects, site_parameters,
    sites, standard_curves,
};
use crate::routes::private::{collection_events, readings};

use super::models::services::{SyncService, SyncServiceList};
use super::models::*;

/// An authenticated sync service.
#[derive(Debug, Clone)]
pub struct SyncSession {
    pub service_id: Uuid,
}

/// Resolve a raw bearer token to a live sync session. `Ok(None)` is an unknown, malformed or
/// expired token; the caller decides whether that is a 401 or a fall-through to another auth
/// method. `Err` is a lookup that could not be made, which says nothing about the token.
pub async fn lookup_sync_session(
    db: &DatabaseConnection,
    raw_token: &str,
) -> Result<Option<SyncSession>, sea_orm::DbErr> {
    if raw_token.is_empty() {
        return Ok(None);
    }

    let token_hash = hash_token(raw_token);
    let Some(token) = tokens::Entity::find()
        .filter(tokens::Column::TokenHash.eq(&token_hash))
        .one(db)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "DB error looking up sync token"))?
    else {
        return Ok(None);
    };

    if token.expires_at.with_timezone(&chrono::Utc) < chrono::Utc::now() {
        tracing::debug!(service_id = %token.service_id, "Sync token expired");
        return Ok(None);
    }

    Ok(Some(SyncSession {
        service_id: token.service_id,
    }))
}

/// The answer to a session lookup: 401 only for a token the database says is not live, 503 when
/// the database could not be asked, so an outage never reads to the client as revoked credentials.
pub fn session_or_rejection(
    lookup: Result<Option<SyncSession>, sea_orm::DbErr>,
) -> Result<SyncSession, AppError> {
    match lookup {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(AppError::Unauthorized("Invalid session token".to_string())),
        Err(_) => Err(AppError::ServiceUnavailable(
            "Session lookup unavailable".to_string(),
        )),
    }
}

/// Extract the raw bearer token from an `Authorization` header value.
pub fn bearer(value: Option<&str>) -> Option<&str> {
    value.and_then(|v| v.strip_prefix("Bearer ")).map(str::trim)
}

/// The authenticated sync service behind a control plane request.
#[derive(Debug, Clone)]
pub struct SyncServiceContext {
    pub service_id: Uuid,
}

impl FromRequestParts<AppState> for SyncServiceContext {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw_token = bearer(
            parts
                .headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
        )
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::Unauthorized("Bearer token required".to_string()))?;

        let session = session_or_rejection(lookup_sync_session(&state.db, raw_token).await)?;

        Ok(Self {
            service_id: session.service_id,
        })
    }
}

/// 32 random bytes as lowercase hex. Used for sync session tokens and for the two halves of an
/// enrollment credential.
///
/// This must not be replaced by `api_tokens::service`'s minting, which prefixes `rvd_`. The dual
/// auth middleware routes any `rvd_`-prefixed bearer down the argon2 API-token path before the
/// sync session fallback runs, so an `rvd_`-prefixed session token would be rejected.
#[must_use]
pub fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The default `SYNC_SESSION_TOKEN_TTL_SECS`, so the cache has a sane window when nothing has
/// declared one (the test harness builds the router without going through `main`).
pub(super) const DEFAULT_SESSION_TOKEN_TTL_SECS: u64 = 900;

/// A cached token must expire before the row backing it does, or the heartbeat hands out a token
/// the database has already dropped and the service churns through re-enrollment. Four fifths of
/// the configured lifetime leaves a margin no operator has to know about.
pub(super) const CACHE_FRACTION_OF_TTL: f64 = 0.8;

pub(super) static SESSION_TOKEN_CACHE_TTL: OnceLock<Duration> = OnceLock::new();

/// Fix the session-token cache window from the configured token lifetime. Called once at startup,
/// before the cache is first touched; without it the default lifetime applies.
pub fn init_session_token_cache_ttl(token_ttl_secs: u64) {
    let window = (token_ttl_secs as f64 * CACHE_FRACTION_OF_TTL) as u64;
    let _ = SESSION_TOKEN_CACHE_TTL.set(Duration::from_secs(Ord::max(window, 1)));
}

pub(crate) static SESSION_TOKEN_CACHE: LazyLock<Cache<Uuid, String>> = LazyLock::new(|| {
    let ttl = *SESSION_TOKEN_CACHE_TTL.get_or_init(|| {
        Duration::from_secs((DEFAULT_SESSION_TOKEN_TTL_SECS as f64 * CACHE_FRACTION_OF_TTL) as u64)
    });
    Cache::builder().max_capacity(100).time_to_live(ttl).build()
});

/// A stored hash is argon2 when it is a PHC string; anything else is read as the old hex digest.
pub(super) fn secret_format(stored: &str) -> SecretFormat {
    if stored.starts_with("$argon2") {
        SecretFormat::Argon2
    } else {
        SecretFormat::LegacyDigest
    }
}

/// Whether a submitted secret enrolls against a stored credential, and under which hash it did.
/// Both comparisons are constant time: argon2's verifier is, and the legacy digest is compared
/// with `ct_eq` rather than `!=`, which would return at the first differing byte and time the
/// answer.
pub(super) fn check_credential(
    cred: Option<&credentials::Model>,
    client_secret: &str,
) -> Result<SecretFormat, EnrollDenial> {
    use crate::routes::private::api_tokens::service::{hash_token, verify_api_secret};

    let Some(cred) = cred else {
        return Err(EnrollDenial::UnknownClient);
    };
    if cred.revoked {
        return Err(EnrollDenial::Revoked);
    }
    match secret_format(&cred.client_secret_hash) {
        SecretFormat::Argon2 => {
            if verify_api_secret(client_secret, &cred.client_secret_hash) {
                Ok(SecretFormat::Argon2)
            } else {
                Err(EnrollDenial::BadSecret)
            }
        }
        SecretFormat::LegacyDigest => {
            let submitted = hash_token(client_secret);
            if submitted
                .as_bytes()
                .ct_eq(cred.client_secret_hash.as_bytes())
                .into()
            {
                Ok(SecretFormat::LegacyDigest)
            } else {
                Err(EnrollDenial::BadSecret)
            }
        }
    }
}

pub(crate) async fn create_session_token(state: &AppState, service_id: Uuid) -> AppResult<String> {
    let raw_token = generate_token();
    let token_hash = crate::routes::private::api_tokens::service::hash_token(&raw_token);
    let ttl_secs = state.config.sync_session_token_ttl_secs as i64;

    let token = tokens::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(service_id),
        token_hash: Set(token_hash.clone()),
        expires_at: Set((Utc::now() + chrono::Duration::seconds(ttl_secs)).into()),
        created_at: Set(Utc::now().into()),
    };
    token.insert(&state.db).await?;
    tracing::debug!(%service_id, token_hash_prefix = %&token_hash[..8], "Session token created");

    let db_clone = state.db.clone();
    tokio::spawn(async move {
        let _ = tokens::Entity::delete_many()
            .filter(tokens::Column::ServiceId.eq(service_id))
            .filter(tokens::Column::ExpiresAt.lt(Utc::now()))
            .exec(&db_clone)
            .await;
    });

    Ok(raw_token)
}

/// The first error of a service's most recent cycle that reported one, or None when its recent
/// cycles were clean. One query for every service on the page.
/// The last error a sync service recorded, if it recorded one.
#[derive(FromQueryResult)]
pub(super) struct LastFailure {
    pub(super) service_id: Uuid,
    pub(super) error: Option<String>,
}

pub(super) async fn recent_errors<C: sea_orm::ConnectionTrait>(
    conn: &C,
    service_ids: &[Uuid],
) -> AppResult<std::collections::HashMap<Uuid, String>> {
    if service_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = conn
        .query_all_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (service_id) service_id, errors->>0 AS error
             FROM sync_events
             WHERE service_id = ANY($1)
               AND jsonb_typeof(errors) = 'array' AND jsonb_array_length(errors) > 0
             ORDER BY service_id, started_at DESC",
            [service_ids.to_vec().into()],
        ))
        .await?;
    let mut out = std::collections::HashMap::new();
    for row in &rows {
        let failure = LastFailure::from_query_result(row, "")?;
        if let Some(error) = failure.error {
            out.insert(failure.service_id, error);
        }
    }
    Ok(out)
}

pub(super) fn command_to_response(c: commands::Model) -> SyncCommandResponse {
    SyncCommandResponse {
        id: c.id,
        service_id: c.service_id,
        command: c.command,
        payload: c.payload,
        status: c.status,
        result: c.result,
        created_at: c.created_at.to_rfc3339(),
        expires_at: c.expires_at.to_rfc3339(),
        acknowledged_at: c.acknowledged_at.map(|t| t.to_rfc3339()),
        completed_at: c.completed_at.map(|t| t.to_rfc3339()),
    }
}

/// Command names an operator may queue, and the payload each accepts.
pub(super) const VALID_COMMANDS: [&str; 6] = [
    core_commands::TRIGGER_SYNC,
    core_commands::TRIGGER_FULL_SYNC,
    core_commands::PAUSE,
    core_commands::RESUME,
    core_commands::RESYNC_STREAMS,
    core_commands::SOURCE_AUDIT,
];

/// Refuse a command the driver would not run: an unknown name, or `resync_streams` without a
/// non-empty `source_keys` list of strings.
pub fn validate_command(command: &str, payload: Option<&serde_json::Value>) -> Result<(), String> {
    if !VALID_COMMANDS.contains(&command) {
        return Err(format!(
            "Invalid command '{command}'. Valid commands: {}",
            VALID_COMMANDS.join(", ")
        ));
    }
    if command != core_commands::RESYNC_STREAMS {
        return Ok(());
    }
    let keys = payload
        .and_then(|p| p.get("source_keys"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "resync_streams needs a payload with a source_keys list".to_string())?;
    if keys.is_empty() {
        return Err("resync_streams: source_keys is empty".to_string());
    }
    if let Some(bad) = keys.iter().find(|k| !k.is_string()) {
        return Err(format!(
            "resync_streams: source_keys entry {bad} is not a string"
        ));
    }
    Ok(())
}

/// Shortest cadence an operator may set. Matches the floor the sync runner applies, so the
/// portal refuses a number the service would silently override.
pub(super) const MIN_SYNC_INTERVAL_SECS: i32 = 30;

/// The one URL per sync service is the generated CRUD route, so the cadence floor and the error a
/// service's last cycle reported are hooks on it rather than a second handler beside it.
pub struct SyncServiceOperations;

impl crudcrate::CRUDOperations for SyncServiceOperations {
    type Resource = SyncService;

    /// The runner floors the cadence at `MIN_SYNC_INTERVAL_SECS`, so a number below it is refused
    /// here rather than accepted and silently overridden. An explicit null clears the override.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        _id: Uuid,
        data: &<SyncService as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), crudcrate::ApiError> {
        if let Some(Some(secs)) = data.sync_interval_secs
            && secs < MIN_SYNC_INTERVAL_SECS
        {
            return Err(crudcrate::ApiError::bad_request(format!(
                "sync_interval_secs must be at least {MIN_SYNC_INTERVAL_SECS} seconds"
            )));
        }
        Ok(())
    }

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut SyncService,
    ) -> Result<(), crudcrate::ApiError> {
        let id = entity.id;
        entity.last_error = recent_errors(db, &[id])
            .await
            .map_err(|e| crudcrate::ApiError::internal(e.to_string(), None))?
            .remove(&id);
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<SyncServiceList>,
    ) -> Result<(), crudcrate::ApiError> {
        let ids: Vec<Uuid> = entities.iter().map(|s| s.id).collect();
        let mut errors = recent_errors(db, &ids)
            .await
            .map_err(|e| crudcrate::ApiError::internal(e.to_string(), None))?;
        for entity in entities {
            entity.last_error = errors.remove(&entity.id);
        }
        Ok(())
    }
}

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
/// Confine a hold action to the caller's projects, resolved through the stream's paired site,
/// as the readings flag handlers do. An unpaired stream's hold (deferred) belongs to no project,
/// so it is actionable only by a caller without project restriction.
pub(super) async fn enforce_hold_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    hold_id: Uuid,
) -> AppResult<()> {
    let hold = Alias::new("h");
    let stream = Alias::new("ds");
    let slot = Alias::new("sp");
    let statement = SeaQuery::select()
        .expr_as(
            Func::coalesce::<[Expr; 2], _>([
                Expr::col((slot.clone(), site_parameters::models::Column::SiteId)),
                Expr::col((hold.clone(), hold_model::Column::SiteId)),
            ]),
            Alias::new("site_id"),
        )
        .from_as(hold_model::Entity, hold.clone())
        .join_as(
            JoinType::LeftJoin,
            data_streams::models::Entity,
            stream.clone(),
            Expr::col((stream.clone(), data_streams::models::Column::Id))
                .equals((hold.clone(), hold_model::Column::StreamId)),
        )
        .join_as(
            JoinType::LeftJoin,
            site_parameters::models::Entity,
            slot.clone(),
            Expr::col((slot, site_parameters::models::Column::Id))
                .equals((stream, data_streams::models::Column::SiteParameterId)),
        )
        .and_where(Expr::col((hold, hold_model::Column::Id)).eq(hold_id))
        .take();
    let (sql, values) = statement.build(PostgresQueryBuilder);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no replicate audit hold {hold_id}")))?;
    match row.try_get::<Option<Uuid>>("", "site_id")? {
        Some(site_id) => enforce_project_scope_for_sites(db, scope, &[site_id]).await,
        None => {
            if scope.is_restricted() {
                return Err(AppError::Forbidden(
                    "This hold's stream is unpaired, so it belongs to no project; only a caller \
                     without project restriction can act on it"
                        .to_string(),
                ));
            }
            Ok(())
        }
    }
}

/// Relative tolerance for the mean comparison. The portals store aggregates in MySQL FLOAT
/// columns, so bit-exact equality is not on the table.
pub const DEFAULT_REL_TOL: f64 = 1e-5;

/// Absolute floor, for values near zero where a relative bound collapses.
pub const DEFAULT_ABS_TOL: f64 = 1e-4;

/// The standard deviation gets a looser bound than the mean: against real portal data the stored
/// sd routinely disagrees with a recompute from its own replicate cells at the 1e-5 relative
/// level (FLOAT storage, historical R rounding chains), and a hold per micro-mismatch would bury
/// the real findings. A genuinely wrong sd (n divisor against n-1, stale after an edit) sits at
/// percent level and still trips this.
pub const SD_REL_TOL: f64 = 1e-3;

pub const SD_ABS_TOL: f64 = 1e-3;

/// The portals round aggregate cells to 2 decimals before storing, so a disagreement below half
/// the stored quantum is not auditable: the portal's own cell cannot represent it.
pub const PORTAL_QUANTUM: f64 = 0.005;

/// The floor under every tolerance bound, [`PORTAL_QUANTUM`] plus an epsilon so a delta of
/// exactly half the quantum stays inside. Shared by [`stats_agree_with`] and [`bound_sql`] so the
/// in-process comparison and the SQL one cannot drift apart.
pub const QUANTUM_FLOOR: f64 = PORTAL_QUANTUM + 1e-9;

/// The tolerance bound between two statistics: relative to the larger magnitude, with an absolute
/// floor, never below [`QUANTUM_FLOOR`].
#[must_use]
pub fn tolerance_bound(e: f64, c: f64, rel_tol: f64, abs_tol: f64) -> f64 {
    f64::max(rel_tol * f64::max(e.abs(), c.abs()), abs_tol).max(QUANTUM_FLOOR)
}

/// SQL for the same bound between two value expressions, with the relative tolerance bound as
/// `rel_bind`. The one producer of the tolerance in SQL form.
#[must_use]
pub fn bound_sql(a: &str, b: &str, rel_bind: &str, abs_tol: f64) -> String {
    format!("GREATEST({rel_bind} * GREATEST(abs({a}), abs({b})), {abs_tol}, {QUANTUM_FLOOR})")
}

/// The recomputed statistics of a group of would-be-stored values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupStats {
    pub n: usize,
    pub mean: Option<f64>,
    /// The sample standard deviation (n-1). None below n=2.
    pub sd: Option<f64>,
}

#[must_use]
pub fn group_stats(values: &[f64]) -> GroupStats {
    let n = values.len();
    if n == 0 {
        return GroupStats {
            n,
            mean: None,
            sd: None,
        };
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = values.iter().sum::<f64>() / n as f64;
    let sd = if n >= 2 {
        #[allow(clippy::cast_precision_loss)]
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        Some(var.sqrt())
    } else {
        None
    };
    GroupStats {
        n,
        mean: Some(mean),
        sd,
    }
}

/// Whether two statistics agree within tolerance. A missing side is not a mismatch: a portal
/// stores no sd for two of three nutrient families, and n=1 groups have no sd to compare.
#[must_use]
pub fn stats_agree(expected: Option<f64>, computed: Option<f64>, rel_tol: f64) -> bool {
    stats_agree_with(expected, computed, rel_tol, DEFAULT_ABS_TOL)
}

#[must_use]
pub fn stats_agree_with(
    expected: Option<f64>,
    computed: Option<f64>,
    rel_tol: f64,
    abs_tol: f64,
) -> bool {
    match (expected, computed) {
        (Some(e), Some(c)) => (e - c).abs() <= tolerance_bound(e, c, rel_tol, abs_tol),
        _ => true,
    }
}

/// Whether a group's recomputed statistics meet the source's claim: mean and sd within their
/// tolerances, and the count equal when the source stated one. The one comparison the audit,
/// the review queue and the preview all make.
#[must_use]
pub fn agrees(expected: &GroupAudit, stats: &GroupStats) -> bool {
    stats_agree(expected.expected_mean, stats.mean, DEFAULT_REL_TOL)
        && stats_agree_with(expected.expected_sd, stats.sd, SD_REL_TOL, SD_ABS_TOL)
        && expected
            .expected_n
            .is_none_or(|n| i64::try_from(stats.n) == Ok(n))
}

/// One stored value with the replicate index it is stored at. The index is the source's column
/// position and nothing renumbers it, so it is the only handle a resolution can flag by.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ReplicateValue {
    pub index: i16,
    pub value: f64,
}

/// The values a hold was recorded over, in the order the source sent them. Holds written before
/// the index travelled with the value hold bare numbers; their index is unrecoverable, because no
/// position in the array stands for one, so it reads as `None` rather than as the position.
#[must_use]
pub fn stored_values(computed: &serde_json::Value) -> Vec<(Option<i16>, f64)> {
    computed
        .get("values")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::Object(_) => Some((
                        v.get("index")
                            .and_then(serde_json::Value::as_i64)
                            .and_then(|i| i16::try_from(i).ok()),
                        f64_at(v, "value")?,
                    )),
                    _ => Some((None, v.as_f64()?)),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The audit verdict for one group.
#[derive(Debug, Clone, Serialize)]
pub struct GroupMismatch {
    pub time: DateTime<Utc>,
    pub expected_mean: Option<f64>,
    pub expected_sd: Option<f64>,
    pub expected_n: Option<i64>,
    pub computed_mean: Option<f64>,
    pub computed_sd: Option<f64>,
    pub n: usize,
    /// The stored values the statistics were computed over, each at its replicate index.
    pub values: Vec<ReplicateValue>,
}

/// Open holds: the ones still awaiting review, unique per (stream, group_time). Must match the
/// partial index predicate in m20260821_000002 exactly, since the upsert names it as its
/// conflict target. Everything else is a decision or an outcome and is never rewritten by the
/// gate.
pub(crate) static OPEN: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| HoldStatus::sql_list(&HoldStatus::OPEN));

/// The same set for a built predicate: `status IN (...)` without spelling the list.
#[must_use]
pub fn open_statuses() -> [&'static str; 2] {
    HoldStatus::OPEN.map(HoldStatus::as_str)
}

/// The most recent hold for a group, as the ingest gate reads it. Terminal decisions matter to
/// the gate as much as open holds: a re-detected disagreement must not reopen a group an
/// operator already ruled on.
pub struct LatestHold {
    pub time: DateTime<Utc>,
    pub status: String,
    pub id: Uuid,
    /// The portal expectation the hold was recorded against, for [`expected_changed`].
    pub expected: serde_json::Value,
}

/// Whether an incoming audit claim differs from the expectation a hold recorded, under the same
/// tolerances detection uses. A terminal decision stands against re-detection of the SAME
/// disagreement; a cycle whose expected statistics have moved is new evidence and opens a fresh
/// hold.
#[must_use]
pub fn expected_changed(recorded: &serde_json::Value, audit: &GroupAudit) -> bool {
    fn side_changed(a: Option<f64>, b: Option<f64>, rel_tol: f64, abs_tol: f64) -> bool {
        match (a, b) {
            (Some(_), Some(_)) => !stats_agree_with(a, b, rel_tol, abs_tol),
            (None, None) => false,
            _ => true,
        }
    }
    side_changed(
        f64_at(recorded, "mean"),
        audit.expected_mean,
        DEFAULT_REL_TOL,
        DEFAULT_ABS_TOL,
    ) || side_changed(
        f64_at(recorded, "sd"),
        audit.expected_sd,
        SD_REL_TOL,
        SD_ABS_TOL,
    ) || recorded.get("n").and_then(serde_json::Value::as_i64) != audit.expected_n
}

/// The most recent statistics hold per group for a stream at the given instants, any status.
///
/// Scoped to `replicate_stats`: the caller is the statistics audit deciding whether a group's
/// recorded expectation changed at source, and a `source_modified` or `brake_fired` row at the same
/// instant answers a different question.
pub async fn latest_holds<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    times: &[DateTime<Utc>],
) -> AppResult<Vec<LatestHold>> {
    let (Some(lo), Some(hi)) = (times.iter().min(), times.iter().max()) else {
        return Ok(Vec::new());
    };
    // Range bind + exact-match filter here: a timestamptz array bind panics in the driver, and
    // one batch's audit instants are contiguous anyway.
    let wanted: std::collections::HashSet<DateTime<Utc>> = times.iter().copied().collect();
    let latest = SeaQuery::select()
        .distinct_on([hold_model::Column::GroupTime])
        .columns([
            hold_model::Column::Id,
            hold_model::Column::GroupTime,
            hold_model::Column::Status,
            hold_model::Column::Expected,
        ])
        .from(hold_model::Entity)
        .and_where(Expr::col(hold_model::Column::StreamId).eq(stream_id))
        .and_where(
            Expr::col(hold_model::Column::GroupTime)
                .gte(sea_orm::prelude::DateTimeWithTimeZone::from(*lo)),
        )
        .and_where(
            Expr::col(hold_model::Column::GroupTime)
                .lte(sea_orm::prelude::DateTimeWithTimeZone::from(*hi)),
        )
        .and_where(Expr::col(hold_model::Column::Kind).eq(HoldKind::ReplicateStats.as_str()))
        .order_by(hold_model::Column::GroupTime, Order::Asc)
        .order_by(hold_model::Column::CreatedAt, Order::Desc)
        .order_by(hold_model::Column::Id, Order::Desc)
        .take();
    let (sql, values) = latest.build(PostgresQueryBuilder);
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    rows.iter()
        .filter_map(|r| {
            let entry = LatestHoldRow::from_query_result(r, "").map(|row| LatestHold {
                time: row.group_time.with_timezone(&Utc),
                status: row.status,
                id: row.id,
                expected: row.expected,
            });
            match entry {
                Ok(e) if wanted.contains(&e.time) => Some(Ok(e)),
                Ok(_) => None,
                Err(e) => Some(Err(e.into())),
            }
        })
        .collect()
}

/// The status a detection asks for: `pending` on a paired stream (the review queue), `deferred` on
/// an unpaired one, promoted to pending when the stream is paired.
#[must_use]
pub fn status_for(paired: bool) -> HoldStatus {
    if paired {
        HoldStatus::Pending
    } else {
        HoldStatus::Deferred
    }
}

/// What a hold is keyed by, which is also which open-unique index the upsert conflicts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldKey {
    /// A stream's replicate group at one instant.
    Stream {
        stream_id: Uuid,
        group_time: DateTime<Utc>,
    },
    /// A slot's instant, for a finding no stream produced (the event audit, an unverified entry).
    Slot {
        site_id: Uuid,
        parameter_id: Uuid,
        group_time: DateTime<Utc>,
    },
    /// A visit: a site and an instant with no parameter, for a finding about the field day
    /// itself rather than about any one of its slots.
    Visit {
        site_id: Uuid,
        group_time: DateTime<Utc>,
    },
    /// One standing hold per stream whatever the instant: the device behind the feed changed, and
    /// a second detection updates the standing row rather than adding one per sync cycle.
    StreamStanding { stream_id: Uuid },
}

/// A detection, in the shape every writer states it.
pub struct Hold<'a> {
    pub key: HoldKey,
    pub kind: HoldKind,
    pub expected: serde_json::Value,
    pub computed: serde_json::Value,
    pub delta: serde_json::Value,
    /// `pending` or `deferred`; see [`status_for`].
    pub status: HoldStatus,
    /// The calculation a finding is about, where one produced it.
    pub tool: Option<&'a str>,
}

impl Hold<'_> {
    /// The columns the key fills, and the conflict target that makes a re-detection an update of
    /// the same row rather than a duplicate. Each target is an open-only partial index, so a
    /// decision already taken is never rewritten: a detection beside a terminal hold inserts a
    /// fresh open row.
    pub(super) fn target(&self) -> (Vec<hold_model::Column>, Vec<Expr>, OnConflict) {
        use hold_model::Column;
        let instant = |t: DateTime<Utc>| Expr::val(sea_orm::prelude::DateTimeWithTimeZone::from(t));
        match self.key {
            HoldKey::Stream {
                stream_id,
                group_time,
            } => (
                vec![Column::StreamId, Column::GroupTime],
                vec![Expr::val(stream_id), instant(group_time)],
                OnConflict::columns([Column::StreamId, Column::GroupTime, Column::Kind])
                    .target_and_where(Expr::cust(format!("status IN {}", *OPEN)))
                    .to_owned(),
            ),
            HoldKey::Slot {
                site_id,
                parameter_id,
                group_time,
            } => (
                vec![Column::SiteId, Column::ParameterId, Column::GroupTime],
                vec![
                    Expr::val(site_id),
                    Expr::val(parameter_id),
                    instant(group_time),
                ],
                OnConflict::columns([
                    Column::Kind,
                    Column::SiteId,
                    Column::ParameterId,
                    Column::GroupTime,
                ])
                .target_and_where(Expr::cust(format!(
                    "stream_id IS NULL AND status = '{}'",
                    HoldStatus::Pending.as_str()
                )))
                .to_owned(),
            ),
            HoldKey::Visit {
                site_id,
                group_time,
            } => (
                vec![Column::SiteId, Column::GroupTime],
                vec![Expr::val(site_id), instant(group_time)],
                OnConflict::columns([Column::Kind, Column::SiteId, Column::GroupTime])
                    .target_and_where(Expr::cust(format!(
                        "stream_id IS NULL AND parameter_id IS NULL AND status = '{}'",
                        HoldStatus::Pending.as_str()
                    )))
                    .to_owned(),
            ),
            HoldKey::StreamStanding { stream_id } => (
                vec![Column::StreamId, Column::GroupTime],
                vec![Expr::val(stream_id), Expr::cust("NOW()")],
                OnConflict::column(Column::StreamId)
                    .target_and_where(Expr::cust(format!(
                        "kind = '{}' AND status IN {}",
                        HoldKind::SourceIdentityChanged.as_str(),
                        *OPEN
                    )))
                    .to_owned(),
            ),
        }
    }
}

/// The one statement every hold is written by. Read it back in a test rather than a database.
#[must_use]
pub fn hold_statement(hold: &Hold) -> sea_orm::sea_query::InsertStatement {
    use hold_model::Column;
    let (mut columns, mut values, mut conflict) = hold.target();
    columns.extend([
        Column::Kind,
        Column::Expected,
        Column::Computed,
        Column::Delta,
        Column::Status,
        Column::Tool,
    ]);
    values.extend([
        Expr::val(hold.kind.as_str()),
        Expr::val(hold.expected.clone()),
        Expr::val(hold.computed.clone()),
        Expr::val(hold.delta.clone()),
        Expr::val(hold.status.as_str()),
        Expr::val(hold.tool),
    ]);
    // A re-detection refreshes the payload and promotes a deferred hold, and never rewrites a
    // decision already taken.
    conflict
        .update_columns([
            Column::Expected,
            Column::Computed,
            Column::Delta,
            Column::Tool,
        ])
        .values([
            (Column::CreatedAt, Expr::cust("NOW()")),
            (
                Column::Status,
                Expr::cust(format!(
                    "CASE WHEN replicate_audit_holds.status = '{deferred}' \
                          AND EXCLUDED.status = '{pending}' \
                     THEN '{pending}' ELSE replicate_audit_holds.status END",
                    deferred = HoldStatus::Deferred.as_str(),
                    pending = HoldStatus::Pending.as_str(),
                )),
            ),
        ]);
    SeaQuery::insert()
        .into_table(hold_model::Entity)
        .columns(columns)
        .values_panic(values)
        .on_conflict(conflict)
        .to_owned()
}

/// The slots of [`plan_slots`] that hold a reading. A slot with none has nothing to re-derive, and
/// on the CNET apply of 2026-09-22 that was 1124 of 2852 (B418).
pub fn plan_slots_holding_readings(plan_id: Uuid) -> sea_orm::Select<data_streams::models::Entity> {
    let slot = |column| Expr::col((site_parameters::models::Entity, column));
    let reading = |column| Expr::col((readings::models::Entity, column));
    plan_slots(plan_id).filter(Expr::exists(
        SeaQuery::select()
            .expr(Expr::val(1))
            .from(readings::models::Entity)
            .and_where(
                reading(readings::models::Column::SiteId)
                    .eq(slot(site_parameters::models::Column::SiteId)),
            )
            .and_where(
                reading(readings::models::Column::ParameterId)
                    .eq(slot(site_parameters::models::Column::ParameterId)),
            )
            .take(),
    ))
}

/// The (site, parameter) slots a plan's streams are paired to, each once. The re-derivation after
/// an apply covers these and no others.
pub(super) fn plan_slots(plan_id: Uuid) -> sea_orm::Select<data_streams::models::Entity> {
    data_streams::models::Entity::find()
        .inner_join(site_parameters::models::Entity)
        .select_only()
        .column(site_parameters::models::Column::SiteId)
        .column(site_parameters::models::Column::ParameterId)
        .distinct()
        .filter(data_streams::models::Column::PairingPlanId.eq(plan_id))
}

/// Which streams' holds a pairing change moves.
#[derive(Debug, Clone, Copy)]
pub enum HoldScope {
    /// One stream.
    Stream(Uuid),
    /// Every stream a pairing plan owns.
    Plan(Uuid),
}

/// Pairing promotes a stream's deferred holds into the review queue; unpairing defers them again.
/// A slot-keyed hold names no stream and is never moved by either.
pub async fn repoint_holds<C: ConnectionTrait>(
    conn: &C,
    scope: HoldScope,
    paired: bool,
) -> AppResult<()> {
    let (to, from) = if paired {
        ("pending", "deferred")
    } else {
        ("deferred", "pending")
    };
    // The streams a plan owns are read through the `data_streams` entity rather than joined by
    // name.
    let stream_ids: Vec<Uuid> = match scope {
        HoldScope::Stream(stream_id) => vec![stream_id],
        HoldScope::Plan(plan_id) => {
            data_streams::models::Entity::find()
                .filter(data_streams::models::Column::PairingPlanId.eq(plan_id))
                .select_only()
                .column(data_streams::models::Column::Id)
                .into_tuple()
                .all(conn)
                .await?
        }
    };
    if stream_ids.is_empty() {
        return Ok(());
    }
    hold_model::Entity::update_many()
        .col_expr(hold_model::Column::Status, Expr::val(to))
        .filter(hold_model::Column::StreamId.is_in(stream_ids))
        .filter(hold_model::Column::Status.eq(from))
        .exec(conn)
        .await?;
    Ok(())
}

/// Record (or refresh) a hold. The open unique index makes a re-detection on every sync cycle an
/// update of the same row, never a duplicate; a deferred row found by a paired-stream detection is
/// promoted to pending.
pub async fn upsert_hold<C: ConnectionTrait>(conn: &C, hold: &Hold<'_>) -> AppResult<()> {
    let (sql, values) = hold_statement(hold).build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;
    Ok(())
}

/// Record (or refresh) a hold for a group whose statistics disagree with the source's own.
pub async fn upsert_stats_hold<C: ConnectionTrait>(
    conn: &C,
    stream_id: Uuid,
    mismatch: &GroupMismatch,
    status: HoldStatus,
) -> AppResult<()> {
    let mut expected = serde_json::json!({
        "mean": mismatch.expected_mean,
        "sd": mismatch.expected_sd,
    });
    let computed = serde_json::json!({
        "mean": mismatch.computed_mean,
        "sd": mismatch.computed_sd,
        "n": mismatch.n,
        "values": mismatch.values,
    });
    let mut delta = serde_json::json!({
        "mean": delta_of(mismatch.expected_mean, mismatch.computed_mean),
        "sd": delta_of(mismatch.expected_sd, mismatch.computed_sd),
    });
    if let Some(expected_n) = mismatch.expected_n {
        expected["n"] = expected_n.into();
        delta["n"] =
            i64::try_from(mismatch.n).map_or(serde_json::Value::Null, |n| (expected_n - n).into());
    }
    upsert_hold(
        conn,
        &Hold {
            key: HoldKey::Stream {
                stream_id,
                group_time: mismatch.time,
            },
            kind: HoldKind::ReplicateStats,
            expected,
            computed,
            delta,
            status,
            tool: None,
        },
    )
    .await
}

pub(super) fn delta_of(expected: Option<f64>, computed: Option<f64>) -> Option<f64> {
    Some(expected? - computed?)
}

/// Close a hold whose group now matches at source.
pub async fn close_hold<C: ConnectionTrait>(
    conn: &C,
    hold_id: Uuid,
    terminal_status: HoldStatus,
) -> AppResult<()> {
    hold_model::Entity::update_many()
        .col_expr(
            hold_model::Column::Status,
            Expr::val(terminal_status.as_str()),
        )
        .filter(hold_model::Column::Id.eq(hold_id))
        .exec(conn)
        .await?;
    Ok(())
}

pub(super) fn f64_at(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(serde_json::Value::as_f64)
}

/// Signature classification of a disagreement, first match wins. The signatures cover the
/// failure classes observed in the real portal data, so the review queue reads as a triage
/// list rather than columns of deltas.
#[must_use]
pub fn classify(expected: &serde_json::Value, computed: &serde_json::Value) -> &'static str {
    let expected_n = expected.get("n").and_then(serde_json::Value::as_i64);
    let computed_n = computed.get("n").and_then(serde_json::Value::as_i64);
    if let Some(en) = expected_n
        && computed_n != Some(en)
    {
        return "n_mismatch";
    }
    let expected_mean = f64_at(expected, "mean");
    let expected_sd = f64_at(expected, "sd");
    let computed_mean = f64_at(computed, "mean");
    let computed_sd = f64_at(computed, "sd");
    // An n-divisor sd relates to the sample one by sqrt((n-1)/n). The signature claims
    // the sd is the ONLY disagreement, so it requires the means to agree: a wrong mean with a
    // coincidentally n-divisor-shaped sd is not explained by the divisor.
    if let (Some(esd), Some(csd), Some(n)) = (expected_sd, computed_sd, computed_n)
        && n >= 2
        && stats_agree(expected_mean, computed_mean, DEFAULT_REL_TOL)
    {
        #[allow(clippy::cast_precision_loss)]
        let n_divisor = csd * (((n - 1) as f64) / n as f64).sqrt();
        if stats_agree_with(Some(esd), Some(n_divisor), SD_REL_TOL, SD_ABS_TOL) {
            return "source_sd_matches_n_divisor";
        }
    }
    // A stale cell frozen over the first k replicates before later ones were entered.
    let values: Vec<f64> = stored_values(computed)
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    if let Some(em) = expected_mean
        && values.len() >= 2
    {
        for k in 1..values.len() {
            #[allow(clippy::cast_precision_loss)]
            let prefix_mean = values[..k].iter().sum::<f64>() / k as f64;
            if (prefix_mean - em).abs() <= PORTAL_QUANTUM + 1e-9 {
                return "stale_subset";
            }
        }
    }
    "unexplained"
}

/// The next resolution object, stamped with the acting identity and time, with the previous one
/// appended under `history` so the decision trail survives reopen and re-resolve cycles.
pub(super) fn merged_resolution(
    prev: Option<serde_json::Value>,
    mut next: serde_json::Value,
    by: &str,
) -> serde_json::Value {
    next["by"] = by.into();
    next["at"] = Utc::now().to_rfc3339().into();
    if let Some(prev) = prev {
        let mut history = prev
            .get("history")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut stripped = prev;
        if let Some(obj) = stripped.as_object_mut() {
            obj.remove("history");
        }
        history.push(stripped);
        next["history"] = serde_json::Value::Array(history);
    }
    next
}

/// A statistic of one of the hold's jsonb documents, as a float: `(h.<doc>-><statistic>)::float8`.
///
/// The three documents a hold carries are opaque `serde_json::Value` on the entity, so the reader
/// names the statistic rather than a column; what stays built is the column the document lives in,
/// so a renamed column is a compile error here.
fn statistic(column: hold_model::Column, name: &str) -> Expr {
    use sea_orm::sea_query::extension::postgres::PgExpr as _;
    Expr::col((Alias::new("h"), column))
        .cast_json_field(name)
        .cast_as(Alias::new("float8"))
}

/// The one denominator every relative delta is normalised by: the MEAN magnitude, not each
/// statistic's own, because that is the scale an operator judges significance on (an sd off by 2
/// on a value of 150 is noise; a mean off by 2 is not).
pub fn scale_expr() -> Expr {
    greatest([
        abs(coalesce_zero(statistic(
            hold_model::Column::Expected,
            "mean",
        ))),
        abs(coalesce_zero(statistic(
            hold_model::Column::Computed,
            "mean",
        ))),
        Expr::val(1e-9),
    ])
}

/// How far the source and the recomputation disagree on one statistic, against the mean's scale.
pub fn relative_delta_of(name: &str) -> Expr {
    coalesce_zero(abs(statistic(hold_model::Column::Delta, name))).div(scale_expr())
}

/// One scalar per hold saying how large the disagreement is, whichever statistic carries it.
/// The list's per-row value, the sort and the threshold bulk acknowledge all read this one
/// producer, so what the UI shows and what the slider acknowledges can never disagree.
pub fn relative_delta_expr() -> Expr {
    greatest([
        coalesce_zero(abs(statistic(hold_model::Column::Delta, "mean"))),
        coalesce_zero(abs(statistic(hold_model::Column::Delta, "sd"))),
    ])
    .div(scale_expr())
}

/// The same bound as [`tolerance_bound`], between two value expressions.
fn bound_expr(a: Expr, b: Expr, rel_tol: f64, abs_tol: f64) -> Expr {
    greatest([
        Expr::val(rel_tol).mul(greatest([abs(a), abs(b)])),
        Expr::val(abs_tol),
        Expr::val(QUANTUM_FLOOR),
    ])
}

/// The source-sd-matches-n-divisor signature, over the alias `h` (`replicate_audit_holds`).
///
/// It reproduces exactly the arm [`classify`] returns `source_sd_matches_n_divisor` from: the
/// replicate counts agree (so `n_mismatch` cannot preempt it), the means agree, and the source's sd
/// is our sd under the other divisor, `s * sqrt((n-1)/n)`. The prefix search behind `stale_subset`
/// has no built spelling, but it is tested after this arm, so a row matching here is
/// `source_sd_matches_n_divisor` in both.
pub fn source_sd_matches_n_divisor_expr() -> Condition {
    use sea_orm::sea_query::extension::postgres::PgExpr as _;
    let expected_n =
        Expr::col((Alias::new("h"), hold_model::Column::Expected)).cast_json_field("n");
    let computed_n =
        Expr::col((Alias::new("h"), hold_model::Column::Computed)).cast_json_field("n");
    let expected_mean = statistic(hold_model::Column::Expected, "mean");
    let computed_mean = statistic(hold_model::Column::Computed, "mean");
    let expected_sd = statistic(hold_model::Column::Expected, "sd");
    let n_divisor_sd = statistic(hold_model::Column::Computed, "sd").mul(sqrt(
        statistic(hold_model::Column::Computed, "n")
            .sub(Expr::val(1))
            .div(statistic(hold_model::Column::Computed, "n")),
    ));
    Condition::all()
        .add(
            Condition::any().add(expected_n.clone().is_null()).add(
                expected_n
                    .cast_as(Alias::new("int"))
                    .eq(computed_n.clone().cast_as(Alias::new("int"))),
            ),
        )
        .add(computed_n.cast_as(Alias::new("int")).gte(2))
        .add(json_present(hold_model::Column::Expected, "mean"))
        .add(json_present(hold_model::Column::Computed, "mean"))
        .add(
            abs(expected_mean.clone().sub(computed_mean.clone())).lte(bound_expr(
                expected_mean,
                computed_mean,
                DEFAULT_REL_TOL,
                DEFAULT_ABS_TOL,
            )),
        )
        .add(json_present(hold_model::Column::Expected, "sd"))
        .add(json_present(hold_model::Column::Computed, "sd"))
        .add(
            abs(expected_sd.clone().sub(n_divisor_sd.clone())).lte(bound_expr(
                expected_sd,
                n_divisor_sd,
                SD_REL_TOL,
                SD_ABS_TOL,
            )),
        )
}

/// `h.<doc>-><statistic> IS NOT NULL`, the guard each comparison takes before reading a statistic.
fn json_present(column: hold_model::Column, name: &str) -> Expr {
    use sea_orm::sea_query::extension::postgres::PgExpr as _;
    Expr::col((Alias::new("h"), column))
        .cast_json_field(name)
        .is_not_null()
}

fn abs(e: Expr) -> Expr {
    Func::cust(Alias::new("abs")).arg(e).into()
}

fn sqrt(e: Expr) -> Expr {
    Func::cust(Alias::new("sqrt")).arg(e).into()
}

fn greatest<I: IntoIterator<Item = Expr>>(args: I) -> Expr {
    args.into_iter()
        .fold(Func::cust(Alias::new("GREATEST")), |f, a| f.arg(a))
        .into()
}

fn coalesce_zero(e: Expr) -> Expr {
    Func::coalesce([e, Expr::val(0)]).into()
}

/// The source-sd-matches-n-divisor signature, in SQL, over the alias `h` (`replicate_audit_holds`).
///
/// It reproduces exactly the arm [`classify`] returns `source_sd_matches_n_divisor` from: the
/// replicate counts agree (so `n_mismatch` cannot preempt it), the means agree, and the source's sd
/// is our sd under the other divisor, `s * sqrt((n-1)/n)`. The prefix search behind `stale_subset`
/// has no SQL spelling, but it is tested after this arm, so a row matching here is
/// `source_sd_matches_n_divisor` in both.
/// `the_sql_signature_and_classify_agree` pins that.
///
/// Built from [`bound_sql`] and the same tolerance constants the in-process comparison uses, so
/// the two spellings cannot drift. One producer: the list filter, the resolution gate, the bulk
/// skip and the declaration counts all read this, so what the UI counts and what the gate blocks
/// can never disagree.
pub static SOURCE_SD_MATCHES_N_DIVISOR_SQL: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| {
        let expected_mean = "(h.expected->>'mean')::float8";
        let computed_mean = "(h.computed->>'mean')::float8";
        let expected_sd = "(h.expected->>'sd')::float8";
        let n_divisor_sd = "((h.computed->>'sd')::float8 \
                         * sqrt(((h.computed->>'n')::float8 - 1) / (h.computed->>'n')::float8))";
        let mean_bound = bound_sql(
            expected_mean,
            computed_mean,
            &DEFAULT_REL_TOL.to_string(),
            DEFAULT_ABS_TOL,
        );
        let sd_bound = bound_sql(
            expected_sd,
            n_divisor_sd,
            &SD_REL_TOL.to_string(),
            SD_ABS_TOL,
        );
        format!(
            "((h.expected->>'n') IS NULL \
           OR (h.expected->>'n')::int = (h.computed->>'n')::int) \
         AND (h.computed->>'n')::int >= 2 \
         AND (h.expected->>'mean') IS NOT NULL AND (h.computed->>'mean') IS NOT NULL \
         AND abs({expected_mean} - {computed_mean}) <= {mean_bound} \
         AND (h.expected->>'sd') IS NOT NULL AND (h.computed->>'sd') IS NOT NULL \
         AND abs({expected_sd} - {n_divisor_sd}) <= {sd_bound}"
        )
    });

// --- The review queue's statements ---

/// A built statement, rendered once for the connection to run. Rendering lives here rather than at
/// the call site so a handler never holds SQL text.
pub fn built(statement: SelectStatement) -> Statement {
    let (sql, values) = statement.build(PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

/// A column of the hold, under the alias `h` every expression in this file is written over.
fn hold_col(column: hold_model::Column) -> Expr {
    Expr::col((Alias::new("h"), column))
}

/// A column of the hold's stream, under the alias `ds`.
fn stream_col(column: data_streams::models::Column) -> Expr {
    Expr::col((Alias::new("ds"), column))
}

/// `COUNT(*) FILTER (WHERE condition)`, as a count over a CASE that is NULL where the condition
/// does not hold. An empty set counts zero, which is what the page reads the number as.
fn count_where(condition: Condition) -> Expr {
    let case: Expr = sea_orm::sea_query::CaseStatement::new()
        .case(condition, 1)
        .into();
    Expr::from(Func::count(case)).cast_as(Alias::new("bigint"))
}

/// The caller's projects, reached either through the hold's stream pairing or through the site a
/// stream-less finding names; `None` when the caller is unrestricted.
///
/// A restricted caller sees only holds whose stream is paired into their projects; unpaired
/// (deferred) holds belong to no project and are visible only without project restriction.
fn holds_in_scope(scope: &AccessScope) -> Option<Condition> {
    let confine = || {
        crate::common::scope::project_filter(scope, (Alias::new("st"), sites::Column::ProjectId))
    };
    let paired = Expr::exists(
        SeaQuery::select()
            .expr(Expr::val(1))
            .from_as(site_parameters::Entity, Alias::new("sp_scope"))
            .join_as(
                JoinType::InnerJoin,
                sites::Entity,
                Alias::new("st"),
                Expr::col((Alias::new("st"), sites::Column::Id))
                    .equals((Alias::new("sp_scope"), site_parameters::Column::SiteId)),
            )
            .and_where(
                Expr::col((Alias::new("sp_scope"), site_parameters::Column::Id)).equals((
                    Alias::new("ds"),
                    data_streams::models::Column::SiteParameterId,
                )),
            )
            .and_where(confine()?)
            .to_owned(),
    );
    let own_site = Expr::exists(
        SeaQuery::select()
            .expr(Expr::val(1))
            .from_as(sites::Entity, Alias::new("st"))
            .and_where(
                Expr::col((Alias::new("st"), sites::Column::Id))
                    .equals((Alias::new("h"), hold_model::Column::SiteId)),
            )
            .and_where(confine()?)
            .to_owned(),
    );
    Some(Condition::any().add(paired).add(own_site))
}

/// The filters the queue's three statements share: the caller's project confinement, the identity
/// and source filters, the three relative-delta ceilings and the two classification arms.
///
/// The status view is not among them. It is applied per statement, so `pending` and `deferred`
/// report the whole backlog under these filters whichever view the page shows.
pub fn hold_filters(scope: &AccessScope, query: &ListHoldsQuery) -> AppResult<Condition> {
    let mut filters = Condition::all();
    if let Some(in_scope) = holds_in_scope(scope) {
        filters = filters.add(in_scope);
    }
    if let Some(id) = query.id {
        filters = filters.add(hold_col(hold_model::Column::Id).eq(id));
    }
    if let Some(stream_id) = query.stream_id {
        filters = filters.add(hold_col(hold_model::Column::StreamId).eq(stream_id));
    }
    if let Some(stream_ids) = query.stream_ids.as_deref().filter(|s| !s.is_empty()) {
        let ids: Vec<Uuid> = stream_ids
            .split(',')
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| AppError::BadRequest(format!("invalid stream id '{s}'")))
            })
            .collect::<Result<_, _>>()?;
        filters = filters.add(hold_col(hold_model::Column::StreamId).is_in(ids));
    }
    if let Some(source_system) = query.source_system.clone() {
        filters =
            filters.add(stream_col(data_streams::models::Column::SourceSystem).eq(source_system));
    }
    if let Some(ceiling) = query.max_relative_delta {
        filters = filters.add(relative_delta_expr().lte(ceiling));
    }
    if let Some(ceiling) = query.max_mean_relative_delta {
        filters = filters.add(relative_delta_of("mean").lte(ceiling));
    }
    if let Some(ceiling) = query.max_sd_relative_delta {
        filters = filters.add(relative_delta_of("sd").lte(ceiling));
    }
    let replicate_stats =
        || hold_col(hold_model::Column::Kind).eq(HoldKind::ReplicateStats.as_str());
    match query.classification.as_deref() {
        Some("source_sd_matches_n_divisor") => {
            filters = filters
                .add(replicate_stats())
                .add(source_sd_matches_n_divisor_expr());
        }
        Some("not_source_sd_matches_n_divisor") => {
            // COALESCE, not a bare NOT: a hold missing a statistic leaves the signature NULL, and
            // a NULL is not the n-divisor signature, so it belongs to the complement. This is
            // what makes the two filters partition the replicate-stats holds exactly.
            filters = filters.add(replicate_stats()).add(
                Expr::from(Func::coalesce([
                    Expr::from(source_sd_matches_n_divisor_expr()),
                    Expr::val(false),
                ]))
                .not(),
            );
        }
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "classification '{other}' has no filter; only 'source_sd_matches_n_divisor' and \
                 'not_source_sd_matches_n_divisor' are filterable"
            )));
        }
        None => {}
    }
    if let Some(kinds) = query.kind.as_deref() {
        let names = HoldKind::parse_list(kinds)?
            .into_iter()
            .map(HoldKind::as_str);
        filters = filters.add(hold_col(hold_model::Column::Kind).is_in(names));
    }
    if let Some(tool) = query.tool.clone() {
        filters = filters.add(hold_col(hold_model::Column::Tool).eq(tool));
    }
    if let Some(site_id) = query.site_id {
        filters = filters.add(hold_at_slot(
            hold_model::Column::SiteId,
            site_parameters::Column::SiteId,
            site_id,
        ));
    }
    if let Some(parameter_id) = query.parameter_id {
        filters = filters.add(hold_at_slot(
            hold_model::Column::ParameterId,
            site_parameters::Column::ParameterId,
            parameter_id,
        ));
    }
    if let Some(from) = query.from {
        filters = filters.add(hold_col(hold_model::Column::GroupTime).gte(from));
    }
    if let Some(to) = query.to {
        filters = filters.add(hold_col(hold_model::Column::GroupTime).lt(to));
    }
    Ok(filters)
}

/// The hold names `id` in `own`, or its stream is paired into a slot whose `paired` is `id`.
fn hold_at_slot(own: hold_model::Column, paired: site_parameters::Column, id: Uuid) -> Condition {
    let through_pairing = Expr::exists(
        SeaQuery::select()
            .expr(Expr::val(1))
            .from_as(site_parameters::Entity, Alias::new("sp_slot"))
            .and_where(
                Expr::col((Alias::new("sp_slot"), site_parameters::Column::Id)).equals((
                    Alias::new("ds"),
                    data_streams::models::Column::SiteParameterId,
                )),
            )
            .and_where(Expr::col((Alias::new("sp_slot"), paired)).eq(id))
            .to_owned(),
    );
    Condition::any()
        .add(hold_col(own).eq(id))
        .add(through_pairing)
}

/// The status view the page asks for: one status, the `resolved` set, every status, or the
/// pending queue.
pub fn hold_status_condition(status: Option<&str>) -> AppResult<Condition> {
    let column = hold_col(hold_model::Column::Status);
    match status {
        Some("any") => Ok(Condition::all()),
        Some("resolved") => {
            Ok(Condition::all().add(column.is_in(HoldStatus::RESOLVED.iter().map(|s| s.as_str()))))
        }
        Some(other) => match HoldStatus::parse(other) {
            Some(status) => Ok(Condition::all().add(column.eq(status.as_str()))),
            None => Err(AppError::BadRequest(format!(
                "unknown hold status '{other}'"
            ))),
        },
        None => Ok(Condition::all().add(column.eq(HoldStatus::Pending.as_str()))),
    }
}

/// How the queue is ordered: newest first, or largest disagreement first, which is what lets an
/// operator triage the whole backlog across pages.
pub fn hold_order_by(sort: Option<&str>) -> AppResult<Vec<(Expr, Order)>> {
    let created = || (hold_col(hold_model::Column::CreatedAt), Order::Desc);
    match sort {
        None | Some("created_at_desc") => Ok(vec![created()]),
        Some("relative_delta_desc") => Ok(vec![(relative_delta_expr(), Order::Desc), created()]),
        Some("relative_delta_asc") => Ok(vec![(relative_delta_expr(), Order::Asc), created()]),
        Some(other) => Err(AppError::BadRequest(format!("unknown sort '{other}'"))),
    }
}

/// `replicate_audit_holds h LEFT JOIN data_streams ds`, the reach every shared filter needs.
fn holds_with_stream() -> SelectStatement {
    let mut q = SeaQuery::select();
    q.from_as(hold_model::Entity, Alias::new("h")).join_as(
        JoinType::LeftJoin,
        data_streams::models::Entity,
        Alias::new("ds"),
        stream_col(data_streams::models::Column::Id)
            .equals((Alias::new("h"), hold_model::Column::StreamId)),
    );
    q.take()
}

/// What the page shows above the queue: the total under the asked-for view, and the pending and
/// deferred backlogs under the same filters.
pub fn hold_counts_statement(filters: &Condition, status: &Condition) -> SelectStatement {
    let at =
        |s: HoldStatus| Condition::all().add(hold_col(hold_model::Column::Status).eq(s.as_str()));
    let mut q = holds_with_stream();
    q.expr_as(count_where(status.clone()), Alias::new("total"))
        .expr_as(count_where(at(HoldStatus::Pending)), Alias::new("pending"))
        .expr_as(
            count_where(at(HoldStatus::Deferred)),
            Alias::new("deferred"),
        )
        .cond_where(filters.clone());
    q.take()
}

/// The pending backlog split by kind: the entry points announce what is waiting, and a fired brake
/// is not a replicate-statistics disagreement.
pub fn hold_kind_counts_statement(filters: &Condition) -> SelectStatement {
    let mut q = holds_with_stream();
    q.column((Alias::new("h"), hold_model::Column::Kind))
        .expr_as(
            Expr::from(Func::count(Expr::val(1))).cast_as(Alias::new("bigint")),
            Alias::new("n"),
        )
        .cond_where(
            filters
                .clone()
                .add(hold_col(hold_model::Column::Status).eq(HoldStatus::Pending.as_str())),
        )
        .group_by_col((Alias::new("h"), hold_model::Column::Kind));
    q.take()
}

/// One page of the queue, with the slot labels a reviewer reads a hold by. A stream-less finding
/// carries its own site and parameter, so each label falls back to the pair the hold names.
pub fn hold_list_statement(
    filters: &Condition,
    status: &Condition,
    order_by: &[(Expr, Order)],
    limit: u64,
    offset: u64,
) -> SelectStatement {
    let sp = Alias::new("sp");
    let site = Alias::new("s");
    let parameter = Alias::new("p");
    let event_site = Alias::new("es");
    let event_parameter = Alias::new("ep");
    let event_slot = Alias::new("esp");
    let mut q = holds_with_stream();
    q.join_as(
        JoinType::LeftJoin,
        site_parameters::Entity,
        sp.clone(),
        Expr::col((sp.clone(), site_parameters::Column::Id)).equals((
            Alias::new("ds"),
            data_streams::models::Column::SiteParameterId,
        )),
    )
    .join_as(
        JoinType::LeftJoin,
        sites::Entity,
        site.clone(),
        Expr::col((site.clone(), sites::Column::Id))
            .equals((sp.clone(), site_parameters::Column::SiteId)),
    )
    .join_as(
        JoinType::LeftJoin,
        parameters::Entity,
        parameter.clone(),
        Expr::col((parameter.clone(), parameters::Column::Id))
            .equals((sp.clone(), site_parameters::Column::ParameterId)),
    )
    .join_as(
        JoinType::LeftJoin,
        sites::Entity,
        event_site.clone(),
        Expr::col((event_site.clone(), sites::Column::Id))
            .equals((Alias::new("h"), hold_model::Column::SiteId)),
    )
    .join_as(
        JoinType::LeftJoin,
        parameters::Entity,
        event_parameter.clone(),
        Expr::col((event_parameter.clone(), parameters::Column::Id))
            .equals((Alias::new("h"), hold_model::Column::ParameterId)),
    )
    .join_as(
        JoinType::LeftJoin,
        site_parameters::Entity,
        event_slot.clone(),
        Condition::all()
            .add(
                Expr::col((event_slot.clone(), site_parameters::Column::SiteId))
                    .equals((Alias::new("h"), hold_model::Column::SiteId)),
            )
            .add(
                Expr::col((event_slot.clone(), site_parameters::Column::ParameterId))
                    .equals((Alias::new("h"), hold_model::Column::ParameterId)),
            ),
    );
    for column in [
        hold_model::Column::Id,
        hold_model::Column::StreamId,
        hold_model::Column::Kind,
    ] {
        q.column((Alias::new("h"), column));
    }
    for column in [
        data_streams::models::Column::SourceSystem,
        data_streams::models::Column::SourceKey,
        data_streams::models::Column::SourceName,
    ] {
        q.column((Alias::new("ds"), column));
    }
    q.expr_as(
        Func::coalesce([
            Expr::col((site.clone(), sites::Column::Id)),
            Expr::col((event_site.clone(), sites::Column::Id)),
        ]),
        Alias::new("site_id"),
    )
    .expr_as(
        Func::coalesce([
            stream_col(data_streams::models::Column::SiteParameterId),
            Expr::col((event_slot, site_parameters::Column::Id)),
        ]),
        Alias::new("site_parameter_id"),
    )
    .expr_as(
        Func::coalesce([
            Expr::col((site, sites::Column::Name)),
            Expr::col((event_site, sites::Column::Name)),
        ]),
        Alias::new("site_name"),
    )
    .expr_as(
        Func::coalesce([
            Expr::col((parameter.clone(), parameters::Column::Name)),
            Expr::col((event_parameter.clone(), parameters::Column::Name)),
        ]),
        Alias::new("parameter_name"),
    )
    .expr_as(
        Func::coalesce([
            Expr::col((parameter, parameters::Column::Code)),
            Expr::col((event_parameter, parameters::Column::Code)),
        ]),
        Alias::new("parameter_code"),
    )
    .column((Alias::new("h"), hold_model::Column::Tool))
    .expr_as(
        stream_col(data_streams::models::Column::SiteParameterId).is_not_null(),
        Alias::new("paired"),
    );
    for column in [
        hold_model::Column::GroupTime,
        hold_model::Column::Expected,
        hold_model::Column::Computed,
        hold_model::Column::Delta,
        hold_model::Column::Status,
    ] {
        q.column((Alias::new("h"), column));
    }
    // The signature is computed per row from the two documents once the page is decoded; the
    // column is here so the decoder has one to fill.
    q.expr_as(
        Expr::val("").cast_as(Alias::new("text")),
        Alias::new("classification"),
    );
    for column in [
        hold_model::Column::Resolution,
        hold_model::Column::CreatedAt,
        hold_model::Column::AcknowledgedBy,
        hold_model::Column::AcknowledgedAt,
    ] {
        q.column((Alias::new("h"), column));
    }
    q.expr_as(relative_delta_expr(), Alias::new("relative_delta"))
        .expr_as(relative_delta_of("mean"), Alias::new("mean_relative_delta"))
        .expr_as(relative_delta_of("sd"), Alias::new("sd_relative_delta"))
        .cond_where(filters.clone().add(status.clone()))
        .limit(limit)
        .offset(offset);
    for (expr, order) in order_by {
        q.order_by_expr(expr.clone(), order.clone());
    }
    q.take()
}

/// The row shapes the raw hold queries return. A derived decoder is checked against the SELECT it
/// fills, so a column renamed in one query and not in its mapper fails where the query is written.
#[derive(FromQueryResult)]
pub(super) struct LatestHoldRow {
    pub(super) group_time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) status: String,
    pub(super) id: Uuid,
    pub(super) expected: serde_json::Value,
}

#[derive(FromQueryResult)]
pub(super) struct KindCountRow {
    pub(super) kind: String,
    pub(super) n: i64,
}

#[derive(FromQueryResult)]
pub(super) struct ReplicateStateRow {
    pub(super) replicate_index: i16,
    pub(super) flagged: bool,
}

#[derive(FromQueryResult)]
pub(super) struct HoldCountsRow {
    pub(super) total: i64,
    pub(super) pending: i64,
    pub(super) deferred: i64,
}

#[derive(FromQueryResult)]
pub(super) struct FlagHoldRow {
    pub(super) stream_id: Uuid,
    pub(super) group_time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) resolution: Option<serde_json::Value>,
    pub(super) computed: serde_json::Value,
}

#[derive(FromQueryResult)]
pub(super) struct ReopenHoldRow {
    pub(super) stream_id: Uuid,
    pub(super) group_time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) status: String,
    pub(super) paired: bool,
    pub(super) resolution: Option<serde_json::Value>,
}

/// SQL fragment producing an acknowledgement's resolution object (actor and time stamped on the
/// entry) while preserving any prior actions under `history` (a reopened hold can be
/// re-resolved). The action is the purpose's own, so a resolution record names what was decided.
pub(super) fn acknowledgement_resolution(action: &str, by: &str) -> Expr {
    Expr::cust_with_values(
        "CASE \
         WHEN h.resolution IS NULL \
             THEN jsonb_build_object('action', $1::text, 'by', $2::text, 'at', NOW()) \
         ELSE jsonb_build_object('action', $1::text, 'by', $2::text, 'at', NOW(), \
              'history', \
              COALESCE(h.resolution->'history', '[]'::jsonb) \
                  || jsonb_build_array(h.resolution - 'history')) \
         END",
        [sea_orm::Value::from(action), sea_orm::Value::from(by)],
    )
}

/// What an acknowledgement is for. Each purpose owns the kinds one screen raises, and a route
/// takes a purpose, so a route cannot reach another purpose's rows or mint another purpose's
/// sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The reconciliation brake held a pass and a person admits it.
    StreamBrake,
    /// A calculation output the audit reported as missing, stale or skipped.
    CalculationFinding,
    /// The source reports an instrument identity that is not the one on record.
    IdentityChange,
    /// The source changed a value river-data had already stored, and the correction applied.
    SourceCorrection,
    /// The statistics recomputed from the stored replicates stand.
    Statistics,
}

impl Purpose {
    pub const ALL: [Self; 5] = [
        Self::StreamBrake,
        Self::CalculationFinding,
        Self::IdentityChange,
        Self::SourceCorrection,
        Self::Statistics,
    ];

    /// The kinds this purpose acknowledges, and no others.
    #[must_use]
    pub fn kinds(self) -> &'static [HoldKind] {
        match self {
            Self::StreamBrake => &[HoldKind::BrakeFired],
            Self::CalculationFinding => &[
                HoldKind::MissingOutput,
                HoldKind::StaleOutput,
                HoldKind::SkippedOutput,
            ],
            Self::IdentityChange => &[HoldKind::SourceIdentityChanged],
            Self::SourceCorrection => &[HoldKind::SourceModified],
            Self::Statistics => &[HoldKind::ReplicateStats],
        }
    }

    #[must_use]
    pub fn owns(self, kind: HoldKind) -> bool {
        self.kinds().contains(&kind)
    }

    /// The action recorded in the hold's `resolution`.
    #[must_use]
    pub fn action(self) -> &'static str {
        match self {
            Self::StreamBrake => "release_brake",
            Self::CalculationFinding => "dismiss_finding",
            Self::IdentityChange => "accept_identity",
            Self::SourceCorrection => "accept_correction",
            Self::Statistics => "accept_ours",
        }
    }

    /// The sentence this purpose puts on the chart. The numbers belong to the statistics
    /// sentence alone: another kind's `expected` carries a window or an output name, not a mean.
    #[must_use]
    pub fn sentence(
        self,
        expected: &serde_json::Value,
        computed: &serde_json::Value,
        by: &str,
    ) -> String {
        match self {
            Self::StreamBrake => format!(
                "Brake released: one braked-scale reconciliation pass is admitted. Released by {by}."
            ),
            Self::CalculationFinding => {
                format!(
                    "Calculation finding dismissed: no recomputation is owed here. Dismissed by {by}."
                )
            }
            Self::IdentityChange => format!(
                "Identity change accepted: the instrument the source reports stands. Accepted by {by}."
            ),
            Self::SourceCorrection => format!(
                "Source correction reviewed: the curation on the corrected reading stands. Reviewed by {by}."
            ),
            Self::Statistics => format!(
                "Audit accepted: the statistics computed here stand ({}). Accepted by {by}.",
                disagreement_phrase(expected, computed)
            ),
        }
    }
}

/// The audit annotation category. Minted server-side only; the annotate dialog does not offer it.
pub(super) const AUDIT_ANNOTATION_CATEGORY: &str = "audit";

/// Put an audit decision on the charts for the instant it concerns.
///
/// A hold lives in a queue nobody reads while looking at a plot, so a decision about a value is
/// invisible exactly where the value is. This mints a point annotation at the group's instant on
/// the slot the hold sits on, carrying both numbers and what was decided, and the existing chart
/// band, tooltip and chip machinery renders it with no further wiring.
///
/// Best-effort by design: an unpaired stream resolves to no slot, and a decision must not fail
/// because it could not also be drawn. Failures are logged, never returned.
pub(super) async fn mint_audit_annotation<C: ConnectionTrait>(
    conn: &C,
    hold_id: Uuid,
    text: &str,
    by: &str,
) {
    let result = conn
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO annotations
                 (site_id, parameter_id, start_time, end_time, text, category,
                  created_by, audit_hold_id)
             SELECT COALESCE(sp.site_id, h.site_id), COALESCE(sp.parameter_id, h.parameter_id),
                    h.group_time, h.group_time, $2, $3, $4, h.id
             FROM replicate_audit_holds h
             LEFT JOIN data_streams ds ON ds.id = h.stream_id
             LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
             WHERE h.id = $1
               AND COALESCE(sp.site_id, h.site_id) IS NOT NULL
               AND COALESCE(sp.parameter_id, h.parameter_id) IS NOT NULL",
            [
                hold_id.into(),
                text.to_string().into(),
                AUDIT_ANNOTATION_CATEGORY.into(),
                by.to_string().into(),
            ],
        ))
        .await;
    if let Err(e) = result {
        tracing::warn!("could not annotate audit hold {hold_id}: {e}");
    }
}

/// Remove the annotations a hold's decisions minted. Runs on reopen, inside its transaction: the
/// note said a decision had been taken, and it has not any more.
pub(super) async fn delete_audit_annotations<C: ConnectionTrait>(
    conn: &C,
    hold_id: Uuid,
) -> AppResult<()> {
    annotations::models::Entity::delete_many()
        .filter(annotations::models::Column::AuditHoldId.eq(hold_id))
        .exec(conn)
        .await?;
    Ok(())
}

/// The numbers a hold disagrees over, phrased for an annotation: what the source stored against
/// what the replicates produce.
pub(super) fn disagreement_phrase(
    expected: &serde_json::Value,
    computed: &serde_json::Value,
) -> String {
    let fmt = |v: Option<f64>| v.map_or_else(|| "none".to_string(), |v| format!("{v:.4}"));
    format!(
        "source mean {} sd {}, recomputed mean {} sd {} over {} replicates",
        fmt(f64_at(expected, "mean")),
        fmt(f64_at(expected, "sd")),
        fmt(f64_at(computed, "mean")),
        fmt(f64_at(computed, "sd")),
        computed
            .get("n")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    )
}

/// Acknowledge one pending hold for the purpose the route owns: the hold goes terminal, the
/// decision is recorded on it under that purpose's action, and the sentence that draws it on the
/// chart is minted. A hold of a kind the purpose does not own is refused, so a route cannot mint
/// another purpose's sentence over it.
pub(super) async fn acknowledge(
    state: &AppState,
    id: Uuid,
    purpose: Purpose,
    by: &str,
) -> AppResult<()> {
    let hold = hold_model::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no replicate audit hold {id}")))?;
    if !HoldKind::parse(&hold.kind).is_ok_and(|kind| purpose.owns(kind)) {
        return Err(AppError::BadRequest(format!(
            "hold {id} is of kind '{}', which this route does not acknowledge",
            hold.kind
        )));
    }
    // The resolution expression names the row as `h`, so the statement aliases the table.
    let (sql, values) = SeaQuery::update()
        .table(
            sea_orm::sea_query::IntoTableRef::into_table_ref(hold_model::Entity)
                .alias(Alias::new("h")),
        )
        .value(
            hold_model::Column::Status,
            Expr::val(HoldStatus::Acknowledged.as_str()),
        )
        .value(
            hold_model::Column::Resolution,
            acknowledgement_resolution(purpose.action(), by),
        )
        .value(hold_model::Column::AcknowledgedBy, Expr::val(by))
        .value(hold_model::Column::AcknowledgedAt, Expr::cust("NOW()"))
        .and_where(Expr::col(hold_model::Column::Id).eq(id))
        .and_where(Expr::col(hold_model::Column::Status).eq(HoldStatus::Pending.as_str()))
        .to_owned()
        .build(PostgresQueryBuilder);
    let updated = state
        .db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound(format!(
            "no pending replicate audit hold {id}"
        )));
    }
    mint_audit_annotation(
        &state.db,
        id,
        &purpose.sentence(&hold.expected, &hold.computed, by),
        by,
    )
    .await;
    Ok(())
}

/// The visit at a site and instant, when one stands there.
async fn visit_at<C: sea_orm::ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    at: sea_orm::prelude::DateTimeWithTimeZone,
) -> AppResult<Option<collection_events::Model>> {
    use collection_events::{Column, Entity};
    Ok(Entity::find()
        .filter(Column::SiteId.eq(site_id))
        .filter(Column::CollectedAt.eq(at))
        .one(conn)
        .await?)
}

/// The field day happened: it is no longer awaiting a ruling. Its measurements are untouched.
async fn accept_visit<C: sea_orm::ConnectionTrait>(conn: &C, visit_id: Uuid) -> AppResult<()> {
    collection_events::Entity::update_many()
        .col_expr(collection_events::Column::Unverified, Expr::val(false))
        .filter(collection_events::Column::Id.eq(visit_id))
        .exec(conn)
        .await?;
    Ok(())
}

/// Withdraw every reading entered at the visit. Each withdrawal is a decision on the record like
/// any other (ADR 0008), so a reassert restores it.
async fn withdraw_visit_readings<C: sea_orm::ConnectionTrait>(
    conn: &C,
    visit_id: Uuid,
    by: &str,
    reason: &str,
) -> AppResult<u64> {
    use crate::routes::private::collection_events::flows::row;
    use crate::routes::private::readings::models::{Column, Kind, Origin};
    use crate::routes::private::readings::service::{NewValue, record_many};
    let rejected = record_many(
        conn,
        Kind::Reject,
        sea_orm::Condition::all()
            .add(row(Column::CollectionEventId).eq(visit_id))
            .add(Expr::col((Alias::new("r"), Column::WithdrawnAt)).is_null()),
        NewValue::Literal(serde_json::json!({ "reason": reason })),
        by,
        Some(reason),
        Origin::Audit,
        None,
    )
    .await?;
    Ok(rejected.rows)
}

/// The field day should not have been opened. Nothing deletes it: the stamp is what a reader sees.
async fn withdraw_visit<C: sea_orm::ConnectionTrait>(conn: &C, visit_id: Uuid) -> AppResult<()> {
    collection_events::Entity::update_many()
        .col_expr(collection_events::Column::Unverified, Expr::val(false))
        .col_expr(collection_events::Column::WithdrawnAt, Expr::cust("NOW()"))
        .filter(collection_events::Column::Id.eq(visit_id))
        .exec(conn)
        .await?;
    Ok(())
}

/// Record the ruling on the hold, naming the visit it was about and how many readings moved with
/// it.
async fn decide_visit_hold<C: sea_orm::ConnectionTrait>(
    conn: &C,
    id: Uuid,
    mode: &str,
    status: &str,
    by: &str,
    visit_id: Uuid,
    withdrawn: u64,
) -> AppResult<()> {
    hold_model::Entity::update_many()
        .col_expr(hold_model::Column::Status, Expr::val(status))
        .col_expr(hold_model::Column::AcknowledgedBy, Expr::val(by))
        .col_expr(hold_model::Column::AcknowledgedAt, Expr::cust("NOW()"))
        .col_expr(
            hold_model::Column::Resolution,
            Expr::cust_with_values(
                "jsonb_build_object('mode', $1::text, 'by', $2::text, \
                 'at', to_jsonb(NOW()), 'visit_id', $3::text, 'rows', $4::bigint)",
                [
                    sea_orm::Value::from(mode),
                    sea_orm::Value::from(by),
                    sea_orm::Value::from(visit_id.to_string()),
                    sea_orm::Value::from(i64::try_from(withdrawn).unwrap_or(i64::MAX)),
                ],
            ),
        )
        .filter(hold_model::Column::Id.eq(id))
        .exec(conn)
        .await?;
    Ok(())
}

/// Rule on an intern's field day (Q177): `verify` says the visit should exist, `reject` withdraws
/// it with every reading attached to it. Nothing deletes a visit.
///
/// Verifying the visit verifies none of its measurements: each is its own entry with its own hold,
/// and the visit being real says nothing about whether a number typed into it is.
pub(super) async fn rule_on_visit(
    state: &AppState,
    id: Uuid,
    mode: &str,
    reason: Option<&str>,
    by: &str,
) -> AppResult<Json<ResolveHoldResponse>> {
    let hold = hold_model::Entity::find_by_id(id)
        .filter(hold_model::Column::Kind.eq(HoldKind::UnverifiedVisit.as_str()))
        .filter(hold_model::Column::Status.is_in(HoldStatus::OPEN.map(HoldStatus::as_str)))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no pending unverified visit hold {id}")))?;
    let Some(site_id) = hold.site_id else {
        return Err(AppError::BadRequest(format!(
            "unverified visit hold {id} names no site"
        )));
    };
    let visit = visit_at(&state.db, site_id, hold.group_time)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "unverified visit hold {id} names no visit at {} on site {site_id}",
                hold.group_time
            ))
        })?;
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map_or_else(|| format!("unverified visit hold {id}"), String::from);
    let verify = mode == "verify";
    let status = if verify { "acknowledged" } else { "remediated" };
    let withdrawn = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let withdrawn = if verify {
            accept_visit(txn, visit.id).await?;
            0
        } else {
            let rows = withdraw_visit_readings(txn, visit.id, by, &reason).await?;
            withdraw_visit(txn, visit.id).await?;
            rows
        };
        decide_visit_hold(txn, id, mode, status, by, visit.id, withdrawn).await?;
        Ok(withdrawn)
    })
    .await?;
    state.response_cache.invalidate_all();
    Ok(Json(ResolveHoldResponse {
        status: status.to_string(),
        samples_affected: Some(i64::try_from(withdrawn).unwrap_or(i64::MAX)),
    }))
}

/// Rule on an intern's entry (Q21, M44): `verify` accepts it as it stands, `reject` withdraws it.
/// Both are decisions on the record, so both are reversible: a rejected entry is re-asserted, and
/// reopen returns the hold to review.
pub(super) async fn rule_on_entry(
    state: &AppState,
    id: Uuid,
    mode: &str,
    reason: Option<&str>,
    by: &str,
) -> AppResult<Json<ResolveHoldResponse>> {
    use crate::routes::private::readings::models::{Kind, Origin};
    use crate::routes::private::readings::service::{NewValue, record_many};
    let hold = hold_model::Entity::find_by_id(id)
        .filter(hold_model::Column::Kind.eq(HoldKind::UnverifiedEntry.as_str()))
        .filter(hold_model::Column::Status.is_in(HoldStatus::OPEN.map(HoldStatus::as_str)))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no pending unverified entry hold {id}")))?;
    let group_time = hold.group_time;
    let (Some(site_id), Some(parameter_id)) = (hold.site_id, hold.parameter_id) else {
        return Err(AppError::BadRequest(format!(
            "unverified entry hold {id} names no slot"
        )));
    };
    // The field day is ruled on first (Q177): a value entered at a visit nobody has accepted
    // cannot be verified, because what is in question is whether the visit happened at all.
    if let Some(visit) = visit_at(&state.db, site_id, group_time).await?
        && visit.unverified
    {
        return Err(AppError::Conflict(format!(
            "visit {} at {} is itself unverified; rule on the field day before its measurements",
            visit.id, visit.collected_at
        )));
    }
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map_or_else(|| format!("unverified entry hold {id}"), String::from);
    let (kind, new, status) = if mode == "verify" {
        (
            Kind::Verify,
            serde_json::json!({ "unverified": false }),
            "acknowledged",
        )
    } else {
        (
            Kind::Reject,
            serde_json::json!({ "reason": reason.clone() }),
            "remediated",
        )
    };
    let recorded = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let at = group_time.with_timezone(&chrono::Utc);
        let ruled = pending_entry_keys(txn, site_id, parameter_id, at).await?;
        let mut recorded = record_many(
            txn,
            kind,
            {
                use crate::routes::private::collection_events::flows::row;
                use crate::routes::private::readings::models::Column;
                sea_orm::Condition::all()
                    .add(row(Column::SiteId).eq(site_id))
                    .add(row(Column::ParameterId).eq(parameter_id))
                    .add(row(Column::Time).eq(group_time))
                    .add(
                        crate::routes::private::collection_events::flows::row_is_true(
                            Column::Unverified,
                            true,
                        ),
                    )
            },
            NewValue::Literal(new),
            by,
            Some(&reason),
            Origin::Audit,
            None,
        )
        .await?;
        recorded.absorb(
            outputs_follow_ruling(txn, site_id, at, &ruled, mode == "verify", status, by).await?,
        );
        hold_model::Entity::update_many()
            .col_expr(hold_model::Column::Status, Expr::val(status))
            .col_expr(hold_model::Column::AcknowledgedBy, Expr::val(by))
            .col_expr(hold_model::Column::AcknowledgedAt, Expr::cust("NOW()"))
            .col_expr(
                hold_model::Column::Resolution,
                Expr::cust_with_values(
                    "jsonb_build_object('mode', $1::text, 'by', $2::text, \
                     'at', to_jsonb(NOW()), 'rows', $3::bigint)",
                    [
                        sea_orm::Value::from(mode),
                        sea_orm::Value::from(by),
                        sea_orm::Value::from(i64::try_from(recorded.rows).unwrap_or(i64::MAX)),
                    ],
                ),
            )
            .filter(hold_model::Column::Id.eq(id))
            .exec(txn)
            .await?;
        Ok(recorded)
    })
    .await?;
    // A ruling moves what the visit's calculations read and what the rollups hold, as any other
    // curation decision does.
    crate::routes::private::readings::service::propagate(state, &recorded, by).await?;
    state.response_cache.invalidate_all();
    Ok(Json(ResolveHoldResponse {
        status: status.to_string(),
        samples_affected: Some(i64::try_from(recorded.rows).unwrap_or(i64::MAX)),
    }))
}

/// The keys of the pending readings an entry ruling is about to decide on.
async fn pending_entry_keys<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Vec<crate::routes::private::readings::consumed::ReadingKey>> {
    Ok(crate::routes::private::readings::Entity::find()
        .filter(crate::routes::private::readings::Column::SiteId.eq(site_id))
        .filter(crate::routes::private::readings::Column::ParameterId.eq(parameter_id))
        .filter(crate::routes::private::readings::Column::Time.eq(at))
        .filter(crate::routes::private::readings::Column::Unverified.eq(true))
        .all(conn)
        .await?
        .into_iter()
        .map(|r| {
            (
                r.stream_id,
                r.time.with_timezone(&chrono::Utc),
                r.replicate_index,
            )
        })
        .collect())
}

/// The computed readings a ruling on the entries `ruled` would carry with it (Q257), and the visit
/// they were found at. The reject preview and the ruling both read this, so the notice a manager is
/// shown and what the reject does are one computation.
async fn ruling_follows<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
    ruled: &[crate::routes::private::readings::consumed::ReadingKey],
    verify: bool,
) -> AppResult<(
    Vec<crate::routes::private::readings::consumed::ReadingKey>,
    crate::routes::private::readings::consumed::VisitComputed,
)> {
    use crate::routes::private::readings::consumed::{
        computed_at, continuous_pending, follow_ruling,
    };
    let computed = computed_at(conn, site_id, at).await?;
    let mut outputs = computed.outputs.clone();
    let mut pending = computed.pending.clone();
    // A verify also releases what the continuous engine computed from the entry. Only the visit's
    // own outputs follow a reject, because a withdrawal is confined to spot rows.
    if verify {
        let continuous = continuous_pending(conn, site_id).await?;
        outputs.extend(continuous.outputs);
        pending.extend(continuous.pending);
    }
    let followed = follow_ruling(&outputs, &pending, ruled, verify);
    Ok((followed, computed))
}

/// What rejecting an intern's entry would withdraw beside it: every value computed from it, and
/// from those, at the entry's visit.
pub(super) async fn reject_takes(state: &AppState, id: Uuid) -> AppResult<RejectPreview> {
    let hold = hold_model::Entity::find_by_id(id)
        .filter(hold_model::Column::Kind.eq(HoldKind::UnverifiedEntry.as_str()))
        .filter(hold_model::Column::Status.is_in(HoldStatus::OPEN.map(HoldStatus::as_str)))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no pending unverified entry hold {id}")))?;
    let (Some(site_id), Some(parameter_id)) = (hold.site_id, hold.parameter_id) else {
        return Err(AppError::BadRequest(format!(
            "unverified entry hold {id} names no slot"
        )));
    };
    let at = hold.group_time.with_timezone(&chrono::Utc);
    let ruled = pending_entry_keys(&state.db, site_id, parameter_id, at).await?;
    let (followed, computed) = ruling_follows(&state.db, site_id, at, &ruled, false).await?;
    let parameter_ids: HashSet<Uuid> = followed
        .iter()
        .filter_map(|key| computed.parameters.get(key).copied())
        .collect();
    let names: HashMap<Uuid, (String, String)> = crate::routes::private::parameters::Entity::find()
        .filter(crate::routes::private::parameters::Column::Id.is_in(parameter_ids))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|p| (p.id, (p.code, p.name)))
        .collect();
    let withdrawn = followed
        .iter()
        .filter_map(|key| {
            let parameter_id = *computed.parameters.get(key)?;
            let (code, name) = names.get(&parameter_id).cloned()?;
            Some(RejectPreviewOutput {
                parameter_id,
                code,
                name,
                time: key.1,
                replicate_index: key.2,
                value: computed.values.get(key).copied(),
            })
        })
        .collect();
    Ok(RejectPreview {
        hold_id: id,
        entries: ruled.len(),
        withdrawn,
    })
}

/// Carry an entry ruling to the values computed from it (Q257): a verify releases each output whose
/// inputs are all verified now, a reject withdraws each output computed from what it withdrew. Each
/// is a decision on the record, and each output's own review-queue row is closed with the ruling.
async fn outputs_follow_ruling<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
    ruled: &[crate::routes::private::readings::consumed::ReadingKey],
    verify: bool,
    status: &str,
    by: &str,
) -> AppResult<crate::routes::private::readings::service::Recorded> {
    use crate::routes::private::collection_events::flows::row;
    use crate::routes::private::readings::models::{Column, Kind, Origin};
    use crate::routes::private::readings::service::{NewValue, Recorded, record_many};

    let (followed, computed) = ruling_follows(conn, site_id, at, ruled, verify).await?;
    if followed.is_empty() {
        return Ok(Recorded::default());
    }
    let keys = followed.iter().fold(
        sea_orm::Condition::any(),
        |any, (stream_id, time, index)| {
            any.add(
                sea_orm::Condition::all()
                    .add(row(Column::StreamId).eq(*stream_id))
                    .add(row(Column::Time).eq(*time))
                    .add(row(Column::ReplicateIndex).eq(*index)),
            )
        },
    );
    let (kind, new, reason) = if verify {
        (
            Kind::Verify,
            serde_json::json!({ "unverified": false }),
            "every input it was computed from is verified",
        )
    } else {
        (
            Kind::Withdraw,
            serde_json::json!({ "reason": "computed from a rejected entry" }),
            "computed from a rejected entry",
        )
    };
    let recorded = record_many(
        conn,
        kind,
        keys,
        NewValue::Literal(new),
        by,
        Some(reason),
        Origin::Audit,
        None,
    )
    .await?;
    let parameters: HashSet<Uuid> = followed
        .iter()
        .filter_map(|key| computed.parameters.get(key).copied())
        .collect();
    hold_model::Entity::update_many()
        .col_expr(hold_model::Column::Status, Expr::val(status))
        .col_expr(hold_model::Column::AcknowledgedBy, Expr::val(by))
        .col_expr(hold_model::Column::AcknowledgedAt, Expr::cust("NOW()"))
        .filter(hold_model::Column::Kind.eq(HoldKind::UnverifiedEntry.as_str()))
        .filter(hold_model::Column::Status.is_in(HoldStatus::OPEN.map(HoldStatus::as_str)))
        .filter(hold_model::Column::SiteId.eq(site_id))
        .filter(hold_model::Column::GroupTime.eq(at))
        .filter(hold_model::Column::ParameterId.is_in(parameters))
        .exec(conn)
        .await?;
    Ok(recorded)
}

/// How many plan entries an apply pairs between progress reports.
pub(super) const PROGRESS_BATCH: usize = 25;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHierarchy {
    pub project: String,
    pub site: String,
    pub parameter: String,
    /// Human-readable label for the parameter (the portal's dropdown text). The `parameter`
    /// field itself is the source's machine identity (its DB column name), which is what a
    /// scientist looking at the portal's own tables recognises.
    pub parameter_label: Option<String>,
    pub units: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

/// Extract the project/site/parameter hierarchy from a stream's metadata.
///
/// Priority:
/// 1. metadata.hierarchy (set by all portal backends)
/// 2. source_path segment parsing (fallback)
/// 3. source_name splitting on " - " (last resort)
pub fn extract_hierarchy(stream: &data_streams::Model) -> StreamHierarchy {
    let meta = &stream.metadata;

    // Try metadata.hierarchy first
    if let Some(h) = meta.get("hierarchy") {
        let project = h
            .get("project")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let site = h
            .get("site")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parameter = h
            .get("parameter")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parameter_label = h
            .get("parameter_label")
            .and_then(|v| v.as_str())
            .or_else(|| {
                meta.get("parameter")
                    .and_then(|p| p.get("display_name"))
                    .and_then(|v| v.as_str())
            })
            .filter(|l| !l.is_empty() && *l != parameter)
            .map(ToString::to_string);
        let units = meta
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let coords = meta.get("coordinates");
        let lat = coords
            .and_then(|c| c.get("latitude"))
            .and_then(|v| v.as_f64());
        let lon = coords
            .and_then(|c| c.get("longitude"))
            .and_then(|v| v.as_f64());
        let alt = coords
            .and_then(|c| c.get("altitude_m"))
            .and_then(|v| v.as_f64());

        if !project.is_empty() || !site.is_empty() || !parameter.is_empty() {
            return StreamHierarchy {
                project,
                site,
                parameter,
                parameter_label,
                units,
                latitude: lat,
                longitude: lon,
                altitude_m: alt,
            };
        }
    }

    // Fallback: source_path segment parsing
    if let Some(ref path) = stream.source_path {
        let segs: Vec<&str> = path.split('/').collect();
        let project = segs.get(1).unwrap_or(&"").to_string();
        let site = segs.get(2).unwrap_or(&"").to_string();
        let parameter = segs.get(3).unwrap_or(&"").to_string();
        let units = meta
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        return StreamHierarchy {
            project,
            site,
            parameter,
            parameter_label: None,
            units,
            latitude: None,
            longitude: None,
            altitude_m: None,
        };
    }

    // Last resort: source_name, stripping the "{site} - " prefix without truncating
    // display names that themselves contain " - "
    let parameter = stream
        .source_name
        .as_deref()
        .and_then(|n| n.split_once(" - ").map(|x| x.1))
        .unwrap_or("")
        .to_string();
    let units = meta
        .get("units")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    StreamHierarchy {
        project: stream.source_system.to_uppercase(),
        site: String::new(),
        parameter,
        parameter_label: None,
        units,
        latitude: None,
        longitude: None,
        altitude_m: None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanEntry {
    pub stream_id: Uuid,
    pub source_key: String,
    #[schema(required)]
    pub source_name: Option<String>,
    pub action: String, // "pair" | "skip"
    pub project: PlanEntityRef,
    pub site: PlanSiteRef,
    pub parameter: PlanParamRef,
    pub confidence: String, // "exact" | "none"
    #[serde(default)]
    #[schema(required)]
    pub warnings: Vec<PlanWarning>,
    #[serde(default)]
    pub original_parameter_name: Option<String>,
    /// Present when the stream is a replicate family: what is being paired is the group of
    /// member columns, not the portal's average.
    #[serde(default)]
    pub replicates: Option<PlanReplicates>,
    /// The lab instrument this stream's standard curves belong to. Present when the stream names
    /// an instrument already, or when its replicate spec names a curve column, and absent
    /// otherwise. A curve is fitted on one instrument, so a reading naming a curve must name that
    /// instrument too; a stream that will carry curve references and resolves to no instrument has
    /// its readings refused (`/readings/batch`) or dropped (`/ingest`), which is what makes this a
    /// decision the plan has to settle rather than report.
    #[serde(default)]
    pub instrument: Option<PlanInstrumentRef>,
    /// The decimal places the source declared for this stream, written onto the slot on apply
    /// where the slot declares none. An operator's declaration on the slot is never overwritten.
    #[serde(default)]
    pub decimal_places: Option<i16>,
    /// Open replicate-statistics holds on this stream, written at plan creation.
    #[serde(default)]
    #[schema(required)]
    pub sd_holds: i64,
    /// A person has looked at this entry and agreed with it. Set explicitly, never inferred from
    /// an edit: an operator who toggles a parameter group to skip and back has decided nothing.
    /// Only [`ReviewState::NeedsChecking`] entries wait on it; a fully matched entry with no
    /// warning is self-validated and needs no tick.
    #[serde(default)]
    #[schema(required)]
    pub acknowledged: bool,
    /// Whether the source reports this feed as a device. That, not the presence of a serial, is
    /// what makes a feed field-shaped: its instrument is minted from the feed's own provenance
    /// when the stream is paired, so the plan proposes no lab instrument for it. A source may
    /// describe a device and report no serial for it, which is why the two are separate.
    #[serde(default)]
    #[schema(required)]
    pub is_device: bool,
    /// The device serial the source names for this feed, where it names one. Information the plan
    /// displays; never the instrument's identity.
    #[serde(default)]
    pub device_serial: Option<String>,
    /// The device model, where the source reports one. Naming only.
    #[serde(default)]
    pub device_model: Option<String>,
}

/// A catalog parameter a plan entry collides with, and what already depends on it. "Exists" on its
/// own does not say where or whether anything uses it, which is the question an operator has to
/// answer to resolve a units conflict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ExistingParamRef {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub units: String,
    pub category: String,
    pub site_parameter_count: i64,
    pub reading_count: i64,
}

/// Something the review has to decide about, carried as data rather than a sentence so the UI can
/// offer the resolutions instead of only naming the problem. `message` is the rendered form, kept
/// so a warning always reads as something even where the structure is not used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanWarning {
    /// `units_mismatch` | `empty_name` | `near_duplicate` | `catalog_match` |
    /// `duplicate_parameter_code` | `duplicate_site_name` | `duplicate_project_name`.
    pub kind: String,
    pub message: String,
    #[serde(default)]
    pub parameter: Option<String>,
    #[serde(default)]
    pub existing: Option<ExistingParamRef>,
    /// The units this source declares, against `existing.units`.
    #[serde(default)]
    pub source_units: Option<String>,
}

impl PlanWarning {
    pub fn units_mismatch(parameter: &str, existing: &CatalogParam, source_units: &str) -> Self {
        Self {
            kind: "units_mismatch".to_string(),
            message: format!(
                "Parameter '{parameter}' exists in the catalog with units '{}' but this source \
                 uses '{source_units}'",
                existing.units
            ),
            parameter: Some(parameter.to_string()),
            existing: Some(ExistingParamRef {
                id: existing.id,
                code: existing.code.clone(),
                name: existing.name.clone(),
                units: existing.units.clone(),
                category: existing.category.clone(),
                site_parameter_count: existing.site_parameter_count,
                reading_count: existing.reading_count,
            }),
            source_units: Some(source_units.to_string()),
        }
    }

    /// A name this plan would create reads as one the catalog already holds. The catalog matches
    /// exactly, so `FP-1` beside a stored `FP1` is two entities and no reader would call them two
    /// places.
    pub fn near_duplicate(kind: &str, proposed: &str, existing: &str) -> Self {
        Self {
            kind: "near_duplicate".to_string(),
            message: format!(
                "This plan would create the {kind} '{proposed}', and '{existing}' already exists. \
                 They differ only in case, spacing or punctuation."
            ),
            parameter: None,
            existing: None,
            source_units: None,
        }
    }

    /// A code the plan would create under names a parameter the catalog already holds. A typed code
    /// attaches to nothing on its own (Q221), so the operator is told what they have written beside;
    /// where the code itself is taken, the apply refuses outright.
    pub fn catalog_match(proposed: &str, existing: &CatalogParam) -> Self {
        Self {
            kind: "catalog_match".to_string(),
            message: format!(
                "The catalog already holds '{}' ({}). Attach this column to it, or give the new \
                 parameter a code of its own.",
                existing.code, existing.name
            ),
            parameter: Some(proposed.to_string()),
            existing: Some(ExistingParamRef {
                id: existing.id,
                code: existing.code.clone(),
                name: existing.name.clone(),
                units: existing.units.clone(),
                category: existing.category.clone(),
                site_parameter_count: existing.site_parameter_count,
                reading_count: existing.reading_count,
            }),
            source_units: None,
        }
    }

    /// Two of the plan's own rows would create the same thing: one code with two sets of units, or
    /// two names that read as one place. The catalog cannot say so, because neither exists yet.
    pub fn duplicate_in_plan(kind: &str, proposed: &str, other: &str) -> Self {
        Self {
            kind: format!("duplicate_{kind}"),
            message: match kind {
                "parameter_code" => format!(
                    "This plan would create '{proposed}' twice, once per set of units ({other}). \
                     One code is one parameter: give each row a code of its own, or agree on the \
                     units."
                ),
                _ => format!(
                    "This plan would also create '{other}', which reads as the same name. They \
                     differ only in case, spacing or punctuation."
                ),
            },
            parameter: Some(proposed.to_string()),
            existing: None,
            source_units: None,
        }
    }

    pub fn empty_name() -> Self {
        Self {
            kind: "empty_name".to_string(),
            message: "site or parameter name is empty".to_string(),
            parameter: None,
            existing: None,
            source_units: None,
        }
    }
}

/// One of an instrument's standard curves, carried so the review can show what a save would
/// correct with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanCurveRef {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
}

/// The instrument a plan entry's curve references resolve to, and how that was decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanInstrumentRef {
    /// The source column naming a curve per reading, e.g. `doc_std_curve_id`. Absent when the
    /// instrument came from the stream and no column names a curve (the chla families, corrected
    /// upstream).
    #[serde(default)]
    #[schema(required)]
    pub curve_column: Option<String>,
    /// The resolved instrument, or None when one has to be created.
    #[schema(required)]
    pub id: Option<Uuid>,
    pub name: String,
    /// `(source_system, source_key)` is an instrument's identity, so a later rename cannot break
    /// the mapping.
    pub source_key: String,
    /// `stream` (already attributed), `curve_label` (suggested from the source's own curve
    /// labels), `manual` (repointed in the review), `ambiguous_label` (the label matched more than
    /// one, so nothing is suggested), or `placeholder` (nothing matched).
    pub resolved_by: String,
    pub create: bool,
    /// The instrument row was minted by stream registration rather than named by the source or an
    /// operator (`sensors.metadata.minted_from_stream`). It is the one a review may want to
    /// replace with the real device.
    #[serde(default)]
    #[schema(required)]
    pub defaulted: bool,
    /// A creation an operator has agreed to. Apply refuses a plan holding an unconfirmed one.
    #[serde(default)]
    #[schema(required)]
    pub confirmed: bool,
    /// True when each reading stores a `standard_curve_id` (the family's own calculation names
    /// the curve, members are raw). False when the curve was applied upstream and only the
    /// instrument is attributed, where stamping would correct the value a second time.
    pub stamps_readings: bool,
    #[serde(default)]
    #[schema(required)]
    pub curves: Vec<PlanCurveRef>,
    /// The name this decision proposes creating, kept whatever else the entry resolves to. An
    /// operator who attaches an existing instrument by mistake has the proposal to go back to;
    /// without it, the only record of what the plan suggested is gone the moment it is overwritten.
    #[serde(default)]
    pub proposed_name: Option<String>,
    /// An instrument that already carries the proposed name. Creating a second one under it is
    /// allowed, and so is attaching to this one, but neither may happen by default: readings
    /// joining an instrument that already holds data is not something a plan decides on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name_conflict: Option<InstrumentNameConflict>,
    /// The instruments a curve label matched when it matched more than one. Which analyser the
    /// source meant is not in the label, so the tie is named and nothing is suggested (Q195).
    #[serde(default)]
    #[schema(required)]
    pub label_candidates: Vec<PlanLabelCandidate>,
}

/// One instrument a curve label matched, enough of it to choose by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct PlanLabelCandidate {
    pub id: Uuid,
    pub name: String,
}

/// The instrument a proposed name collides with, enough of it to choose by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct InstrumentNameConflict {
    pub id: Uuid,
    pub name: String,
    /// Where it came from, so an operator can tell a hand entry from an earlier import.
    #[schema(required)]
    pub source_system: Option<String>,
    /// True when it already carries readings; attaching adds to them.
    pub has_readings: bool,
}

/// Replicate-family summary carried on a plan entry, from the stream's registered spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanReplicates {
    pub n: usize,
    pub member_columns: Vec<String>,
    #[schema(required)]
    pub curve_ref_column: Option<String>,
    #[schema(required)]
    pub portal_mean_column: Option<String>,
    #[schema(required)]
    pub portal_sd_column: Option<String>,
}

/// The instruments a source has registered, with their curves, plus any instrument the plan's
/// streams already name (which may belong to no source, e.g. a device registered by serial).
#[derive(Clone)]
pub struct InstrumentCatalog {
    /// Instrument id -> (display name, source_key).
    by_id: HashMap<Uuid, (String, Option<String>)>,
    /// The source's own instruments, as (normalised label, id), for curve-column matching.
    labels: Vec<(String, Uuid)>,
    /// The source's own instruments by `source_key`, which is the identity an apply mints and
    /// dedupes on. Looked up before anything is proposed, so a plan built after an earlier one
    /// reports the instrument it already created rather than asking to create it again.
    by_source_key: HashMap<String, Uuid>,
    /// **Every** instrument by lowercased name, this source's and everyone else's. A name
    /// collision is about what a person reads, so it does not stop at the source boundary: the
    /// lab's `DOC` may have arrived by hand or from another import.
    by_name: HashMap<String, InstrumentNameConflict>,
    /// Every instrument by serial number, on the same reasoning as `by_name`: a register row
    /// naming a serial another instrument already carries describes the same probe.
    by_serial: HashMap<String, InstrumentNameConflict>,
    curves: HashMap<Uuid, Vec<PlanCurveRef>>,
    /// Instruments registration minted for a stream that named none, rather than ones a source or
    /// an operator attributed. They carry `metadata.minted_from_stream`
    /// (`sensors/identity.rs::resolve_or_mint_stream_instrument`). A default is what keeps a
    /// reading from naming nothing; it is not evidence about which analyser produced a correction,
    /// so it loses to a curve label that matches the stream's own curve column.
    defaulted: std::collections::HashSet<Uuid>,
}

impl InstrumentCatalog {
    /// The instrument already carrying this name, if any.
    #[must_use]
    pub fn named(&self, name: &str) -> Option<InstrumentNameConflict> {
        self.by_name.get(&name.trim().to_lowercase()).cloned()
    }

    /// The instrument already carrying this serial number, if any.
    #[must_use]
    pub fn holding_serial(&self, serial: &str) -> Option<InstrumentNameConflict> {
        self.by_serial.get(serial.trim()).cloned()
    }
}

/// The instrument an admitted register row would duplicate: the one already carrying its name,
/// else the one already carrying its serial. Q76's attach-or-create choice is about what a person
/// reads in the inventory, and two rows for one probe read the same whichever field matched.
#[must_use]
pub fn proposal_conflict(
    name: &str,
    serial: Option<&str>,
    catalog: &InstrumentCatalog,
) -> Option<InstrumentNameConflict> {
    if let Some(hit) = catalog.named(name) {
        return Some(hit);
    }
    serial
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| catalog.holding_serial(s))
}

/// A curve column's stem, normalised for comparison against an instrument label:
/// `doc_std_curve_id` -> `doc`, `chla_acid_std_curve_id` -> `chla acid`.
pub(super) fn curve_column_stem(column: &str) -> String {
    column
        .to_lowercase()
        .trim_end_matches("_std_curve_id")
        .replace('_', " ")
        .trim()
        .to_string()
}

/// An instrument's label, normalised the same way. The source prefix is dropped because it is
/// already the thing being matched within.
pub(super) fn instrument_label(source_key: &str, source_system: &str) -> String {
    source_key
        .strip_prefix(&format!("{source_system}:"))
        .unwrap_or(source_key)
        .to_lowercase()
        .replace('_', " ")
        .trim()
        .to_string()
}

pub async fn load_instrument_catalog(
    db: &impl ConnectionTrait,
    source_system: &str,
    named_ids: &[Uuid],
) -> AppResult<InstrumentCatalog> {
    let rows = sensors::Entity::find()
        .filter(
            Condition::any()
                .add(sensors::Column::SourceSystem.eq(source_system))
                .add(sensors::Column::Id.is_in(named_ids.to_vec())),
        )
        .all(db)
        .await?;

    // Names are read across every source: see `by_name`.
    let named_rows = sensors::Entity::find().all(db).await?;
    let with_readings: std::collections::HashSet<Uuid> = readings::models::Entity::find()
        .select_only()
        .column(readings::models::Column::SensorId)
        .distinct()
        .filter(readings::models::Column::SensorId.is_not_null())
        .into_tuple::<Option<Uuid>>()
        .all(db)
        .await?
        .into_iter()
        .flatten()
        .collect();
    let mut by_name: HashMap<String, InstrumentNameConflict> = HashMap::new();
    for row in &named_rows {
        let Some(name) = row
            .name
            .as_ref()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        by_name
            .entry(name.to_lowercase())
            .or_insert_with(|| InstrumentNameConflict {
                id: row.id,
                name: name.to_string(),
                source_system: row.source_system.clone(),
                has_readings: with_readings.contains(&row.id),
            });
    }
    let mut by_serial: HashMap<String, InstrumentNameConflict> = HashMap::new();
    for row in &named_rows {
        let Some(serial) = row
            .serial_number
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        by_serial
            .entry(serial.to_string())
            .or_insert_with(|| InstrumentNameConflict {
                id: row.id,
                name: row.name.clone().unwrap_or_else(|| serial.to_string()),
                source_system: row.source_system.clone(),
                has_readings: with_readings.contains(&row.id),
            });
    }

    let mut by_id = HashMap::new();
    let mut labels = Vec::new();
    let mut by_source_key = HashMap::new();
    for row in &rows {
        let name = row
            .name
            .clone()
            .or_else(|| row.serial_number.clone())
            .unwrap_or_else(|| row.id.to_string());
        if row.source_system.as_deref() == Some(source_system)
            && let Some(key) = &row.source_key
        {
            labels.push((instrument_label(key, source_system), row.id));
            by_source_key.insert(key.clone(), row.id);
        }
        by_id.insert(row.id, (name, row.source_key.clone()));
    }

    let ids: Vec<Uuid> = by_id.keys().copied().collect();
    let mut curves: HashMap<Uuid, Vec<PlanCurveRef>> = HashMap::new();
    if !ids.is_empty() {
        for c in standard_curves::Entity::find()
            .filter(standard_curves::Column::SensorId.is_in(ids))
            .all(db)
            .await?
        {
            curves.entry(c.sensor_id).or_default().push(PlanCurveRef {
                id: c.id,
                name: c.name.clone(),
                slope: c.slope,
                intercept: c.intercept,
            });
        }
    }

    let defaulted = rows
        .iter()
        .filter(|row| {
            row.metadata
                .as_ref()
                .and_then(|m| m.get(sensors::models::MINTED_FROM_STREAM))
                .is_some()
        })
        .map(|row| row.id)
        .collect();

    Ok(InstrumentCatalog {
        by_id,
        labels,
        by_source_key,
        by_name,
        by_serial,
        curves,
        defaulted,
    })
}

/// What a curve column's stem matched among this source's instrument labels.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LabelMatch {
    /// Nothing carries the stem.
    None,
    /// One instrument does. It is a suggestion, never a settled attribution: words matching is not
    /// the source saying so.
    One(Uuid),
    /// Several do, and which analyser the source meant is not in the label. Reported rather than
    /// dropped, because a tie is the review's question to answer.
    Tie(Vec<Uuid>),
}

/// The instruments of this source whose label matches a curve column's stem.
///
/// A registration-minted default is not a candidate: its label is the parameter's own name, so
/// `DOC` would tie with the analyser labelled `DOC corr` and make every stem ambiguous. The
/// question a curve column asks is which instrument the source says produced the correction, and a
/// default is the absence of that answer.
pub(super) fn label_match(curve_column: &str, catalog: &InstrumentCatalog) -> LabelMatch {
    let stem = curve_column_stem(curve_column);
    let mut matches: Vec<Uuid> = catalog
        .labels
        .iter()
        .filter(|(_, id)| !catalog.defaulted.contains(id))
        .filter(|(label, _)| *label == stem || label.starts_with(&format!("{stem} ")))
        .map(|(_, id)| *id)
        .collect();
    let mut seen = std::collections::HashSet::new();
    matches.retain(|id| seen.insert(*id));
    match matches.len() {
        0 => LabelMatch::None,
        1 => LabelMatch::One(matches[0]),
        _ => LabelMatch::Tie(matches),
    }
}

/// Which instrument a stream's curve references belong to, most specific first: the instrument the
/// stream already names, then the source's own curve labels matched against the curve column, then
/// a placeholder for an operator to confirm.
///
/// The label match is what lets a portal whose curve column is empty in the data still resolve: the
/// curve catalog is replicated independently of the readings, so the instrument is knowable even
/// when no row has yet named a curve. It is a heuristic, so it is reported as one, and an
/// ambiguous stem resolves to nothing rather than to a guess.
pub fn resolve_instrument(
    stream_sensor_id: Option<Uuid>,
    curve_column: Option<&str>,
    source_system: &str,
    catalog: &InstrumentCatalog,
) -> Option<PlanInstrumentRef> {
    let stamps_readings = curve_column.is_some();
    let curve_column = curve_column.map(str::to_string);

    // An instrument the stream already names is an attribution somebody made: a declaration on the
    // descriptor, a pairing, or an operator's repoint. Registration mints none (M172), so there is
    // no default to see through here.
    if let Some(id) = stream_sensor_id {
        let (name, source_key) = catalog
            .by_id
            .get(&id)
            .cloned()
            .unwrap_or_else(|| (id.to_string(), None));
        return Some(PlanInstrumentRef {
            curve_column,
            id: Some(id),
            name,
            source_key: source_key.unwrap_or_default(),
            resolved_by: "stream".to_string(),
            create: false,
            defaulted: catalog.defaulted.contains(&id),
            confirmed: true,
            stamps_readings,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
            label_candidates: vec![],
        });
    }

    let column = curve_column.clone()?;
    let stem = curve_column_stem(&column);
    let source_key = format!("{source_system}:{column}");

    // An instrument this source already has under the key an apply would mint is that decision,
    // already taken. Looking it up before proposing is what keeps a second plan from re-asking.
    if let Some(id) = catalog.by_source_key.get(&source_key).copied() {
        let (name, key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
        return Some(PlanInstrumentRef {
            curve_column,
            id: Some(id),
            name,
            source_key: key.unwrap_or(source_key),
            resolved_by: "source_key".to_string(),
            create: false,
            defaulted: catalog.defaulted.contains(&id),
            confirmed: true,
            stamps_readings,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
            label_candidates: vec![],
        });
    }

    // A label match is words agreeing, not the source naming an instrument, so it is carried as a
    // suggestion the review confirms (Q195). A tie names its candidates and suggests none.
    let tied = match label_match(&column, catalog) {
        LabelMatch::One(id) => {
            let (name, source_key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
            return Some(PlanInstrumentRef {
                curve_column,
                id: Some(id),
                name,
                source_key: source_key.unwrap_or_default(),
                resolved_by: "curve_label".to_string(),
                create: false,
                defaulted: catalog.defaulted.contains(&id),
                confirmed: false,
                stamps_readings,
                curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
                proposed_name: None,
                name_conflict: None,
                label_candidates: vec![],
            });
        }
        LabelMatch::Tie(ids) => label_candidates(&ids, catalog),
        LabelMatch::None => vec![],
    };

    // Unconfirmed, and the apply refuses until an operator says yes (Q123). The name is a stem
    // taken from a column heading, and a lab instrument is provenance: once readings name it,
    // renaming is the only repair. The question this arm asks is which instrument fitted the curve
    // the source says corrected these readings, and nothing here knows the answer.
    let name = format!("{stem} {source_system}");
    Some(PlanInstrumentRef {
        curve_column: Some(column),
        id: None,
        name: name.clone(),
        source_key,
        resolved_by: if tied.is_empty() {
            "placeholder".to_string()
        } else {
            "ambiguous_label".to_string()
        },
        create: true,
        defaulted: false,
        confirmed: false,
        stamps_readings,
        curves: vec![],
        proposed_name: Some(name),
        name_conflict: None,
        label_candidates: tied,
    })
}

/// The tied instruments, named so the review can pick one instead of creating another.
fn label_candidates(ids: &[Uuid], catalog: &InstrumentCatalog) -> Vec<PlanLabelCandidate> {
    let mut candidates: Vec<PlanLabelCandidate> = ids
        .iter()
        .map(|id| PlanLabelCandidate {
            id: *id,
            name: catalog
                .by_id
                .get(id)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| id.to_string()),
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates
}

/// The provenance key a stream's instrument is held under: the source's instrument for the raw
/// column the feed carries, or the feed's own key when it names no parameter.
///
/// The plan and the pairing both key through here, so the row one proposes is the row the other
/// mints. A plan's suggested parameter is a display name (a family's `DOC_avg_ppb` reads as `DOC`)
/// and the regrouping loop rewrites it again, so keying off it mints a second instrument for the
/// same analyte.
pub fn stream_instrument_key(stream: &data_streams::Model) -> String {
    let parameter = extract_hierarchy(stream).parameter;
    let key_part = if parameter.is_empty() {
        stream.source_key.as_str()
    } else {
        parameter.as_str()
    };
    format!("{}:{key_part}", stream.source_system)
}

/// The instrument a source parameter resolves to, for the feeds that name no curve column.
///
/// The source's own instrument under the key an apply mints ([`stream_instrument_key`]) when it has
/// one, and otherwise that same key proposed for creation, unconfirmed: the plan suggests and a
/// person confirms, one row at a time or all at once. `parameter` names the proposal, it does not
/// key it.
pub fn resolve_parameter_instrument(
    source_key: String,
    parameter: &str,
    catalog: &InstrumentCatalog,
) -> PlanInstrumentRef {
    if let Some(id) = catalog.by_source_key.get(&source_key).copied() {
        let (name, key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
        return PlanInstrumentRef {
            curve_column: None,
            id: Some(id),
            name,
            source_key: key.unwrap_or(source_key),
            resolved_by: "source_key".to_string(),
            create: false,
            defaulted: catalog.defaulted.contains(&id),
            confirmed: true,
            stamps_readings: false,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
            label_candidates: vec![],
        };
    }
    // The lab's DOC analyser is one machine carried to every station, so it is called DOC. The
    // source is provenance, held in `source_key`, and putting it in the name would make every
    // import read as a different instrument to the person choosing between them.
    let name = parameter.to_string();
    let conflict = catalog.named(&name);
    PlanInstrumentRef {
        curve_column: None,
        id: None,
        name: name.clone(),
        source_key,
        resolved_by: "parameter".to_string(),
        create: true,
        defaulted: false,
        confirmed: false,
        stamps_readings: false,
        curves: vec![],
        proposed_name: Some(name),
        name_conflict: conflict,
        label_candidates: vec![],
    }
}

/// The instrument a device feed resolves to: its own channel, keyed on the feed's `source_key`.
///
/// The source's row under that key when it has one, and otherwise a proposal named for the slot
/// the feed serves, `{site} {parameter}`, unconfirmed: the pairing mints it and opens its
/// deployment, so a person agrees to it first like any other instrument the plan creates.
pub fn resolve_device_instrument(
    source_key: &str,
    slot: &str,
    catalog: &InstrumentCatalog,
) -> PlanInstrumentRef {
    if let Some(id) = catalog.by_source_key.get(source_key).copied() {
        let (name, key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
        return PlanInstrumentRef {
            curve_column: None,
            id: Some(id),
            name,
            source_key: key.unwrap_or_else(|| source_key.to_string()),
            resolved_by: "source_key".to_string(),
            create: false,
            defaulted: catalog.defaulted.contains(&id),
            confirmed: true,
            stamps_readings: false,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
            label_candidates: vec![],
        };
    }
    PlanInstrumentRef {
        curve_column: None,
        id: None,
        name: slot.to_string(),
        source_key: source_key.to_string(),
        resolved_by: "device".to_string(),
        create: true,
        defaulted: false,
        confirmed: false,
        stamps_readings: false,
        curves: vec![],
        proposed_name: Some(slot.to_string()),
        name_conflict: catalog.named(slot),
        label_candidates: vec![],
    }
}

/// The name a device channel is proposed under: the slot it serves, `{site} {parameter}`. An
/// existing site or parameter reads as the inventory names it, and a new one as the apply will.
#[must_use]
pub fn device_slot_name(entry: &PlanEntry, catalog: &EntityCatalog) -> String {
    let site = entry
        .site
        .id
        .and_then(|id| catalog.sites.iter().find(|(s, _)| *s == id))
        .map_or(entry.site.name.as_str(), |(_, name)| name.as_str());
    let parameter = entry
        .parameter
        .id
        .and_then(|id| catalog.params.iter().find(|p| p.id == id))
        .map(|p| p.name.as_str())
        .or(entry.parameter.label.as_deref())
        .unwrap_or(&entry.parameter.name);
    format!("{site} {parameter}")
}

pub(super) fn plan_replicates(metadata: &serde_json::Value) -> Option<PlanReplicates> {
    let spec =
        crate::routes::private::data_streams::models::ReplicateSpec::from_metadata(metadata)?;
    Some(PlanReplicates {
        n: spec.declared.source_columns.len(),
        member_columns: spec.declared.source_columns,
        curve_ref_column: spec.declared.curve_ref_column,
        portal_mean_column: spec.declared.portal_mean_column,
        portal_sd_column: spec.declared.portal_sd_column,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanEntityRef {
    #[schema(required)]
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
    /// The name the source sends, which the apply links to the resolved row.
    #[serde(default)]
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanSiteRef {
    #[schema(required)]
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
    /// The name the source sends, which the apply links to the resolved row.
    #[serde(default)]
    pub source_name: Option<String>,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    #[schema(required)]
    pub altitude_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanParamRef {
    #[schema(required)]
    pub id: Option<Uuid>,
    /// The parameter identity: the source's own column name (matches what the portal DB shows).
    pub name: String,
    /// Human-readable label carried alongside; becomes `parameters.name` when the apply creates
    /// the parameter, while `name` becomes its `code`.
    #[serde(default)]
    pub label: Option<String>,
    pub create: bool,
    pub units: String,
    #[serde(default)]
    pub group_key: Option<String>,
    /// The parameter group the source's registry places this column in, resolved against the
    /// groups that already exist. Absent where the source declares no category.
    #[serde(default)]
    #[schema(required)]
    pub group: Option<PlanGroupRef>,
    /// The source calculation behind an `output` column, where the source declares one.
    #[serde(default)]
    #[schema(required)]
    pub calculation: Option<PlanCalculationRef>,
    #[serde(default)]
    #[schema(required)]
    pub original_names: Vec<String>,
    /// What the review decided this parameter is, kept on the plan so it survives a reload. Absent
    /// where nobody has decided and the source's own name is still matched against the catalog as a
    /// proposal; a code somebody typed attaches to nothing on its own (Q221).
    #[serde(default)]
    #[schema(required)]
    pub attach: Option<PlanParamAttach>,
}

/// The review's answer to "which catalog parameter is this column": one that exists, or a new one
/// under the code the entry carries. One field rather than an id beside a flag, so "attached to
/// nothing in particular" cannot be written down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub enum PlanParamAttach {
    /// Attach every entry of this group to the catalog parameter with this id.
    Existing { id: Uuid },
    /// Create a parameter under the entry's own code.
    New,
}

/// The parameter group a column belongs to, as the source's own registry places it.
///
/// A group is one decision behind every column of its category, so the plan carries it on each
/// entry and the apply creates it once. `ordinal` is the member's position within the group, which
/// is the registry's own order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanGroupRef {
    #[schema(required)]
    pub id: Option<Uuid>,
    /// The group's stable code, slugged from the label the source gives it.
    pub code: String,
    pub label: String,
    /// The member's position within the group.
    pub ordinal: i32,
    #[schema(required)]
    pub description: Option<String>,
    pub create: bool,
}

/// What the source computed a column with: its own calculation function and the columns that
/// function reads.
///
/// It is the statement a formula set is authored against, and the one an output still waiting for
/// one is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanCalculationRef {
    /// The source's own function name, verbatim (`calcPCO2`).
    pub function: String,
    /// The columns it reads, in the order the source lists them.
    pub inputs: Vec<String>,
}

/// A group's code: lowercase, non-alphanumerics collapsed to underscores. The rule the portal seed
/// used, so a database carrying groups from either route agrees with itself.
#[must_use]
pub fn group_code(label: &str) -> String {
    let mut code = String::with_capacity(label.len());
    let mut pending_break = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_break && !code.is_empty() {
                code.push('_');
            }
            pending_break = false;
            code.extend(c.to_lowercase());
        } else {
            pending_break = true;
        }
    }
    code
}

/// The group a stream's metadata places its column in, where the source declares one.
#[must_use]
pub fn plan_group(metadata: &serde_json::Value) -> Option<PlanGroupRef> {
    let param = metadata.get("parameter")?;
    let label = param.get("category")?.as_str()?.trim();
    if label.is_empty() {
        return None;
    }
    Some(PlanGroupRef {
        id: None,
        code: group_code(label),
        label: label.to_string(),
        ordinal: param
            .get("category_ordinal")
            .and_then(serde_json::Value::as_i64)
            .and_then(|o| i32::try_from(o).ok())
            .unwrap_or(0),
        description: param
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(ToString::to_string),
        create: true,
    })
}

/// The calculation a stream's metadata declares for its column, where the source names one.
///
/// A function with no inputs is not a calculation anybody can read back, so both are required.
#[must_use]
pub fn plan_calculation(metadata: &serde_json::Value) -> Option<PlanCalculationRef> {
    let declared = metadata.get("parameter")?.get("source_calculation")?;
    let function = declared.get("function")?.as_str()?.trim();
    if function.is_empty() {
        return None;
    }
    let inputs: Vec<String> = declared
        .get("inputs")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(ToString::to_string)
        .collect();
    if inputs.is_empty() {
        return None;
    }
    Some(PlanCalculationRef {
        function: function.to_string(),
        inputs,
    })
}

#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
pub struct PlanSummary {
    // Every count defaults, because the column is read back from plans stored before it carried
    // the field, and a count nobody wrote is zero. Every summary written since carries all of
    // them, which is what `schema(required)` says.
    #[serde(default)]
    #[schema(required)]
    pub total_streams: usize,
    #[serde(default)]
    #[schema(required)]
    pub will_pair: usize,
    /// The three review states over the entries the plan would pair, so the review can say what
    /// share of the plan waits on a person and what share stands on its own evidence.
    #[serde(default)]
    #[schema(required)]
    pub needs_checking: usize,
    #[serde(default)]
    #[schema(required)]
    pub self_validated: usize,
    #[serde(default)]
    #[schema(required)]
    pub acknowledged: usize,
    #[serde(default)]
    #[schema(required)]
    pub will_skip: usize,
    #[serde(default)]
    #[schema(required)]
    pub projects_to_create: usize,
    #[serde(default)]
    #[schema(required)]
    pub sites_to_create: usize,
    #[serde(default)]
    #[schema(required)]
    pub parameters_to_create: usize,
    /// Distinct parameter groups the source's registry names that the database does not hold.
    #[serde(default)]
    #[schema(required)]
    pub groups_to_create: usize,
    /// Distinct lab instruments the apply would create, and how many of those an operator has
    /// not yet agreed to. Apply refuses while the second is non-zero.
    #[serde(default)]
    #[schema(required)]
    pub instruments_to_create: usize,
    #[serde(default)]
    #[schema(required)]
    pub instruments_unconfirmed: usize,
    #[serde(default)]
    #[schema(required)]
    pub unique_projects: usize,
    #[serde(default)]
    #[schema(required)]
    pub unique_sites: usize,
    #[serde(default)]
    #[schema(required)]
    pub unique_parameters: usize,
}

pub(super) struct ParamGroupProposal {
    pub(super) proposed_name: String,
    pub(super) units: String,
    pub(super) original_names: Vec<String>,
    pub(super) entry_indices: Vec<usize>,
}

pub(super) fn group_streams_by_parameter(
    entries: &[(usize, String, String)],
) -> Vec<ParamGroupProposal> {
    // Distinct quantities can share a units suffix (e.g. "Nitrate [µg/L]" vs
    // "Ammonia [µg/L]"), so only entries whose names are identical group together.
    let mut by_key: HashMap<(String, String), Vec<(usize, String)>> = HashMap::new();
    for (idx, name, units) in entries {
        by_key
            .entry((units.to_lowercase(), name.to_lowercase()))
            .or_default()
            .push((*idx, name.clone()));
    }

    by_key
        .into_iter()
        .map(|((units, _), members)| {
            let mut original_names: Vec<String> = members.iter().map(|(_, n)| n.clone()).collect();
            original_names.sort();
            original_names.dedup();
            ParamGroupProposal {
                proposed_name: members[0].1.clone(),
                units,
                original_names,
                entry_indices: members.iter().map(|(idx, _)| *idx).collect(),
            }
        })
        .collect()
}

/// Create a pairing plan for all unpaired streams of a given source system.
pub async fn create_plan(
    db: &impl ConnectionTrait,
    source_system: &str,
) -> AppResult<pairing_plans::Model> {
    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(source_system))
        .filter(data_streams::Column::SiteParameterId.is_null())
        .order_by_asc(data_streams::Column::SourceKey)
        .all(db)
        .await?;

    // A stream superseded by a replicate family (another stream at `source_key || ':reps'`) is a
    // retired legacy single whose stale metadata still carries the old label identity; planning it
    // would seed duplicate parameter rows. One query for the whole superseded set.
    let keys: std::collections::HashSet<String> = data_streams::models::Entity::find()
        .filter(data_streams::models::Column::SourceSystem.eq(source_system))
        .select_only()
        .column(data_streams::models::Column::SourceKey)
        .into_tuple::<String>()
        .all(db)
        .await?
        .into_iter()
        .collect();
    let superseded: std::collections::HashSet<String> = keys
        .iter()
        .filter(|key| keys.contains(&format!("{key}:reps")))
        .cloned()
        .collect();
    let streams: Vec<data_streams::Model> = streams
        .into_iter()
        .filter(|s| !superseded.contains(&s.source_key))
        .collect();

    if streams.is_empty() {
        return Err(AppError::BadRequest(format!(
            "No unpaired streams found for source_system '{source_system}'"
        )));
    }

    let catalog = load_entity_catalog(db, source_system).await?;
    let named_instruments: Vec<Uuid> = streams.iter().filter_map(|s| s.sensor_id).collect();
    let instruments = load_instrument_catalog(db, source_system, &named_instruments).await?;

    // Open replicate-statistics holds per stream, quoted by the review.
    let stream_ids: Vec<Uuid> = streams.iter().map(|s| s.id).collect();
    let sd_evidence: std::collections::HashMap<Uuid, i64> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT h.stream_id, count(*) AS holds \
                 FROM replicate_audit_holds h \
                 WHERE h.kind = '{REPLICATE_STATS}' \
                   AND h.status IN {open} \
                   AND h.stream_id = ANY($1) \
                 GROUP BY h.stream_id",
                REPLICATE_STATS = HoldKind::ReplicateStats.as_str(),
                open = *OPEN
            ),
            [stream_ids.into()],
        ))
        .await?
        .iter()
        .filter_map(|r| {
            let r = HoldCountRow::from_query_result(r, "").ok()?;
            Some((r.stream_id, r.holds))
        })
        .collect();

    // Build entries
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(streams.len());

    for stream in &streams {
        let h = extract_hierarchy(stream);

        let action = if h.site.is_empty() || h.parameter.is_empty() {
            "skip".to_string()
        } else {
            "pair".to_string()
        };

        // What is paired for a family is the replicate group, whose avg and sd this system
        // computes, so the suggested parameter is the measurand rather than the incoming
        // statistic column. The incoming name survives as original_parameter_name.
        let replicates = plan_replicates(&stream.metadata);
        let parameter_name = if replicates.is_some() && !h.parameter.is_empty() {
            family_parameter_suggestion(&h.parameter)
        } else {
            h.parameter.clone()
        };

        let sd_holds = sd_evidence.get(&stream.id).copied().unwrap_or(0);

        let mut entry = PlanEntry {
            stream_id: stream.id,
            source_key: stream.source_key.clone(),
            source_name: stream.source_name.clone(),
            action,
            project: PlanEntityRef {
                id: None,
                source_name: source_name(&h.project),
                name: h.project,
                create: false,
            },
            site: PlanSiteRef {
                id: None,
                source_name: source_name(&h.site),
                name: h.site,
                create: false,
                latitude: h.latitude,
                longitude: h.longitude,
                altitude_m: h.altitude_m,
            },
            parameter: PlanParamRef {
                id: None,
                name: parameter_name,
                label: h.parameter_label.clone(),
                create: false,
                units: h.units,
                group_key: None,
                group: plan_group(&stream.metadata),
                calculation: plan_calculation(&stream.metadata),
                original_names: vec![],
                attach: None,
            },
            confidence: "none".to_string(),
            warnings: vec![],
            original_parameter_name: Some(h.parameter),
            instrument: resolve_instrument(
                stream.sensor_id,
                replicates
                    .as_ref()
                    .and_then(|r| r.curve_ref_column.clone())
                    .as_deref(),
                source_system,
                &instruments,
            ),
            replicates,
            decimal_places: crate::routes::private::data_streams::service::declared_decimal_places(
                &stream.metadata,
            ),
            sd_holds,
            acknowledged: false,
            is_device: crate::routes::private::sensors::service::is_per_site_instrument(
                &stream.metadata,
            ),
            device_serial: crate::routes::private::sensors::service::extract_vaisala_device_serial(
                &stream.metadata,
            ),
            device_model: stream
                .metadata
                .get("device")
                .and_then(|d| d.get("logger_device"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        };
        reclassify_entry(&mut entry, &catalog);
        // A feed naming no curve column can still belong to an instrument this source created in
        // an earlier plan. A device-shaped feed is never one of those: its instrument is its own
        // channel, proposed once the parameter names below are settled.
        if entry.instrument.is_none() && !entry.is_device {
            entry.instrument = Some(resolve_parameter_instrument(
                stream_instrument_key(stream),
                &entry.parameter.name,
                &instruments,
            ));
        }
        entries.push(entry);
    }

    // Group new-to-create parameters with identical names (per units) across sites
    let to_group: Vec<(usize, String, String)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.action == "pair" && e.parameter.create)
        .map(|(i, e)| (i, e.parameter.name.clone(), e.parameter.units.clone()))
        .collect();

    if !to_group.is_empty() {
        for group in group_streams_by_parameter(&to_group) {
            if group.entry_indices.len() <= 1 {
                continue;
            }
            let key = format!("{}::{}", group.units, group.proposed_name);
            for &idx in &group.entry_indices {
                entries[idx].parameter.name = group.proposed_name.clone();
                entries[idx].parameter.group_key = Some(key.clone());
                entries[idx].parameter.original_names = group.original_names.clone();
            }
        }
    }

    // Named for the slot after the grouping above, which is what renames a parameter.
    for entry in entries
        .iter_mut()
        .filter(|e| e.is_device && e.instrument.is_none())
    {
        entry.instrument = Some(resolve_device_instrument(
            &entry.source_key,
            &device_slot_name(entry, &catalog),
            &instruments,
        ));
    }

    flag_duplicates_in_plan(&mut entries);
    let summary = compute_summary(&entries);
    // The register rows this source has offered and no plan has taken yet. Snapshotted onto the
    // plan so the review's decisions are the plan's, like every other proposal it carries.
    let proposals = pending_instrument_proposals(db, source_system, &instruments).await?;

    let plan = pairing_plans::ActiveModel {
        id: Set(Uuid::new_v4()),
        source_system: Set(source_system.to_string()),
        status: Set("draft".to_string()),
        created_by: Set(None),
        summary: Set(summary),
        entries: Set(PlanEntries(entries)),
        curve_assignments: Set(PlanCurveIntents::default()),
        curve_attachments: Set(PlanCurveAttachments::default()),
        accepted_objects: Set(PlanAcceptedObjects::default()),
        instrument_proposals: Set(PlanInstrumentProposals(proposals)),
        version: Set(0),
        created_at: Set(Utc::now().into()),
        applied_at: Set(None),
        apply_result: Set(None),
    };

    let inserted = plan.insert(db).await?;
    Ok(inserted)
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
pub struct ApplyResult {
    pub projects_created: u32,
    pub sites_created: u32,
    pub parameters_created: u32,
    pub site_parameters_created: u32,
    pub streams_paired: u32,
    #[serde(default)]
    #[schema(required)]
    pub streams_skipped: u32,
    #[serde(default)]
    #[schema(required)]
    pub instruments_created: u32,
    /// Standard curves moved onto instruments this apply minted.
    #[serde(default)]
    #[schema(required)]
    pub curves_assigned: u32,
    /// Held curves this apply created under the instruments the review attached them to.
    #[serde(default)]
    #[schema(required)]
    pub curves_created: u32,
    /// Parameter groups the source's registry named that the database did not hold, and the
    /// memberships placed in them.
    #[serde(default)]
    #[schema(required)]
    pub groups_created: u32,
    #[serde(default)]
    #[schema(required)]
    pub group_members_created: u32,
    pub readings_backfilled: u64,
}

/// The plan's entry list as the column holds it, so the row carries the entries themselves rather
/// than a JSON document nothing describes.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
#[serde(transparent)]
pub struct PlanEntries(pub Vec<PlanEntry>);

/// The assigned curves as the column holds them.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
#[serde(transparent)]
pub struct PlanCurveIntents(pub Vec<PlanCurveIntent>);

/// The objects the review has accepted, as the column holds them.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
#[serde(transparent)]
pub struct PlanAcceptedObjects(pub Vec<PlanAcceptedObject>);

/// A project, site or parameter the review agreed to, keyed `{kind}:{name}` the way the card
/// names it. Recorded because it is one decision behind many rows, and no row can carry it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanAcceptedObject {
    pub key: String,
    #[schema(required)]
    pub accepted_by: Option<String>,
    pub accepted_at: chrono::DateTime<chrono::FixedOffset>,
}

/// The held curves the review attached, as the column holds them.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
#[serde(transparent)]
pub struct PlanCurveAttachments(pub Vec<PlanCurveAttachment>);

/// A held curve the review attached to one of the plan's instruments: one the plan creates, by
/// `instrument_source_key`, or one that already exists, by `instrument_id`. The apply creates the
/// curve under it (Q195).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanCurveAttachment {
    pub proposal_id: Uuid,
    #[serde(default)]
    #[schema(required)]
    pub instrument_source_key: Option<String>,
    #[serde(default)]
    #[schema(required)]
    pub instrument_id: Option<Uuid>,
}

/// A standard curve the review assigned to an instrument the plan creates, keyed by the
/// instrument's `source_key` because the row does not exist until the apply mints it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanCurveIntent {
    pub curve_id: Uuid,
    pub instrument_source_key: String,
}

/// Readings naming one curve on one stream: how many, and the span they cover.
pub struct CurveUse {
    pub curve_id: Uuid,
    pub stream_id: Uuid,
    pub n: i64,
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
}

/// What a curve corrects, in data terms: the parameters and stations of the plan's streams whose
/// readings name it, and the period those readings cover. A stream outside the plan counts toward
/// the readings and the period but names no station.
#[derive(Debug, Default)]
pub struct CurveReach {
    pub reading_count: i64,
    pub parameters: Vec<String>,
    pub sites: Vec<String>,
    pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>,
}

pub fn curve_reach(uses: &[CurveUse], entries: &[PlanEntry]) -> HashMap<Uuid, CurveReach> {
    let by_stream: HashMap<Uuid, &PlanEntry> = entries.iter().map(|e| (e.stream_id, e)).collect();
    let mut reach: HashMap<Uuid, CurveReach> = HashMap::new();
    for u in uses {
        let r = reach.entry(u.curve_id).or_default();
        r.reading_count += u.n;
        r.first = Some(r.first.map_or(u.first, |f| Ord::min(f, u.first)));
        r.last = Some(r.last.map_or(u.last, |l| Ord::max(l, u.last)));
        if let Some(entry) = by_stream.get(&u.stream_id) {
            r.parameters.push(entry.parameter.name.clone());
            r.sites.push(entry.site.name.clone());
        }
    }
    for r in reach.values_mut() {
        r.parameters.sort();
        r.parameters.dedup();
        r.sites.sort();
        r.sites.dedup();
    }
    reach
}

/// The curve assignments a plan carries.
pub fn plan_curve_intents(plan: &pairing_plans::Model) -> AppResult<Vec<PlanCurveIntent>> {
    Ok(plan.curve_assignments.0.clone())
}

/// Move each assigned curve onto the instrument the apply minted for its `source_key`. Runs
/// inside the apply transaction, after `mint_plan_instruments`. An assignment naming an
/// instrument the plan no longer creates, or a curve readings already name, fails the apply
/// rather than being dropped: the review chose it, so nothing here may quietly not do it.
pub(super) async fn assign_plan_curves<C: ConnectionTrait>(
    txn: &C,
    intents: &[PlanCurveIntent],
    minted: &HashMap<String, Uuid>,
) -> AppResult<u32> {
    let mut moved = 0u32;
    for intent in intents {
        let Some(&sensor_id) = minted.get(&intent.instrument_source_key) else {
            return Err(AppError::BadRequest(format!(
                "curve {} is assigned to instrument '{}', which this plan no longer creates; \
                 reassign or clear the curve before applying",
                intent.curve_id, intent.instrument_source_key
            )));
        };
        if crate::routes::private::standard_curves::views::curve_is_used(txn, intent.curve_id)
            .await?
        {
            return Err(AppError::BadRequest(format!(
                "curve {} has already been applied to readings, so its instrument is fixed; \
                 clear the assignment before applying",
                intent.curve_id
            )));
        }
        let result = standard_curves::models::Entity::update_many()
            .col_expr(
                standard_curves::models::Column::SensorId,
                Expr::value(sensor_id),
            )
            .filter(standard_curves::models::Column::Id.eq(intent.curve_id))
            .exec(txn)
            .await?;
        if result.rows_affected == 0 {
            return Err(AppError::BadRequest(format!(
                "curve {} no longer exists; clear the assignment before applying",
                intent.curve_id
            )));
        }
        moved += 1;
    }
    Ok(moved)
}

/// The curves a source holds for a pairing plan, in the order the review lists them.
pub async fn held_curves<C: ConnectionTrait>(
    conn: &C,
    source_system: &str,
) -> AppResult<Vec<standard_curves::models::proposal::Model>> {
    use standard_curves::models::proposal;
    Ok(proposal::Entity::find()
        .filter(proposal::Column::SourceSystem.eq(source_system))
        .order_by_asc(proposal::Column::Label)
        .order_by_asc(proposal::Column::SourceKey)
        .all(conn)
        .await?)
}

/// How a held curve is named to the operator: its name where the source gave one, else its label
/// and key.
fn held_curve_title(curve: &standard_curves::models::proposal::Model) -> String {
    curve
        .name
        .clone()
        .unwrap_or_else(|| format!("{} ({})", curve.label, curve.source_key))
}

/// Refuse a plan while any curve its source holds has been neither attached to one of its
/// instruments nor skipped. The curve cannot be stored without an instrument, and leaving it
/// behind unsaid would leave the readings that name it uncorrected with nothing saying why (Q195);
/// a skip is that saying so (Q220).
pub async fn refuse_unattached_curves<C: ConnectionTrait>(
    conn: &C,
    source_system: &str,
    attachments: &[PlanCurveAttachment],
) -> AppResult<()> {
    let undecided: Vec<String> = held_curves(conn, source_system)
        .await?
        .iter()
        .filter(|c| c.skipped_at.is_none())
        .filter(|c| !attachments.iter().any(|a| a.proposal_id == c.id))
        .map(held_curve_title)
        .collect();
    if undecided.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "{} standard curve{} from {source_system} {} been ruled on: {}. Attach each to one of the \
         plan's instruments, or skip it, before applying.",
        undecided.len(),
        if undecided.len() == 1 { "" } else { "s" },
        if undecided.len() == 1 {
            "has not"
        } else {
            "have not"
        },
        undecided.join(", ")
    )))
}

/// Stamp or clear a held curve's skip. Refuses a curve of another source, which is the same guard
/// the attachment arm applies, so a stray id cannot reach across sources.
async fn stamp_held_curve_skip(
    db: &sea_orm::DatabaseConnection,
    source_system: &str,
    proposal_id: Uuid,
    skip: bool,
    actor: &str,
) -> AppResult<()> {
    use standard_curves::models::proposal;
    let affected = proposal::Entity::update_many()
        .col_expr(
            proposal::Column::SkippedAt,
            Expr::value(skip.then(Utc::now)),
        )
        .col_expr(
            proposal::Column::SkippedBy,
            Expr::value(skip.then(|| actor.to_string())),
        )
        .filter(proposal::Column::Id.eq(proposal_id))
        .filter(proposal::Column::SourceSystem.eq(source_system))
        .exec(db)
        .await?;
    if affected.rows_affected == 0 {
        return Err(AppError::BadRequest(format!(
            "{source_system} holds no curve {proposal_id}"
        )));
    }
    Ok(())
}

/// Fold the review's held-curve decisions into the plan. A held curve of this source may be
/// attached to an instrument some paired entry proposes creating, or to one that exists; naming
/// neither clears it. A skip is the other decision the apply accepts: it is stamped on the held
/// row, and attaching takes it back, because a curve cannot be both left behind and created
/// (Q220). Anything else is a 400 now rather than a failed apply later.
pub(super) async fn apply_held_curve_updates(
    db: &sea_orm::DatabaseConnection,
    source_system: &str,
    entries: &[PlanEntry],
    attachments: &mut Vec<PlanCurveAttachment>,
    updates: &[PlanHeldCurveUpdate],
    actor: &str,
) -> AppResult<()> {
    use standard_curves::models::proposal;
    for update in updates {
        attachments.retain(|a| a.proposal_id != update.proposal_id);
        let source_key = update
            .instrument_source_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        if update.skip == Some(true) && (source_key.is_some() || update.instrument_id.is_some()) {
            return Err(AppError::BadRequest(
                "a held curve is either skipped or attached to an instrument, not both".to_string(),
            ));
        }
        if let Some(skip) = update.skip {
            stamp_held_curve_skip(db, source_system, update.proposal_id, skip, actor).await?;
            if skip {
                continue;
            }
        }
        if source_key.is_none() && update.instrument_id.is_none() {
            continue;
        }
        if source_key.is_some() && update.instrument_id.is_some() {
            return Err(AppError::BadRequest(
                "attach a held curve to one instrument: a source key the plan creates, or an \
                 existing instrument id, not both"
                    .to_string(),
            ));
        }
        let held = proposal::Entity::find_by_id(update.proposal_id)
            .one(db)
            .await?
            .filter(|c| c.source_system == source_system);
        if held.is_none() {
            return Err(AppError::BadRequest(format!(
                "{source_system} holds no curve {}",
                update.proposal_id
            )));
        }
        if let Some(key) = source_key {
            let proposed = entries.iter().any(|e| {
                e.action == "pair"
                    && e.instrument
                        .as_ref()
                        .is_some_and(|i| i.create && i.source_key == key)
            });
            if !proposed {
                return Err(AppError::BadRequest(format!(
                    "this plan does not create an instrument with source key '{key}'"
                )));
            }
        }
        if let Some(id) = update.instrument_id
            && sensors::models::Entity::find_by_id(id)
                .one(db)
                .await?
                .is_none()
        {
            return Err(AppError::BadRequest(format!(
                "instrument {id} does not exist"
            )));
        }
        stamp_held_curve_skip(db, source_system, update.proposal_id, false, actor).await?;
        attachments.push(PlanCurveAttachment {
            proposal_id: update.proposal_id,
            instrument_source_key: source_key.map(str::to_string),
            instrument_id: update.instrument_id,
        });
    }
    Ok(())
}

/// Create each attached held curve under its instrument, and stop holding it. Runs inside the
/// apply transaction, after `mint_plan_instruments`, so an instrument this plan creates exists by
/// its `source_key`. An attachment the apply cannot honour fails the apply rather than being
/// dropped.
pub(super) async fn create_attached_curves<C: ConnectionTrait>(
    txn: &C,
    source_system: &str,
    attachments: &[PlanCurveAttachment],
    minted: &HashMap<String, Uuid>,
) -> AppResult<u32> {
    use standard_curves::models::proposal;
    let mut created = 0u32;
    for attachment in attachments {
        let Some(held) = proposal::Entity::find_by_id(attachment.proposal_id)
            .one(txn)
            .await?
            .filter(|c| c.source_system == source_system)
        else {
            return Err(AppError::BadRequest(format!(
                "curve {} is no longer held; clear its attachment before applying",
                attachment.proposal_id
            )));
        };
        let sensor_id = match (&attachment.instrument_source_key, attachment.instrument_id) {
            (Some(key), _) => minted.get(key).copied().ok_or_else(|| {
                AppError::BadRequest(format!(
                    "curve {} is attached to instrument '{key}', which this plan no longer \
                     creates; attach it again before applying",
                    held_curve_title(&held)
                ))
            })?,
            (None, Some(id)) => id,
            (None, None) => continue,
        };
        standard_curves::models::ActiveModel {
            id: Set(Uuid::new_v4()),
            sensor_id: Set(sensor_id),
            name: Set(held.name.clone()),
            fitted_on: Set(Some(
                held.fitted_on.unwrap_or_else(|| Utc::now().date_naive()),
            )),
            slope: Set(held.slope),
            intercept: Set(held.intercept),
            r_squared: Set(held.r_squared),
            notes: Set(held.notes.clone()),
            created_at: Set(Utc::now()),
            created_by: Set(Some(format!("sync:{source_system}"))),
            source_system: Set(Some(source_system.to_string())),
            source_key: Set(Some(held.source_key.clone())),
            copied_from_id: Set(None),
            retired_at: Set(None),
            retired_by: Set(None),
            retired_reason: Set(None),
        }
        .insert(txn)
        .await?;
        proposal::Entity::delete_by_id(held.id).exec(txn).await?;
        created += 1;
    }
    Ok(created)
}

pub(super) struct EntityCaches {
    pub(super) projects: HashMap<String, Uuid>,
    pub(super) groups: HashMap<String, Uuid>,
    pub(super) sites: HashMap<String, Uuid>,
    pub(super) params: HashMap<String, Uuid>,
    pub(super) site_params: HashMap<(Uuid, Uuid), Uuid>,
    pub(super) param_names: HashMap<Uuid, String>,
}

pub(super) struct ApplyCounters {
    pub(super) projects_created: u32,
    pub(super) groups_created: u32,
    pub(super) group_members_created: u32,
    pub(super) sites_created: u32,
    pub(super) params_created: u32,
    pub(super) sp_created: u32,
    pub(super) streams_paired: u32,
    pub(super) streams_skipped: u32,
    pub(super) instruments_created: u32,
    pub(super) curves_assigned: u32,
    pub(super) curves_created: u32,
}

/// The streams whose instrument nobody has agreed to: a creation, or an attachment the plan
/// suggested from a curve label.
pub fn unconfirmed_instruments(entries: &[PlanEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter(|e| e.instrument.as_ref().is_some_and(|i| !i.confirmed))
        .map(|e| e.source_key.as_str())
        .collect()
}

/// An instrument nobody agreed to is not written silently: once readings name it, it is provenance,
/// so a suggestion is confirmed before the apply mints it or attaches to it.
pub fn refuse_unconfirmed_instruments(entries: &[PlanEntry]) -> AppResult<()> {
    let unconfirmed = unconfirmed_instruments(entries);
    if unconfirmed.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "{} stream(s) have an instrument nobody has confirmed: {}",
        unconfirmed.len(),
        unconfirmed
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join(", "),
    )))
}

/// A plan is applied once, and rectifying it afterwards costs more than reviewing it did (Q133),
/// so the apply waits until every pairing row has been ticked: a tick records that a person
/// looked, and a row that resolved cleanly is looked at like any other (Q155). Enforced here
/// rather than only on the button: a route reachable by curl, by a stale tab or by a second
/// client is a gate nobody holds. `review_state` is the same rule the review reads.
pub fn refuse_unchecked_entries(entries: &[PlanEntry]) -> AppResult<()> {
    let unchecked: Vec<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter(|e| review_state(e) != ReviewState::Acknowledged)
        .map(|e| e.source_key.as_str())
        .collect();
    if unchecked.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "{} row(s) still to tick before this plan can be applied: {}",
        unchecked.len(),
        unchecked
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join(", "),
    )))
}

/// Warn every pairing row the plan would create twice over: a parameter code proposed with two sets
/// of units, and a site or project name another row spells differently. The catalog check cannot see
/// these, because neither side of the pair exists yet, and the review is where the two rows are
/// visible together.
pub fn flag_duplicates_in_plan(entries: &mut [PlanEntry]) {
    let mut param_units: HashMap<String, Vec<String>> = HashMap::new();
    let mut site_spellings: HashMap<String, Vec<String>> = HashMap::new();
    let mut project_spellings: HashMap<String, Vec<String>> = HashMap::new();
    for entry in entries.iter().filter(|e| e.action == "pair") {
        if entry.parameter.create && !entry.parameter.name.trim().is_empty() {
            let units = param_units
                .entry(entry.parameter.name.trim().to_lowercase())
                .or_default();
            if !units.contains(&entry.parameter.units) {
                units.push(entry.parameter.units.clone());
            }
        }
        for (create, name, spellings) in [
            (entry.site.create, &entry.site.name, &mut site_spellings),
            (
                entry.project.create,
                &entry.project.name,
                &mut project_spellings,
            ),
        ] {
            let canonical = canonical_name(name);
            if !create || canonical.is_empty() {
                continue;
            }
            let seen = spellings.entry(canonical).or_default();
            if !seen.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                seen.push(name.clone());
            }
        }
    }

    for entry in entries.iter_mut().filter(|e| e.action == "pair") {
        if let Some(units) = param_units.get(&entry.parameter.name.trim().to_lowercase())
            && units.len() > 1
        {
            let others: Vec<&str> = units.iter().map(String::as_str).collect();
            entry.warnings.push(PlanWarning::duplicate_in_plan(
                "parameter_code",
                &entry.parameter.name,
                &others.join(", "),
            ));
        }
        for (kind, name, spellings) in [
            ("site_name", &entry.site.name, &site_spellings),
            ("project_name", &entry.project.name, &project_spellings),
        ] {
            if let Some(seen) = spellings.get(&canonical_name(name))
                && let Some(other) = seen.iter().find(|s| !s.eq_ignore_ascii_case(name))
            {
                entry
                    .warnings
                    .push(PlanWarning::duplicate_in_plan(kind, name, other));
            }
        }
    }
}

/// A parameter code two pairing rows would both create under, or one the catalog already holds:
/// `LOWER(code)` is unique, so the apply's second insert fails halfway through the run. Reported
/// as the plan's own refusal, naming the code and what it collides with.
pub fn colliding_parameter_codes(entries: &[PlanEntry], catalog: &EntityCatalog) -> Vec<String> {
    let mut by_code: HashMap<String, Vec<&PlanEntry>> = HashMap::new();
    for entry in entries
        .iter()
        .filter(|e| e.action == "pair" && e.parameter.create)
    {
        let code = entry.parameter.name.trim().to_lowercase();
        if code.is_empty() {
            continue;
        }
        by_code.entry(code).or_default().push(entry);
    }
    let mut collisions: Vec<String> = Vec::new();
    for (code, group) in by_code {
        if let Some(existing) = catalog
            .params
            .iter()
            .find(|p| p.code.to_lowercase() == code)
        {
            collisions.push(format!(
                "'{}' is already a catalog parameter; attach those columns to it or rename them",
                existing.code
            ));
            continue;
        }
        let mut units: Vec<&str> = group.iter().map(|e| e.parameter.units.as_str()).collect();
        units.sort_unstable();
        units.dedup();
        if units.len() > 1 {
            collisions.push(format!(
                "'{code}' is proposed with {} sets of units ({}); one code is one parameter",
                units.len(),
                units.join(", ")
            ));
        }
    }
    collisions.sort();
    collisions
}

/// One code is one parameter, so a plan that would create two of them cannot be applied.
pub fn refuse_colliding_parameter_codes(
    entries: &[PlanEntry],
    catalog: &EntityCatalog,
) -> AppResult<()> {
    let collisions = colliding_parameter_codes(entries, catalog);
    if collisions.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "{} parameter code(s) cannot be created as proposed: {}",
        collisions.len(),
        collisions.join("; ")
    )))
}

/// Apply a pairing plan: create entities, pair streams, backfill readings.
/// A named step of a long-running flow, written to the run's timeline when one is watching. The
/// flows below are reached both from a job and from a request, so the line is conditional on the
/// context rather than on a branch at every call site.
async fn step(
    progress: Option<&crate::routes::private::reprocessing_jobs::service::JobContext>,
    line: &str,
) {
    if let Some(ctx) = progress {
        ctx.info(line).await;
    }
}

pub async fn apply_plan(
    db: &sea_orm::DatabaseConnection,
    plan_id: Uuid,
    progress: Option<&crate::routes::private::reprocessing_jobs::service::JobContext>,
) -> AppResult<ApplyResult> {
    let plan = pairing_plans::Entity::find_by_id(plan_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{}', can only apply 'draft' plans",
            plan.status
        )));
    }

    let entries: Vec<PlanEntry> = plan.entries.0.clone();
    let curve_intents = plan_curve_intents(&plan)?;

    refuse_unconfirmed_instruments(&entries)?;
    refuse_unchecked_entries(&entries)?;
    refuse_colliding_parameter_codes(
        &entries,
        &load_entity_catalog(db, &plan.source_system).await?,
    )?;
    refuse_unattached_curves(db, &plan.source_system, &plan.curve_attachments.0).await?;
    if let Some(reason) =
        crate::routes::private::data_streams::service::pairing_refusal(&plan.source_system)
    {
        return Err(AppError::BadRequest(reason));
    }

    let txn = db.begin().await?;

    // Atomic status claim: a concurrent apply of the same plan matches zero rows and bails.
    // A rollback restores 'draft'.
    if !claim_plan_status(&txn, plan_id, "draft", "applying").await? {
        return Err(AppError::BadRequest(
            "Plan is no longer in draft status".to_string(),
        ));
    }

    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

    let param_names: HashMap<Uuid, String> = parameters::Entity::find()
        .all(&txn)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();

    let accepted_objects: HashSet<String> = plan
        .accepted_objects
        .0
        .iter()
        .map(|a| a.key.clone())
        .collect();

    let mut caches = EntityCaches {
        projects: HashMap::new(),
        groups: HashMap::new(),
        sites: HashMap::new(),
        params: HashMap::new(),
        site_params: HashMap::new(),
        param_names,
    };
    let mut counters = ApplyCounters {
        projects_created: 0,
        groups_created: 0,
        group_members_created: 0,
        sites_created: 0,
        params_created: 0,
        sp_created: 0,
        streams_paired: 0,
        streams_skipped: 0,
        instruments_created: 0,
        curves_assigned: 0,
        curves_created: 0,
    };

    let minted = mint_plan_instruments(&txn, &plan.source_system, &entries).await?;
    counters.instruments_created = minted.len() as u32;
    counters.instruments_created += entries
        .iter()
        .filter(|e| e.action == "pair" && e.is_device)
        .filter(|e| {
            e.instrument
                .as_ref()
                .is_some_and(|i| i.create && i.id.is_none())
        })
        .count() as u32;
    // The source's own register, admitted by the same apply: an instrument exists because a plan an
    // operator validated created it, whether it came from a feed or from the register (Q134).
    counters.instruments_created +=
        admit_instrument_proposals(&txn, &plan.source_system, &plan.instrument_proposals.0).await?;
    counters.curves_assigned = assign_plan_curves(&txn, &curve_intents, &minted).await?;
    counters.curves_created = create_attached_curves(
        &txn,
        &plan.source_system,
        &plan.curve_attachments.0,
        &minted,
    )
    .await?;
    step(
        progress,
        &format!(
            "Registered {} instruments, assigned {} curves and created {}",
            counters.instruments_created, counters.curves_assigned, counters.curves_created
        ),
    )
    .await;

    // How far the apply has got, on the pool connection rather than inside `txn`, so the operator
    // sees an import of a couple of thousand entries move instead of a spinner.
    let pairing_total = entries.iter().filter(|e| e.action == "pair").count();
    if let Some(ctx) = progress {
        ctx.set_progress(0, Some(i32::try_from(pairing_total).unwrap_or(i32::MAX)))
            .await;
    }
    let mut entries_seen: usize = 0;

    for entry in entries.iter().filter(|e| e.action == "pair") {
        entries_seen += 1;
        if let Some(ctx) = progress
            && (entries_seen.is_multiple_of(PROGRESS_BATCH) || entries_seen == pairing_total)
        {
            ctx.set_progress(i32::try_from(entries_seen).unwrap_or(i32::MAX), None)
                .await;
        }
        if (entry.site.id.is_none() && entry.site.name.trim().is_empty())
            || (entry.parameter.id.is_none() && entry.parameter.name.trim().is_empty())
        {
            tracing::warn!(
                stream_id = %entry.stream_id,
                "apply_plan: skipping entry with empty site or parameter name",
            );
            counters.streams_skipped += 1;
            continue;
        }
        let Some(stream) = data_streams::Entity::find_by_id(entry.stream_id)
            .one(&txn)
            .await?
        else {
            tracing::warn!(
                stream_id = %entry.stream_id,
                "apply_plan: skipping entry whose stream no longer exists",
            );
            counters.streams_skipped += 1;
            continue;
        };
        // Checked before resolving so a skipped entry leaves no orphan site or parameter behind.
        if let Some(existing_sp) = stream.site_parameter_id {
            tracing::warn!(
                stream_id = %entry.stream_id,
                site_parameter_id = %existing_sp,
                "apply_plan: skipping stream that is already paired",
            );
            counters.streams_skipped += 1;
            continue;
        }
        let (site_parameter_id, parameter_id) = resolve_plan_entry(
            &txn,
            entry,
            &plan.source_system,
            &mut caches,
            &mut counters,
            &accepted_objects,
        )
        .await?;
        let instrument_id = entry
            .instrument
            .as_ref()
            .and_then(|i| i.id.or_else(|| minted.get(&i.source_key).copied()));
        let device_name = entry
            .instrument
            .as_ref()
            .filter(|i| entry.is_device && i.create)
            .map(|i| i.name.as_str());
        pair_entry_stream(
            &txn,
            stream,
            plan_id,
            site_parameter_id,
            parameter_id,
            instrument_id,
            device_name,
        )
        .await?;
        counters.streams_paired += 1;
    }

    step(
        progress,
        &format!(
            "Paired {} streams, skipped {}; created {} sites, {} parameters and {} slots",
            counters.streams_paired,
            counters.streams_skipped,
            counters.sites_created,
            counters.params_created,
            counters.sp_created
        ),
    )
    .await;

    let backfilled = backfill_plan_readings(&txn, plan_id).await?;
    let readings_backfilled = backfilled.readings;
    step(
        progress,
        &format!(
            "Attributed {readings_backfilled} readings to the slots this plan paired, across {} visits",
            backfilled.touched_events.len()
        ),
    )
    .await;
    finalize_plan(&txn, plan_id, &counters, readings_backfilled).await?;
    txn.commit().await?;

    // Attribution is what made these readings visit values; the calculations that read them at
    // each manual visit run now (ADR 0007). The plan runs as a job, so the writer it records is
    // the system rather than a person.
    crate::routes::private::collection_events::flows::enqueue_for(
        db,
        &backfilled.touched_events,
        "system",
        crate::routes::private::collection_events::flows::Writer::Person,
    )
    .await?;

    // Re-derive the paired readings by the deployment + calibration windows for each touched
    // (site, parameter) slot, then a full refresh as a safety net. `backfill_plan_readings` only
    // stamps site_id/parameter_id; the window-aware engine (same one ingest/reprocess use) assigns
    // sensor_id/deployment_id/calibration_id and the per-window calibrated_value, while its recall
    // guard leaves pre-deployment history attributed by the pairing. Runs post-commit because the
    // reprocess opens its own transaction and refreshes continuous aggregates (which can't run
    // inside one).
    let slots: Vec<(Uuid, Uuid)> = plan_slots(plan_id).into_tuple().all(db).await?;
    // History that ended before river-data held it did not go stale on river-data's watch, so the
    // first dispatcher tick after the apply announces none of it. Only the running service has the
    // threshold; a process without one sends no notifications either.
    if let Some(state) = crate::common::global_app_state() {
        let suppressed = crate::routes::private::notifications::flows::suppress_stale_for_history(
            db,
            &slots,
            state.config.stale_data_threshold_hours,
        )
        .await?;
        if suppressed > 0 {
            tracing::info!(
                plan_id = %plan_id,
                suppressed,
                "apply_plan: slots whose history had already stopped start out silent",
            );
        }
    }

    // Re-derivation runs as one tracked job under the apply, so a failure is visible and the panel
    // shows the plan's own progress rather than a row per slot (B418).
    crate::routes::private::reprocessing_jobs::service::enqueue_child(
        db,
        "plan_attribution",
        &serde_json::json!({ "plan_id": plan_id }),
        progress.map(crate::routes::private::reprocessing_jobs::service::JobContext::job_id),
    )
    .await?;
    step(
        progress,
        &format!(
            "Queued re-derivation of {} slots by deployment and calibration window",
            slots.len()
        ),
    )
    .await;
    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        streams_skipped: counters.streams_skipped,
        instruments_created: counters.instruments_created,
        curves_assigned: counters.curves_assigned,
        curves_created: counters.curves_created,
        groups_created: counters.groups_created,
        group_members_created: counters.group_members_created,
        readings_backfilled,
    };

    tracing::info!(
        plan_id = %plan_id,
        streams_paired = counters.streams_paired,
        streams_skipped = counters.streams_skipped,
        sites_created = counters.sites_created,
        params_created = counters.params_created,
        readings_backfilled,
        "Pairing plan applied"
    );

    Ok(result)
}

/// Resolve or create all entities for one plan entry. Returns (site_parameter_id, parameter_id).
pub(super) async fn resolve_plan_entry<C: ConnectionTrait>(
    txn: &C,
    entry: &PlanEntry,
    source_system: &str,
    caches: &mut EntityCaches,
    counters: &mut ApplyCounters,
    accepted_objects: &HashSet<String>,
) -> AppResult<(Uuid, Uuid)> {
    let project_id = resolve_or_create_project(
        txn,
        &entry.project,
        &mut caches.projects,
        &mut counters.projects_created,
        source_system,
    )
    .await?;
    let site_id = resolve_or_create_site(
        txn,
        &entry.site,
        &mut caches.sites,
        &mut counters.sites_created,
        project_id,
        source_system,
    )
    .await?;
    if let Some(source_name) = &entry.project.source_name {
        project_source_links::link(txn, source_system, source_name, project_id).await?;
    }
    if let Some(source_name) = &entry.site.source_name {
        site_source_links::link(txn, source_system, source_name, site_id).await?;
    }
    let parameter_id = resolve_or_create_param(
        txn,
        &entry.parameter,
        entry.original_parameter_name.as_deref(),
        &mut caches.params,
        &mut caches.param_names,
        &mut counters.params_created,
        accepted_objects,
    )
    .await?;
    if let Some(group) = entry.parameter.group.as_ref() {
        place_in_group(
            txn,
            parameter_id,
            group,
            entry.parameter.calculation.as_ref(),
            &mut caches.groups,
            &mut counters.groups_created,
            &mut counters.group_members_created,
        )
        .await?;
    }
    let site_parameter_id = resolve_or_create_site_param(
        txn,
        site_id,
        parameter_id,
        entry,
        caches,
        &mut counters.sp_created,
    )
    .await?;
    Ok((site_parameter_id, parameter_id))
}

/// Put one parameter in the group its source's registry names, creating the group on first use.
///
/// A parameter belongs to at most one group (`parameter_group_members.parameter_id` is UNIQUE), so
/// a parameter someone has already placed keeps the placement it has: the apply fills a gap, it
/// does not move what an operator decided. The group's own ordinal is its first member's, which is
/// the order the registry lists the categories in.
pub(super) async fn place_in_group<C: ConnectionTrait>(
    txn: &C,
    parameter_id: Uuid,
    group: &PlanGroupRef,
    calculation: Option<&PlanCalculationRef>,
    cache: &mut HashMap<String, Uuid>,
    groups_created: &mut u32,
    members_created: &mut u32,
) -> AppResult<()> {
    let group_id = match cache.get(&group.code) {
        Some(&id) => id,
        None => {
            let id = group.id.unwrap_or_else(Uuid::new_v4);
            let written = parameter_groups::Entity::insert(parameter_groups::ActiveModel {
                id: Set(id),
                code: Set(group.code.clone()),
                label: Set(group.label.clone()),
                ordinal: Set(group.ordinal),
                ..Default::default()
            })
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(parameter_groups::Column::Code)
                    .do_nothing()
                    .to_owned(),
            )
            .try_insert()
            .exec(txn)
            .await?;
            if matches!(written, sea_orm::TryInsertResult::Inserted(_)) {
                *groups_created += 1;
            }
            // The insert may have lost the race with another entry of this same pass, so the id is
            // read back rather than assumed.
            let resolved = parameter_groups::Entity::find()
                .filter(parameter_groups::Column::Code.eq(group.code.as_str()))
                .select_only()
                .column(parameter_groups::Column::Id)
                .into_tuple::<Uuid>()
                .one(txn)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(format!(
                        "parameter group '{}' was neither found nor created",
                        group.code
                    ))
                })?;
            cache.insert(group.code.clone(), resolved);
            resolved
        }
    };
    let source_calculation = calculation
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| AppError::Internal(format!("source calculation is not serialisable: {e}")))?;
    let written = member_model::Entity::insert(member_model::ActiveModel {
        id: Set(Uuid::new_v4()),
        group_id: Set(group_id),
        parameter_id: Set(parameter_id),
        ordinal: Set(group.ordinal),
        description: Set(group.description.clone()),
        source_calculation: Set(source_calculation),
        ..Default::default()
    })
    .on_conflict(
        sea_orm::sea_query::OnConflict::column(member_model::Column::ParameterId)
            .do_nothing()
            .to_owned(),
    )
    .try_insert()
    .exec(txn)
    .await?;
    if matches!(written, sea_orm::TryInsertResult::Inserted(_)) {
        *members_created += 1;
    }
    Ok(())
}

/// The names a new slot may take, most preferred first: the parameter's label, then the label
/// qualified by units, then by the parameter's code. Beyond those a counter is appended, because
/// two parameters can share a label, its units and nothing else.
pub(super) fn slot_name_candidates(base: &str, units: &str, code: &str) -> Vec<String> {
    let mut names = vec![base.to_string()];
    let units = units.trim();
    if !units.is_empty() {
        names.push(format!("{base} ({units})"));
    }
    if !code.is_empty() && code != units {
        names.push(format!("{base} ({code})"));
    }
    names
}

/// The first candidate the site does not already hold. `(site_id, name)` is unique and the apply is
/// one transaction, so the query sees the slots this same pass has created.
pub(super) async fn free_slot_name<C: ConnectionTrait>(
    txn: &C,
    site_id: Uuid,
    base: &str,
    units: &str,
    code: &str,
) -> AppResult<String> {
    let taken = async |name: &str| -> AppResult<bool> {
        Ok(site_parameters::Entity::find()
            .filter(
                Condition::all()
                    .add(site_parameters::Column::SiteId.eq(site_id))
                    .add(site_parameters::Column::Name.eq(name.to_string())),
            )
            .one(txn)
            .await?
            .is_some())
    };
    for name in slot_name_candidates(base, units, code) {
        if !taken(&name).await? {
            return Ok(name);
        }
    }
    let qualified = slot_name_candidates(base, units, code)
        .pop()
        .unwrap_or_else(|| base.to_string());
    for n in 2.. {
        let name = format!("{qualified} {n}");
        if !taken(&name).await? {
            return Ok(name);
        }
    }
    unreachable!()
}

/// The cadence the slot a stream pairs into is filled at, read from the stream's own declaration:
/// a portal's grab column declares `spot` and is the visit arm's, everything else is the stream
/// arm's. An undeclared stream falls back to `high`, as a slot with no declaration does.
async fn stream_cadence<C: ConnectionTrait>(txn: &C, stream_id: Uuid) -> AppResult<String> {
    let declared = data_streams::models::Entity::find_by_id(stream_id)
        .select_only()
        .column(data_streams::models::Column::MeasurementType)
        .into_tuple::<Option<String>>()
        .one(txn)
        .await?
        .flatten();
    Ok(if declared.as_deref() == Some("spot") {
        "low".to_string()
    } else {
        "high".to_string()
    })
}

/// The slot an entry pairs into, created when the site has none. The entry's declared decimal
/// places reach an existing slot too.
pub(super) async fn resolve_or_create_site_param<C: ConnectionTrait>(
    txn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    entry: &PlanEntry,
    caches: &mut EntityCaches,
    sp_created: &mut u32,
) -> AppResult<Uuid> {
    let units = entry.parameter.units.as_str();
    let decimal_places = entry.decimal_places;
    let key = (site_id, parameter_id);
    if let Some(&id) = caches.site_params.get(&key) {
        return Ok(id);
    }

    let existing = site_parameters::Entity::find()
        .filter(
            Condition::all()
                .add(site_parameters::Column::SiteId.eq(site_id))
                .add(site_parameters::Column::ParameterId.eq(parameter_id)),
        )
        .one(txn)
        .await?;

    let id = if let Some(existing) = existing {
        crate::routes::private::data_streams::service::declare_slot_decimal_places(
            txn,
            existing.id,
            decimal_places,
        )
        .await?;
        existing.id
    } else {
        let id = Uuid::new_v4();
        let base = caches
            .param_names
            .get(&parameter_id)
            .cloned()
            .unwrap_or_default();
        let code = parameters::Entity::find_by_id(parameter_id)
            .one(txn)
            .await?
            .map_or_else(|| parameter_id.to_string(), |p| p.code);
        let param_name_val = free_slot_name(txn, site_id, &base, units, &code).await?;
        site_parameters::ActiveModel {
            id: Set(id),
            instrument_sensor_id: Set(None),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            name: Set(param_name_val),
            sensor_type: Set(String::new()),
            decimal_places: Set(decimal_places),
            sample_interval_sec: Set(None),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
            needs_review: Set(false),
            entry_mode: Set("manual".to_string()),
            cadence: Set(stream_cadence(txn, entry.stream_id).await?),
            variable_mappings: Set(None),
            created_at: Set(Some(Utc::now())),
            updated_at: Set(Some(Utc::now())),
            discovered_at: Set(Some(Utc::now())),
        }
        .insert(txn)
        .await?;
        *sp_created += 1;
        id
    };
    caches.site_params.insert(key, id);
    Ok(id)
}

/// Create the lab instruments a plan's confirmed entries ask for, one per `source_key` however
/// many streams share it, and return them by that key. Find-or-create, so re-running an apply
/// after a partial failure resolves the same rows.
/// One row of a source's own instrument register, as the plan puts it to the operator.
///
/// The register is the only record of which probe carried which serial and when it was installed,
/// and it goes with the portal, so it travels ahead of the plan and waits (M185). Admitting one is
/// the plan's act, like creating a site: nothing exists until the apply runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanInstrumentProposal {
    /// The source's own identity for it, e.g. `sensor_inventory:62`.
    pub source_key: String,
    pub name: String,
    #[schema(required)]
    pub serial_number: Option<String>,
    #[schema(required)]
    pub manufacturer: Option<String>,
    #[schema(required)]
    pub model: Option<String>,
    #[schema(required)]
    pub notes: Option<String>,
    pub is_lab_instrument: bool,
    /// Whatever the register holds that river-data has no column for: the station it was installed
    /// at, the dates, the state the lab recorded.
    #[schema(required)]
    pub metadata: Option<serde_json::Value>,
    /// Whether the apply creates it. Proposed admitted: the register is the lab's own record, so
    /// the question is which rows to leave behind rather than which to take. A row that collides
    /// with an instrument already in the inventory is proposed unadmitted instead, because taking
    /// it would leave two rows for one probe (Q76).
    pub admit: bool,
    /// The instrument this row would duplicate, by name or by serial. The review answers it the
    /// way the parameter path answers a `name_conflict`: admit as a second instrument, or attach.
    #[schema(required)]
    pub conflict: Option<InstrumentNameConflict>,
    /// The instrument the review chose to merge this register row onto instead of creating one.
    #[schema(required)]
    pub attach_to: Option<Uuid>,
}

/// The register rows waiting for this source, as the plan carries them.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    sea_orm::FromJsonQueryResult,
)]
#[serde(transparent)]
pub struct PlanInstrumentProposals(pub Vec<PlanInstrumentProposal>);

/// The proposals a source has offered and no plan has admitted yet.
pub async fn pending_instrument_proposals<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    catalog: &InstrumentCatalog,
) -> AppResult<Vec<PlanInstrumentProposal>> {
    let rows = proposal::Entity::find()
        .filter(proposal::Column::SourceSystem.eq(source_system))
        .order_by_asc(proposal::Column::SourceKey)
        .all(db)
        .await?;
    let proposals = rows
        .into_iter()
        .map(|row| {
            let conflict = proposal_conflict(&row.name, row.serial_number.as_deref(), catalog);
            PlanInstrumentProposal {
                admit: conflict.is_none(),
                source_key: row.source_key,
                name: row.name,
                serial_number: row.serial_number,
                manufacturer: row.manufacturer,
                model: row.model,
                notes: row.notes,
                is_lab_instrument: row.is_lab_instrument,
                metadata: row.metadata,
                conflict,
                attach_to: None,
            }
        })
        .collect();
    Ok(proposals)
}

/// Create the register rows the review admitted, in the apply's transaction, and clear them from
/// the queue. A row left unadmitted stays a proposal: the next plan offers it again.
pub(super) async fn admit_instrument_proposals<C: ConnectionTrait>(
    txn: &C,
    source_system: &str,
    proposals: &[PlanInstrumentProposal],
) -> AppResult<u32> {
    let mut created = 0u32;
    for offered in proposals
        .iter()
        .filter(|p| p.admit || p.attach_to.is_some())
    {
        // Attached, the register row describes an instrument the inventory already holds: its
        // serial, model and metadata are merged onto that row rather than minting a second one.
        let id = match offered.attach_to {
            Some(existing) => existing,
            None => {
                upsert_source_instrument(
                    txn,
                    source_system,
                    &offered.source_key,
                    &offered.name,
                    if offered.is_lab_instrument {
                        InstrumentKind::Lab
                    } else {
                        InstrumentKind::Device
                    },
                    "high",
                    offered.metadata.clone(),
                )
                .await?
            }
        };
        if offered.attach_to.is_some()
            && let Some(metadata) = &offered.metadata
        {
            txn.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE sensors SET metadata = COALESCE(metadata, '{}'::jsonb) || $2::jsonb \
                  WHERE id = $1",
                [id.into(), metadata.clone().into()],
            ))
            .await?;
        }
        // The serial is claimed only where no other instrument holds it: METALP's register carries
        // one serial on two probes, and losing the instrument over that would be worse than storing
        // it without one.
        if let Some(serial) = offered
            .serial_number
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            txn.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE sensors SET serial_number = $2, manufacturer = $3, model = $4 \
                  WHERE id = $1 \
                    AND NOT EXISTS (SELECT 1 FROM sensors o WHERE o.serial_number = $2 AND o.id <> $1)",
                [
                    id.into(),
                    serial.to_string().into(),
                    offered.manufacturer.clone().into(),
                    offered.model.clone().into(),
                ],
            ))
            .await?;
        }
        proposal::Entity::delete_many()
            .filter(proposal::Column::SourceSystem.eq(source_system))
            .filter(proposal::Column::SourceKey.eq(offered.source_key.clone()))
            .exec(txn)
            .await?;
        if offered.attach_to.is_none() {
            created += 1;
        }
    }
    Ok(created)
}

pub(super) async fn mint_plan_instruments<C: ConnectionTrait>(
    txn: &C,
    source_system: &str,
    entries: &[PlanEntry],
) -> AppResult<HashMap<String, Uuid>> {
    let mut wanted: HashMap<&str, &PlanInstrumentRef> = HashMap::new();
    // A device channel is minted by its own pairing, where the stream's provenance and the slot's
    // deployment are to hand.
    for entry in entries
        .iter()
        .filter(|e| e.action == "pair" && !e.is_device)
    {
        if let Some(i) = &entry.instrument
            && i.create
            && i.id.is_none()
        {
            wanted.entry(i.source_key.as_str()).or_insert(i);
        }
    }

    // Through `upsert_source_instrument` rather than a bare insert: this runs inside `apply_plan`'s
    // transaction, where a unique violation on `sensors_provenance_uniq` from a concurrent apply
    // would poison the whole plan, not just this row.
    let mut minted = HashMap::new();
    for (source_key, want) in wanted {
        let id = upsert_source_instrument(
            txn,
            source_system,
            source_key,
            &want.name,
            // The key is `stream_instrument_key`'s on both sides, so a hand pairing and a plan
            // converge on one row rather than on two that disagree about what it is.
            InstrumentKind::SourceParameter,
            "high",
            None,
        )
        .await?;
        // The key is already taken by the instrument registration minted for the stream, so the
        // upsert resolved to that row rather than creating one. It is the default the review exists
        // to answer, so it takes the name the operator gave and stops being a default; without this
        // the name is silently discarded and the plan reports a creation that did not happen.
        sensors::Entity::update_many()
            .col_expr(sensors::Column::Name, Expr::value(Some(want.name.clone())))
            .col_expr(
                sensors::Column::Metadata,
                Expr::col(sensors::Column::Metadata).sub(sensors::models::MINTED_FROM_STREAM),
            )
            .filter(sensors::Column::Id.eq(id))
            .filter(Expr::expr(
                Func::cust(Alias::new("jsonb_exists"))
                    .arg(Expr::col(sensors::Column::Metadata))
                    .arg(sensors::models::MINTED_FROM_STREAM),
            ))
            .exec(txn)
            .await?;
        minted.insert(source_key.to_string(), id);
    }
    Ok(minted)
}

pub(super) async fn pair_entry_stream<C: ConnectionTrait>(
    txn: &C,
    stream: data_streams::Model,
    plan_id: Uuid,
    site_parameter_id: Uuid,
    parameter_id: Uuid,
    instrument_id: Option<Uuid>,
    device_name: Option<&str>,
) -> AppResult<()> {
    // The plan's instrument wins over the one the stream carries. Since registration mints an
    // instrument for every stream, a review that only filled the gaps would fill none: the entry's
    // instrument is the operator's answer to the question the review asked, so the apply repoints
    // the stream onto it. A lab instrument gets no deployment: it corrects a grab, it is not
    // stationed at the site, and the "attributed but not deployed" state is the one
    // `import_sensor_for_stream` documents.
    let from_plan = instrument_id.filter(|id| stream.sensor_id != Some(*id));
    let needs_sensor = stream.sensor_id.is_none() && from_plan.is_none();
    let device =
        crate::routes::private::sensors::service::extract_vaisala_device_serial(&stream.metadata)
            .is_some();
    // Read once, and only for the entries that will use it: an apply runs this per stream.
    let site_id = if needs_sensor || device {
        site_parameters::Entity::find_by_id(site_parameter_id)
            .one(txn)
            .await?
            .map(|sp| sp.site_id)
            .unwrap_or_default()
    } else {
        Uuid::nil()
    };

    // An entry the review left without an instrument takes the one its own source and parameter
    // resolve, minted here. The apply fails rather than pairing a slot whose readings would name
    // nothing that measured them.
    if needs_sensor {
        create_sensor_for_stream(txn, &stream, parameter_id, site_id, device_name).await?;
    } else if device && let Some(sensor_id) = stream.sensor_id.or(from_plan) {
        // A device is stationed at the site whichever route named it, so the slot's deployment is
        // opened here too. Without this the plan's own instrument choice silently costs the
        // deployment that pairing the same stream by hand would have opened.
        let opens_at =
            crate::routes::private::sensors::service::stream_history_start(txn, stream.id).await?;
        if let Err(e) = crate::routes::private::sensors::service::find_or_create_deployment(
            txn,
            sensor_id,
            site_id,
            parameter_id,
            opens_at,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                stream_id = %stream.id,
                %sensor_id,
                "Failed to open the deployment for a device-shaped stream during pairing",
            );
        }
    }

    let now = Utc::now();
    let mut active: data_streams::ActiveModel = stream.into();
    // Only assign when the plan resolved it. `create_sensor_for_stream` links the stream itself,
    // and this model predates that write, so setting the field unconditionally would clobber it.
    if let Some(id) = from_plan {
        active.sensor_id = Set(Some(id));
    }
    active.site_parameter_id = Set(Some(site_parameter_id));
    active.pairing_plan_id = Set(Some(plan_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(txn).await?;
    Ok(())
}

/// Rows the plan's readings point at through `column`, read before the readings lose it.
/// Clear the attribution a plan's pairing gave `table`'s rows, keyed through the streams the plan
/// paired. The columns are the ones the pairing set, per table.
/// The rows a plan's streams own, as both the write and the span query select them.
fn of_this_plan(plan_id: Uuid) -> Condition {
    use sea_orm::sea_query::ExprTrait as _;
    Condition::all()
        .add(Expr::cust("stream_id = data_streams.id"))
        .add(
            Expr::col((
                data_streams::models::Entity,
                data_streams::models::Column::PairingPlanId,
            ))
            .eq(plan_id),
        )
}

fn unattribute_plan_rows(
    table: impl sea_orm::sea_query::IntoTableRef,
    plan_id: Uuid,
    columns: &[&str],
) -> sea_orm::sea_query::UpdateStatement {
    let mut update = SeaQuery::update();
    update.table(table);
    for column in columns {
        update.value(Alias::new(*column), Expr::value(Option::<Uuid>::None));
    }
    update
        .from(data_streams::models::Entity)
        .cond_where(of_this_plan(plan_id))
        .take()
}

/// The `time` of every row [`unattribute_plan_rows`] is about to clear, for the span.
fn plan_row_times(
    table: impl sea_orm::sea_query::IntoTableRef,
    plan_id: Uuid,
) -> sea_orm::sea_query::SelectStatement {
    SeaQuery::select()
        .column(Alias::new("time"))
        .from(table)
        .from(data_streams::models::Entity)
        .cond_where(of_this_plan(plan_id))
        .take()
}

pub(super) async fn plan_reading_references<C: ConnectionTrait>(
    conn: &C,
    plan_id: Uuid,
    column: readings::models::Column,
) -> AppResult<Vec<Uuid>> {
    use sea_orm::sea_query::ExprTrait as _;
    let r = Alias::new("r");
    let ds = Alias::new("ds");
    let (sql, values) = SeaQuery::select()
        .distinct()
        .expr_as(Expr::col((r.clone(), column)), Alias::new("id"))
        .from_as(readings::models::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::models::Entity,
            ds.clone(),
            Expr::col((r.clone(), readings::models::Column::StreamId))
                .equals((ds.clone(), data_streams::models::Column::Id)),
        )
        .and_where(Expr::col((ds, data_streams::models::Column::PairingPlanId)).eq(plan_id))
        .and_where(Expr::col((r, column)).is_not_null())
        .take()
        .build(PostgresQueryBuilder);
    Ok(conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .iter()
        .map(|row| row.try_get::<Uuid>("", "id"))
        .collect::<Result<_, _>>()?)
}

/// Attribute everything the plan's newly paired streams already hold, through the helper every
/// pairing path runs. Deployment attribution is left to the slot reprocess the caller enqueues:
/// a plan pairs many streams, and each reading's deployment is the one covering its own time.
pub(super) async fn backfill_plan_readings<C: ConnectionTrait>(
    txn: &C,
    plan_id: Uuid,
) -> AppResult<crate::routes::private::data_streams::models::Backfilled> {
    crate::routes::private::data_streams::flows::backfill(txn, HoldScope::Plan(plan_id), None).await
}

pub(super) async fn finalize_plan<C: ConnectionTrait>(
    txn: &C,
    plan_id: Uuid,
    counters: &ApplyCounters,
    readings_backfilled: u64,
) -> AppResult<()> {
    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        streams_skipped: counters.streams_skipped,
        instruments_created: counters.instruments_created,
        curves_assigned: counters.curves_assigned,
        curves_created: counters.curves_created,
        groups_created: counters.groups_created,
        group_members_created: counters.group_members_created,
        readings_backfilled,
    };

    let mut plan_active: pairing_plans::ActiveModel = pairing_plans::Entity::find_by_id(plan_id)
        .one(txn)
        .await?
        .ok_or_else(|| AppError::Internal("Plan disappeared during apply".to_string()))?
        .into();
    plan_active.status = Set("applied".to_string());
    plan_active.applied_at = Set(Some(Utc::now().into()));
    plan_active.apply_result = Set(Some(result.clone()));
    plan_active.update(txn).await?;
    Ok(())
}

/// Revert a pairing plan: bulk unpair all streams that were paired by this plan.
pub async fn revert_plan(db: &sea_orm::DatabaseConnection, plan_id: Uuid) -> AppResult<u32> {
    let plan = pairing_plans::Entity::find_by_id(plan_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "applied" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{}', can only revert 'applied' plans",
            plan.status
        )));
    }

    let txn = db.begin().await?;

    // Atomic status claim: a concurrent revert of the same plan matches zero rows and bails.
    if !claim_plan_status(&txn, plan_id, "applied", "reverting").await? {
        return Err(AppError::BadRequest(
            "Plan is no longer in applied status".to_string(),
        ));
    }

    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

    // NULL out readings for streams from this plan; samples formed by the pairing backfill
    // lose their last reference and are removed below
    // Samples referenced by this plan's readings, so only those can be removed below.
    let sample_ids =
        plan_reading_references(&txn, plan_id, readings::models::Column::SampleId).await?;
    // The visit is attributed state too: `collection_events::attach` only stamps a reading whose
    // collection_event_id is NULL, so a reading left pointing at the reverted site's visit would
    // never be re-attached when the stream is paired somewhere else.
    let event_ids =
        plan_reading_references(&txn, plan_id, readings::models::Column::CollectionEventId).await?;

    let unattributed = bulk_write::mutation(
        &txn,
        bulk_write::Spanned::new(
            plan_row_times(readings::models::Entity, plan_id),
            unattribute_plan_rows(
                readings::models::Entity,
                plan_id,
                &[
                    "site_id",
                    "parameter_id",
                    "sample_id",
                    "collection_event_id",
                ],
            ),
        ),
    )
    .await?;

    // Reverting the pairing takes the reviewer away again; open reviews wait as deferred.
    repoint_holds(&txn, HoldScope::Plan(plan_id), false).await?;

    if !sample_ids.is_empty() {
        // A sample a reading still points at is not this plan's to delete. The subquery is bounded
        // to the same ids, so it stays the anti-join the correlated form was.
        samples::Entity::delete_many()
            .filter(samples::Column::Id.is_in(sample_ids.clone()))
            .filter(
                samples::Column::Id.not_in_subquery(
                    SeaQuery::select()
                        .column(readings::models::Column::SampleId)
                        .from(readings::models::Entity)
                        .and_where(readings::models::Column::SampleId.is_in(sample_ids))
                        .take(),
                ),
            )
            .exec(&txn)
            .await?;
    }

    if !event_ids.is_empty() {
        // A visit that still has readings on it is not this plan's to delete. The subquery is
        // bounded to the same ids, so it stays the anti-join the correlated form was.
        collection_events::models::Entity::delete_many()
            .filter(collection_events::models::Column::Id.is_in(event_ids.clone()))
            .filter(
                collection_events::models::Column::Id.not_in_subquery(
                    sea_orm::sea_query::Query::select()
                        .column(readings::models::Column::CollectionEventId)
                        .from(readings::models::Entity)
                        .and_where(
                            readings::models::Column::CollectionEventId.is_in(event_ids.clone()),
                        )
                        .to_owned(),
                ),
            )
            .exec(&txn)
            .await?;
    }

    let (sql, values) =
        unattribute_plan_rows(status_events::Entity, plan_id, &["site_id", "parameter_id"])
            .build(PostgresQueryBuilder);
    txn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    // Unpair the streams; pairing_plan_id stays as the audit link back to this plan
    let result = data_streams::models::Entity::update_many()
        .col_expr(
            data_streams::models::Column::SiteParameterId,
            Expr::value(Option::<Uuid>::None),
        )
        .col_expr(
            data_streams::models::Column::PairedAt,
            Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
        )
        .filter(data_streams::models::Column::PairingPlanId.eq(plan_id))
        .exec(&txn)
        .await?;
    let reverted = result.rows_affected as u32;

    // Update plan status
    let mut plan_active: pairing_plans::ActiveModel = pairing_plans::Entity::find_by_id(plan_id)
        .one(&txn)
        .await?
        .ok_or_else(|| AppError::Internal("Plan disappeared during revert".to_string()))?
        .into();
    plan_active.status = Set("reverted".to_string());
    plan_active.update(&txn).await?;

    txn.commit().await?;

    // The readings that left the rollups did so over the span the unattribution touched, and the
    // policy would carry it on its next tick; refreshing it here is so the caller sees consistent
    // state now.
    if let Some(window) = crate::common::aggregates::Window::touched(&unattributed) {
        crate::common::aggregates::refresh(db, window).await?;
    }

    tracing::info!(plan_id = %plan_id, reverted, "Pairing plan reverted");
    Ok(reverted)
}

/// How much attention one entry still wants, the three states the review renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// Project, site and parameter all resolve and nothing warned, so the proposal stands on its
    /// own evidence. Worth looking over, not waiting on anyone.
    SelfValidated,
    /// Something did not resolve, or the entry carries a warning. A person decides this one.
    NeedsChecking,
    /// A person looked and agreed.
    Acknowledged,
}

/// The state of one entry, most decided first.
#[must_use]
pub fn review_state(entry: &PlanEntry) -> ReviewState {
    if entry.acknowledged {
        return ReviewState::Acknowledged;
    }
    if entry.confidence == "exact" && entry.warnings.is_empty() {
        return ReviewState::SelfValidated;
    }
    ReviewState::NeedsChecking
}

pub fn compute_summary_pub(entries: &[PlanEntry]) -> PlanSummary {
    compute_summary(entries)
}

pub(super) fn compute_summary(entries: &[PlanEntry]) -> PlanSummary {
    let will_pair = entries.iter().filter(|e| e.action == "pair").count();
    let will_skip = entries.iter().filter(|e| e.action == "skip").count();

    let unique_projects: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.project.name.as_str())
        .collect();
    let unique_sites: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.site.name.as_str())
        .collect();
    let unique_params: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.parameter.name.as_str())
        .collect();

    let projects_to_create = entries
        .iter()
        .filter(|e| e.action == "pair" && e.project.create)
        .map(|e| &e.project.name)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let sites_to_create = entries
        .iter()
        .filter(|e| e.action == "pair" && e.site.create)
        .map(|e| &e.site.name)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let params_to_create = entries
        .iter()
        .filter(|e| e.action == "pair" && e.parameter.create)
        .map(|e| &e.parameter.name)
        .collect::<std::collections::HashSet<_>>()
        .len();

    // A group is one decision behind every column of its category, so it is counted by code.
    let groups_to_create = entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter_map(|e| e.parameter.group.as_ref())
        .filter(|g| g.create)
        .map(|g| &g.code)
        .collect::<std::collections::HashSet<_>>()
        .len();

    // Instruments are counted by identity, not by entry: one curve column serves every station in
    // the source, so 31 DOC streams create at most one instrument.
    let instruments_to_create = entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create)
        .map(|i| &i.source_key)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let instruments_unconfirmed = entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create && !i.confirmed)
        .map(|i| &i.source_key)
        .collect::<std::collections::HashSet<_>>()
        .len();

    let pairing = entries.iter().filter(|e| e.action == "pair");
    let mut needs_checking = 0usize;
    let mut self_validated = 0usize;
    let mut acknowledged = 0usize;
    for entry in pairing {
        match review_state(entry) {
            ReviewState::NeedsChecking => needs_checking += 1,
            ReviewState::SelfValidated => self_validated += 1,
            ReviewState::Acknowledged => acknowledged += 1,
        }
    }

    PlanSummary {
        total_streams: entries.len(),
        will_pair,
        needs_checking,
        self_validated,
        acknowledged,
        will_skip,
        projects_to_create,
        sites_to_create,
        parameters_to_create: params_to_create,
        groups_to_create,
        instruments_to_create,
        instruments_unconfirmed,
        unique_projects: unique_projects.len(),
        unique_sites: unique_sites.len(),
        unique_parameters: unique_params.len(),
    }
}

/// A name reduced to what a reader would call it the same by: letters and digits only, lowercase,
/// with the leading zeros of each digit run dropped. `FP-1`, `fp 1` and `FP01` all read as `fp1`.
pub(super) fn canonical_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut digits = String::new();
    let flush = |digits: &mut String, out: &mut String| {
        if digits.is_empty() {
            return;
        }
        let trimmed = digits.trim_start_matches('0');
        out.push_str(if trimmed.is_empty() { "0" } else { trimmed });
        digits.clear();
    };
    for c in name.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut digits, &mut out);
            if c.is_alphanumeric() {
                out.extend(c.to_lowercase());
            }
        }
    }
    flush(&mut digits, &mut out);
    out
}

/// An existing name a proposed creation reads as, without being the exact match the catalog needs.
/// Nothing here guesses at typos: `FP1` and `FP2` are two stations, and an edit distance would
/// call them one.
pub(super) fn near_duplicate_of<'a, I>(proposed: &str, existing: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let canonical = canonical_name(proposed);
    if canonical.is_empty() {
        return None;
    }
    let lower = proposed.to_lowercase();
    existing
        .into_iter()
        .find(|name| name.to_lowercase() != lower && canonical_name(name) == canonical)
}

/// The source name an entry still carries, or `None` once a reviewer has renamed the entry.
fn unrenamed_source_name<'a>(source_name: Option<&'a str>, name: &str) -> Option<&'a str> {
    source_name.filter(|source| source.eq_ignore_ascii_case(name.trim()))
}

/// A source's name for a site or project, as the links key it.
fn source_name(name: &str) -> Option<String> {
    Some(name.trim().to_string()).filter(|n| !n.is_empty())
}

/// The row an entry's source name is linked to, with that row's current name. An entry a reviewer
/// has renamed in the plan names its target itself, so the link is only read while the entry still
/// carries the source's name.
pub(super) fn linked_entity(
    source_name: Option<&str>,
    name: &str,
    links: &HashMap<String, Uuid>,
    existing: &[(Uuid, String)],
) -> Option<(Uuid, String)> {
    let id = links.get(unrenamed_source_name(source_name, name)?)?;
    existing
        .iter()
        .find(|(existing_id, _)| existing_id == id)
        .cloned()
}

pub(super) fn match_entity(name: &str, existing: &[(Uuid, String)]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    let lower = name.to_lowercase();
    if let Some((id, _)) = existing.iter().find(|(_, n)| n.to_lowercase() == lower) {
        (Some(*id), false)
    } else {
        (None, true)
    }
}

/// A stream names its column by code, display name or alias, so all three resolve. The order is
/// canonical: `resolve_or_create_param` runs it as SQL at apply time and this builds the same
/// precedence into the review's lookup map, so a review shows what apply will produce.
pub fn lookup_parameter_by_code_name_or_alias(
    name: &str,
    existing: &[CatalogParam],
) -> Option<Uuid> {
    if name.is_empty() {
        return None;
    }
    let lower = name.to_lowercase();
    existing
        .iter()
        .find(|p| p.code.to_lowercase() == lower)
        .or_else(|| existing.iter().find(|p| p.name.to_lowercase() == lower))
        .or_else(|| {
            existing
                .iter()
                .find(|p| p.aliases.iter().any(|a| a.to_lowercase() == lower))
        })
        .map(|p| p.id)
}

/// The parameter a replicate family should suggest: the measurand, not the incoming statistic
/// column. Strips the `avg` marker (`DOC_avg_ppb` -> `DOC_ppb`), and when dropping a trailing
/// token on top of that finds an existing catalog parameter (`DOC_ppb` -> `DOC`), prefers it, so
/// a synced family and a tool save land on one slot instead of minting a sibling.
/// The catalog code a replicate family's mean column suggests.
///
/// The incoming column header is the code, because that is how the data is already stored, so
/// nothing is stripped from it except `avg`: an `_avg` column is by construction the mean of a
/// replicate family, which makes that segment structural rather than a suffix, and the family is
/// what is being paired. `DOC_avg_ppb` is `DOC_ppb`, units and all; a units-bearing column never
/// resolves onto a shorter code, so a catalog that happens to hold `DOC` does not pull `DOC_ppb`
/// onto it and give two portals different export headers for the same measurand.
pub(super) fn family_parameter_suggestion(name: &str) -> String {
    let stripped: String = name
        .split('_')
        .filter(|seg| !seg.eq_ignore_ascii_case("avg"))
        .collect::<Vec<_>>()
        .join("_");
    if stripped.is_empty() {
        return name.to_string();
    }
    stripped
}

/// The catalog parameter an entry resolves to, and whether the apply creates one. The review's
/// own choice answers where it has been made; where it has not, the source's own name is matched
/// against the catalog as a proposal. An attachment to a parameter that has since left the catalog
/// falls back to creating one, which the code-conflict warning then reports if the code is taken.
pub(super) fn resolve_plan_parameter(
    param: &PlanParamRef,
    catalog: &EntityCatalog,
) -> (Option<Uuid>, bool) {
    match &param.attach {
        Some(PlanParamAttach::Existing { id }) if catalog.params.iter().any(|p| p.id == *id) => {
            (Some(*id), false)
        }
        Some(_) => (None, !param.name.trim().is_empty()),
        None => match_entity_display(&param.name, &catalog.params),
    }
}

pub(super) fn match_entity_display(name: &str, existing: &[CatalogParam]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    match lookup_parameter_by_code_name_or_alias(name, existing) {
        Some(id) => (Some(id), false),
        None => (None, true),
    }
}

pub struct CatalogParam {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub units: String,
    pub category: String,
    /// What already depends on this parameter. A catalog entry nothing uses is a different
    /// proposition from one carrying years of readings, and a units conflict cannot be judged
    /// without knowing which it is.
    pub site_parameter_count: i64,
    pub reading_count: i64,
}

#[derive(Default)]
pub struct EntityCatalog {
    pub projects: Vec<(Uuid, String)>,
    pub sites: Vec<(Uuid, String)>,
    pub params: Vec<CatalogParam>,
    /// Parameter groups that already exist, by code, so a proposal resolves onto one rather than
    /// proposing a second group under a name the database already carries.
    pub groups: Vec<(Uuid, String)>,
    /// The plan's source names for projects and sites, and the rows they are linked to.
    pub project_links: HashMap<String, Uuid>,
    pub site_links: HashMap<String, Uuid>,
}

pub async fn load_entity_catalog(
    db: &impl ConnectionTrait,
    source_system: &str,
) -> AppResult<EntityCatalog> {
    let projects = projects::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let sites = sites::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    let groups: Vec<(Uuid, String)> = parameter_groups::Entity::find()
        .select_only()
        .column(parameter_groups::Column::Id)
        .column(parameter_groups::Column::Code)
        .into_tuple()
        .all(db)
        .await?;
    // Usage per parameter in one pass. `readings.parameter_id` is indexed and the group-by is over
    // the slots, not the hypertable's rows, so this stays a catalog-sized query.
    let mut usage: HashMap<Uuid, (i64, i64)> = HashMap::new();
    let sp = Alias::new("sp");
    let r = Alias::new("r");
    let per_slot = SeaQuery::select()
        .column(readings::models::Column::SiteId)
        .column(readings::models::Column::ParameterId)
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .from(readings::models::Entity)
        .and_where(Expr::col(readings::models::Column::ParameterId).is_not_null())
        .add_group_by([
            Expr::col(readings::models::Column::SiteId),
            Expr::col(readings::models::Column::ParameterId),
        ])
        .take();
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::ParameterId)),
            Alias::new("parameter_id"),
        )
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("slots"))
        .expr_as(
            Expr::cust("COALESCE(SUM(r.n), 0)::bigint"),
            Alias::new("readings"),
        )
        .from_as(site_parameters::Entity, sp.clone())
        .join_subquery(
            JoinType::LeftJoin,
            per_slot,
            r.clone(),
            Condition::all()
                .add(
                    Expr::col((r.clone(), site_parameters::Column::ParameterId))
                        .equals((sp.clone(), site_parameters::Column::ParameterId)),
                )
                .add(
                    Expr::col((r, site_parameters::Column::SiteId))
                        .equals((sp.clone(), site_parameters::Column::SiteId)),
                ),
        )
        .add_group_by([Expr::col((sp, site_parameters::Column::ParameterId))])
        .take()
        .build(PostgresQueryBuilder);
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
    {
        let row = UsageRow::from_query_result(&row, "")?;
        usage.insert(row.parameter_id, (row.slots, row.readings));
    }

    let params = parameters::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| {
            let (slots, readings) = usage.get(&p.id).copied().unwrap_or((0, 0));
            CatalogParam {
                id: p.id,
                code: p.code,
                name: p.name,
                aliases: p.aliases,
                units: p.default_units,
                category: p.category,
                site_parameter_count: slots,
                reading_count: readings,
            }
        })
        .collect();
    Ok(EntityCatalog {
        projects,
        sites,
        params,
        groups,
        project_links: project_source_links::for_source(db, source_system).await?,
        site_links: site_source_links::for_source(db, source_system).await?,
    })
}

/// Recompute an entry's entity resolution against the current catalog: project/site/parameter
/// id + create flags, unit-mismatch warnings, and overall confidence. Warnings are rebuilt from
/// scratch so ones that no longer apply are cleared. Does not touch action or grouping fields.
pub fn reclassify_entry(entry: &mut PlanEntry, catalog: &EntityCatalog) {
    if let Some((_, name)) = linked_entity(
        entry.project.source_name.as_deref(),
        &entry.project.name,
        &catalog.project_links,
        &catalog.projects,
    ) {
        entry.project.name = name;
    }
    let (proj_id, proj_create) = match_entity(&entry.project.name, &catalog.projects);
    entry.project.id = proj_id;
    entry.project.create = proj_create;

    if let Some((_, name)) = linked_entity(
        entry.site.source_name.as_deref(),
        &entry.site.name,
        &catalog.site_links,
        &catalog.sites,
    ) {
        entry.site.name = name;
    }
    let (site_id, site_create) = match_entity(&entry.site.name, &catalog.sites);
    entry.site.id = site_id;
    entry.site.create = site_create;

    let (param_id, param_create) = resolve_plan_parameter(&entry.parameter, catalog);
    entry.parameter.id = param_id;
    entry.parameter.create = param_create;

    if let Some(group) = entry.parameter.group.as_mut() {
        let existing = catalog
            .groups
            .iter()
            .find(|(_, code)| code.eq_ignore_ascii_case(&group.code));
        group.id = existing.map(|(id, _)| *id);
        group.create = existing.is_none();
    }

    entry.warnings.clear();
    if site_create
        && let Some(existing) = near_duplicate_of(
            &entry.site.name,
            catalog.sites.iter().map(|(_, n)| n.as_str()),
        )
    {
        entry.warnings.push(PlanWarning::near_duplicate(
            "site",
            &entry.site.name,
            existing,
        ));
    }
    if param_create
        && let Some(existing) = near_duplicate_of(
            &entry.parameter.name,
            catalog.params.iter().flat_map(|p| {
                std::iter::once(p.code.as_str())
                    .chain(std::iter::once(p.name.as_str()))
                    .chain(p.aliases.iter().map(String::as_str))
            }),
        )
    {
        entry.warnings.push(PlanWarning::near_duplicate(
            "parameter",
            &entry.parameter.name,
            existing,
        ));
    }
    if param_create
        && let Some(id) =
            lookup_parameter_by_code_name_or_alias(entry.parameter.name.trim(), &catalog.params)
        && let Some(existing) = catalog.params.iter().find(|p| p.id == id)
    {
        entry
            .warnings
            .push(PlanWarning::catalog_match(&entry.parameter.name, existing));
    }
    if let Some(pid) = param_id
        && let Some(p) = catalog.params.iter().find(|p| p.id == pid)
        && !p.units.is_empty()
        && !entry.parameter.units.is_empty()
        && p.units.to_lowercase() != entry.parameter.units.to_lowercase()
    {
        entry.warnings.push(PlanWarning::units_mismatch(
            &entry.parameter.name,
            p,
            &entry.parameter.units,
        ));
    }
    entry.confidence = if proj_id.is_some() && site_id.is_some() && param_id.is_some() {
        "exact"
    } else {
        "none"
    }
    .to_string();
}

pub(super) async fn resolve_or_create_project<C: ConnectionTrait>(
    txn: &C,
    entity_ref: &PlanEntityRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
    source_system: &str,
) -> AppResult<Uuid> {
    if let Some(id) = entity_ref.id {
        return Ok(id);
    }
    let key = entity_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    if let Some(source_name) =
        unrenamed_source_name(entity_ref.source_name.as_deref(), &entity_ref.name)
        && let Some(id) = project_source_links::find(txn, source_system, source_name).await?
    {
        cache.insert(key, id);
        return Ok(id);
    }
    let existing = projects::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    projects::ActiveModel {
        id: Set(id),
        name: Set(entity_ref.name.clone()),
        description: Set(None),
        data_source: Set(Some(source_system.to_string())),
        is_public: Set(false),
        public_code: Set(None),
        public_api_title: Set(None),
        public_api_description: Set(None),
        public_api_version: Set(None),
        public_contact_email: Set(None),
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    }
    .insert(txn)
    .await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

pub(super) async fn resolve_or_create_site(
    txn: &impl ConnectionTrait,
    site_ref: &PlanSiteRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
    project_id: Uuid,
    source_system: &str,
) -> AppResult<Uuid> {
    if let Some(id) = site_ref.id {
        // The site was matched at plan-creation time. Still backfill coordinates from the stream
        // metadata if the site lacks them, otherwise a site discovered before its coordinates were
        // known never picks them up (the common case, since match_entity sets the id).
        if site_ref.latitude.is_some()
            && let Some(existing) = sites::Entity::find_by_id(id).one(txn).await?
            && existing.latitude.is_none()
        {
            let mut update: sites::ActiveModel = existing.into();
            update.latitude = Set(site_ref.latitude);
            update.longitude = Set(site_ref.longitude);
            update.altitude_m = Set(site_ref.altitude_m);
            update.update(txn).await?;
        }
        return Ok(id);
    }
    let key = site_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    if let Some(source_name) =
        unrenamed_source_name(site_ref.source_name.as_deref(), &site_ref.name)
        && let Some(id) = site_source_links::find(txn, source_system, source_name).await?
    {
        cache.insert(key, id);
        return Ok(id);
    }
    let existing = sites::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        if existing.latitude.is_none() && site_ref.latitude.is_some() {
            let mut update: sites::ActiveModel = existing.clone().into();
            update.latitude = Set(site_ref.latitude);
            update.longitude = Set(site_ref.longitude);
            update.altitude_m = Set(site_ref.altitude_m);
            update.update(txn).await?;
        }
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    sites::ActiveModel {
        id: Set(id),
        project_id: Set(Some(project_id)),
        subproject_id: sea_orm::ActiveValue::NotSet,
        name: Set(site_ref.name.clone()),
        latitude: Set(site_ref.latitude),
        longitude: Set(site_ref.longitude),
        altitude_m: Set(site_ref.altitude_m),
        public_code: Set(None),
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    }
    .insert(txn)
    .await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

pub(super) async fn resolve_or_create_param(
    txn: &impl ConnectionTrait,
    param_ref: &PlanParamRef,
    original_parameter_name: Option<&str>,
    cache: &mut HashMap<String, Uuid>,
    param_names: &mut HashMap<Uuid, String>,
    created_count: &mut u32,
    accepted_objects: &HashSet<String>,
) -> AppResult<Uuid> {
    if let Some(id) = param_ref.id {
        return Ok(id);
    }
    let key = param_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    // Resolution order mirrors `match_entity_display`: code, then name, then alias,
    // all case-insensitive.
    let existing = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(code) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        param_names.entry(existing.id).or_insert(existing.name);
        return Ok(existing.id);
    }
    let name_match = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(matched) = name_match {
        cache.insert(key, matched.id);
        param_names.entry(matched.id).or_insert(matched.name);
        return Ok(matched.id);
    }
    let alias_match = parameters::Entity::find()
        .filter(Expr::cust_with_values(
            "EXISTS (SELECT 1 FROM unnest(aliases) a WHERE LOWER(a) = $1)",
            [key.clone()],
        ))
        .one(txn)
        .await?;
    if let Some(matched) = alias_match {
        cache.insert(key, matched.id);
        param_names.entry(matched.id).or_insert(matched.name);
        return Ok(matched.id);
    }
    // No match: create. The column name is the code (the stable machine id a scientist can match
    // against the portal's own tables), the label is the human name, and both plus the source
    // names seed the aliases so future plans resolve any of them.
    let mut aliases: Vec<String> = param_ref
        .original_names
        .iter()
        .cloned()
        .chain(original_parameter_name.map(str::to_string))
        .chain(param_ref.label.clone())
        .filter(|a| !a.trim().is_empty() && a.to_lowercase() != key)
        .collect();
    aliases.sort();
    aliases.dedup_by(|a, b| a.to_lowercase() == b.to_lowercase());
    let category = infer_category(&param_ref.name);
    let id = Uuid::new_v4();
    parameters::ActiveModel {
        id: Set(id),
        code: Set(param_ref.name.clone()),
        name: Set(param_ref
            .label
            .clone()
            .unwrap_or_else(|| param_ref.name.clone())),
        default_units: Set(param_ref.units.clone()),
        category: Set(category),
        needs_review: Set(minted_param_needs_review(accepted_objects, &param_ref.name)),
        description: Set(None),
        aliases: Set(aliases),
        created_at: Set(Some(Utc::now())),
    }
    .insert(txn)
    .await?;
    *created_count += 1;
    cache.insert(key, id);
    param_names.insert(id, param_ref.name.clone());
    Ok(id)
}

/// Whether a parameter this apply mints is still waiting on a manager.
///
/// The review accepts each object the plan creates before the apply is allowed to run, and that
/// acceptance is the review the flag asks for. A mint no acceptance covers is mechanical, and
/// stays flagged.
pub(super) fn minted_param_needs_review(accepted_objects: &HashSet<String>, name: &str) -> bool {
    !accepted_objects.contains(&format!("parameter:{name}"))
}

pub(super) fn infer_category(_name: &str) -> String {
    "measurement".to_string()
}

/// Which entries a plan-wide bulk action covers. Every field is a further narrowing, so an empty
/// `BulkWhere` selects the whole plan; a plan-wide action is then one predicate on the wire rather
/// than one update per entry (1891 for CNET, 29,400 for NOMIS).
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkWhere {
    /// `exact` when the project, site and parameter all resolved, `none` otherwise.
    #[serde(default)]
    pub confidence: Option<String>,
    #[serde(default)]
    pub has_warnings: Option<bool>,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub parameter_name: Option<String>,
    /// `pair` or `skip`, what the entry is set to now.
    #[serde(default)]
    pub action: Option<String>,
}

/// The positions in `entries` the predicate picks.
pub fn select_entries(entries: &[PlanEntry], filter: &BulkWhere) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            filter
                .confidence
                .as_deref()
                .is_none_or(|c| e.confidence.eq_ignore_ascii_case(c))
                && filter
                    .has_warnings
                    .is_none_or(|w| e.warnings.is_empty() != w)
                && filter
                    .site_name
                    .as_deref()
                    .is_none_or(|n| e.site.name.eq_ignore_ascii_case(n))
                && filter
                    .parameter_name
                    .as_deref()
                    .is_none_or(|n| e.parameter.name.eq_ignore_ascii_case(n))
                && filter.action.as_deref().is_none_or(|a| e.action == a)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Apply a bulk action to the selected entries, returning how many moved. An entry with no site
/// or no parameter name is never set to `pair`: there is no slot to pair it to, which is the rule
/// the per-entry updates already enforce.
pub fn apply_bulk_action(
    entries: &mut [PlanEntry],
    filter: &BulkWhere,
    action: Option<&str>,
    acknowledged: Option<bool>,
) -> usize {
    let selected = select_entries(entries, filter);
    let mut changed = 0;
    for i in selected {
        let entry = &mut entries[i];
        let mut moved = false;
        if let Some(action) = action {
            let target = if action == "pair"
                && (entry.site.name.trim().is_empty() || entry.parameter.name.trim().is_empty())
            {
                "skip"
            } else {
                action
            };
            if entry.action != target {
                entry.action = target.to_string();
                moved = true;
            }
        }
        if let Some(acknowledged) = acknowledged
            && entry.acknowledged != acknowledged
        {
            entry.acknowledged = acknowledged;
            moved = true;
        }
        changed += usize::from(moved);
    }
    changed
}

/// The four aggregate queries this module makes that fill a shape of their own.
#[derive(FromQueryResult)]
pub(super) struct HoldCountRow {
    pub(super) stream_id: Uuid,
    pub(super) holds: i64,
}

/// A catalog parameter's usage. `SUM` over a bigint is NUMERIC in Postgres, so the sum is cast in
/// the query: decoded as an integer it fails, which the hand mapping's `unwrap_or(0)` was
/// swallowing, and every parameter reported no readings.
#[derive(FromQueryResult)]
pub(super) struct UsageRow {
    pub(super) parameter_id: Uuid,
    pub(super) slots: i64,
    pub(super) readings: i64,
}

/// How much of one source system is paired, the dashboard's "needs attention" count.
#[derive(Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct UnpairedSummaryRow {
    pub source_system: String,
    pub unpaired: i64,
    pub paired: i64,
}

/// One site's metadata as the plan's streams carry it: every field is text in `metadata`, so the
/// row reads them as text and the parses below turn them into what the response holds.
#[derive(sea_orm::FromQueryResult)]
pub(super) struct PlanSiteMetadataRow {
    pub(super) site_name: Option<String>,
    pub(super) latitude: Option<String>,
    pub(super) longitude: Option<String>,
    pub(super) altitude_m: Option<String>,
    pub(super) glacier_name: Option<String>,
    pub(super) glacier_rgi: Option<String>,
    pub(super) location_type: Option<String>,
    pub(super) catchment: Option<String>,
    pub(super) full_name: Option<String>,
    pub(super) elevation: Option<String>,
    pub(super) channel_id: Option<String>,
    pub(super) sample_interval_sec: Option<String>,
}

#[derive(sea_orm::FromQueryResult)]
pub(super) struct PlanSiteDeviceRow {
    pub(super) site_name: Option<String>,
    pub(super) serial: Option<String>,
    pub(super) model: Option<String>,
    pub(super) streams: i64,
}

/// Streams a draft does not cover: unpaired now, plannable (`create_plan` skips a legacy single
/// superseded by its `:reps` family), and named by no entry. The plan's own entries decide it, so
/// the count is exact where comparing entry totals with a stream total is not: a replicate family
/// is one entry over several source columns.
pub(super) async fn uncovered_stream_count(
    db: &sea_orm::DatabaseConnection,
    plan_id: Uuid,
    source_system: &str,
) -> AppResult<Option<i64>> {
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT count(*) AS n
              FROM data_streams ds
             WHERE ds.source_system = $2
               AND ds.site_parameter_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM data_streams fam
                                WHERE fam.source_system = ds.source_system
                                  AND fam.source_key = ds.source_key || ':reps')
               AND ds.id NOT IN (SELECT (e ->> 'stream_id')::uuid
                                   FROM pairing_plans p, jsonb_array_elements(p.entries) e
                                  WHERE p.id = $1)",
            [plan_id.into(), source_system.into()],
        ))
        .await?;
    Ok(row.map(|r| r.try_get::<i64>("", "n")).transpose()?)
}

/// Fold the review's curve assignments into the plan's list. An assignment must name a curve that
/// exists and that no reading names yet (the curve's own update route refuses a used curve the
/// same way), and an instrument some paired entry proposes creating; anything else is a 400 now
/// rather than a failed apply later.
pub(super) async fn apply_curve_updates(
    db: &sea_orm::DatabaseConnection,
    entries: &[crate::routes::private::sync::service::PlanEntry],
    intents: &mut Vec<crate::routes::private::sync::service::PlanCurveIntent>,
    updates: &[PlanCurveUpdate],
) -> AppResult<()> {
    for update in updates {
        intents.retain(|i| i.curve_id != update.curve_id);
        let Some(source_key) = update
            .instrument_source_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            continue;
        };
        let proposed = entries.iter().any(|e| {
            e.action == "pair"
                && e.instrument
                    .as_ref()
                    .is_some_and(|i| i.create && i.source_key == source_key)
        });
        if !proposed {
            return Err(AppError::BadRequest(format!(
                "this plan does not create an instrument with source key '{source_key}'; a \
                 curve can only be assigned here to an instrument the plan will create, an \
                 existing instrument takes it through the curve itself"
            )));
        }
        if crate::routes::private::standard_curves::Entity::find_by_id(update.curve_id)
            .one(db)
            .await?
            .is_none()
        {
            return Err(AppError::BadRequest(format!(
                "standard curve {} does not exist",
                update.curve_id
            )));
        }
        if crate::routes::private::standard_curves::views::curve_is_used(db, update.curve_id)
            .await?
        {
            return Err(AppError::BadRequest(format!(
                "standard curve {} has already been applied to readings, so its instrument is \
                 fixed. Create a new curve on the new instrument and re-enter the affected \
                 measurements against it.",
                update.curve_id
            )));
        }
        intents.push(crate::routes::private::sync::service::PlanCurveIntent {
            curve_id: update.curve_id,
            instrument_source_key: source_key.to_string(),
        });
    }
    Ok(())
}

/// What an instrument decision covers where the entry names no instrument yet. A curve column is
/// one instrument across the whole source, so settling it on any one entry settles every entry
/// sharing the column; where no column names a curve, the source parameter plays that role, so
/// choosing the fluorometer for `chla_acid` covers all 31 stations rather than one.
pub(super) fn instrument_scope(entry: &crate::routes::private::sync::service::PlanEntry) -> String {
    match entry
        .instrument
        .as_ref()
        .and_then(|i| i.curve_column.as_deref())
    {
        Some(column) => format!("column:{column}"),
        None => format!(
            "parameter:{}",
            entry
                .parameter
                .group_key
                .as_deref()
                .unwrap_or(&entry.parameter.name)
        ),
    }
}

/// Whether an instrument row was minted by stream registration rather than named by the source or
/// an operator.
pub(super) fn is_minted_default(sensor: &sensors::Model) -> bool {
    sensor
        .metadata
        .as_ref()
        .and_then(|m| m.get(sensors::models::MINTED_FROM_STREAM))
        .is_some()
}

/// The identity an instrument decision belongs to. An entry that already names an instrument
/// belongs to that instrument, however many source columns share it: the portal's `chla acid`
/// curve corrects both `Chla_acid_ugL` and `Chla_acid_ugm2` from one lab instrument, and keying
/// those by parameter would report one instrument as two rows and move only half of it when the
/// operator repointed it. An entry with no instrument has only its scope to be keyed by.
pub(super) fn instrument_key(entry: &crate::routes::private::sync::service::PlanEntry) -> String {
    match entry.instrument.as_ref() {
        Some(instrument) if !instrument.source_key.is_empty() => {
            format!("instrument:{}", instrument.source_key)
        }
        _ => instrument_scope(entry),
    }
}

/// Join the instrument an entry was just named to another this plan creates under that name, so
/// two parameters named after one instrument are one row in the review and one instrument on
/// apply. The joined row takes the other instrument's identity and review. A parameter still
/// waiting on its own suggested instrument of that name joins too, unreviewed.
pub(super) fn join_named_proposal(entries: &mut [PlanEntry], stream_id: Uuid, source_system: &str) {
    let Some(named) = entries.iter().find(|e| e.stream_id == stream_id) else {
        return;
    };
    let Some(proposal) = named
        .instrument
        .as_ref()
        .filter(|i| !named.is_device && i.create && i.id.is_none())
    else {
        return;
    };
    let own_key = instrument_key(named);
    let name = proposal.name.trim().to_lowercase();
    let template = proposal.clone();

    let other = entries
        .iter()
        .filter(|e| !e.is_device && instrument_key(e) != own_key)
        .filter_map(|e| e.instrument.as_ref())
        .find(|i| i.create && i.id.is_none() && i.name.trim().to_lowercase() == name)
        .map(|i| (i.source_key.clone(), i.confirmed));
    let (source_key, confirmed) = match other {
        Some(joined) => joined,
        None => {
            let waiting: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    !e.is_device
                        && e.instrument.is_none()
                        && e.parameter.name.trim().to_lowercase() == name
                })
                .map(|(i, _)| i)
                .collect();
            let Some(&first) = waiting.first() else {
                return;
            };
            // The key that parameter's own suggestion would have been created under.
            let key = format!("{source_system}:{}", entries[first].parameter.name);
            for i in waiting {
                entries[i].instrument = Some(PlanInstrumentRef {
                    curve_column: None,
                    source_key: key.clone(),
                    confirmed: false,
                    curves: Vec::new(),
                    name_conflict: None,
                    ..template.clone()
                });
            }
            (key, false)
        }
    };
    for entry in entries.iter_mut().filter(|e| instrument_key(e) == own_key) {
        if let Some(instrument) = entry.instrument.as_mut() {
            instrument.source_key.clone_from(&source_key);
            instrument.confirmed = confirmed;
        }
    }
}

/// Rename the parameter group a plan proposes, across every entry the same category placed.
///
/// A category is one decision behind every column it holds, so renaming it on one entry renames it
/// on all of them; leaving the others at the source's label would make the created group's name
/// depend on which entry the apply read first. The code is re-slugged from the new label, which is
/// what lets a rename onto an existing group's label join that group: [`reclassify_entry`] then
/// resolves the new code against the catalog.
pub(super) fn apply_group_updates(
    entries: &mut [PlanEntry],
    updates: &[PlanEntryUpdate],
    catalog: &EntityCatalog,
) {
    for update in updates {
        if update.group_label.is_none() && update.group_description.is_none() {
            continue;
        }
        let Some(code) = entries
            .iter()
            .find(|e| e.stream_id == update.stream_id)
            .and_then(|e| e.parameter.group.as_ref())
            .map(|g| g.code.clone())
        else {
            continue;
        };
        let renamed: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.parameter.group.as_ref().is_some_and(|g| g.code == code))
            .map(|(i, _)| i)
            .collect();
        for index in renamed {
            let Some(group) = entries[index].parameter.group.as_mut() else {
                continue;
            };
            if let Some(label) = &update.group_label {
                let label = label.trim();
                if !label.is_empty() {
                    group.label = label.to_string();
                    group.code = group_code(label);
                }
            }
            if let Some(description) = &update.group_description {
                let description = description.trim();
                group.description = (!description.is_empty()).then(|| description.to_string());
            }
            reclassify_entry(&mut entries[index], catalog);
        }
    }
}

/// Apply a site's coordinate edits.
///
/// Kept apart from the per-entry loop for the reason the instrument half is: where a site is
/// concerned, every entry naming it is one row to the operator. Editing the elevation on one of a
/// station's twenty-three feeds and leaving the other twenty-two at the source's value would make
/// the created site's attributes depend on which entry the apply read first.
///
/// A site the plan resolved to an existing row is left alone: its attributes are its own page's,
/// and the apply only ever backfills a coordinate such a site is missing.
pub(super) fn apply_site_attribute_updates(
    entries: &mut [crate::routes::private::sync::service::PlanEntry],
    updates: &[PlanEntryUpdate],
) {
    for update in updates {
        if update.site_latitude.is_none()
            && update.site_longitude.is_none()
            && update.site_altitude_m.is_none()
        {
            continue;
        }
        let Some(target) = entries.iter().find(|e| e.stream_id == update.stream_id) else {
            continue;
        };
        if target.site.id.is_some() {
            continue;
        }
        let name = target.site.name.to_lowercase();
        for entry in entries
            .iter_mut()
            .filter(|e| e.site.id.is_none() && e.site.name.to_lowercase() == name)
        {
            if let Some(lat) = update.site_latitude {
                entry.site.latitude = Some(lat);
            }
            if let Some(lon) = update.site_longitude {
                entry.site.longitude = Some(lon);
            }
            if let Some(alt) = update.site_altitude_m {
                entry.site.altitude_m = Some(alt);
            }
        }
    }
}

/// Apply the instrument half of a plan edit.
///
/// Kept apart from the per-entry loop because an instrument decision is per instrument, not per
/// stream: one instrument serves the whole source, so confirming or repointing it on any one entry
/// settles every entry that shares it. Doing it per entry would leave 30 of 31 DOC streams still
/// asking.
pub(super) async fn apply_instrument_updates(
    state: &AppState,
    source_system: &str,
    entries: &mut [crate::routes::private::sync::service::PlanEntry],
    updates: &[PlanEntryUpdate],
) -> AppResult<()> {
    for update in updates {
        if update.instrument_id.is_none()
            && update.instrument_name.is_none()
            && update.instrument_confirmed.is_none()
            && update.instrument_clear != Some(true)
        {
            continue;
        }
        let Some(target) = entries.iter().find(|e| e.stream_id == update.stream_id) else {
            continue;
        };
        let key = instrument_key(target);
        // One key in, one key out: the proposal a rename mints is derived from the entry the
        // operator edited, so a row covering several source columns stays one row.
        let proposed_source_key = match target
            .instrument
            .as_ref()
            .and_then(|i| i.curve_column.as_deref())
        {
            Some(column) => format!("{source_system}:{column}"),
            None if target.is_device => target.source_key.clone(),
            None => format!("{source_system}:{}", target.parameter.name),
        };

        // A feed the source reports as a device is its own channel instrument, minted by its
        // pairing with the slot's deployment. Its proposal can be renamed and confirmed like any
        // other, but an existing one is the inventory's to rename, and clearing it would leave the
        // pairing to mint one nobody confirmed.
        if target.is_device && update.instrument_id.is_none() {
            let existing = target.instrument.as_ref().is_some_and(|i| !i.create);
            let refused = if update.instrument_clear == Some(true) {
                Some("its instrument cannot be cleared; attach an existing one instead")
            } else if update.instrument_name.is_some() && existing {
                Some("its instrument is already in the inventory and is renamed on its own page")
            } else {
                None
            };
            if let Some(reason) = refused {
                let named = match &target.device_serial {
                    Some(serial) => format!(" (the source names device serial {serial})"),
                    None => String::new(),
                };
                return Err(AppError::BadRequest(format!(
                    "stream {} is reported as a device{named}, so {reason}.",
                    update.stream_id
                )));
            }
        }

        // A repoint has to name an instrument that exists; otherwise the plan would carry an id
        // the apply cannot resolve.
        let repointed = match update.instrument_id {
            Some(id) => Some(
                sensors::Entity::find_by_id(id)
                    .one(&state.db)
                    .await?
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Instrument {id} does not exist"))
                    })?,
            ),
            None => None,
        };
        // The chosen instrument's curves travel with the entry, so the review shows what it
        // corrects with rather than only its name.
        let repointed_curves = match &repointed {
            Some(sensor) => crate::routes::private::standard_curves::Entity::find()
                .filter(crate::routes::private::standard_curves::Column::SensorId.eq(sensor.id))
                .all(&state.db)
                .await?
                .into_iter()
                .map(|c| crate::routes::private::sync::service::PlanCurveRef {
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                })
                .collect(),
            None => Vec::new(),
        };

        for entry in entries.iter_mut().filter(|e| instrument_key(e) == key) {
            if update.instrument_clear == Some(true) {
                entry.instrument = None;
                continue;
            }
            // A stream whose source names no curve per reading has no instrument until someone
            // says which one corrected it upstream. Attaching an existing one records that, and
            // naming a new one proposes it; neither stamps, because the value already carries the
            // correction. The identity is the parameter, so every station moves together.
            if entry.instrument.is_none() {
                let proposed = update
                    .instrument_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                if repointed.is_some() || proposed.is_some() {
                    let name = proposed
                        .map(str::to_string)
                        .unwrap_or_else(|| entry.parameter.name.clone());
                    entry.instrument =
                        Some(crate::routes::private::sync::service::PlanInstrumentRef {
                            curve_column: None,
                            id: None,
                            name: name.clone(),
                            source_key: proposed_source_key.clone(),
                            resolved_by: if repointed.is_some() {
                                "manual".to_string()
                            } else {
                                "placeholder".to_string()
                            },
                            create: repointed.is_none(),
                            defaulted: repointed.as_ref().is_some_and(is_minted_default),
                            confirmed: repointed.is_some(),
                            stamps_readings: false,
                            curves: Vec::new(),
                            proposed_name: Some(name),
                            name_conflict: None,
                            label_candidates: vec![],
                        });
                }
            }
            let Some(instrument) = entry.instrument.as_mut() else {
                continue;
            };
            if let Some(sensor) = &repointed {
                instrument.id = Some(sensor.id);
                instrument.name = sensor
                    .name
                    .clone()
                    .or_else(|| sensor.serial_number.clone())
                    .unwrap_or_else(|| sensor.id.to_string());
                instrument.source_key = sensor.source_key.clone().unwrap_or_default();
                instrument.resolved_by = "manual".to_string();
                instrument.create = false;
                instrument.defaulted = is_minted_default(sensor);
                instrument.confirmed = true;
                instrument.curves = repointed_curves.clone();
                instrument.label_candidates.clear();
            }
            // Naming an instrument proposes one; picking from the inventory attaches one. So a
            // name arriving at an entry that holds an existing instrument returns it to a
            // proposal, so an operator can undo a mis-click, rather than renaming the inventory row
            // (which this route never does).
            if let Some(name) = &update.instrument_name
                && repointed.is_none()
                && !name.trim().is_empty()
            {
                let name = name.trim().to_string();
                if instrument.create {
                    instrument.name = name.clone();
                    instrument.proposed_name = Some(name);
                } else {
                    instrument.id = None;
                    instrument.name = name.clone();
                    instrument.proposed_name = Some(name);
                    instrument.source_key = proposed_source_key.clone();
                    instrument.resolved_by = "placeholder".to_string();
                    instrument.create = true;
                    instrument.defaulted = false;
                    instrument.confirmed = false;
                    instrument.curves = Vec::new();
                    instrument.label_candidates.clear();
                }
            }

            // A tie is a choice, so it is not what an accept-everything button is for: the label
            // matched two instruments and the plan suggested neither. Refused here rather than in
            // the button, which a second client does not hold.
            if update.instrument_confirmed == Some(true)
                && instrument.resolved_by == "ambiguous_label"
            {
                return Err(AppError::BadRequest(format!(
                    "the curve label of stream {} matches {}: pick one, or name the instrument to \
                     create, before confirming",
                    update.stream_id,
                    instrument
                        .label_candidates
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" and ")
                )));
            }
            if let Some(confirmed) = update.instrument_confirmed {
                instrument.confirmed = confirmed;
            }
        }
        if update.instrument_name.is_some() && repointed.is_none() {
            join_named_proposal(entries, update.stream_id, source_system);
        }
    }
    Ok(())
}

/// Fold the review's object decisions into the plan's accepted list. Accepting a key already
/// accepted leaves the first decision, and its actor, standing; taking one back removes it.
pub(super) fn apply_object_updates(
    accepted: &mut Vec<crate::routes::private::sync::service::PlanAcceptedObject>,
    updates: &[PlanObjectUpdate],
    actor: &str,
) {
    for update in updates {
        let at = accepted.iter().position(|a| a.key == update.key);
        match (update.accepted, at) {
            (true, None) => {
                accepted.push(crate::routes::private::sync::service::PlanAcceptedObject {
                    key: update.key.clone(),
                    accepted_by: Some(actor.to_string()),
                    accepted_at: chrono::Utc::now().into(),
                });
            }
            (false, Some(i)) => {
                accepted.remove(i);
            }
            _ => {}
        }
    }
}

/// The refusal a writer gets when the draft has moved on: the version it should reload is in the
/// detail, so the client re-reads and re-applies rather than guessing.
pub(super) fn stale_plan(current_version: i32) -> AppError {
    AppError::ConflictDetail {
        message: "The plan changed since you read it; reload it and reapply your edits".to_string(),
        detail: serde_json::json!({ "current_version": current_version }),
    }
}

/// Move a plan from one status to the next, atomically. `false` means a concurrent writer got
/// there first and the row is no longer in `from`, which is what every caller checks before
/// doing the work the new status claims.
pub(super) async fn claim_plan_status<C: ConnectionTrait>(
    db: &C,
    plan_id: Uuid,
    from: &str,
    to: &str,
) -> AppResult<bool> {
    let claimed = pairing_plans::models::Entity::update_many()
        .col_expr(pairing_plans::models::Column::Status, Expr::value(to))
        .filter(pairing_plans::models::Column::Id.eq(plan_id))
        .filter(pairing_plans::models::Column::Status.eq(from))
        .exec(db)
        .await?;
    Ok(claimed.rows_affected > 0)
}

/// Fetch a pairing plan's version, or 404 if unknown.
pub(super) async fn plan_version(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<i32> {
    let plan = pairing_plans::models::Entity::find_by_id(id)
        .select_only()
        .column(pairing_plans::models::Column::Version)
        .into_tuple::<i32>()
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(plan)
}

/// Fetch a pairing plan's status, or 404 if unknown.
pub(super) async fn plan_status(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<String> {
    let plan = pairing_plans::models::Entity::find_by_id(id)
        .select_only()
        .column(pairing_plans::models::Column::Status)
        .into_tuple::<String>()
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(plan)
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/review_state.rs"]
mod review_state_tests;

#[cfg(test)]
#[path = "tests/family_suggestion.rs"]
mod family_suggestion_tests;

#[cfg(test)]
#[path = "tests/control_tokens.rs"]
mod control_tokens_tests;

#[cfg(test)]
#[path = "tests/enroll.rs"]
mod enroll_tests;

#[cfg(test)]
#[path = "tests/heartbeat.rs"]
mod heartbeat_tests;

#[cfg(test)]
#[path = "tests/replicate_audit.rs"]
mod replicate_audit_tests;

#[cfg(test)]
#[path = "tests/acknowledgement.rs"]
mod acknowledgement_tests;
