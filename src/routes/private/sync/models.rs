//! The five sync entities, the control-plane wire shapes, and the operator surface's request
//! and response types.

use chrono::{DateTime, Utc};
use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::paging::Window;
use crate::error::{AppError, AppResult};

/// One replicate group's expectation, as the portal stored it. Declared in `river-data-core`.
pub use river_data_core::models::GroupAudit;

/// The row a sync service's event create wrote.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreatedSyncEventResponse {
    pub id: String,
    pub service_id: Uuid,
    pub status: String,
}

/// What an update to a sync service's own row answers with.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UpdatedResponse {
    /// `true`, always: the row is written by the time the response is written.
    pub updated: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateSyncEventRequest {
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    pub event_type: Option<String>,
    pub status: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateSyncEventRequest {
    pub status: Option<String>,
    pub readings_synced: Option<i64>,
    /// Connectors on river-data-core 0.5.0 do not send this; absent leaves the stored count alone.
    #[serde(default)]
    pub readings_skipped: Option<i64>,
    pub status_events_synced: Option<i64>,
    /// The messages the pass reported, in order.
    #[schema(value_type = Option<Vec<String>>)]
    pub errors: Option<serde_json::Value>,
    /// The lines the pass logged, in order.
    #[schema(value_type = Option<Vec<String>>)]
    pub log: Option<serde_json::Value>,
    pub duration_ms: Option<i64>,
}

/// The one answer every refused enrollment gets. A probe that walks client ids must not be able
/// to tell an id that exists from one that does not, so the reason stays server-side.
pub const ENROLL_DENIED: &str = "Invalid client credentials";

/// Why an enrollment was refused. Logged, never served.
#[derive(Debug, PartialEq, Eq)]
pub enum EnrollDenial {
    UnknownClient,
    Revoked,
    BadSecret,
}

impl EnrollDenial {
    pub const fn public_message(&self) -> &'static str {
        ENROLL_DENIED
    }

    pub const fn reason(&self) -> &'static str {
        match self {
            Self::UnknownClient => "unknown client_id",
            Self::Revoked => "credential revoked",
            Self::BadSecret => "client_secret does not match",
        }
    }
}

/// How a stored credential's secret was hashed, so a credential that predates argon2 can be
/// upgraded in place on the one occasion the plaintext is in hand.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SecretFormat {
    /// Argon2id PHC string, what `create_credential` mints.
    Argon2,
    /// Unsalted SHA-256 hex, what credentials minted before this stored. Accepted so an enrolled
    /// service keeps working, and rewritten as argon2 the first time it enrolls.
    LegacyDigest,
}

// ============================================================================
// Response Types
// ============================================================================

