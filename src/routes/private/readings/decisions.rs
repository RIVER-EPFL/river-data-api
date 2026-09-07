//! The curation record. A flag, a withdrawal, a curve choice, a pin, a value correction or a
//! verification is a decision appended here, and the reading's curation columns are its
//! projection, written by the table's trigger inside the writer's transaction (ADR 0008, Q17).
//! Nothing reads the record to serve a value; the projected columns are what every query reads.
//!
//! A rollback is a decision too: it carries the state the inverted decision recorded as `old`
//! and restores exactly that, so reversibility never depends on a client remembering anything
//! (Q36 rule 3).

use axum::{Json, extract::Query, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Flag,
    Unflag,
    Withdraw,
    Reassert,
    Curve,
    CalibrationPin,
    InstrumentPin,
    SlotMove,
    ValueCorrection,
    UnverifiedEntry,
    Verify,
    Reject,
    Chain,
    Detach,
    Return,
    Rollback,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Unflag => "unflag",
            Self::Withdraw => "withdraw",
            Self::Reassert => "reassert",
            Self::Curve => "curve",
            Self::CalibrationPin => "calibration_pin",
            Self::InstrumentPin => "instrument_pin",
            Self::SlotMove => "slot_move",
            Self::ValueCorrection => "value_correction",
            Self::UnverifiedEntry => "unverified_entry",
            Self::Verify => "verify",
            Self::Reject => "reject",
            Self::Chain => "chain",
            Self::Detach => "detach",
            Self::Return => "return",
            Self::Rollback => "rollback",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Flag,
            Self::Unflag,
            Self::Withdraw,
            Self::Reassert,
            Self::Curve,
            Self::CalibrationPin,
            Self::InstrumentPin,
            Self::SlotMove,
            Self::ValueCorrection,
            Self::UnverifiedEntry,
            Self::Verify,
            Self::Reject,
            Self::Chain,
            Self::Detach,
            Self::Return,
            Self::Rollback,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }

    /// The reading columns this kind projects to, which is also the state a decision records as
    /// `old` so a rollback can restore exactly it. Empty for kinds that project nothing.
    ///
    /// `calibrated_value` is not among them for a value correction: a corrected value is what the
    /// curves the row names produce from its raw value, so it is recomposed after the projection
    /// rather than recorded and restored (`calibrations::service::recompose_from_own_curves`).
    #[must_use]
    pub fn projected_columns(self) -> &'static [&'static str] {
        match self {
            Self::Flag | Self::Unflag => &["is_flagged", "flag_reason"],
            Self::Withdraw | Self::Reassert => &["withdrawn_at", "withdrawn_reason"],
            Self::Reject => &["withdrawn_at", "withdrawn_reason", "unverified"],
            Self::Curve => &["standard_curve_id"],
            Self::CalibrationPin => &["calibration_id"],
            Self::InstrumentPin => &["sensor_id"],
            Self::ValueCorrection => &["raw_value", "ingested_at"],
            Self::UnverifiedEntry | Self::Verify => &["unverified"],
            Self::SlotMove | Self::Chain | Self::Detach | Self::Return | Self::Rollback => &[],
        }
    }

    /// The family a decision supersedes within: the latest live decision of the same family on
    /// the same key is what `supersedes` names. `None` for a rollback, which inverts one decision
    /// rather than replacing a family's latest.
    #[must_use]
    pub fn family(self) -> Option<&'static str> {
        match self {
            Self::Flag | Self::Unflag => Some("flag"),
            Self::Withdraw | Self::Reassert | Self::Reject => Some("withdrawn"),
            Self::Curve => Some("curve"),
            Self::CalibrationPin => Some("calibration"),
            Self::InstrumentPin => Some("instrument"),
            Self::SlotMove => Some("slot"),
            Self::ValueCorrection => Some("value"),
            Self::UnverifiedEntry | Self::Verify => Some("verified"),
            Self::Chain | Self::Detach | Self::Return => Some("ownership"),
            Self::Rollback => None,
        }
    }

    /// The columns a decision records as `old`: the projected ones, plus, for an ownership
    /// decision, the run that produced the value it supersedes.
    #[must_use]
    pub fn recorded_columns(self) -> &'static [&'static str] {
        match self {
            Self::Chain | Self::Detach | Self::Return => &["raw_value", "run_id"],
            Self::SlotMove => &["site_id", "parameter_id"],
            other => other.projected_columns(),
        }
    }

    /// A per-row kind cannot be recorded for a whole group: the value it writes belongs to one
    /// replicate.
    #[must_use]
    pub fn per_row_only(self) -> bool {
        matches!(self, Self::ValueCorrection | Self::Chain)
    }

    /// Whether a decision of this kind changes a served spot value, so the visit's calculations
    /// run again (ADR 0007): the hook is reached from the decision write.
    #[must_use]
    pub fn fires_recompute(self) -> bool {
        matches!(
            self,
            Self::Flag
                | Self::Unflag
                | Self::Withdraw
                | Self::Reassert
                | Self::Reject
                | Self::ValueCorrection
                | Self::Rollback
        )
    }
}

/// Every code path that writes a curation column, and what it becomes under the record: a
/// curation writer appends a decision of the given kind; a derivation writer appends nothing and
/// must honour pins. A new writer declares itself here or the classification test fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    FlagRoute,
    UnflagRoute,
    AuditResolveFlag,
    AuditReopen,
    WindowedWithdraw,
    WindowedReassert,
    CsvDisplacement,
    CsvDisplacementReversal,
    GrabCurveClaim,
    IngestCurveClaim,
    GrabReplace,
    IngestOverwrite,
    BatchOverwrite,
    ChainSave,
    MergeMove,
    ReprocessSensor,
    ReprocessSlot,
    CalibrationResolver,
    BackfillAttribution,
    PairingBackfill,
    JanitorRecompose,
    MeasurementRetag,
    SdEstimatorRetag,
}

impl Writer {
    /// The decision a writer appends, or `None` for a derivation.
    #[must_use]
    pub fn decision(self) -> Option<(Kind, Origin)> {
        match self {
            Self::FlagRoute => Some((Kind::Flag, Origin::Manual)),
            Self::UnflagRoute => Some((Kind::Unflag, Origin::Manual)),
            Self::AuditResolveFlag => Some((Kind::Flag, Origin::Audit)),
            Self::AuditReopen => Some((Kind::Unflag, Origin::Audit)),
            Self::WindowedWithdraw => Some((Kind::Withdraw, Origin::Sync)),
            Self::WindowedReassert => Some((Kind::Reassert, Origin::Sync)),
            Self::CsvDisplacement => Some((Kind::Withdraw, Origin::Csv)),
            Self::CsvDisplacementReversal => Some((Kind::Reassert, Origin::Csv)),
            Self::GrabCurveClaim => Some((Kind::Curve, Origin::Manual)),
            Self::IngestCurveClaim => Some((Kind::Curve, Origin::Sync)),
            Self::GrabReplace => Some((Kind::ValueCorrection, Origin::Manual)),
            Self::IngestOverwrite => Some((Kind::ValueCorrection, Origin::Sync)),
            Self::BatchOverwrite => Some((Kind::ValueCorrection, Origin::Manual)),
            Self::ChainSave => Some((Kind::Chain, Origin::Chain)),
            Self::MergeMove => Some((Kind::SlotMove, Origin::Manual)),
            Self::ReprocessSensor
            | Self::ReprocessSlot
            | Self::CalibrationResolver
            | Self::BackfillAttribution
            | Self::PairingBackfill
            | Self::JanitorRecompose
            | Self::MeasurementRetag
            | Self::SdEstimatorRetag => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Manual,
    Sync,
    Csv,
    Audit,
    Chain,
    Rollback,
    Migration,
    System,
}

impl Origin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Sync => "sync",
            Self::Csv => "csv",
            Self::Audit => "audit",
            Self::Chain => "chain",
            Self::Rollback => "rollback",
            Self::Migration => "migration",
            Self::System => "system",
        }
    }
}

/// The reading, or the whole replicate group, a decision is about.
#[derive(Debug, Clone, Copy)]
pub struct DecisionKey {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    /// `None` for a decision over every replicate at the instant.
    pub replicate_index: Option<i16>,
}

/// A decision to record. `old` is captured by the record from the reading itself.
#[derive(Debug, Clone)]
pub struct Decision {
    pub key: DecisionKey,
    pub kind: Kind,
    pub new: serde_json::Value,
    pub actor: String,
    pub reason: Option<String>,
    pub origin: Origin,
    pub set_id: Option<Uuid>,
}

/// A stored decision.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DecisionRow {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicate_index: Option<i16>,
    pub kind: Kind,
    #[schema(value_type = Object)]
    pub old: serde_json::Value,
    #[schema(value_type = Object)]
    pub new: serde_json::Value,
    pub actor: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub reason: Option<String>,
    pub origin: Origin,
    pub supersedes: Option<Uuid>,
    pub rolled_back_by: Option<Uuid>,
    pub set_id: Option<Uuid>,
}

const ROW_COLUMNS: &str = "id, stream_id, time, replicate_index, kind, old, new, actor, at, reason, \
                           origin, supersedes, rolled_back_by, set_id";

fn row_from(r: &sea_orm::QueryResult) -> AppResult<DecisionRow> {
    let kind: String = r.try_get("", "kind")?;
    let origin: String = r.try_get("", "origin")?;
    Ok(DecisionRow {
        id: r.try_get("", "id")?,
        stream_id: r.try_get("", "stream_id")?,
        time: r
            .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "time")?
            .with_timezone(&chrono::Utc),
        replicate_index: r.try_get("", "replicate_index")?,
        kind: Kind::parse(&kind)
            .ok_or_else(|| AppError::Internal(format!("unknown decision kind {kind}")))?,
        old: r.try_get("", "old")?,
        new: r.try_get("", "new")?,
        actor: r.try_get("", "actor")?,
        at: r
            .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "at")?
            .with_timezone(&chrono::Utc),
        reason: r.try_get("", "reason")?,
        origin: match origin.as_str() {
            "manual" => Origin::Manual,
            "sync" => Origin::Sync,
            "csv" => Origin::Csv,
            "audit" => Origin::Audit,
            "chain" => Origin::Chain,
            "rollback" => Origin::Rollback,
            "migration" => Origin::Migration,
            "system" => Origin::System,
            other => {
                return Err(AppError::Internal(format!(
                    "unknown decision origin {other}"
                )));
            }
        },
        supersedes: r.try_get("", "supersedes")?,
        rolled_back_by: r.try_get("", "rolled_back_by")?,
        set_id: r.try_get("", "set_id")?,
    })
}

fn key_binds(key: &DecisionKey) -> Vec<sea_orm::Value> {
    vec![
        key.stream_id.into(),
        sea_orm::prelude::DateTimeWithTimeZone::from(key.time).into(),
        key.replicate_index.into(),
    ]
}

/// The projected state of the columns `kind` touches, read from the key's lowest replicate. This
/// is what the decision records as `old`. `None` when nothing is stored at the key.
async fn current_state<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
    kind: Kind,
) -> AppResult<Option<serde_json::Value>> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT jsonb_build_object(
                 'is_flagged', COALESCE(is_flagged, false),
                 'flag_reason', flag_reason,
                 'withdrawn_at', withdrawn_at,
                 'withdrawn_reason', withdrawn_reason,
                 'standard_curve_id', standard_curve_id,
                 'sensor_id', sensor_id,
                 'calibration_id', calibration_id,
                 'raw_value', raw_value,
                 'calibrated_value', calibrated_value,
                 'unverified', unverified,
                 'ingested_at', ingested_at,
                 'run_id', provenance ->> 'run_id',
                 'site_id', site_id,
                 'parameter_id', parameter_id) AS state
             FROM readings
             WHERE stream_id = $1 AND time = $2
               AND ($3::smallint IS NULL OR replicate_index = $3)
             ORDER BY replicate_index LIMIT 1",
            key_binds(key),
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let state: serde_json::Value = row.try_get("", "state")?;
    Ok(Some(old_state_for(kind, &state)))
}

/// The subset of a row's projected state a kind records as `old`.
#[must_use]
pub fn old_state_for(kind: Kind, state: &serde_json::Value) -> serde_json::Value {
    let mut old = serde_json::Map::new();
    for col in kind.recorded_columns() {
        old.insert(
            (*col).to_string(),
            state.get(*col).cloned().unwrap_or(serde_json::Value::Null),
        );
    }
    serde_json::Value::Object(old)
}

/// The latest live decision of `family` on the key: the one a new decision of that family
/// supersedes.
async fn latest_live<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
    family: &str,
) -> AppResult<Option<Uuid>> {
    let kinds: Vec<String> = [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::Reject,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::SlotMove,
        Kind::ValueCorrection,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
    ]
    .into_iter()
    .filter(|k| k.family() == Some(family))
    .map(|k| k.as_str().to_string())
    .collect();
    let mut binds = key_binds(key);
    binds.push(kinds.into());
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM reading_decisions
             WHERE stream_id = $1 AND time = $2
               AND replicate_index IS NOT DISTINCT FROM $3
               AND kind = ANY($4) AND rolled_back_by IS NULL
             ORDER BY at DESC, id DESC LIMIT 1",
            binds,
        ))
        .await?;
    Ok(row.map(|r| r.try_get("", "id")).transpose()?)
}

/// Append a decision and, through the table's trigger, project it onto the reading. Runs on the
/// caller's connection, which must be a guarded transaction when the row can sit in a compressed
/// chunk. Refuses a key nothing is stored at, and a per-row kind recorded for a whole group.
pub async fn record<C: ConnectionTrait>(conn: &C, d: &Decision) -> AppResult<Uuid> {
    if d.kind == Kind::Rollback {
        return Err(AppError::BadRequest(
            "A rollback is recorded through rollback(), not as a decision of its own".to_string(),
        ));
    }
    if d.kind.per_row_only() && d.key.replicate_index.is_none() {
        return Err(AppError::BadRequest(format!(
            "A {} names one replicate, not a group",
            d.kind.as_str()
        )));
    }
    let Some(old) = current_state(conn, &d.key, d.kind).await? else {
        return Err(AppError::NotFound(format!(
            "No reading at stream {} / {} / replicate {:?}",
            d.key.stream_id, d.key.time, d.key.replicate_index
        )));
    };
    let supersedes = match d.kind.family() {
        Some(family) => latest_live(conn, &d.key, family).await?,
        None => None,
    };
    let mut binds = key_binds(&d.key);
    binds.extend([
        d.kind.as_str().into(),
        old.into(),
        d.new.clone().into(),
        d.actor.clone().into(),
        d.reason.clone().into(),
        d.origin.as_str().into(),
        supersedes.into(),
        d.set_id.into(),
    ]);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO reading_decisions
                 (stream_id, time, replicate_index, kind, old, new, actor, reason, origin,
                  supersedes, set_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING id",
            binds,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("recording a decision returned no row".to_string()))?;
    if d.kind == Kind::ValueCorrection {
        recompose_corrected(
            conn,
            "r.stream_id = $1 AND r.time = $2 \
             AND ($3::smallint IS NULL OR r.replicate_index = $3)",
            key_binds(&d.key),
        )
        .await?;
    }
    Ok(row.try_get("", "id")?)
}

/// Invert one decision: append a `rollback` carrying the state the decision recorded as `old`,
/// which the trigger restores, and stamp the decision `rolled_back_by`. A decision already rolled
/// back, or a rollback itself, is refused: the way forward from there is a fresh decision.
pub async fn rollback<C: ConnectionTrait>(
    conn: &C,
    decision_id: Uuid,
    actor: &str,
    reason: Option<&str>,
) -> AppResult<Uuid> {
    let d = load(conn, decision_id).await?;
    if d.rolled_back_by.is_some() {
        return Err(AppError::Conflict(format!(
            "Decision {decision_id} was already rolled back"
        )));
    }
    if d.kind == Kind::Rollback {
        return Err(AppError::Conflict(
            "A rollback is not rolled back; record the decision again instead".to_string(),
        ));
    }
    if d.kind.projected_columns().is_empty() {
        return Err(AppError::Conflict(format!(
            "A {} projects nothing, so there is nothing to restore",
            d.kind.as_str()
        )));
    }
    let restore = old_state_for(d.kind, &d.old);
    let key = DecisionKey {
        stream_id: d.stream_id,
        time: d.time,
        replicate_index: d.replicate_index,
    };
    let Some(current) = current_state(conn, &key, d.kind).await? else {
        return Err(AppError::NotFound(format!(
            "No reading at stream {} / {} / replicate {:?}",
            key.stream_id, key.time, key.replicate_index
        )));
    };
    let mut binds = key_binds(&key);
    binds.extend([
        current.into(),
        serde_json::json!({ "columns": restore, "of": decision_id }).into(),
        actor.into(),
        reason.into(),
    ]);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO reading_decisions
                 (stream_id, time, replicate_index, kind, old, new, actor, reason, origin)
             VALUES ($1, $2, $3, 'rollback', $4, $5, $6, $7, 'rollback')
             RETURNING id",
            binds,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("recording a rollback returned no row".to_string()))?;
    let rollback_id: Uuid = row.try_get("", "id")?;
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE reading_decisions SET rolled_back_by = $1 WHERE id = $2",
        [rollback_id.into(), decision_id.into()],
    ))
    .await?;
    if d.kind == Kind::ValueCorrection {
        recompose_corrected(
            conn,
            "r.stream_id = $1 AND r.time = $2 \
             AND ($3::smallint IS NULL OR r.replicate_index = $3)",
            key_binds(&key),
        )
        .await?;
    }
    Ok(rollback_id)
}

/// What a bulk record did: rows decided, the time span they cover (for the aggregate refresh),
/// and the visits they touched (for the reactive hook, enqueued by the caller after commit).
#[derive(Debug, Default)]
pub struct Recorded {
    pub rows: u64,
    pub span: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    pub touched_events: Vec<crate::routes::private::collection_events::recompute::TouchedEvent>,
}

impl Recorded {
    #[must_use]
    pub fn touched(&self) -> crate::common::bulk_write::TouchedRange {
        crate::common::bulk_write::TouchedRange {
            rows: self.rows,
            min_time: self.span.map(|(lo, _)| lo),
            max_time: self.span.map(|(_, hi)| hi),
        }
    }
}

/// How a bulk record fills `new`.
#[derive(Debug, Clone)]
pub enum NewValue {
    /// The same assertion for every row.
    Literal(serde_json::Value),
    /// The row's own projected state of the kind's columns, for a decision recorded over rows
    /// born with the state already in place (an insert-time curve claim): `old` is then null.
    Born,
}

const STATE_SQL: &str = "jsonb_build_object(
    'is_flagged', COALESCE(r.is_flagged, false),
    'flag_reason', r.flag_reason,
    'withdrawn_at', r.withdrawn_at,
    'withdrawn_reason', r.withdrawn_reason,
    'standard_curve_id', r.standard_curve_id,
    'sensor_id', r.sensor_id,
    'calibration_id', r.calibration_id,
    'raw_value', r.raw_value,
    'calibrated_value', r.calibrated_value,
    'unverified', r.unverified,
    'ingested_at', r.ingested_at,
    'run_id', r.provenance ->> 'run_id',
    'site_id', r.site_id,
    'parameter_id', r.parameter_id)";

/// Which rows a keyed record decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyed {
    /// Every key that exists.
    All,
    /// Only keys whose projected state does not already contain `new` (a correction that
    /// changes nothing is not a decision).
    Changed,
    /// Only keys with no live decision of this kind already asserting `new` (a claim re-sent
    /// every cycle is recorded once).
    Claim,
}

/// Every kind, in declaration order. The enumerations below filter this rather than repeat it.
const ALL_KINDS: [Kind; 16] = [
    Kind::Flag,
    Kind::Unflag,
    Kind::Withdraw,
    Kind::Reassert,
    Kind::Curve,
    Kind::CalibrationPin,
    Kind::InstrumentPin,
    Kind::SlotMove,
    Kind::ValueCorrection,
    Kind::UnverifiedEntry,
    Kind::Verify,
    Kind::Reject,
    Kind::Chain,
    Kind::Detach,
    Kind::Return,
    Kind::Rollback,
];

fn family_kinds(kind: Kind) -> Vec<String> {
    [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::Reject,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::SlotMove,
        Kind::ValueCorrection,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
    ]
    .into_iter()
    .filter(|k| k.family().is_some() && k.family() == kind.family())
    .map(|k| k.as_str().to_string())
    .collect()
}