#[derive(Serialize, utoipa::ToSchema)]
pub struct SyncCommandResponse {
    pub id: Uuid,
    pub service_id: Uuid,
    pub command: String,
    #[schema(value_type = Option<std::collections::HashMap<String, serde_json::Value>>)]
    #[schema(required)]
    pub payload: Option<serde_json::Value>,
    pub status: String,
    /// What the service reported back, shaped by the command it answers.
    #[schema(value_type = Option<std::collections::HashMap<String, serde_json::Value>>)]
    #[schema(required)]
    pub result: Option<serde_json::Value>,
    pub created_at: String,
    pub expires_at: String,
    #[schema(required)]
    pub acknowledged_at: Option<String>,
    #[schema(required)]
    pub completed_at: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct IssueCommandRequest {
    pub command: String,
    #[schema(value_type = Object)]
    pub payload: Option<serde_json::Value>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateCredentialRequest {
    pub service_type: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CreateCredentialResponse {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct PaginationQuery {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_per_page")]
    pub per_page: u64,
}

pub fn default_page() -> u64 {
    1
}

pub fn default_per_page() -> u64 {
    DEFAULT_PER_PAGE
}

/// Page size a caller naming none is served.
pub const DEFAULT_PER_PAGE: u64 = 25;

/// Largest page either listing will serve.
pub const MAX_PER_PAGE: u64 = 100;

impl PaginationQuery {
    /// The window to query with.
    ///
    /// An oversized page is clamped, but a page size of zero is refused: `Paginator::paginate`
    /// asserts a non-zero size, so a silent clamp would turn a caller's mistake into a page they
    /// did not ask for while a panic would take the request down with no answer.
    pub fn resolve(&self) -> AppResult<Window> {
        if self.per_page == 0 {
            return Err(AppError::BadRequest(
                "per_page must be at least 1".to_string(),
            ));
        }
        Ok(Window::from_page(
            Some(self.page),
            Some(self.per_page),
            DEFAULT_PER_PAGE,
            MAX_PER_PAGE,
        ))
    }
}

/// What a revocation answers with.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RevokedResponse {
    /// `true`, always: the row is revoked by the time the response is written.
    pub revoked: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ListHoldsQuery {
    /// One hold, for a link that names it.
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub stream_id: Option<Uuid>,
    /// Comma-separated stream UUIDs.
    #[serde(default)]
    pub stream_ids: Option<String>,
    #[serde(default)]
    pub source_system: Option<String>,
    /// One status, the view `resolved` (every decided or cleared hold), or `any`; defaults to
    /// `pending`, the review queue.
    #[serde(default)]
    pub status: Option<String>,
    /// Only holds whose relative_delta is at or below this, mirroring the bulk-acknowledge
    /// ceiling so the UI can preview exactly what a threshold would acknowledge.
    #[serde(default)]
    pub max_relative_delta: Option<f64>,
    /// Only holds whose mean_relative_delta is at or below this. ANDs with the other ceilings.
    #[serde(default)]
    pub max_mean_relative_delta: Option<f64>,
    /// Only holds whose sd_relative_delta is at or below this. ANDs with the other ceilings.
    #[serde(default)]
    pub max_sd_relative_delta: Option<f64>,
    /// `relative_delta_desc` | `relative_delta_asc` | `created_at_desc` (default). Sorting by
    /// scale is what lets an operator triage the whole backlog largest-first across pages.
    #[serde(default)]
    pub sort: Option<String>,
    /// Restrict to one disagreement signature: `source_sd_matches_n_divisor`, or
    /// `not_source_sd_matches_n_divisor` for the disagreements it does not explain. Only these two
    /// are filterable, because [`SOURCE_SD_MATCHES_N_DIVISOR_SQL`] is the one signature with a SQL
    /// spelling and its complement is exactly as spellable, so both filter and page honestly rather
    /// than dropping rows out of an already-counted page.
    #[serde(default)]
    pub classification: Option<String>,
    /// Comma-separated hold kinds, e.g. `unverified_visit,unverified_entry`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Holds raised against one calculation.
    #[serde(default)]
    pub tool: Option<String>,
    /// Holds at one site, through the stream's pairing or the site a finding names.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Holds on one parameter, reached the same way as `site_id`.
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Holds whose instant is at or after this.
    #[serde(default)]
    pub from: Option<DateTime<Utc>>,
    /// Holds whose instant is before this.
    #[serde(default)]
    pub to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub page: Option<u64>,
    #[serde(default)]
    pub page_size: Option<u64>,
}

/// What a review-queue hold is about. The column is `text` with no CHECK, so the vocabulary lives
/// here and every SQL predicate over `kind` is written from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HoldKind {
    /// A source's own statistics disagree with what its replicates compute to.
    ReplicateStats,
    /// A calculation that should have an output at this slot and instant has none.
    MissingOutput,
    /// An output is older than an input it was computed from.
    StaleOutput,
    /// The chain declined to run a step, and says so itself rather than leaving it to the audit.
    SkippedOutput,
    /// The source changed a value river-data had already stored.
    SourceModified,
    /// A windowed ingest pass tripped the brake.
    BrakeFired,
    /// The device behind a feed is not the one that was there.
    SourceIdentityChanged,
    /// A curve claim arrived on a row that may not carry one, and was dropped.
    CurveClaimStripped,
    /// A value was entered by hand and nobody has verified it yet.
    UnverifiedEntry,
    /// A field day was opened by hand and nobody has ruled on whether it should exist.
    UnverifiedVisit,
}

impl HoldKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReplicateStats => "replicate_stats",
            Self::MissingOutput => "missing_output",
            Self::StaleOutput => "stale_output",
            Self::SkippedOutput => "skipped_output",
            Self::SourceModified => "source_modified",
            Self::BrakeFired => "brake_fired",
            Self::SourceIdentityChanged => "source_identity_changed",
            Self::CurveClaimStripped => "curve_claim_stripped",
            Self::UnverifiedEntry => "unverified_entry",
            Self::UnverifiedVisit => "unverified_visit",
        }
    }

    /// One kind by name, as the column stores it.
    pub fn parse(name: &str) -> AppResult<Self> {
        serde_json::from_value(serde_json::Value::String(name.to_string()))
            .map_err(|_| AppError::BadRequest(format!("unknown hold kind '{name}'")))
    }

    /// A comma-separated list of kind names, as a list filter takes them.
    pub fn parse_list(list: &str) -> AppResult<Vec<Self>> {
        let kinds = list
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(Self::parse)
            .collect::<AppResult<Vec<Self>>>()?;
        if kinds.is_empty() {
            return Err(AppError::BadRequest("kind names no hold kind".to_string()));
        }
        Ok(kinds)
    }

    /// The kinds the event audit and the chain raise, which every reader of calculation findings
    /// filters on together. A kind added here reaches those readers; one named in their SQL by
    /// hand does not.
    pub const EVENT_AUDIT: [Self; 3] =
        [Self::MissingOutput, Self::StaleOutput, Self::SkippedOutput];

    /// The kinds a manager owes an action on, which is what `holds_open` announces.
    pub const OWED: [Self; 6] = [
        Self::UnverifiedEntry,
        Self::UnverifiedVisit,
        Self::MissingOutput,
        Self::StaleOutput,
        Self::BrakeFired,
        Self::SourceIdentityChanged,
    ];

    /// A `kind IN (...)` list for a set of kinds, quoted for SQL.
    #[must_use]
    pub fn sql_list(kinds: &[Self]) -> String {
        let names: Vec<String> = kinds.iter().map(|k| format!("'{}'", k.as_str())).collect();
        format!("({})", names.join(", "))
    }
}

/// Where a hold stands. The table's CHECK is the same list
/// (`migration/src/m20260905_000001_baseline.rs`), so a value added there is added here and every
/// predicate over `status` follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HoldStatus {
    /// Raised and awaiting review.
    Pending,
    /// Raised on a stream nobody has paired yet, so there is no slot to review it against.
    Deferred,
    /// Reviewed, and the stored value stands.
    Acknowledged,
    /// Reviewed, and something was changed in response.
    Remediated,
    /// Overtaken by a later account of the same slot, so there is nothing left to review.
    Superseded,
    /// Legacy, kept for history; nothing produces these.
    UsePortal,
    UseManual,
    Consumed,
}

impl HoldStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Deferred => "deferred",
            Self::Acknowledged => "acknowledged",
            Self::Remediated => "remediated",
            Self::Superseded => "superseded",
            Self::UsePortal => "use_portal",
            Self::UseManual => "use_manual",
            Self::Consumed => "consumed",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Every status, which is what the table's CHECK allows.
    pub const ALL: [Self; 8] = [
        Self::Pending,
        Self::Deferred,
        Self::Acknowledged,
        Self::Remediated,
        Self::Superseded,
        Self::UsePortal,
        Self::UseManual,
        Self::Consumed,
    ];

    /// Still awaiting review. Must match the open-only partial indexes exactly, since the raise
    /// names them as its conflict target.
    pub const OPEN: [Self; 2] = [Self::Pending, Self::Deferred];

    /// Past review. A decision or an outcome, never rewritten by a re-detection.
    pub const RESOLVED: [Self; 6] = [
        Self::Acknowledged,
        Self::Remediated,
        Self::Superseded,
        Self::UsePortal,
        Self::UseManual,
        Self::Consumed,
    ];

    /// The statuses `reopen` takes a hold back from.
    pub const REOPENABLE: [Self; 2] = [Self::Acknowledged, Self::Remediated];

    /// A `status IN (...)` list, quoted for SQL.
    #[must_use]
    pub fn sql_list(statuses: &[Self]) -> String {
        let names: Vec<String> = statuses
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect();
        format!("({})", names.join(", "))
    }
}

/// One stored value with the replicate index it sits at, which is the source's column position and
/// the only handle a flag can name. A hold recorded before the index travelled with the value
/// carries the bare number, and no position in that array stands for an index.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum HoldValue {
    Indexed { index: i16, value: f64 },
    Bare(f64),
}

/// The source's own statistics for the group, as the hold recorded them.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HoldExpected {
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
    /// The replicate count the source declares, where it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub n: Option<i64>,
}

/// What river-data computes over the values it stores, which is what it serves.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HoldComputed {
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
    pub n: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub values: Option<Vec<HoldValue>>,
}