/// Record one decision per reading a predicate selects, in one statement, capturing each row's
/// prior state as `old` and naming the decision each supersedes. `row_predicate` is SQL over
/// `r` (`readings`) and `ds` (`data_streams`) with `binds` numbered from `$1`.
#[allow(clippy::too_many_arguments)]
pub async fn record_many<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    row_predicate: &str,
    mut binds: Vec<sea_orm::Value>,
    new: NewValue,
    actor: &str,
    reason: Option<&str>,
    origin: Origin,
    set_id: Option<Uuid>,
) -> AppResult<Recorded> {
    if kind == Kind::Rollback {
        return Err(AppError::BadRequest(
            "A rollback is recorded through rollback(), not in bulk".to_string(),
        ));
    }
    let cols: Vec<String> = kind
        .recorded_columns()
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    let base = binds.len();
    let (old_sql, new_sql) = match &new {
        NewValue::Literal(value) => {
            binds.push(value.clone().into());
            (
                format!(
                    "(SELECT COALESCE(jsonb_object_agg(k, t.state -> k), '{{}}'::jsonb) \
                      FROM unnest(${cols_b}::text[]) AS k)",
                    cols_b = base + 6
                ),
                format!("${}::jsonb", base + 1),
            )
        }
        NewValue::Born => {
            binds.push(serde_json::Value::Null.into());
            (
                format!(
                    "(SELECT COALESCE(jsonb_object_agg(k, 'null'::jsonb), '{{}}'::jsonb) \
                      FROM unnest(${cols_b}::text[]) AS k)",
                    cols_b = base + 6
                ),
                format!(
                    "(SELECT COALESCE(jsonb_object_agg(k, t.state -> k), '{{}}'::jsonb) \
                      FROM unnest(${cols_b}::text[]) AS k)",
                    cols_b = base + 6
                ),
            )
        }
    };
    binds.push(kind.as_str().into());
    binds.push(actor.into());
    binds.push(reason.into());
    binds.push(origin.as_str().into());
    binds.push(cols.into());
    binds.push(family_kinds(kind).into());
    binds.push(set_id.into());
    let sql = format!(
        "WITH target AS (
             SELECT r.stream_id, r.time, r.replicate_index, {STATE_SQL} AS state
             FROM readings r
             JOIN data_streams ds ON ds.id = r.stream_id
             WHERE {row_predicate}
         ), ins AS (
             INSERT INTO reading_decisions
                 (stream_id, time, replicate_index, kind, old, new, actor, reason, origin,
                  supersedes, set_id)
             SELECT t.stream_id, t.time, t.replicate_index, ${kind_b}, {old_sql}, {new_sql},
                    ${actor_b}, ${reason_b}, ${origin_b},
                    (SELECT d.id FROM reading_decisions d
                      WHERE d.stream_id = t.stream_id AND d.time = t.time
                        AND d.replicate_index IS NOT DISTINCT FROM t.replicate_index
                        AND d.kind = ANY(${family_b}) AND d.rolled_back_by IS NULL
                      ORDER BY d.at DESC, d.id DESC LIMIT 1),
                    ${set_b}
             FROM target t
             RETURNING time
         )
         SELECT count(*)::bigint AS rows, min(time) AS lo, max(time) AS hi FROM ins",
        kind_b = base + 2,
        actor_b = base + 3,
        reason_b = base + 4,
        origin_b = base + 5,
        family_b = base + 7,
        set_b = base + 8,
    );
    // The visits the decisions touch are read before the insert: the predicate may name the
    // state the projection is about to change, so afterwards it would match nothing.
    let touched_events = if kind.fires_recompute() {
        crate::routes::private::collection_events::recompute::touched_events(
            conn,
            row_predicate,
            binds[..base].to_vec(),
        )
        .await?
    } else {
        Vec::new()
    };
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            binds,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("recording decisions returned no row".to_string()))?;
    let rows: i64 = row.try_get("", "rows")?;
    let lo: Option<sea_orm::prelude::DateTimeWithTimeZone> = row.try_get("", "lo")?;
    let hi: Option<sea_orm::prelude::DateTimeWithTimeZone> = row.try_get("", "hi")?;
    let rows = u64::try_from(rows).unwrap_or(0);
    Ok(Recorded {
        rows,
        span: lo
            .zip(hi)
            .map(|(a, b)| (a.with_timezone(&chrono::Utc), b.with_timezone(&chrono::Utc))),
        touched_events: if rows > 0 { touched_events } else { Vec::new() },
    })
}

/// One decision per explicit key on one stream, each with its own `new`. Keys nothing stores
/// are skipped; `mode` says which of the rest are decided.
#[allow(clippy::too_many_arguments)]
pub async fn record_keyed<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    stream_id: Uuid,
    rows: &[(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)],
    actor: &str,
    reason: Option<&str>,
    origin: Origin,
    mode: Keyed,
    guard: Option<&str>,
) -> AppResult<Recorded> {
    if rows.is_empty() {
        return Ok(Recorded::default());
    }
    let guard = guard.map(|g| format!(" AND ({g})")).unwrap_or_default();
    let times: Vec<String> = rows.iter().map(|(t, _, _)| t.to_rfc3339()).collect();
    let indices: Vec<i32> = rows.iter().map(|(_, i, _)| i32::from(*i)).collect();
    let news: Vec<String> = rows.iter().map(|(_, _, n)| n.to_string()).collect();
    let filter = match mode {
        Keyed::All => "",
        Keyed::Changed => " AND NOT ({state} @> k.n)",
        Keyed::Claim => {
            " AND NOT EXISTS (SELECT 1 FROM reading_decisions d \
                 WHERE d.stream_id = r.stream_id AND d.time = r.time \
                   AND d.replicate_index IS NOT DISTINCT FROM r.replicate_index \
                   AND d.kind = $5 AND d.rolled_back_by IS NULL AND d.new @> k.n)"
        }
    }
    .replace("{state}", STATE_SQL);
    let cols: Vec<String> = kind
        .recorded_columns()
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    let sql = format!(
        "WITH target AS (
             SELECT r.stream_id, r.time, r.replicate_index, {STATE_SQL} AS state, k.n
             FROM unnest($2::text[]::timestamptz[], $3::int[]::smallint[], $4::text[]::jsonb[])
                  AS k(t, ri, n)
             JOIN readings r ON r.stream_id = $1 AND r.time = k.t AND r.replicate_index = k.ri
             WHERE TRUE{filter}{guard}
         ), ins AS (
             INSERT INTO reading_decisions
                 (stream_id, time, replicate_index, kind, old, new, actor, reason, origin,
                  supersedes)
             SELECT t.stream_id, t.time, t.replicate_index, $5,
                    (SELECT COALESCE(jsonb_object_agg(c, t.state -> c), '{{}}'::jsonb)
                       FROM unnest($9::text[]) AS c),
                    t.n, $6, $7, $8,
                    (SELECT d.id FROM reading_decisions d
                      WHERE d.stream_id = t.stream_id AND d.time = t.time
                        AND d.replicate_index IS NOT DISTINCT FROM t.replicate_index
                        AND d.kind = ANY($10) AND d.rolled_back_by IS NULL
                      ORDER BY d.at DESC, d.id DESC LIMIT 1)
             FROM target t
             RETURNING time
         )
         SELECT count(*)::bigint AS rows, min(time) AS lo, max(time) AS hi FROM ins"
    );
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [
                stream_id.into(),
                times.clone().into(),
                indices.clone().into(),
                news.into(),
                kind.as_str().into(),
                actor.into(),
                reason.into(),
                origin.as_str().into(),
                cols.into(),
                family_kinds(kind).into(),
            ],
        ))
        .await?
        .ok_or_else(|| AppError::Internal("recording decisions returned no row".to_string()))?;
    let rows_n: i64 = row.try_get("", "rows")?;
    let lo: Option<sea_orm::prelude::DateTimeWithTimeZone> = row.try_get("", "lo")?;
    let hi: Option<sea_orm::prelude::DateTimeWithTimeZone> = row.try_get("", "hi")?;
    let mut recorded = Recorded {
        rows: u64::try_from(rows_n).unwrap_or(0),
        span: lo
            .zip(hi)
            .map(|(a, b)| (a.with_timezone(&chrono::Utc), b.with_timezone(&chrono::Utc))),
        touched_events: Vec::new(),
    };
    if recorded.rows > 0 && kind.fires_recompute() {
        recorded.touched_events =
            crate::routes::private::collection_events::recompute::touched_events(
                conn,
                "r.stream_id = $1 AND (r.time, r.replicate_index) IN \
                 (SELECT t, ri FROM unnest($2::text[]::timestamptz[], $3::int[]::smallint[]) AS k(t, ri))",
                vec![stream_id.into(), times.into(), indices.into()],
            )
            .await?;
    }
    Ok(recorded)
}

type Model = crate::routes::private::readings::model::ActiveModel;

fn model_key(m: &Model) -> Option<(Uuid, chrono::DateTime<chrono::Utc>, i16)> {
    let stream = *m.stream_id.try_as_ref()?;
    let time = m.time.try_as_ref()?.with_timezone(&chrono::Utc);
    let index = *m.replicate_index.try_as_ref()?;
    Some((stream, time, index))
}

/// Group `(time, index, new)` rows by stream for a keyed record.
fn by_stream(
    models: &[Model],
    new_for: impl Fn(&Model) -> Option<serde_json::Value>,
) -> HashMapByStream {
    let mut out: HashMapByStream = std::collections::HashMap::new();
    for m in models {
        let Some((stream, time, index)) = model_key(m) else {
            continue;
        };
        let Some(new) = new_for(m) else {
            continue;
        };
        out.entry(stream).or_default().push((time, index, new));
    }
    out
}

type HashMapByStream =
    std::collections::HashMap<Uuid, Vec<(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)>>;

/// Before an overwrite lands: one `value_correction` per stored row whose raw value the models
/// about to be written change. Run on the writing transaction, before the upsert.
pub async fn record_value_corrections<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    actor: &str,
    origin: Origin,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |m| {
        m.raw_value
            .try_as_ref()
            .map(|v| serde_json::json!({ "raw_value": v }))
    }) {
        let r = record_keyed(
            conn,
            Kind::ValueCorrection,
            stream,
            &rows,
            actor,
            None,
            origin,
            Keyed::Changed,
            None,
        )
        .await?;
        all.rows += r.rows;
        all.touched_events.extend(r.touched_events);
    }
    Ok(all)
}

/// After a write lands: one `curve` decision per row carrying a hand-picked standard curve the
/// record does not already hold for it. Re-sent claims are recorded once.
pub async fn record_curve_claims<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    actor: &str,
    origin: Origin,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |m| match m.standard_curve_id.try_as_ref() {
        Some(Some(id)) => Some(serde_json::json!({ "standard_curve_id": id })),
        _ => None,
    }) {
        let r = record_keyed(
            conn,
            Kind::Curve,
            stream,
            &rows,
            actor,
            Some("chosen with the entry"),
            origin,
            Keyed::Claim,
            None,
        )
        .await?;
        all.rows += r.rows;
    }
    Ok(all)
}

/// Before the chain rewrites an output: one `chain` decision per stored row a fresh run
/// supersedes, naming the run it replaces and the run that replaces it.
pub async fn record_chain_supersessions<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    run_id: Uuid,
    actor: &str,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |_| Some(serde_json::json!({ "run_id": run_id }))) {
        let r = record_keyed(
            conn,
            Kind::Chain,
            stream,
            &rows,
            actor,
            Some("superseded by a recompute"),
            Origin::Chain,
            Keyed::Changed,
            None,
        )
        .await?;
        all.rows += r.rows;
    }
    Ok(all)
}