/// Source minus computed, per statistic.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HoldDelta {
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub n: Option<i64>,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct HoldRow {
    pub id: Uuid,
    /// NULL on event-audit findings, which are keyed on (site, parameter, instant) instead.
    #[schema(required)]
    pub stream_id: Option<Uuid>,
    /// `replicate_stats` | `source_modified` | `brake_fired` | `missing_output` | `stale_output`
    /// | `skipped_output`.
    pub kind: String,
    #[schema(required)]
    pub source_system: Option<String>,
    #[schema(required)]
    pub source_key: Option<String>,
    /// The stream's human name as registered by the source.
    #[schema(required)]
    pub source_name: Option<String>,
    /// Site and parameter of the paired slot (or of the event finding itself); NULL while the
    /// stream is unpaired.
    #[schema(required)]
    pub site_id: Option<Uuid>,
    /// The slot the hold is about, which a point record is opened on.
    #[schema(required)]
    pub site_parameter_id: Option<Uuid>,
    #[schema(required)]
    pub site_name: Option<String>,
    #[schema(required)]
    pub parameter_name: Option<String>,
    #[schema(required)]
    pub parameter_code: Option<String>,
    /// The tool an event finding names.
    #[schema(required)]
    pub tool: Option<String>,
    pub paired: bool,
    pub group_time: DateTime<Utc>,
    #[schema(value_type = HoldExpected)]
    pub expected: serde_json::Value,
    #[schema(value_type = HoldComputed)]
    pub computed: serde_json::Value,
    #[schema(value_type = HoldDelta)]
    pub delta: serde_json::Value,
    pub status: String,
    /// Signature of the disagreement: `n_mismatch` | `source_sd_matches_n_divisor` |
    /// `stale_subset` | `unexplained`. Computed from the stored expectation and recompute, never
    /// persisted.
    pub classification: String,
    /// The decision record: latest action plus prior actions under `history`.
    #[schema(value_type = Object)]
    #[schema(required)]
    pub resolution: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    #[schema(required)]
    pub acknowledged_by: Option<String>,
    #[schema(required)]
    pub acknowledged_at: Option<DateTime<Utc>>,
    /// Disagreement size relative to the measurement scale: the greater of
    /// `mean_relative_delta` and `sd_relative_delta`; see [`RELATIVE_DELTA_SQL`].
    pub relative_delta: f64,
    /// `|Δmean| / max(|portal mean|, |computed mean|)`.
    pub mean_relative_delta: f64,
    /// `|Δsd|` over the same mean-magnitude denominator as `mean_relative_delta`.
    pub sd_relative_delta: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListHoldsResponse {
    pub holds: Vec<HoldRow>,
    pub total: u64,
    /// Pending holds within the non-status filters, regardless of the status view requested.
    pub pending: u64,
    /// Deferred holds (unpaired streams) within the same filters.
    pub deferred: u64,
    /// Pending holds per `kind`, so an entry point can say what is waiting rather than calling
    /// every kind a replicate-statistics disagreement.
    pub pending_by_kind: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AcknowledgeResponse {
    pub acknowledged: u64,
    /// Pending holds this call could not reach: a hold with no stream is keyed on its slot rather
    /// than a stream, and the sweep's statement and every filter it takes are about streams. The
    /// count is what keeps `acknowledged` from reading as the whole queue.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_no_stream: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveHoldRequest {
    /// `ours` (accept the recomputed statistics; identical to acknowledge) | `flag` (flag the
    /// named replicates so the sample statistics recompute over the rest) | `verify` / `reject`
    /// (rule on an intern's entry: accept it as it stands, or withdraw it).
    pub mode: String,
    /// The replicate indexes to flag; required for `flag`. Each must be among the values the
    /// hold recorded and unflagged, and at least one unflagged replicate must remain after.
    #[serde(default)]
    pub replicate_indexes: Option<Vec<i16>>,
    /// Recorded as the readings' flag_reason; defaults to a reference to this hold.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What rejecting an intern's entry would withdraw beside it (Q257).
#[derive(Debug, Serialize, ToSchema)]
pub struct RejectPreview {
    pub hold_id: Uuid,
    /// The entered readings the reject decides on.
    pub entries: usize,
    /// The computed values that go with them, in the order the reject withdraws them.
    pub withdrawn: Vec<RejectPreviewOutput>,
}

/// One computed value a reject would withdraw.
#[derive(Debug, Serialize, ToSchema)]
pub struct RejectPreviewOutput {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicate_index: i16,
    #[schema(required)]
    pub value: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResolveHoldResponse {
    /// The status the hold moved to: `acknowledged` | `remediated`.
    pub status: String,
    /// `verify` / `reject`: the readings the ruling decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub samples_affected: Option<i64>,
}

#[derive(Deserialize)]
pub struct CreatePairingPlanRequest {
    pub source_system: String,
}

/// A plan action that runs as a tracked job: the row to watch, and the state it starts in.
#[derive(Serialize, ToSchema)]
pub struct PlanJobQueued {
    /// Absent when the same action was already queued: `enqueue` dedupes rather than raising a
    /// second job for one plan.
    #[schema(required)]
    pub job_id: Option<Uuid>,
    pub status: String,
}

/// A plan whose status this request moved, with no job behind it.
#[derive(Serialize, ToSchema)]
pub struct PlanStatusChanged {
    pub id: Uuid,
    pub status: String,
}

/// One logger a site's streams name, counted per site: a site instrumented with two loggers has
/// two rows, and reporting one of them would name channels belonging to the other.
#[derive(Serialize, ToSchema)]
pub struct PlanSiteDevice {
    pub serial: String,
    #[schema(required)]
    pub model: Option<String>,
    pub streams: i64,
}

/// What the source knows about one site, as the review renders it beside the pairing.
#[derive(Serialize, ToSchema)]
pub struct PlanSiteMetadata {
    pub site_name: String,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    #[schema(required)]
    pub altitude_m: Option<f64>,
    #[schema(required)]
    pub glacier_name: Option<String>,
    #[schema(required)]
    pub glacier_rgi: Option<String>,
    #[schema(required)]
    pub location_type: Option<String>,
    #[schema(required)]
    pub catchment: Option<String>,
    #[schema(required)]
    pub full_name: Option<String>,
    #[schema(required)]
    pub elevation: Option<f64>,
    #[schema(required)]
    pub channel_id: Option<String>,
    #[schema(required)]
    pub sample_interval_sec: Option<i64>,
    pub devices: Vec<PlanSiteDevice>,
}

#[derive(Deserialize)]
pub struct ListPairingPlansQuery {
    #[serde(default)]
    pub source_system: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// A plan without its `entries` document. A CNET draft's entries are 185 kB and a NOMIS draft's
/// 2.5 MB, and the listing is a way back into a review, not a way to read every draft at once.
#[derive(Serialize, ToSchema)]
pub struct PairingPlanSummary {
    pub(super) id: Uuid,
    pub(super) source_system: String,
    pub(super) status: String,
    pub(super) created_by: Option<String>,
    #[schema(value_type = crate::routes::private::sync::service::PlanSummary)]
    pub(super) summary: serde_json::Value,
    pub(super) created_at: chrono::DateTime<chrono::FixedOffset>,
    pub(super) applied_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    /// Streams of this source that are unpaired now and not in the plan, so a draft built while a
    /// sync service was still registering says how much of the source it leaves behind. Counted for
    /// a draft only; a plan that has been applied or superseded is history.
    pub(super) uncovered_streams: Option<i64>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdatePairingPlanRequest {
    /// The version the client read. The write is refused if the plan has moved on since.
    pub expected_version: i32,
    /// One decision over every entry a predicate selects, applied before the per-entry updates.
    #[serde(default)]
    pub bulk: Option<BulkAction>,
    #[serde(default)]
    pub updates: Vec<PlanEntryUpdate>,
    /// Standard curves to assign to instruments this plan will create. The move happens when the
    /// plan is applied, in the transaction that mints the instrument.
    #[serde(default)]
    pub curves: Vec<PlanCurveUpdate>,
    /// Held curves to attach to one of this plan's instruments, created under it when the plan is
    /// applied.
    #[serde(default)]
    pub held_curves: Vec<PlanHeldCurveUpdate>,
    /// Objects the review has accepted or taken back, `{kind}:{name}` as the card names them.
    #[serde(default)]
    pub objects: Vec<PlanObjectUpdate>,
    /// Register rows the review has decided to admit or leave behind, by the source's own key.
    #[serde(default)]
    pub instruments: Vec<PlanProposalUpdate>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanProposalUpdate {
    pub source_key: String,
    pub admit: bool,
    /// The instrument to merge this register row onto instead of creating one. Sent in place of
    /// `admit` where the review answered a conflict by attaching.
    #[serde(default)]
    pub attach_to: Option<uuid::Uuid>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanObjectUpdate {
    pub key: String,
    pub accepted: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkAction {
    #[serde(default)]
    pub r#where: crate::routes::private::sync::service::BulkWhere,
    /// `pair` or `skip`.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub acknowledged: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PlanCurveUpdate {
    pub curve_id: Uuid,
    /// The `source_key` of an instrument the plan proposes creating. Null clears the assignment,
    /// leaving the curve on the instrument it has.
    #[serde(default)]
    pub instrument_source_key: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PlanHeldCurveUpdate {
    pub proposal_id: Uuid,
    /// The `source_key` of an instrument the plan proposes creating.
    #[serde(default)]
    pub instrument_source_key: Option<String>,
    /// An instrument that already exists. Naming neither clears the attachment.
    #[serde(default)]
    pub instrument_id: Option<Uuid>,
    /// Leave the curve behind instead of attaching it: it is never stored and the source stops
    /// sending the readings that name it. `Some(false)` takes the skip back (Q220).
    #[serde(default)]
    pub skip: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PlanEntryUpdate {
    pub stream_id: Uuid,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
    #[serde(default)]
    pub site_name: Option<String>,
    /// The coordinates and elevation the apply will create this site with. A site is created once,
    /// so a value the source recorded wrong (an elevation of 1 m) is corrected here rather than on
    /// the site afterwards. Ignored where the entry resolves to a site that already exists: that
    /// site's own page owns its attributes. Applied to every entry naming the same site, so the
    /// twenty-three feeds at one station do not disagree about where it is.
    #[serde(default)]
    pub site_latitude: Option<f64>,
    #[serde(default)]
    pub site_longitude: Option<f64>,
    #[serde(default)]
    pub site_altitude_m: Option<f64>,
    #[serde(default)]
    pub parameter_name: Option<String>,
    /// Which catalog parameter this column is, decided rather than matched: `{"choice":"existing",
    /// "id":...}` attaches to that row, `{"choice":"new"}` creates one under the entry's code. A
    /// `parameter_name` sent without it records `new`, so typing a code that happens to exist does
    /// not attach to it (Q221).
    #[serde(default)]
    pub parameter_attach: Option<crate::routes::private::sync::service::PlanParamAttach>,
    #[serde(default)]
    pub parameter_units: Option<String>,
    /// Human display label for the parameter. Takes effect only when apply creates the
    /// parameter (`create: true`); a matched existing parameter keeps its own name.
    #[serde(default)]
    pub parameter_label: Option<String>,
    /// Point this entry's curve references at an existing lab instrument instead of the resolved
    /// one. Applies to every entry sharing the same curve column, since one column is one
    /// instrument across the source.
    #[serde(default)]
    pub instrument_id: Option<Uuid>,
    /// Rename an instrument the plan will create. Ignored once it resolves to an existing one.
    #[serde(default)]
    pub instrument_name: Option<String>,
    /// Agree to creating the proposed instrument. Apply refuses while any remain unconfirmed.
    #[serde(default)]
    pub instrument_confirmed: Option<bool>,
    /// Detach the instrument from every entry this one groups with. Attaching is `instrument_id`;
    /// this is its inverse, since an absent `instrument_id` means "unchanged", not "none".
    #[serde(default)]
    pub instrument_clear: Option<bool>,
    /// Record that a person looked at this entry and agreed with it, or take that back. Separate
    /// from `action`, so changing what an entry does is not the same as deciding it.
    #[serde(default)]
    pub acknowledged: Option<bool>,
    /// Rename the parameter group the source's registry proposes for this column. The code is
    /// re-slugged from the new label, so a rename onto an existing group's label joins that group
    /// instead of creating one. Applied to every entry the same category placed, since a category
    /// is one decision behind every column it holds.
    #[serde(default)]
    pub group_label: Option<String>,
    /// Describe the proposed group. Applied to the same set as `group_label`; an empty string
    /// clears the source's own description.
    #[serde(default)]
    pub group_description: Option<String>,
}

#[derive(Deserialize)]
pub struct ApplyPairingPlanRequest {
    /// The version the client read. Applying a draft someone else has edited since is refused.
    pub expected_version: i32,
}

/// One instrument decision in a pairing plan: the instrument, what it covers, and the curves it
/// owns. Only instruments the plan actually binds are listed; the rest of the inventory is
/// reachable through the picker, so this stays a list of decisions rather than a catalog.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlanInstrumentGroup {
    /// The decision's scope: `column:<curve column>` or `parameter:<source parameter>`, matching
    /// what an update to any member stream settles. Absent for an unbound instrument.
    #[schema(required)]
    pub scope: Option<String>,
    #[schema(required)]
    pub instrument_id: Option<Uuid>,
    pub name: String,
    pub source_key: String,
    /// `stream` | `curve_label` (suggested from the label) | `manual` | `ambiguous_label` (the
    /// label matched more than one, so nothing is suggested) | `placeholder`.
    pub resolved_by: String,
    pub create: bool,
    pub confirmed: bool,
    /// Whether readings under this decision will store a `standard_curve_id`.
    pub stamps_readings: bool,
    #[schema(required)]
    pub curve_column: Option<String>,
    pub stream_count: usize,
    pub parameters: Vec<String>,
    pub site_count: usize,
    /// A stream to address an update to; every entry in the same scope moves with it.
    #[schema(required)]
    pub anchor_stream_id: Option<Uuid>,
    pub curves: Vec<crate::routes::private::sync::service::PlanCurveRef>,
    /// What this decision proposed creating, kept through an attach so the picker can offer it
    /// back. Absent for an instrument that was never a proposal.
    #[schema(required)]
    pub proposed_name: Option<String>,
    /// An instrument already carrying the proposed name, when the proposal collides with one.
    /// The row is then a choice (attach, or create a second) rather than a suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name_conflict: Option<crate::routes::private::sync::service::InstrumentNameConflict>,
    /// The instruments the curve label matched when it matched more than one. The row is then a
    /// choice between them rather than a suggestion.
    #[serde(default)]
    #[schema(required)]
    pub label_candidates: Vec<crate::routes::private::sync::service::PlanLabelCandidate>,
}

/// The source parameters this plan pairs that no instrument covers, so an operator can attach one
/// where the portal corrected a value upstream without naming a curve per reading.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlanUnassignedParameter {
    pub scope: String,
    pub parameter: String,
    pub stream_count: usize,
    pub site_count: usize,
    pub anchor_stream_id: Uuid,
    /// The name an instrument for this parameter would get, proposed the way a site's or a
    /// parameter's name is. Accepting it is what creates the instrument; nothing is minted from a
    /// suggestion alone.
    pub suggested_name: String,
    /// An instrument already carrying that name. Accepting the suggestion would create a second
    /// one beside it, so the row asks instead of suggesting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name_conflict: Option<crate::routes::private::sync::service::InstrumentNameConflict>,
}

/// One standard curve the source has replicated, and the instrument it is currently fitted on.
/// Re-homing is a curve-level decision, so the curves are listed in their own right rather than
/// only inside the instrument that happens to own them.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlanCurveAssignment {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    #[schema(required)]
    pub r_squared: Option<f64>,
    #[schema(required)]
    pub source_key: Option<String>,
    pub sensor_id: Uuid,
    pub instrument_name: String,
    /// Readings this curve has already corrected. A curve with history is one whose instrument a
    /// re-home changes the meaning of, so the number is shown beside the choice.
    pub reading_count: i64,
    /// The parameters of the plan's streams whose readings this curve corrects.
    pub corrected_parameters: Vec<String>,
    /// The stations of the plan's streams whose readings this curve corrects.
    pub corrected_sites: Vec<String>,
    /// The earliest reading this curve corrects.
    #[schema(required)]
    pub first_corrected: Option<DateTime<Utc>>,
    /// The latest reading this curve corrects.
    #[schema(required)]
    pub last_corrected: Option<DateTime<Utc>>,
    /// The instrument this plan will move the curve onto when applied, by `source_key`, and the
    /// name the plan proposes for it. Absent when no assignment is pending.
    #[schema(required)]
    pub pending_source_key: Option<String>,
    #[schema(required)]
    pub pending_instrument_name: Option<String>,
}

/// One device-shaped feed a plan carries, and the slot it serves.
///
/// The feed's own `(source_system, source_key)` is the identity, and pairing mints the instrument
/// for its slot and opens that slot's deployment. One instrument serves one (site, parameter), so a
/// multi-channel logger is one group per channel rather than one group carrying them all; the
/// serial it reports is displayed, never matched on. It is listed so an operator can see which
/// instrument each feed will land on, name a new one, and confirm it before the apply mints it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlanDeviceGroup {
    pub site: String,
    pub serial: String,
    #[schema(required)]
    pub model: Option<String>,
    /// The inventory row this serial already resolves to, when it has one.
    #[schema(required)]
    pub instrument_id: Option<Uuid>,
    #[schema(required)]
    pub instrument_name: Option<String>,
    /// The instrument the plan binds this channel to: the inventory row, or the proposal the
    /// pairing will mint once a person confirms it. Absent on a plan drafted without one.
    #[schema(required)]
    pub instrument: Option<PlanInstrumentGroup>,
    pub parameters: Vec<String>,
    pub stream_count: usize,
    pub anchor_stream_id: Uuid,
}

/// A source's curve held until this plan attaches it to one of its instruments (Q195).
#[derive(Debug, Serialize, ToSchema)]
pub struct PlanHeldCurve {
    pub id: Uuid,
    pub source_key: String,
    /// The source's own label, the curve's parameter cell. It names no instrument.
    pub label: String,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    #[schema(required)]
    pub r_squared: Option<f64>,
    #[schema(required)]
    pub fitted_on: Option<chrono::NaiveDate>,
    /// Where the apply will create it, or null while nothing is attached, which blocks the apply.
    #[schema(required)]
    pub attached: Option<PlanHeldCurveTarget>,
    /// The review left this curve behind: it is not stored and the readings naming it are dropped
    /// at the source. A skipped curve no longer blocks the apply.
    pub skipped: bool,
    #[schema(required)]
    pub skipped_by: Option<String>,
}

/// The instrument a held curve is attached to.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlanHeldCurveTarget {
    #[schema(required)]
    pub instrument_source_key: Option<String>,
    #[schema(required)]
    pub instrument_id: Option<Uuid>,
    pub instrument_name: String,
    /// True when the instrument is one this plan creates.
    pub create: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanInstrumentsResponse {
    pub groups: Vec<PlanInstrumentGroup>,
    pub unassigned: Vec<PlanUnassignedParameter>,
    pub devices: Vec<PlanDeviceGroup>,
    pub curves: Vec<PlanCurveAssignment>,
    pub held_curves: Vec<PlanHeldCurve>,
}

/// Commands the API queues for a sync service to collect on its next heartbeat.
pub mod commands {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "sync_commands")]
    #[crudcrate(
        api_struct = "SyncCommand",
        name_singular = "sync_command",
        name_plural = "sync_commands",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub service_id: Uuid,
        #[crudcrate(filterable)]
        pub command: String,
        #[sea_orm(column_type = "JsonBinary", nullable)]
        pub payload: Option<serde_json::Value>,
        #[crudcrate(filterable)]
        pub status: String,
        #[sea_orm(column_type = "JsonBinary", nullable)]
        pub result: Option<serde_json::Value>,
        #[crudcrate(sortable, exclude(create, update))]
        pub created_at: DateTimeWithTimeZone,
        pub expires_at: DateTimeWithTimeZone,
        pub acknowledged_at: Option<DateTimeWithTimeZone>,
        pub completed_at: Option<DateTimeWithTimeZone>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::services::Entity",
            from = "Column::ServiceId",
            to = "super::services::Column::Id"
        )]
        SyncService,
    }

    impl Related<super::services::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncService.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// The client credentials a sync service enrolls with.
pub mod credentials {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "sync_service_credentials")]
    #[crudcrate(
        api_struct = "SyncServiceCredential",
        name_singular = "sync_service_credential",
        name_plural = "sync_service_credentials",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[sea_orm(unique)]
        #[crudcrate(filterable)]
        pub client_id: String,
        /// Never served: the CRUD list and detail are the only readers of this table now, and a
        /// hash is what a leaked one would be replayed from.
        #[crudcrate(exclude(create, update, one, list))]
        pub client_secret_hash: String,
        #[crudcrate(filterable)]
        pub service_type: String,
        #[crudcrate(filterable)]
        pub service_id: Option<Uuid>,
        #[crudcrate(filterable)]
        pub revoked: bool,
        #[crudcrate(sortable, exclude(create, update))]
        pub created_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::services::Entity",
            from = "Column::ServiceId",
            to = "super::services::Column::Id"
        )]
        SyncService,
    }

    impl Related<super::services::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncService.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// A sync cycle's own record: what it fetched, what it wrote, and how it ended.