/// SQL over `alias` (a `readings` row) that is true when no live pin of `kind` covers the row.
/// Every derivation that would rewrite the pinned column carries this, so a pin outlives reprocess.
#[must_use]
pub fn not_pinned_sql(alias: &str, kind: Kind) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM reading_decisions d \
             WHERE d.stream_id = {alias}.stream_id AND d.time = {alias}.time \
               AND (d.replicate_index IS NULL OR d.replicate_index = {alias}.replicate_index) \
               AND d.kind = '{}' AND d.rolled_back_by IS NULL)",
        kind.as_str()
    )
}

/// A judgement is a person's ruling on a reading: whether it counts, which curve made it, what
/// instrument it belongs to, whether it has been reviewed. Sync owns the measurement, the value
/// and whether the source still asserts the row, and never a judgement (M61), so a re-send may
/// correct a judged row but never clears the ruling and never moves its servedness silently.
#[must_use]
pub fn is_judgement(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Flag
            | Kind::Curve
            | Kind::CalibrationPin
            | Kind::InstrumentPin
            | Kind::UnverifiedEntry
            | Kind::Verify
            | Kind::Reject
    )
}

/// The judgements standing on a row, from its live decision kinds, newest first as given. Empty
/// means a re-send may correct and retract the row without anyone ruling again.
#[must_use]
pub fn judgements_on(live_kinds: &[Kind]) -> Vec<Kind> {
    live_kinds
        .iter()
        .copied()
        .filter(|k| is_judgement(*k))
        .collect()
}

/// Every kind [`is_judgement`] holds, as SQL literals, so the statement and the function cannot
/// disagree about what a judgement is.
fn judgement_kinds_sql() -> String {
    let kinds: Vec<String> = ALL_KINDS
        .iter()
        .copied()
        .filter(|k| is_judgement(*k))
        .map(|k| format!("'{}'", k.as_str()))
        .collect();
    kinds.join(", ")
}

/// SQL over `alias` (a `readings` row) that is true when no live judgement stands on it, so a
/// writer that must not override a person's ruling can say so in one clause.
#[must_use]
pub fn unjudged_sql(alias: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM reading_decisions d \
             WHERE d.stream_id = {alias}.stream_id AND d.time = {alias}.time \
               AND (d.replicate_index IS NULL OR d.replicate_index = {alias}.replicate_index) \
               AND d.rolled_back_by IS NULL AND d.kind IN ({kinds}))",
        kinds = judgement_kinds_sql()
    )
}

/// SQL over `alias` (a `readings` row) producing the live judgements standing on it as a jsonb
/// array of `{id, kind}`, newest first, or `'[]'`. This is what a `source_modified` hold names,
/// so an operator is told which of their rulings the re-send collided with.
#[must_use]
pub fn live_judgements_sql(alias: &str) -> String {
    format!(
        "COALESCE((SELECT jsonb_agg(jsonb_build_object('id', d.id, 'kind', d.kind) \
                            ORDER BY d.at DESC, d.id DESC) \
                     FROM reading_decisions d \
                    WHERE d.stream_id = {alias}.stream_id AND d.time = {alias}.time \
                      AND (d.replicate_index IS NULL \
                           OR d.replicate_index = {alias}.replicate_index) \
                      AND d.rolled_back_by IS NULL AND d.kind IN ({kinds})), '[]'::jsonb)",
        kinds = judgement_kinds_sql()
    )
}

/// The state a save lands in, by the caller's standing: an intern's entry is pending until a
/// manager verifies or rejects it (Q21). Every other level, and every API token, enters verified.
#[must_use]
pub fn entry_state(highest_role: Option<&crate::common::authz::Role>) -> Option<Kind> {
    match highest_role {
        Some(crate::common::authz::Role::Intern) => Some(Kind::UnverifiedEntry),
        _ => None,
    }
}

/// After an entry lands: one `unverified_entry` per row it wrote, so the review queue and every
/// exclusion read the same record the columns project from.
pub async fn record_unverified_entries<C: ConnectionTrait>(
    conn: &C,
    models: &[Model],
    actor: &str,
    origin: Origin,
) -> AppResult<Recorded> {
    let mut all = Recorded::default();
    for (stream, rows) in by_stream(models, |_| Some(serde_json::json!({ "unverified": true }))) {
        let r = record_keyed(
            conn,
            Kind::UnverifiedEntry,
            stream,
            &rows,
            actor,
            Some("entered by an intern, pending review"),
            origin,
            Keyed::Changed,
            None,
        )
        .await?;
        all.rows += r.rows;
    }
    Ok(all)
}

/// The reading columns the record owns outright: nothing outside the projection trigger writes
/// them, so each must equal the fold of the key's live decisions. The attribution and value
/// columns are deliberately absent, because a derivation rewrites those with no decision at all.
pub const FOLDED_COLUMNS: [&str; 5] = [
    "is_flagged",
    "flag_reason",
    "withdrawn_at",
    "withdrawn_reason",
    "unverified",
];

/// One live decision, as the fold reads it.
#[derive(Debug, Clone)]
pub struct FoldEntry {
    pub kind: Kind,
    pub at: chrono::DateTime<chrono::Utc>,
    pub new: serde_json::Value,
}

/// The folded columns of one reading. `Default` is the state a reading is born in, which is what
/// a column no decision asserts must equal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectedColumns {
    pub is_flagged: bool,
    pub flag_reason: Option<String>,
    pub withdrawn_at: Option<chrono::DateTime<chrono::Utc>>,
    pub withdrawn_reason: Option<String>,
    pub unverified: bool,
}

/// What one decision asserts about the folded columns, mirroring `reading_decisions_project()`.
/// A JSON null asserts the column empty; a kind that touches none of them asserts nothing.
fn asserted_columns(entry: &FoldEntry) -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::{Value, json};
    let reason = || entry.new.get("reason").cloned().unwrap_or(Value::Null);
    let withdrawn_at = || {
        entry
            .new
            .get("withdrawn_at")
            .cloned()
            .unwrap_or_else(|| json!(entry.at))
    };
    match entry.kind {
        Kind::Flag => vec![("is_flagged", json!(true)), ("flag_reason", reason())],
        Kind::Unflag => vec![("is_flagged", json!(false)), ("flag_reason", Value::Null)],
        Kind::Withdraw => vec![
            ("withdrawn_at", withdrawn_at()),
            ("withdrawn_reason", reason()),
        ],
        Kind::Reject => vec![
            ("withdrawn_at", withdrawn_at()),
            ("withdrawn_reason", reason()),
            ("unverified", json!(false)),
        ],
        Kind::Reassert => vec![
            ("withdrawn_at", Value::Null),
            ("withdrawn_reason", Value::Null),
        ],
        Kind::UnverifiedEntry => vec![("unverified", json!(true))],
        Kind::Verify => vec![("unverified", json!(false))],
        Kind::Rollback => entry
            .new
            .get("columns")
            .and_then(serde_json::Value::as_object)
            .map(|c| {
                FOLDED_COLUMNS
                    .iter()
                    .filter_map(|col| c.get(*col).map(|v| (*col, v.clone())))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The folded columns as the record says they stand: newest live decision asserting a column
/// wins, and a column nothing asserts stands as the reading was born. `newest_first` is the key's
/// live decisions, group decisions included, ordered by `at` descending.
#[must_use]
pub fn projected_state(newest_first: &[FoldEntry]) -> ProjectedColumns {
    let mut asserted: std::collections::HashMap<&'static str, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in newest_first {
        for (col, value) in asserted_columns(entry) {
            asserted.entry(col).or_insert(value);
        }
    }
    let text = |col| {
        asserted
            .get(col)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let flag = |col| asserted.get(col).and_then(serde_json::Value::as_bool) == Some(true);
    ProjectedColumns {
        is_flagged: flag("is_flagged"),
        flag_reason: text("flag_reason"),
        withdrawn_at: asserted
            .get("withdrawn_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc)),
        withdrawn_reason: text("withdrawn_reason"),
        unverified: flag("unverified"),
    }
}

/// The same fold in SQL, anti-joined against the readings: every key whose folded columns are not
/// what its live decisions say they should be. Report-only, because which side is wrong is a
/// decision (a rollback, or a fresh decision), never something a sweep may pick.
///
/// It folds every column a decision's own assertion determines. The two it cannot are
/// `calibrated_value`, which is recomposed from the row's own curves rather than asserted, and
/// `ingested_at`, which a correction stamps with the clock; a decision records both as `old` for a
/// rollback to restore, and neither can be predicted from `new`. The columns only some kinds
/// assert are compared only where a decision asserted one, so a row carrying an instrument nothing
/// pinned is not drift.
#[must_use]
pub fn inconsistent_rows_sql() -> String {
    "WITH candidate AS (
         SELECT r.stream_id, r.time, r.replicate_index,
                COALESCE(r.is_flagged, false) AS is_flagged, r.flag_reason,
                r.withdrawn_at, r.withdrawn_reason, COALESCE(r.unverified, false) AS unverified,
                r.standard_curve_id, r.calibration_id, r.sensor_id, r.raw_value
         FROM readings r
         WHERE r.is_flagged IS TRUE OR r.flag_reason IS NOT NULL
            OR r.withdrawn_at IS NOT NULL OR r.withdrawn_reason IS NOT NULL
            OR r.unverified IS TRUE
            OR EXISTS (SELECT 1 FROM reading_decisions d
                        WHERE d.stream_id = r.stream_id AND d.time = r.time
                          AND (d.replicate_index IS NULL
                               OR d.replicate_index = r.replicate_index)
                          AND d.rolled_back_by IS NULL)
     )
     SELECT c.stream_id, c.time, c.replicate_index
     FROM candidate c
     LEFT JOIN LATERAL (
         SELECT jsonb_object_agg(a.col, a.val) AS m
         FROM (
             SELECT DISTINCT ON (v.col) v.col, v.val
             FROM reading_decisions d
             CROSS JOIN LATERAL (VALUES
                 ('is_flagged', CASE d.kind
                      WHEN 'flag' THEN 'true'::jsonb
                      WHEN 'unflag' THEN 'false'::jsonb
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'is_flagged' END),
                 ('flag_reason', CASE d.kind
                      WHEN 'flag' THEN COALESCE(d.new -> 'reason', 'null'::jsonb)
                      WHEN 'unflag' THEN 'null'::jsonb
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'flag_reason' END),
                 ('withdrawn_at', CASE
                      WHEN d.kind IN ('withdraw', 'reject')
                          THEN to_jsonb(COALESCE((d.new ->> 'withdrawn_at')::timestamptz, d.at))
                      WHEN d.kind = 'reassert' THEN 'null'::jsonb
                      WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'withdrawn_at' END),
                 ('withdrawn_reason', CASE
                      WHEN d.kind IN ('withdraw', 'reject')
                          THEN COALESCE(d.new -> 'reason', 'null'::jsonb)
                      WHEN d.kind = 'reassert' THEN 'null'::jsonb
                      WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'withdrawn_reason' END),
                 ('unverified', CASE
                      WHEN d.kind = 'unverified_entry' THEN 'true'::jsonb
                      WHEN d.kind IN ('verify', 'reject') THEN 'false'::jsonb
                      WHEN d.kind = 'rollback' THEN d.new -> 'columns' -> 'unverified' END),
                 ('standard_curve_id', CASE d.kind
                      WHEN 'curve' THEN d.new -> 'standard_curve_id'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'standard_curve_id' END),
                 ('calibration_id', CASE d.kind
                      WHEN 'calibration_pin' THEN d.new -> 'calibration_id'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'calibration_id' END),
                 ('sensor_id', CASE d.kind
                      WHEN 'instrument_pin' THEN d.new -> 'sensor_id'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'sensor_id' END),
                 ('raw_value', CASE d.kind
                      WHEN 'value_correction' THEN d.new -> 'raw_value'
                      WHEN 'rollback' THEN d.new -> 'columns' -> 'raw_value' END)
             ) AS v(col, val)
             WHERE d.stream_id = c.stream_id AND d.time = c.time
               AND (d.replicate_index IS NULL OR d.replicate_index = c.replicate_index)
               AND d.rolled_back_by IS NULL AND v.val IS NOT NULL
             ORDER BY v.col, d.at DESC, d.id DESC
         ) a
     ) e ON TRUE
     WHERE c.is_flagged IS DISTINCT FROM COALESCE((e.m ->> 'is_flagged')::boolean, false)
        OR c.flag_reason IS DISTINCT FROM (e.m ->> 'flag_reason')
        OR c.withdrawn_at IS DISTINCT FROM (e.m ->> 'withdrawn_at')::timestamptz
        OR c.withdrawn_reason IS DISTINCT FROM (e.m ->> 'withdrawn_reason')
        OR c.unverified IS DISTINCT FROM COALESCE((e.m ->> 'unverified')::boolean, false)
        OR (e.m ? 'standard_curve_id'
            AND c.standard_curve_id IS DISTINCT FROM (e.m ->> 'standard_curve_id')::uuid)
        OR (e.m ? 'calibration_id'
            AND c.calibration_id IS DISTINCT FROM (e.m ->> 'calibration_id')::uuid)
        OR (e.m ? 'sensor_id' AND c.sensor_id IS DISTINCT FROM (e.m ->> 'sensor_id')::uuid)
        OR (e.m ? 'raw_value'
            AND c.raw_value IS DISTINCT FROM (e.m ->> 'raw_value')::double precision)"
        .to_string()
}

/// How many readings the record and the columns disagree about. The janitor reports it; nothing
/// repairs it.
pub async fn curation_drift_count<C: ConnectionTrait>(conn: &C) -> AppResult<i64> {
    let sql = format!(
        "SELECT count(*)::bigint AS n FROM ({}) drift",
        inconsistent_rows_sql()
    );
    let row = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("counting curation drift returned no row".to_string()))?;
    Ok(row.try_get("", "n")?)
}
/// One reading key inside an explicit selection.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SelectionKey {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
}

/// The readings a set decision covers: a stream, a visit, a slot, or explicit keys, each
/// optionally bounded by a time window.
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default)]
    pub stream_id: Option<Uuid>,
    #[serde(default)]
    pub collection_event_id: Option<Uuid>,
    #[serde(default)]
    pub site_id: Option<Uuid>,
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    #[serde(default)]
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub keys: Vec<SelectionKey>,
}

impl Selection {
    /// The predicate over `r` this selection names, with its binds. A selection that names
    /// nothing (no stream, no slot, no keys) is refused: "every reading" is not a selection.
    pub fn predicate(&self) -> AppResult<(String, Vec<sea_orm::Value>)> {
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<sea_orm::Value> = Vec::new();
        if let Some(stream_id) = self.stream_id {
            binds.push(stream_id.into());
            clauses.push(format!("r.stream_id = ${}", binds.len()));
        }
        if let Some(event_id) = self.collection_event_id {
            binds.push(event_id.into());
            clauses.push(format!("r.collection_event_id = ${}", binds.len()));
        }
        match (self.site_id, self.parameter_id) {
            (Some(site_id), Some(parameter_id)) => {
                binds.push(site_id.into());
                clauses.push(format!("r.site_id = ${}", binds.len()));
                binds.push(parameter_id.into());
                clauses.push(format!("r.parameter_id = ${}", binds.len()));
            }
            (None, None) => {}
            _ => {
                return Err(AppError::BadRequest(
                    "A slot selection names both site_id and parameter_id".to_string(),
                ));
            }
        }
        if let Some(from) = self.from {
            binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(from).into());
            clauses.push(format!("r.time >= ${}", binds.len()));
        }
        if let Some(to) = self.to {
            binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(to).into());
            clauses.push(format!("r.time <= ${}", binds.len()));
        }
        if !self.keys.is_empty() {
            let mut triples = Vec::with_capacity(self.keys.len());
            for k in &self.keys {
                binds.push(k.stream_id.into());
                let a = binds.len();
                binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(k.time).into());
                let b = binds.len();
                binds.push(k.replicate_index.into());
                let c = binds.len();
                triples.push(format!(
                    "(r.stream_id = ${a} AND r.time = ${b} \
                      AND (${c}::smallint IS NULL OR r.replicate_index = ${c}))"
                ));
            }
            clauses.push(format!("({})", triples.join(" OR ")));
        }
        if self.stream_id.is_none()
            && self.collection_event_id.is_none()
            && self.site_id.is_none()
            && self.keys.is_empty()
        {
            return Err(AppError::BadRequest(
                "A selection names a stream, a visit, a slot (site_id and parameter_id), or keys"
                    .to_string(),
            ));
        }
        Ok((clauses.join(" AND "), binds))
    }
}

/// Record one decision per reading a selection covers, as one set. Returns the set id and what
/// was recorded.
/// Put the corrected value back under the rows a value correction touched.
///
/// The projection writes `raw_value` and leaves `calibrated_value` NULL, because a corrected value
/// is a claim about the curves the row itself names rather than a number a decision may record.
/// This recomposes it from exactly those curves, so value and provenance move together.
async fn recompose_corrected<C: ConnectionTrait>(
    conn: &C,
    scope_sql: &str,
    params: Vec<sea_orm::Value>,
) -> AppResult<()> {
    crate::routes::private::sensors::calibrations::service::recompose_from_own_curves(
        conn,
        &crate::routes::private::sensors::calibrations::service::corrected_rows("r"),
        scope_sql,
        params,
    )
    .await?;
    Ok(())
}

pub async fn record_set<C: ConnectionTrait>(
    conn: &C,
    kind: Kind,
    selection: &Selection,
    new: serde_json::Value,
    actor: &str,
    reason: Option<&str>,
    origin: Origin,
) -> AppResult<(Uuid, Recorded)> {
    let (predicate, binds) = selection.predicate()?;
    let set_id = Uuid::new_v4();
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reading_decision_sets (id, kind, selection, new, actor, reason)
         VALUES ($1, $2, $3, $4, $5, $6)",
        [
            set_id.into(),
            kind.as_str().into(),
            serde_json::to_value(selection)
                .map_err(|e| AppError::Internal(e.to_string()))?
                .into(),
            new.clone().into(),
            actor.into(),
            reason.into(),
        ],
    ))
    .await?;
    let recorded = record_many(
        conn,
        kind,
        &predicate,
        binds,
        NewValue::Literal(new),
        actor,
        reason,
        origin,
        Some(set_id),
    )
    .await?;
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE reading_decision_sets SET rows_decided = $2 WHERE id = $1",
        [
            set_id.into(),
            i64::try_from(recorded.rows).unwrap_or(i64::MAX).into(),
        ],
    ))
    .await?;
    if kind == Kind::ValueCorrection && recorded.rows > 0 {
        recompose_corrected(
            conn,
            "EXISTS (SELECT 1 FROM reading_decisions d WHERE d.set_id = $1 \
                       AND d.stream_id = r.stream_id AND d.time = r.time \
                       AND d.replicate_index = r.replicate_index)",
            vec![set_id.into()],
        )
        .await?;
    }
    Ok((set_id, recorded))
}

/// Roll back every live decision of a set, one rollback decision each, and stamp the set.
pub async fn rollback_set<C: ConnectionTrait>(
    conn: &C,
    set_id: Uuid,
    actor: &str,
    reason: Option<&str>,
) -> AppResult<usize> {
    let already = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT rolled_back_at IS NOT NULL AS done FROM reading_decision_sets WHERE id = $1",
            [set_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Decision set {set_id} not found")))?;
    if already.try_get::<bool>("", "done")? {
        return Err(AppError::Conflict(format!(
            "Decision set {set_id} was already rolled back"
        )));
    }
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM reading_decisions WHERE set_id = $1 AND rolled_back_by IS NULL",
            [set_id.into()],
        ))
        .await?;
    let mut n = 0usize;
    for row in &rows {
        let id: Uuid = row.try_get("", "id")?;
        rollback(conn, id, actor, reason).await?;
        n += 1;
    }
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE reading_decision_sets SET rolled_back_at = now(), rolled_back_by = $2 WHERE id = $1",
        [set_id.into(), actor.into()],
    ))
    .await?;
    Ok(n)
}