pub mod events {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "sync_events")]
    #[crudcrate(
        api_struct = "SyncEvent",
        name_singular = "sync_event",
        name_plural = "sync_events",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub service_id: Uuid,
        pub command_id: Option<Uuid>,
        #[crudcrate(filterable)]
        pub event_type: String,
        #[crudcrate(filterable)]
        pub status: String,
        pub readings_synced: i64,
        /// Readings the cycle sent that ingest admission dropped.
        #[crudcrate(on_create = 0)]
        pub readings_skipped: i64,
        pub status_events_synced: i64,
        #[sea_orm(column_type = "JsonBinary", nullable)]
        pub errors: Option<serde_json::Value>,
        #[sea_orm(column_type = "JsonBinary", nullable)]
        pub log: Option<serde_json::Value>,
        #[crudcrate(sortable)]
        pub started_at: DateTimeWithTimeZone,
        #[crudcrate(sortable)]
        pub completed_at: Option<DateTimeWithTimeZone>,
        pub duration_ms: Option<i64>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::services::Entity",
            from = "Column::ServiceId",
            to = "super::services::Column::Id"
        )]
        SyncService,
        #[sea_orm(
            belongs_to = "super::commands::Entity",
            from = "Column::CommandId",
            to = "super::commands::Column::Id"
        )]
        SyncCommand,
    }

    impl Related<super::services::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncService.def()
        }
    }

    impl Related<super::commands::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncCommand.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// A registered sync service instance and the cadence it runs on.
pub mod services {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    use super::super::service::SyncServiceOperations;

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "sync_services")]
    #[crudcrate(
        api_struct = "SyncService",
        name_singular = "sync_service",
        name_plural = "sync_services",
        operations = SyncServiceOperations,
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub service_type: String,
        #[crudcrate(filterable)]
        pub instance_id: String,
        #[crudcrate(filterable)]
        pub status: String,
        #[crudcrate(filterable, exclude(create), on_create = false)]
        pub paused: bool,
        pub current_operation: Option<String>,
        /// Operator-set scheduled sync cadence in seconds. NULL leaves the service on its own
        /// `SYNC_INTERVAL_SECONDS`. `SyncServiceOperations::before_update` enforces the minimum
        /// the runner floors at.
        #[crudcrate(exclude(create))]
        pub sync_interval_secs: Option<i32>,
        /// Whether the weekly `sync_full_reassert` queues a `trigger_full_sync` for this service.
        #[crudcrate(filterable, exclude(create))]
        pub full_reassert_enabled: bool,
        #[crudcrate(sortable, exclude(create, update))]
        pub last_heartbeat: Option<DateTimeWithTimeZone>,
        pub last_sync_completed_at: Option<DateTimeWithTimeZone>,
        /// The first error of the service's most recent cycle that reported one, filled by
        /// `SyncServiceOperations` from `sync_events`. The column itself has never been written: a
        /// service has no field to report an error through, so the row cannot carry one.
        #[crudcrate(exclude(create, update))]
        pub last_error: Option<String>,
        #[crudcrate(sortable, exclude(create, update))]
        pub created_at: DateTimeWithTimeZone,
        #[crudcrate(sortable, exclude(create, update))]
        pub updated_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(has_many = "super::commands::Entity")]
        SyncCommands,
        #[sea_orm(has_many = "super::events::Entity")]
        SyncEvents,
        #[sea_orm(has_many = "super::credentials::Entity")]
        SyncServiceCredentials,
        #[sea_orm(has_many = "super::tokens::Entity")]
        SyncServiceTokens,
    }

    impl Related<super::commands::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncCommands.def()
        }
    }

    impl Related<super::events::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncEvents.def()
        }
    }

    impl Related<super::credentials::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncServiceCredentials.def()
        }
    }

    impl Related<super::tokens::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncServiceTokens.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// The short-lived session token enrollment hands back.
pub mod tokens {
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "sync_service_tokens")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub service_id: Uuid,
        pub token_hash: String,
        pub expires_at: DateTimeWithTimeZone,
        pub created_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::services::Entity",
            from = "Column::ServiceId",
            to = "super::services::Column::Id"
        )]
        SyncService,
    }

    impl Related<super::services::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::SyncService.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
#[path = "tests/hold_kind.rs"]
mod hold_kind_tests;