/// The slots a predicate's readings belong to, for the reprocess a pin enqueues.
async fn slots_of<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<(Uuid, Uuid)>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT DISTINCT r.site_id, r.parameter_id FROM readings r
                 WHERE {predicate} AND r.site_id IS NOT NULL AND r.parameter_id IS NOT NULL"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("", "site_id")?, r.try_get("", "parameter_id")?)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PinKind {
    Instrument,
    Calibration,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PinRequest {
    pub kind: PinKind,
    /// The sensor (instrument pin) or calibration (calibration pin) the readings belong to.
    pub target_id: Uuid,
    pub selection: Selection,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PinResponse {
    pub set_id: Uuid,
    pub rows_decided: u64,
    /// The slot reprocess jobs enqueued so the pinned rows' curves follow the pin.
    pub jobs: Vec<Uuid>,
}

/// Pin a selection of readings to an instrument or a calibration (Q36 addendum): one decision
/// per reading in one set, projected onto the rows now, and honoured by every later reprocess.
/// The slots touched are reprocessed so a pinned instrument's own calibration windows apply.
/// Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/readings/pins",
    request_body = PinRequest,
    responses(
        (status = 200, description = "The set recorded", body = PinResponse),
        (status = 400, description = "Empty selection or unknown target"),
    ),
    tag = "readings"
)]
pub async fn pin_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<PinRequest>,
) -> AppResult<Json<PinResponse>> {
    let actor = crate::routes::private::tools::scripts::actor_label(&auth);
    let (kind, new, exists_sql) = match req.kind {
        PinKind::Instrument => (
            Kind::InstrumentPin,
            serde_json::json!({ "sensor_id": req.target_id }),
            "SELECT 1 FROM sensors WHERE id = $1",
        ),
        PinKind::Calibration => (
            Kind::CalibrationPin,
            serde_json::json!({ "calibration_id": req.target_id }),
            "SELECT 1 FROM sensor_calibrations WHERE id = $1",
        ),
    };
    if state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            exists_sql,
            [req.target_id.into()],
        ))
        .await?
        .is_none()
    {
        return Err(AppError::BadRequest(format!(
            "No {} with id {}",
            match req.kind {
                PinKind::Instrument => "sensor",
                PinKind::Calibration => "calibration",
            },
            req.target_id
        )));
    }
    let (predicate, binds) = req.selection.predicate()?;
    let slots = slots_of(&state.db, &predicate, binds).await?;
    let (set_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        record_set(
            txn,
            kind,
            &req.selection,
            new.clone(),
            &actor,
            req.reason.as_deref(),
            Origin::Manual,
        )
        .await
    })
    .await?;
    let mut jobs = Vec::new();
    for (site_id, parameter_id) in slots {
        if let Some(job) = crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "attribution_pin",
            (req.kind == PinKind::Instrument).then_some(req.target_id),
            Some(set_id),
            &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
            None,
        )
        .await?
        {
            jobs.push(job);
        }
    }
    Ok(Json(PinResponse {
        set_id,
        rows_decided: recorded.rows,
        jobs,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackSetResponse {
    pub set_id: Uuid,
    pub rolled_back: usize,
    pub jobs: Vec<Uuid>,
}

/// Roll a set back: every live decision it made is inverted and the slots reprocessed so the
/// windows own the rows again. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/readings/pins/{set_id}/rollback",
    params(("set_id" = Uuid, Path, description = "Decision set id")),
    responses(
        (status = 200, description = "Rolled back", body = RollbackSetResponse),
        (status = 404, description = "Unknown set"),
        (status = 409, description = "Already rolled back"),
    ),
    tag = "readings"
)]
pub async fn rollback_pin_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    axum::extract::Path(set_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<RollbackSetResponse>> {
    let actor = crate::routes::private::tools::scripts::actor_label(&auth);
    let selection_row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT selection FROM reading_decision_sets WHERE id = $1",
            [set_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Decision set {set_id} not found")))?;
    let selection: Selection = serde_json::from_value(selection_row.try_get("", "selection")?)
        .map_err(|e| AppError::Internal(format!("stored selection unreadable: {e}")))?;
    let (predicate, binds) = selection.predicate()?;
    let slots = slots_of(&state.db, &predicate, binds).await?;
    let rolled_back = crate::common::bulk_write::guarded(&state.db, async |txn| {
        rollback_set(txn, set_id, &actor, Some("set rolled back")).await
    })
    .await?;
    let mut jobs = Vec::new();
    for (site_id, parameter_id) in slots {
        if let Some(job) = crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "attribution_pin",
            None,
            Some(set_id),
            &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
            None,
        )
        .await?
        {
            jobs.push(job);
        }
    }
    Ok(Json(RollbackSetResponse {
        set_id,
        rolled_back,
        jobs,
    }))
}

/// Who owns an output slot at a visit (Q40, Q47): the calculation, or a person who detached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    Tool,
    Manual,
}

/// The ownership fold over a slot's ownership decisions (`chain`, `detach`, `return`), newest
/// first, and the instant of the latest input decision at the visit. A `detach` makes the slot
/// manual until a `return`, a `chain` supersession, or an input decision newer than it, which
/// re-engages the tool (Q47's clarification).
#[must_use]
pub fn slot_owner(
    ownership_newest_first: &[(Kind, chrono::DateTime<chrono::Utc>)],
    latest_input_decision: Option<chrono::DateTime<chrono::Utc>>,
) -> Owner {
    match ownership_newest_first.first() {
        Some((Kind::Detach, at)) => match latest_input_decision {
            Some(input) if input > *at => Owner::Tool,
            _ => Owner::Manual,
        },
        _ => Owner::Tool,
    }
}

/// The output rows at one slot instant: `(stream_id, replicate indices)` per stream.
async fn output_rows_at<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Vec<(Uuid, i16)>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT stream_id, replicate_index FROM readings
             WHERE site_id = $1 AND parameter_id = $2 AND time = $3
               AND measurement_type = 'spot'
             ORDER BY stream_id, replicate_index",
            [
                site_id.into(),
                parameter_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
            ],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("", "stream_id")?,
                r.try_get("", "replicate_index")?,
            ))
        })
        .collect()
}

/// The owner of an output slot at a visit, read from the record: the latest live ownership
/// decision on the slot's rows against the latest decision on any other parameter's rows at the
/// same site and instant (an input edit).
pub async fn output_owner<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Owner> {
    let binds = [
        site_id.into(),
        parameter_id.into(),
        sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
    ];
    let ownership = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.kind, d.at FROM reading_decisions d
             JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time
                AND (d.replicate_index IS NULL OR d.replicate_index = r.replicate_index)
             WHERE r.site_id = $1 AND r.parameter_id = $2 AND r.time = $3
               AND d.kind IN ('chain', 'detach', 'return') AND d.rolled_back_by IS NULL
             ORDER BY d.at DESC, d.id DESC",
            binds.clone(),
        ))
        .await?;
    let ownership: Vec<(Kind, chrono::DateTime<chrono::Utc>)> = ownership
        .iter()
        .filter_map(|r| {
            let kind = Kind::parse(&r.try_get::<String>("", "kind").ok()?)?;
            let at = r
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "at")
                .ok()?
                .with_timezone(&chrono::Utc);
            Some((kind, at))
        })
        .collect();
    let input = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT max(d.at) AS at FROM reading_decisions d
             JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time
                AND (d.replicate_index IS NULL OR d.replicate_index = r.replicate_index)
             WHERE r.site_id = $1 AND r.parameter_id <> $2 AND r.time = $3
               AND d.kind IN ('flag', 'unflag', 'withdraw', 'reassert', 'reject',
                              'value_correction', 'rollback')",
            binds,
        ))
        .await?
        .and_then(|r| {
            r.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "at")
                .ok()
                .flatten()
        })
        .map(|t| t.with_timezone(&chrono::Utc));
    Ok(slot_owner(&ownership, input))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputSlotRequest {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OwnershipResponse {
    pub owner: Owner,
    pub rows_decided: u64,
}

/// Detach an output slot at a visit from its calculation (Q40's admin override): the rows
/// become manual entries and the chain no longer writes the slot there, until an input at the
/// visit changes or the slot is returned. Requires Administrator.
#[utoipa::path(
    post,
    path = "/api/readings/detach",
    request_body = OutputSlotRequest,
    responses(
        (status = 200, description = "Detached", body = OwnershipResponse),
        (status = 404, description = "No readings at the slot instant"),
        (status = 409, description = "Already manual"),
    ),
    tag = "readings"
)]
pub async fn detach_output(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<OutputSlotRequest>,
) -> AppResult<Json<OwnershipResponse>> {
    let actor = crate::routes::private::tools::scripts::actor_label(&auth);
    let rows = output_rows_at(&state.db, req.site_id, req.parameter_id, req.time).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound(
            "No spot readings at that site, parameter and instant".to_string(),
        ));
    }
    if output_owner(&state.db, req.site_id, req.parameter_id, req.time).await? == Owner::Manual {
        return Err(AppError::Conflict(
            "The slot is already detached".to_string(),
        ));
    }
    let mut streams: Vec<Uuid> = rows.iter().map(|(s, _)| *s).collect();
    streams.dedup();
    let mut decided = 0u64;
    let recorded = crate::common::bulk_write::guarded(&state.db, async |txn| {
        for stream_id in &streams {
            record(
                txn,
                &Decision {
                    key: DecisionKey {
                        stream_id: *stream_id,
                        time: req.time,
                        replicate_index: None,
                    },
                    kind: Kind::Detach,
                    new: serde_json::json!({ "owner": "manual" }),
                    actor: actor.clone(),
                    reason: req.reason.clone(),
                    origin: Origin::Manual,
                    set_id: None,
                },
            )
            .await?;
            decided += 1;
        }
        Ok(decided)
    })
    .await?;
    Ok(Json(OwnershipResponse {
        owner: Owner::Manual,
        rows_decided: recorded,
    }))
}

/// Return a detached output slot to its calculation: the value the last correction since the
/// detach replaced is restored and the tool owns the slot again. Requires Administrator.
#[utoipa::path(
    post,
    path = "/api/readings/return",
    request_body = OutputSlotRequest,
    responses(
        (status = 200, description = "Returned", body = OwnershipResponse),
        (status = 404, description = "No readings at the slot instant"),
        (status = 409, description = "Not detached"),
    ),
    tag = "readings"
)]
pub async fn return_output(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<OutputSlotRequest>,
) -> AppResult<Json<OwnershipResponse>> {
    let actor = crate::routes::private::tools::scripts::actor_label(&auth);
    let rows = output_rows_at(&state.db, req.site_id, req.parameter_id, req.time).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound(
            "No spot readings at that site, parameter and instant".to_string(),
        ));
    }
    if output_owner(&state.db, req.site_id, req.parameter_id, req.time).await? == Owner::Tool {
        return Err(AppError::Conflict("The slot is not detached".to_string()));
    }
    let mut streams: Vec<Uuid> = rows.iter().map(|(s, _)| *s).collect();
    streams.dedup();
    let decided = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let mut decided = 0u64;
        for stream_id in &streams {
            // The value the first correction after the detach replaced is the tool's last value.
            let restore = txn
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT DISTINCT ON (c.replicate_index) c.replicate_index, c.old
                     FROM reading_decisions c
                     WHERE c.stream_id = $1 AND c.time = $2 AND c.kind = 'value_correction'
                       AND c.rolled_back_by IS NULL
                       AND c.at > (SELECT max(d.at) FROM reading_decisions d
                                    WHERE d.stream_id = $1 AND d.time = $2 AND d.kind = 'detach'
                                      AND d.rolled_back_by IS NULL)
                     ORDER BY c.replicate_index, c.at ASC, c.id ASC",
                    [
                        (*stream_id).into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(req.time).into(),
                    ],
                ))
                .await?;
            let rows: Vec<(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)> = restore
                .iter()
                .filter_map(|r| {
                    let index: i16 = r.try_get("", "replicate_index").ok()?;
                    let old: serde_json::Value = r.try_get("", "old").ok()?;
                    let raw = old.get("raw_value")?.as_f64()?;
                    Some((req.time, index, serde_json::json!({ "raw_value": raw })))
                })
                .collect();
            record_keyed(
                txn,
                Kind::ValueCorrection,
                *stream_id,
                &rows,
                &actor,
                Some("returned to the calculation's value"),
                Origin::Rollback,
                Keyed::Changed,
                None,
            )
            .await?;
            record(
                txn,
                &Decision {
                    key: DecisionKey {
                        stream_id: *stream_id,
                        time: req.time,
                        replicate_index: None,
                    },
                    kind: Kind::Return,
                    new: serde_json::json!({ "owner": "tool" }),
                    actor: actor.clone(),
                    reason: req.reason.clone(),
                    origin: Origin::Manual,
                    set_id: None,
                },
            )
            .await?;
            decided += 1;
        }
        Ok(decided)
    })
    .await?;
    Ok(Json(OwnershipResponse {
        owner: Owner::Tool,
        rows_decided: decided,
    }))
}

pub async fn load<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<DecisionRow> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT {ROW_COLUMNS} FROM reading_decisions WHERE id = $1"),
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Decision {id} not found")))?;
    row_from(&row)
}

/// Every decision on a key, newest first. A group key lists group decisions only; a replicate
/// key lists the replicate's own decisions and the group decisions that covered it.
pub async fn history<C: ConnectionTrait>(
    conn: &C,
    key: &DecisionKey,
) -> AppResult<Vec<DecisionRow>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT {ROW_COLUMNS} FROM reading_decisions
                 WHERE stream_id = $1 AND time = $2
                   AND (replicate_index IS NULL OR $3::smallint IS NULL OR replicate_index = $3)
                 ORDER BY at DESC, id DESC"
            ),
            key_binds(key),
        ))
        .await?;
    rows.iter().map(row_from).collect()
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct DecisionsQuery {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    /// Omit for the group's decisions.
    #[serde(default)]
    pub replicate_index: Option<i16>,
}

/// The decision history of one reading or replicate group, newest first. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/decisions",
    params(DecisionsQuery),
    responses((status = 200, description = "Decisions, newest first", body = [DecisionRow])),
    tag = "readings"
)]
pub async fn list_decisions(
    State(state): State<AppState>,
    Query(q): Query<DecisionsQuery>,
) -> AppResult<Json<Vec<DecisionRow>>> {
    let key = DecisionKey {
        stream_id: q.stream_id,
        time: q.time,
        replicate_index: q.replicate_index,
    };
    Ok(Json(history(&state.db, &key).await?))
}

#[cfg(test)]
mod tests {
    use super::{Kind, old_state_for};

    #[test]
    fn every_kind_round_trips_its_name() {
        for k in [
            Kind::Flag,
            Kind::Unflag,
            Kind::Withdraw,
            Kind::Reassert,
            Kind::Curve,
            Kind::CalibrationPin,
            Kind::InstrumentPin,
            Kind::SlotMove,
            Kind::ValueCorrection,
            Kind::UnverifiedEntry,
            Kind::Verify,
            Kind::Reject,
            Kind::Chain,
            Kind::Detach,
            Kind::Return,
            Kind::Rollback,
        ] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kind::parse("delete"), None);
    }

    #[test]
    fn each_kind_projects_to_its_own_columns_and_ownership_kinds_project_nothing() {
        assert_eq!(
            Kind::Flag.projected_columns(),
            ["is_flagged", "flag_reason"]
        );
        assert_eq!(
            Kind::Unflag.projected_columns(),
            ["is_flagged", "flag_reason"]
        );
        assert_eq!(
            Kind::Withdraw.projected_columns(),
            ["withdrawn_at", "withdrawn_reason"]
        );
        assert_eq!(
            Kind::Reject.projected_columns(),
            ["withdrawn_at", "withdrawn_reason", "unverified"]
        );
        assert_eq!(Kind::Curve.projected_columns(), ["standard_curve_id"]);
        assert_eq!(Kind::InstrumentPin.projected_columns(), ["sensor_id"]);
        assert_eq!(Kind::CalibrationPin.projected_columns(), ["calibration_id"]);
        assert_eq!(
            Kind::ValueCorrection.projected_columns(),
            ["raw_value", "ingested_at"],
            "the corrected value is recomposed from the row's own curves, never recorded"
        );
        assert_eq!(Kind::Verify.projected_columns(), ["unverified"]);
        for k in [
            Kind::Chain,
            Kind::Detach,
            Kind::Return,
            Kind::SlotMove,
            Kind::Rollback,
        ] {
            assert!(k.projected_columns().is_empty(), "{k:?}");
        }
    }

    #[test]
    fn a_family_pairs_a_decision_with_what_undoes_it() {
        assert_eq!(Kind::Flag.family(), Kind::Unflag.family());
        assert_eq!(Kind::Withdraw.family(), Kind::Reassert.family());
        assert_eq!(Kind::Withdraw.family(), Kind::Reject.family());
        assert_eq!(Kind::UnverifiedEntry.family(), Kind::Verify.family());
        assert_ne!(Kind::Flag.family(), Kind::Withdraw.family());
        assert_ne!(Kind::InstrumentPin.family(), Kind::CalibrationPin.family());
        assert_eq!(Kind::Rollback.family(), None);
    }

    #[test]
    fn old_state_keeps_only_the_columns_the_kind_touches() {
        let state = serde_json::json!({
            "is_flagged": true, "flag_reason": "x", "raw_value": 1.5,
            "calibrated_value": 1.7, "ingested_at": "2025-06-15T10:00:00Z",
            "unverified": false, "sensor_id": null
        });
        assert_eq!(
            old_state_for(Kind::Flag, &state),
            serde_json::json!({ "is_flagged": true, "flag_reason": "x" })
        );
        assert_eq!(
            old_state_for(Kind::ValueCorrection, &state),
            serde_json::json!({ "raw_value": 1.5, "ingested_at": "2025-06-15T10:00:00Z" })
        );
        // An ownership decision records the value and the run it supersedes, and projects nothing.
        assert_eq!(
            old_state_for(Kind::Chain, &state),
            serde_json::json!({ "raw_value": 1.5, "run_id": null })
        );
        // An absent column is recorded as null, so a rollback clears it rather than skipping it.
        assert_eq!(
            old_state_for(Kind::Curve, &state),
            serde_json::json!({ "standard_curve_id": null })
        );
    }

    #[test]
    fn every_writer_is_classified_and_derivation_writers_append_nothing() {
        use super::{Origin, Writer};
        let curation = [
            (Writer::FlagRoute, Kind::Flag, Origin::Manual),
            (Writer::UnflagRoute, Kind::Unflag, Origin::Manual),
            (Writer::AuditResolveFlag, Kind::Flag, Origin::Audit),
            (Writer::AuditReopen, Kind::Unflag, Origin::Audit),
            (Writer::WindowedWithdraw, Kind::Withdraw, Origin::Sync),
            (Writer::WindowedReassert, Kind::Reassert, Origin::Sync),
            (Writer::CsvDisplacement, Kind::Withdraw, Origin::Csv),
            (Writer::CsvDisplacementReversal, Kind::Reassert, Origin::Csv),
            (Writer::GrabCurveClaim, Kind::Curve, Origin::Manual),
            (Writer::IngestCurveClaim, Kind::Curve, Origin::Sync),
            (Writer::GrabReplace, Kind::ValueCorrection, Origin::Manual),
            (Writer::IngestOverwrite, Kind::ValueCorrection, Origin::Sync),
            (
                Writer::BatchOverwrite,
                Kind::ValueCorrection,
                Origin::Manual,
            ),
            (Writer::ChainSave, Kind::Chain, Origin::Chain),
            (Writer::MergeMove, Kind::SlotMove, Origin::Manual),
        ];
        for (w, k, o) in curation {
            assert_eq!(w.decision(), Some((k, o)), "{w:?}");
        }
        for w in [
            Writer::ReprocessSensor,
            Writer::ReprocessSlot,
            Writer::CalibrationResolver,
            Writer::BackfillAttribution,
            Writer::PairingBackfill,
            Writer::JanitorRecompose,
            Writer::MeasurementRetag,
            Writer::SdEstimatorRetag,
        ] {
            assert_eq!(w.decision(), None, "{w:?} derives, it does not decide");
        }
    }

    #[test]
    fn the_kinds_that_move_a_served_value_fire_the_recompute() {
        for k in [
            Kind::Flag,
            Kind::Unflag,
            Kind::Withdraw,
            Kind::Reassert,
            Kind::ValueCorrection,
        ] {
            assert!(k.fires_recompute(), "{k:?}");
        }
        for k in [
            Kind::Curve,
            Kind::InstrumentPin,
            Kind::Chain,
            Kind::Detach,
            Kind::Verify,
        ] {
            assert!(!k.fires_recompute(), "{k:?}");
        }
    }

    #[test]
    fn a_pin_exclusion_names_the_kind_and_covers_group_pins() {
        let sql = super::not_pinned_sql("r", Kind::InstrumentPin);
        assert!(sql.contains("d.kind = 'instrument_pin'"));
        assert!(sql.contains("d.rolled_back_by IS NULL"));
        assert!(sql.contains("d.replicate_index IS NULL OR d.replicate_index = r.replicate_index"));
        assert!(super::not_pinned_sql("tgt", Kind::CalibrationPin).contains("tgt.stream_id"));
    }

    #[test]
    fn a_selection_names_a_stream_a_slot_or_keys_and_nothing_else() {
        use super::{Selection, SelectionKey};
        let none = Selection::default();
        assert!(none.predicate().is_err(), "nothing selected is refused");
        let by_stream = Selection {
            stream_id: Some(uuid::Uuid::nil()),
            ..Default::default()
        };
        let (sql, binds) = by_stream.predicate().unwrap();
        assert_eq!(sql, "r.stream_id = $1");
        assert_eq!(binds.len(), 1);
        let half_slot = Selection {
            site_id: Some(uuid::Uuid::nil()),
            ..Default::default()
        };
        assert!(half_slot.predicate().is_err(), "a slot needs both ids");
        let at = chrono::DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let slot_window = Selection {
            site_id: Some(uuid::Uuid::nil()),
            parameter_id: Some(uuid::Uuid::nil()),
            from: Some(at),
            to: Some(at),
            ..Default::default()
        };
        let (sql, binds) = slot_window.predicate().unwrap();
        assert_eq!(
            sql,
            "r.site_id = $1 AND r.parameter_id = $2 AND r.time >= $3 AND r.time <= $4"
        );
        assert_eq!(binds.len(), 4);
        let keys = Selection {
            keys: vec![
                SelectionKey {
                    stream_id: uuid::Uuid::nil(),
                    time: at,
                    replicate_index: Some(1),
                },
                SelectionKey {
                    stream_id: uuid::Uuid::nil(),
                    time: at,
                    replicate_index: None,
                },
            ],
            ..Default::default()
        };
        let (sql, binds) = keys.predicate().unwrap();
        assert!(sql.contains("r.stream_id = $1 AND r.time = $2"));
        assert!(sql.contains("$6::smallint IS NULL OR r.replicate_index = $6"));
        assert_eq!(binds.len(), 6);
    }

    #[test]
    fn a_selection_can_name_one_visit() {
        use super::Selection;
        let event = uuid::Uuid::nil();
        let by_event = Selection {
            collection_event_id: Some(event),
            ..Default::default()
        };
        let (sql, binds) = by_event.predicate().unwrap();
        assert_eq!(sql, "r.collection_event_id = $1");
        assert_eq!(binds.len(), 1);

        // A visit narrowed to one parameter is still a selection, and the slot rule still holds.
        let one_parameter = Selection {
            collection_event_id: Some(event),
            site_id: Some(uuid::Uuid::nil()),
            parameter_id: Some(uuid::Uuid::nil()),
            ..Default::default()
        };
        let (sql, binds) = one_parameter.predicate().unwrap();
        assert_eq!(
            sql,
            "r.collection_event_id = $1 AND r.site_id = $2 AND r.parameter_id = $3"
        );
        assert_eq!(binds.len(), 3);
    }

    #[test]
    fn a_detach_makes_the_slot_manual_until_an_input_moves_or_it_is_returned() {
        use super::{Owner, slot_owner};
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        assert_eq!(slot_owner(&[], None), Owner::Tool);
        assert_eq!(
            slot_owner(&[(Kind::Chain, t("2025-06-01T00:00:00Z"))], None),
            Owner::Tool
        );
        let detached = [(Kind::Detach, t("2025-06-02T00:00:00Z"))];
        assert_eq!(slot_owner(&detached, None), Owner::Manual);
        assert_eq!(
            slot_owner(&detached, Some(t("2025-06-01T12:00:00Z"))),
            Owner::Manual,
            "an input edit before the detach does not re-engage"
        );
        assert_eq!(
            slot_owner(&detached, Some(t("2025-06-03T00:00:00Z"))),
            Owner::Tool,
            "an input edit after the detach re-engages the tool"
        );
        let returned = [
            (Kind::Return, t("2025-06-04T00:00:00Z")),
            (Kind::Detach, t("2025-06-02T00:00:00Z")),
        ];
        assert_eq!(slot_owner(&returned, None), Owner::Tool);
    }

    #[test]
    fn the_fold_takes_the_newest_live_decision_per_column_and_the_born_state_otherwise() {
        use super::{FoldEntry, ProjectedColumns, projected_state};
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let entry = |kind, at, new| FoldEntry {
            kind,
            at: t(at),
            new,
        };
        assert_eq!(projected_state(&[]), ProjectedColumns::default());
        assert_eq!(
            projected_state(&[entry(
                Kind::Flag,
                "2025-06-02T00:00:00Z",
                serde_json::json!({ "reason": "spike" })
            )]),
            ProjectedColumns {
                is_flagged: true,
                flag_reason: Some("spike".to_string()),
                ..Default::default()
            }
        );
        // Newest first: the unflag stands and the flag under it is not consulted.
        assert_eq!(
            projected_state(&[
                entry(Kind::Unflag, "2025-06-03T00:00:00Z", serde_json::json!({})),
                entry(
                    Kind::Flag,
                    "2025-06-02T00:00:00Z",
                    serde_json::json!({ "reason": "spike" })
                ),
            ]),
            ProjectedColumns::default()
        );
        // A withdraw with no explicit instant is stamped at the decision.
        assert_eq!(
            projected_state(&[entry(
                Kind::Withdraw,
                "2025-06-04T00:00:00Z",
                serde_json::json!({ "reason": "absent from source window" })
            )]),
            ProjectedColumns {
                withdrawn_at: Some(t("2025-06-04T00:00:00Z")),
                withdrawn_reason: Some("absent from source window".to_string()),
                ..Default::default()
            }
        );
        assert_eq!(
            projected_state(&[
                entry(
                    Kind::Reassert,
                    "2025-06-05T00:00:00Z",
                    serde_json::json!({})
                ),
                entry(
                    Kind::Withdraw,
                    "2025-06-04T00:00:00Z",
                    serde_json::json!({ "reason": "gone" })
                ),
            ]),
            ProjectedColumns::default()
        );
        // A reject withdraws and clears the unverified stamp in one decision.
        assert_eq!(
            projected_state(&[entry(
                Kind::Reject,
                "2025-06-06T00:00:00Z",
                serde_json::json!({ "reason": "rejected" })
            )]),
            ProjectedColumns {
                withdrawn_at: Some(t("2025-06-06T00:00:00Z")),
                withdrawn_reason: Some("rejected".to_string()),
                unverified: false,
                ..Default::default()
            }
        );
        assert_eq!(
            projected_state(&[entry(
                Kind::UnverifiedEntry,
                "2025-06-07T00:00:00Z",
                serde_json::json!({})
            )]),
            ProjectedColumns {
                unverified: true,
                ..Default::default()
            }
        );
        // Kinds that project nothing folded leave every column at the born state.
        for k in [
            Kind::Curve,
            Kind::InstrumentPin,
            Kind::CalibrationPin,
            Kind::ValueCorrection,
            Kind::SlotMove,
            Kind::Chain,
            Kind::Detach,
            Kind::Return,
        ] {
            assert_eq!(
                projected_state(&[entry(k, "2025-06-08T00:00:00Z", serde_json::json!({}))]),
                ProjectedColumns::default(),
                "{k:?}"
            );
        }
    }

    #[test]
    fn a_rollback_asserts_the_columns_it_restores_and_the_decision_it_undid_is_not_folded() {
        use super::{FoldEntry, ProjectedColumns, projected_state};
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        // The rolled-back decision is not in the list: `rolled_back_by` takes it out of the fold.
        let rollback = FoldEntry {
            kind: Kind::Rollback,
            at: t("2025-06-09T00:00:00Z"),
            new: serde_json::json!({
                "columns": { "is_flagged": false, "flag_reason": null },
                "of": "00000000-0000-0000-0000-000000000000"
            }),
        };
        assert_eq!(
            projected_state(std::slice::from_ref(&rollback)),
            ProjectedColumns::default()
        );
        // It asserts only the columns it names, so an unrelated live decision still stands.
        let withdrawn = FoldEntry {
            kind: Kind::Withdraw,
            at: t("2025-06-08T00:00:00Z"),
            new: serde_json::json!({ "reason": "gone" }),
        };
        assert_eq!(
            projected_state(&[rollback, withdrawn]),
            ProjectedColumns {
                withdrawn_at: Some(t("2025-06-08T00:00:00Z")),
                withdrawn_reason: Some("gone".to_string()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn the_drift_statement_folds_every_owned_column_and_repairs_none() {
        let sql = super::inconsistent_rows_sql();
        for col in super::FOLDED_COLUMNS {
            assert!(sql.contains(&format!("'{col}'")), "{col} is not folded");
        }
        assert!(sql.contains("d.rolled_back_by IS NULL"));
        assert!(sql.contains("d.replicate_index IS NULL OR d.replicate_index = c.replicate_index"));
        // A row with no decision at all is still a candidate when a column left the born state.
        assert!(sql.contains("r.is_flagged IS TRUE OR r.flag_reason IS NOT NULL"));
        for write in ["UPDATE ", "DELETE ", "INSERT "] {
            assert!(
                !sql.contains(write),
                "the sweep reports, it does not repair"
            );
        }
    }

    #[test]
    fn only_an_intern_enters_a_pending_measurement() {
        use super::entry_state;
        use crate::common::authz::Role;
        assert_eq!(
            entry_state(Some(&Role::Intern)),
            Some(Kind::UnverifiedEntry)
        );
        for role in [
            Role::River,
            Role::Manager,
            Role::Administrator,
            Role::Unknown("offline_access".to_string()),
        ] {
            assert_eq!(entry_state(Some(&role)), None, "{role:?}");
        }
        // An API token has bits, not a level: it enters verified, as it always did.
        assert_eq!(entry_state(None), None);
    }

    #[test]
    fn sync_owns_the_measurement_and_never_a_judgement() {
        use super::{is_judgement, judgements_on};
        for k in [
            Kind::Flag,
            Kind::Curve,
            Kind::CalibrationPin,
            Kind::InstrumentPin,
            Kind::UnverifiedEntry,
            Kind::Verify,
            Kind::Reject,
        ] {
            assert!(is_judgement(k), "{k:?} is a person's ruling");
        }
        // The measurement, and the record's own bookkeeping, are not judgements: a re-send
        // corrects a value and retracts a row it no longer asserts without anyone ruling again.
        for k in [
            Kind::Withdraw,
            Kind::Reassert,
            Kind::ValueCorrection,
            Kind::Unflag,
            Kind::SlotMove,
            Kind::Chain,
            Kind::Detach,
            Kind::Return,
            Kind::Rollback,
        ] {
            assert!(!is_judgement(k), "{k:?} is not a ruling sync must respect");
        }
        assert!(judgements_on(&[]).is_empty(), "an untouched row is free");
        assert!(
            judgements_on(&[Kind::Withdraw, Kind::ValueCorrection]).is_empty(),
            "a row sync has only corrected is still free"
        );
        assert_eq!(
            judgements_on(&[Kind::ValueCorrection, Kind::Flag, Kind::Curve]),
            vec![Kind::Flag, Kind::Curve],
            "the rulings are named in the order they stand"
        );
    }

    #[test]
    fn the_judgement_statement_names_every_judged_kind_and_no_other() {
        let sql = super::live_judgements_sql("r");
        for k in super::ALL_KINDS {
            let named = sql.contains(&format!("'{}'", k.as_str()));
            assert_eq!(named, super::is_judgement(k), "{k:?}");
        }
        assert!(sql.contains("d.rolled_back_by IS NULL"));
        assert!(sql.contains("d.replicate_index IS NULL OR d.replicate_index = r.replicate_index"));
    }

    #[test]
    fn a_value_correction_is_per_row_only() {
        assert!(Kind::ValueCorrection.per_row_only());
        assert!(!Kind::Flag.per_row_only());
        assert!(!Kind::Withdraw.per_row_only());
    }
}
