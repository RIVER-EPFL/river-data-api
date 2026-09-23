use super::service::*;
use std::collections::HashMap;

use chrono::DateTime;
use chrono::Utc;
use sea_orm::Condition;
use sea_orm::EntityTrait;
use sea_orm::ExprTrait;
use sea_orm::FromQueryResult;
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::Alias;
use serde::Deserialize;
use serde::Serialize;
use utoipa::IntoParams;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::AppError;
use crate::error::AppResult;

pub use river_data_core::models::IngestReading;
pub use river_data_core::models::IngestStatusEvent;
pub use river_data_core::models::SourceWindow;

/// A stored reading.
///
/// The key is the triple, so no route addresses one row by an id. Only the read routes are
/// generated: a value arrives through ingest, batch, grab entry or CSV import, each of which
/// resolves its attribution from the pairing and builds its provenance server-side, and it changes
/// only through the curation routes, which append to `reading_decisions` first. Nothing deletes.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, crudcrate::EntityToModels,
)]
#[sea_orm(table_name = "readings")]
#[crudcrate(
    api_struct = "Reading",
    name_singular = "reading",
    name_plural = "readings",
    generate_router,
    routes(read),
    operations = super::service::ReadingOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update), filterable)]
    pub stream_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update), filterable, sortable)]
    pub time: DateTimeWithTimeZone,
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update), filterable)]
    pub replicate_index: i16,
    #[crudcrate(filterable)]
    pub site_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub parameter_id: Option<Uuid>,
    #[crudcrate(sortable)]
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    #[crudcrate(filterable)]
    pub sensor_id: Option<Uuid>,
    /// The time-windowed base calibration the value was corrected with.
    #[crudcrate(filterable)]
    pub calibration_id: Option<Uuid>,
    /// The hand-picked lab curve applied on top of the base calibration, for grab measurements.
    #[crudcrate(filterable)]
    pub standard_curve_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub logged: Option<bool>,
    #[crudcrate(filterable)]
    pub measurement_type: Option<String>,
    #[crudcrate(filterable)]
    pub is_flagged: Option<bool>,
    pub flag_reason: Option<String>,
    #[crudcrate(filterable)]
    pub sample_id: Option<Uuid>,
    /// The collection event (site visit) an attributed spot reading belongs to. Stamped by
    /// `collection_events::attach` after the write; NULL on continuous and derived rows.
    #[crudcrate(filterable)]
    pub collection_event_id: Option<Uuid>,
    /// Retraction stamp: the source's claimed window no longer contains this reading. A withdrawn
    /// reading is excluded from serving, statistics and alarms, and a later honest window that
    /// re-asserts the row clears the stamp. Spot rows only (DB CHECK); never a delete.
    pub withdrawn_at: Option<DateTimeWithTimeZone>,
    pub withdrawn_reason: Option<String>,
    /// When the stored value arrived (DB default on insert, re-stamped when an overwrite changes
    /// the value). NULL on rows that predate tracking.
    #[crudcrate(sortable)]
    pub ingested_at: Option<DateTimeWithTimeZone>,
    /// Where this value came from: `tool_run` | `chain` | `csv_import` | `manual` | `batch` |
    /// `sync` | `derived`. Total, held by a DB trigger for a writer that names none,
    /// so an unrecorded origin is a named kind rather than a NULL blob (Q49).
    #[crudcrate(filterable)]
    pub provenance_kind: Option<String>,
    /// The server-built record of what produced this value: the tool run and pinned script version,
    /// its resolved inputs, constants and curves, and the outputs it returned. Written by the grab
    /// write path from the stored `tool_runs` row, never by a client.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub provenance: Option<serde_json::Value>,
    /// The operator's name for the measurement.
    pub label: Option<String>,
    /// The operator's free text about the measurement.
    #[sea_orm(column_type = "Text", nullable)]
    pub notes: Option<String>,
    /// Who entered it, on a hand-entered measurement.
    pub created_by: Option<String>,
    /// An intern's entry that no manager has verified. Curated surfaces leave it out
    /// (`common/served.rs`); only `reading_decisions` moves it, through its projection trigger.
    #[crudcrate(filterable)]
    pub unverified: bool,
    /// The formula version a derived value was computed under, on a derived row.
    pub derived_version_id: Option<Uuid>,
    /// The names behind the ids above, filled on read so a list reads without a lookup per row.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub source_system: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub source_key: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub site_name: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub parameter_code: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub units: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub instrument_name: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub calibration: Option<ReadingCalibrationRef>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub curve: Option<ReadingCurveRef>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::data_streams::Entity",
        from = "Column::StreamId",
        to = "crate::routes::private::data_streams::Column::Id"
    )]
    DataStream,
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensor_calibrations::Entity",
        from = "Column::CalibrationId",
        to = "crate::routes::private::sensor_calibrations::Column::Id"
    )]
    SensorCalibration,
    #[sea_orm(
        belongs_to = "crate::routes::private::standard_curves::Entity",
        from = "Column::StandardCurveId",
        to = "crate::routes::private::standard_curves::Column::Id"
    )]
    StandardCurve,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensor_deployments::Entity",
        from = "Column::DeploymentId",
        to = "crate::routes::private::sensor_deployments::Column::Id"
    )]
    SensorDeployment,
    #[sea_orm(
        belongs_to = "crate::routes::private::readings::samples::Entity",
        from = "Column::SampleId",
        to = "crate::routes::private::readings::samples::Column::Id",
        on_delete = "SetNull"
    )]
    Sample,
    #[sea_orm(
        belongs_to = "crate::routes::private::collection_events::Entity",
        from = "Column::CollectionEventId",
        to = "crate::routes::private::collection_events::Column::Id",
        on_delete = "SetNull"
    )]
    CollectionEvent,
}

/// A reading's key and value with every other column at what a fresh write means: no curation, no
/// event, no origin blob, no formula version, and `ingested_at` left `NotSet` so the arrival stamp
/// is the database's. Each write path then sets the columns it actually resolves.
#[must_use]
pub fn new(
    stream_id: Uuid,
    time: DateTimeWithTimeZone,
    replicate_index: i16,
    raw_value: f64,
) -> ActiveModel {
    use sea_orm::ActiveValue::{NotSet, Set};
    ActiveModel {
        stream_id: Set(stream_id),
        time: Set(time),
        replicate_index: Set(replicate_index),
        raw_value: Set(raw_value),
        site_id: Set(None),
        parameter_id: Set(None),
        calibrated_value: Set(None),
        sensor_id: Set(None),
        calibration_id: Set(None),
        standard_curve_id: Set(None),
        deployment_id: Set(None),
        logged: Set(Some(true)),
        measurement_type: Set(None),
        is_flagged: Set(Some(false)),
        flag_reason: Set(None),
        sample_id: Set(None),
        collection_event_id: Set(None),
        withdrawn_at: Set(None),
        withdrawn_reason: Set(None),
        ingested_at: NotSet,
        provenance_kind: Set(None),
        provenance: Set(None),
        label: Set(None),
        notes: Set(None),
        created_by: Set(None),
        unverified: Set(false),
        derived_version_id: Set(None),
    }
}

#[cfg(test)]
#[path = "tests/model.rs"]
pub(super) mod tests;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SamplePreviewRequest {
    /// Key the group by its stream, or by site and parameter; the instant is required either way.
    #[serde(default)]
    pub stream_id: Option<Uuid>,
    #[serde(default)]
    pub site_id: Option<Uuid>,
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    pub time: DateTime<Utc>,
    /// Replicates to leave out, as a flag would.
    #[serde(default)]
    pub exclude_replicate_indexes: Vec<i16>,
    /// Flagged replicates to bring back, as an unflag would.
    #[serde(default)]
    pub include_replicate_indexes: Vec<i16>,
    /// A replicate audit hold on this group; the response says whether the proposed statistics
    /// meet its recorded expectation under the audit tolerances.
    #[serde(default)]
    pub hold_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct PreviewStats {
    pub n: usize,
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct PreviewDelta {
    pub n: i64,
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewReplicate {
    pub index: i16,
    pub value: f64,
    pub flagged: bool,
    pub withdrawn: bool,
    /// Whether the value counts in the statistics served now.
    pub included_now: bool,
    /// Whether it would count after the change.
    pub included_after: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HoldMatch {
    pub hold_id: Uuid,
    #[schema(required)]
    pub expected_mean: Option<f64>,
    #[schema(required)]
    pub expected_sd: Option<f64>,
    #[schema(required)]
    pub expected_n: Option<i64>,
    /// Whether the current statistics meet the expectation (they do not, or there is no hold).
    pub meets_now: bool,
    /// Whether the proposed statistics meet it under the audit tolerances.
    pub meets_after: bool,
    pub mean_agrees: bool,
    pub sd_agrees: bool,
    pub n_agrees: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SamplePreviewResponse {
    pub current: PreviewStats,
    pub proposed: PreviewStats,
    pub delta: PreviewDelta,
    pub replicates: Vec<PreviewReplicate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub hold: Option<HoldMatch>,
}

/// One replicate as the preview_statistics query returns it: the slot it belongs to, and the four columns a
/// [`Replicate`] is made of.
#[derive(sea_orm::FromQueryResult)]
pub(super) struct PreviewRow {
    pub(super) site_id: Option<Uuid>,
    pub(super) parameter_id: Option<Uuid>,
    pub(super) replicate_index: i16,
    pub(super) value: f64,
    pub(super) flagged: bool,
    pub(super) withdrawn: bool,
    pub(super) unverified: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeasonalCheckRequest {
    pub site_id: Uuid,
    /// The entry instant; its month anchors the ±2-month seasonal window.
    pub time: chrono::DateTime<chrono::Utc>,
    pub values: Vec<SeasonalCheckValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeasonalCheckValue {
    pub parameter_id: Uuid,
    pub value: f64,
}

/// Where an entered value sits against the seasonal distribution. Only `normal` carries no
/// warning; everything else is advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeasonalClass {
    /// No history to compare against.
    NoHistory,
    BelowMin,
    BelowQ10,
    Normal,
    AboveQ90,
    AboveMax,
}

impl SeasonalClass {
    /// Every class, in the order the method object lists them.
    pub const ALL: [SeasonalClass; 6] = [
        SeasonalClass::NoHistory,
        SeasonalClass::BelowMin,
        SeasonalClass::BelowQ10,
        SeasonalClass::Normal,
        SeasonalClass::AboveQ90,
        SeasonalClass::AboveMax,
    ];

    /// Whether the class is reported as a warning. No history is not a warning: there is
    /// nothing to disagree with.
    #[must_use]
    pub fn is_warning(self) -> bool {
        !matches!(self, SeasonalClass::Normal | SeasonalClass::NoHistory)
    }

    pub(super) fn meaning(self) -> &'static str {
        match self {
            SeasonalClass::NoHistory => "no pooled history for this parameter in the window",
            SeasonalClass::BelowMin => "below the lowest pooled value",
            SeasonalClass::BelowQ10 => "at or above the minimum but below the 10th percentile",
            SeasonalClass::Normal => "between the 10th and 90th percentiles, inclusive",
            SeasonalClass::AboveQ90 => "above the 90th percentile but at or below the maximum",
            SeasonalClass::AboveMax => "above the highest pooled value",
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonalFinding {
    pub parameter_id: Uuid,
    pub value: f64,
    pub class: SeasonalClass,
    pub warning: bool,
    /// Pooled historical values in the seasonal window (unflagged spot replicates, all years).
    pub n: i64,
    #[schema(required)]
    pub min: Option<f64>,
    #[schema(required)]
    pub q10: Option<f64>,
    #[schema(required)]
    pub q90: Option<f64>,
    #[schema(required)]
    pub max: Option<f64>,
    /// A capped sample of the pooled values, for the distribution plot.
    pub distribution: Vec<f64>,
}

/// One classification label and what it means, for the method description.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeasonalClassDescription {
    pub class: SeasonalClass,
    pub meaning: &'static str,
    pub warning: bool,
}

/// What the check computed, in the terms the query uses. Rendered by the UI as the explanation
/// of the check; never authored client-side.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeasonalMethod {
    /// Half-width of the seasonal window in months.
    pub window_months: i32,
    /// Which rows are pooled.
    pub window: String,
    /// Which rows are excluded and how replicates enter.
    pub pooled: &'static str,
    /// Which stored column is compared, and against what.
    pub value: &'static str,
    /// The statistics computed over the pooled values.
    pub statistics: &'static str,
    /// The classification, extremes first.
    pub classes: Vec<SeasonalClassDescription>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonalCheckResponse {
    /// Reference for the save: `/grab_samples` validates its readings against this check's
    /// stored entries when the request names it.
    pub check_id: Uuid,
    pub findings: Vec<SeasonalFinding>,
    pub warnings: usize,
    pub method: SeasonalMethod,
}

/// One pooled value from the seasonal window, which is the whole row the distribution reads.
#[derive(FromQueryResult)]
pub(super) struct PooledValue {
    pub(super) v: f64,
}

/// The pooled distribution of one slot for one entry instant.
#[derive(Debug, Clone, Copy, FromQueryResult)]
pub struct SeasonalStats {
    pub n: i64,
    pub min: Option<f64>,
    pub q10: Option<f64>,
    pub q90: Option<f64>,
    pub max: Option<f64>,
}

impl SeasonalStats {
    #[must_use]
    pub fn classify(&self, value: f64) -> SeasonalClass {
        classify(value, self.min, self.q10, self.q90, self.max)
    }
}

/// One screened cell of a wide file: which row and column it came from, and where it sits.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScreenedCell {
    /// 1-based line number in the file (the header is line 1).
    pub row: usize,
    pub parameter_id: Uuid,
    pub value: f64,
    pub class: SeasonalClass,
    pub warning: bool,
    pub n: i64,
    #[schema(required)]
    pub min: Option<f64>,
    #[schema(required)]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Flag,
    Unflag,
    Withdraw,
    Reassert,
    Curve,
    /// Historical only (Q117): attribution is corrected on the deployment and the calibration
    /// window, never stamped on a row, so nothing writes these any more. They stay in the
    /// vocabulary because rows already carry them, and the reprocess still honours what they
    /// pinned.
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
    /// A curve that corrected readings was taken out of circulation, and they moved onto
    /// whatever else covers them (M146).
    CurveRetire,
    /// A recompute moved a stored derived value onto a new formula version (Q116, M135). Record
    /// only: the recompute writes the value, this says which version it came from and went to.
    FormulaTransition,
    /// The janitor's drift sweep recomposed a corrected value from the curves the row names
    /// (Q118, M159). Record only, for the same reason: the sweep's own UPDATE writes the value.
    CurveRecompose,
    /// A derived value was computed where none was stored (Q57 arm 2, M162). Record only: the
    /// upsert writes the value, this says the slot's first number arrived and under which
    /// formula version.
    DerivedComputed,
    /// A reprocess re-derived a reading's attribution or its corrected value from the deployment
    /// and calibration timelines (Q118, Q125, M160). Record only, and only where the run moved
    /// something: the bulk statements write the columns, this says what they were before.
    Reprocess,
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
            Self::CurveRetire => "curve_retire",
            Self::FormulaTransition => "formula_transition",
            Self::CurveRecompose => "curve_recompose",
            Self::DerivedComputed => "derived_computed",
            Self::Reprocess => "reprocess",
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
            Self::CurveRetire,
            Self::FormulaTransition,
            Self::CurveRecompose,
            Self::DerivedComputed,
            Self::Reprocess,
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
    /// rather than recorded and restored (`sensor_calibrations::service::recompose_from_own_curves`).
    /// `ingested_at` is not among them either: it is the row's first arrival and nothing moves it,
    /// so the arrival of the value a row currently serves is the latest correction's own `at`.
    #[must_use]
    pub fn projected_columns(self) -> &'static [&'static str] {
        match self {
            Self::Flag | Self::Unflag => &["is_flagged", "flag_reason"],
            Self::Withdraw | Self::Reassert => &["withdrawn_at", "withdrawn_reason"],
            Self::Reject => &["withdrawn_at", "withdrawn_reason", "unverified"],
            Self::Curve => &["standard_curve_id"],
            Self::CalibrationPin | Self::CurveRetire => &["calibration_id"],
            Self::InstrumentPin => &["sensor_id"],
            Self::ValueCorrection => &["raw_value"],
            Self::UnverifiedEntry | Self::Verify => &["unverified"],
            Self::SlotMove
            | Self::Chain
            | Self::Detach
            | Self::Return
            | Self::FormulaTransition
            | Self::CurveRecompose
            | Self::DerivedComputed
            | Self::Reprocess
            | Self::Rollback => &[],
        }
    }

    /// Whether a writer may still record this kind. Attribution pins are historical (Q117): the
    /// correction belongs on the deployment or the calibration window, and every reprocess carries
    /// it through, so a row is never stamped with one again.
    #[must_use]
    pub fn writable(self) -> bool {
        !matches!(self, Self::CalibrationPin | Self::InstrumentPin)
    }

    /// Whether [`rollback`] accepts a decision of this kind: it restores the columns the decision
    /// recorded, so a kind that projects none has nothing to restore. A rollback is not itself
    /// rolled back; the decision is recorded again instead.
    #[must_use]
    pub fn reversible(self) -> bool {
        self != Self::Rollback && !self.projected_columns().is_empty()
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
            // Its own family: a retirement must not supersede a pin, which is honoured against
            // every later reprocess, and a repoint is a derivation a reprocess is meant to redo.
            Self::CurveRetire => Some("retire"),
            Self::SlotMove => Some("slot"),
            Self::ValueCorrection => Some("value"),
            Self::UnverifiedEntry | Self::Verify => Some("verified"),
            Self::Chain | Self::Detach | Self::Return => Some("ownership"),
            // Its own family: a transition supersedes neither a pin nor a value correction, and
            // one edit's move must not hide the move before it.
            Self::FormulaTransition => Some("formula"),
            // Its own family: a sweep repairs a value the curves already decided, so it neither
            // supersedes a correction nor hides the repair before it.
            Self::CurveRecompose => Some("recompose"),
            // Its own family: a value's arrival is not superseded by anything, and a slot that
            // was unattributed and computed again is a second arrival, not a replacement.
            Self::DerivedComputed => Some("derived_arrival"),
            // Its own family: a re-derivation replaces neither a curation decision nor the
            // re-derivation before it, each of which moved the row from a different state.
            Self::Reprocess => Some("reprocess"),
            Self::Rollback => None,
        }
    }

    /// The columns a decision records as `old`: the projected ones, plus, for an ownership
    /// decision, the run that produced the value it supersedes.
    #[must_use]
    pub fn recorded_columns(self) -> &'static [&'static str] {
        match self {
            Self::Chain | Self::Detach | Self::Return => &["raw_value", "run_id"],
            Self::FormulaTransition => &["raw_value", "derived_version_id"],
            Self::CurveRecompose => &["calibrated_value"],
            Self::DerivedComputed => &["raw_value", "derived_version_id"],
            Self::SlotMove => &["site_id", "parameter_id"],
            Self::Reprocess => &[
                "site_id",
                "sensor_id",
                "deployment_id",
                "calibration_id",
                "calibrated_value",
            ],
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
                | Self::SlotMove
                | Self::ValueCorrection
                | Self::CurveRetire
                | Self::Rollback
        )
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
    System,
    Janitor,
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
            Self::System => "system",
            Self::Janitor => "janitor",
        }
    }
}

/// The rows this file's raw queries return that carry more than one column. Derived rather than
/// hand-decoded so a column added to a query and not to its reader is a compile error rather than
/// a field silently left behind.
/// One `(site, parameter)` slot a selection covers.
#[derive(FromQueryResult)]
pub(super) struct SlotKeyRow {
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
}

/// A stored decision set, as the rollback path reads it back.
/// One ownership decision at a slot, newest first.
#[derive(FromQueryResult)]
pub(super) struct OwnershipRow {
    pub(super) kind: String,
    pub(super) at: sea_orm::prelude::DateTimeWithTimeZone,
}

#[derive(FromQueryResult)]
pub(super) struct ReplicateKeyRow {
    pub(super) stream_id: Uuid,
    pub(super) replicate_index: i16,
}

/// A stored decision.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DecisionRow {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    #[schema(required)]
    pub replicate_index: Option<i16>,
    pub kind: Kind,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub old: serde_json::Value,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub new: serde_json::Value,
    pub actor: String,
    pub at: chrono::DateTime<chrono::Utc>,
    #[schema(required)]
    pub reason: Option<String>,
    pub origin: Origin,
    #[schema(required)]
    pub supersedes: Option<Uuid>,
    #[schema(required)]
    pub rolled_back_by: Option<Uuid>,
    #[schema(required)]
    pub set_id: Option<Uuid>,
    /// The tracked job that made a system change, where one did. Cleared when that job row is
    /// pruned, so an old decision keeps its record and loses only the link to the run.
    #[schema(required)]
    pub job_id: Option<Uuid>,
    /// Whether `rollback` accepts this kind at all. A kind that projects no column has nothing to
    /// restore, so the reader offers no undo for it rather than learning that from a 409.
    pub reversible: bool,
}

/// The row every write of a decision set returns: how many landed and the span they cover.
#[derive(sea_orm::FromQueryResult)]
pub(super) struct RecordedSpan {
    pub(super) rows: i64,
    pub(super) lo: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    pub(super) hi: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

/// One reading key inside an explicit selection.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SelectionKey {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
    /// The corrected value for this key alone. A block of cells corrected together carries a
    /// different number in each, so a value correction over such a selection is one decision with
    /// a value per key rather than one call per cell.
    #[serde(default)]
    pub value: Option<f64>,
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
    /// The readings a calibration corrected, which is what a retirement decides over.
    #[serde(default)]
    pub calibration_id: Option<Uuid>,
    #[serde(default)]
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub keys: Vec<SelectionKey>,
}

impl Selection {
    /// The predicate over `r` this selection names. A selection that names nothing (no stream,
    /// no slot, no keys) is refused: "every reading" is not a selection.
    pub fn condition(&self) -> AppResult<Condition> {
        let col = |c: Column| Expr::col((Alias::new("r"), c));
        let mut rows = Condition::all();
        if let Some(stream_id) = self.stream_id {
            rows = rows.add(col(Column::StreamId).eq(stream_id));
        }
        if let Some(event_id) = self.collection_event_id {
            rows = rows.add(col(Column::CollectionEventId).eq(event_id));
        }
        match (self.site_id, self.parameter_id) {
            (Some(site_id), Some(parameter_id)) => {
                rows = rows
                    .add(col(Column::SiteId).eq(site_id))
                    .add(col(Column::ParameterId).eq(parameter_id));
            }
            (None, None) => {}
            _ => {
                return Err(AppError::BadRequest(
                    "A slot selection names both site_id and parameter_id".to_string(),
                ));
            }
        }
        if let Some(calibration_id) = self.calibration_id {
            rows = rows.add(col(Column::CalibrationId).eq(calibration_id));
        }
        if let Some(from) = self.from {
            rows = rows.add(col(Column::Time).gte(DateTimeWithTimeZone::from(from)));
        }
        if let Some(to) = self.to {
            rows = rows.add(col(Column::Time).lte(DateTimeWithTimeZone::from(to)));
        }
        if !self.keys.is_empty() {
            let mut keys = Condition::any();
            for k in &self.keys {
                let mut one = Condition::all()
                    .add(col(Column::StreamId).eq(k.stream_id))
                    .add(col(Column::Time).eq(DateTimeWithTimeZone::from(k.time)));
                if let Some(index) = k.replicate_index {
                    one = one.add(col(Column::ReplicateIndex).eq(index));
                }
                keys = keys.add(one);
            }
            rows = rows.add(keys);
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
        Ok(rows)
    }
}

/// Who owns an output slot at a visit (Q40, Q47): the calculation, or a person who detached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    Tool,
    Manual,
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

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct DecisionsQuery {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    /// Omit for the group's decisions.
    #[serde(default)]
    pub replicate_index: Option<i16>,
}

/// What a derived value's replay answers: the arithmetic behind the stored number.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReplayResponse {
    /// The formula the computation recorded, as it stood then.
    pub formula: String,
    /// The version the stored value names, where it names one.
    #[schema(required)]
    pub derived_version_id: Option<Uuid>,
    /// Running that formula over the values the computation consumed.
    pub replayed: f64,
    /// What the reading holds now. A replay that disagrees with it means the row moved without
    /// the ledger, which is the one thing this read is for.
    #[schema(required)]
    pub stored: Option<f64>,
    /// The values the replay bound, by variable name.
    #[schema(value_type = std::collections::HashMap<String, f64>)]
    pub variables: serde_json::Value,
    /// When the captured set was recorded.
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct LedgerQuery {
    /// The instant (exact reading timestamp).
    pub time: DateTime<Utc>,
    /// Key form 1: the stream serving the point.
    pub stream_id: Option<Uuid>,
    /// Key form 2: the site half of the slot (with `parameter_id`).
    pub site_id: Option<Uuid>,
    /// Key form 2: the parameter half of the slot (with `site_id`).
    pub parameter_id: Option<Uuid>,
    /// Narrow key form 2 to one cadence ('continuous' matches rows stored as NULL).
    pub measurement_type: Option<String>,
    /// Keep only entries of this severity: `error`, `warning` or `info`.
    pub severity: Option<String>,
    /// How many entries to return, newest first (default 200, max 1000).
    pub limit: Option<u64>,
}

/// One thing that happened, in the one shape every source answers in.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LedgerEntry {
    pub at: DateTime<Utc>,
    /// Which record this came from: `decision`, `ingest`, `hold`, `tool_run`, `job`, `job_log`,
    /// `change` or `alarm`.
    pub source: String,
    /// `error`, `warning` or `info`, derived per source shape at the read.
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub actor: Option<String>,
    /// What happened, in the source's own vocabulary.
    pub what: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub old: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub new: Option<serde_json::Value>,
    /// The row this entry is, so a reader can open it where it lives.
    pub id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LedgerResponse {
    pub time: DateTime<Utc>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    /// Entries newest first. `truncated` says the window held more than `limit`.
    pub entries: Vec<LedgerEntry>,
    pub truncated: bool,
}

#[derive(FromQueryResult)]
pub(super) struct ReceiptRow {
    pub(super) id: Uuid,
    pub(super) at: DateTime<Utc>,
    pub(super) submitted: i32,
    pub(super) new_rows: i32,
    pub(super) changed: i32,
    pub(super) unchanged: i32,
    pub(super) withdrawn: i32,
    pub(super) rejected_total: i32,
    pub(super) braked: bool,
}

#[derive(FromQueryResult)]
pub(super) struct LedgerHoldRow {
    pub(super) id: Uuid,
    pub(super) kind: String,
    pub(super) status: String,
    pub(super) created_at: DateTime<Utc>,
    pub(super) tool: Option<String>,
}

#[derive(FromQueryResult)]
pub(super) struct JobRow {
    pub(super) id: Uuid,
    pub(super) trigger_type: String,
    pub(super) status: String,
    pub(super) error_message: Option<String>,
    pub(super) created_at: DateTime<Utc>,
    pub(super) completed_at: Option<DateTime<Utc>>,
    pub(super) readings_updated: Option<i32>,
}

#[derive(FromQueryResult)]
pub(super) struct ChangeRow {
    pub(super) id: Uuid,
    pub(super) change: String,
    pub(super) old_value: Option<serde_json::Value>,
    pub(super) new_value: Option<serde_json::Value>,
    pub(super) changed_by: Option<String>,
    pub(super) changed_at: DateTime<Utc>,
}

#[derive(FromQueryResult)]
pub(super) struct AlarmRow {
    pub(super) id: Uuid,
    pub(super) severity: i16,
    pub(super) max_severity: i16,
    pub(super) started_at: DateTime<Utc>,
    pub(super) resolved_at: Option<DateTime<Utc>>,
    pub(super) acknowledged_by: Option<String>,
    pub(super) measurement_type: String,
}

/// What one row's record says, reduced to what the routing turns on.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct RowProvenance {
    /// A tool run stands behind the value, so the tool owns it (Q8 option B).
    pub has_tool_run: bool,
    /// The output slot at this visit has been taken off its calculation, so a person owns the
    /// value (the `slot_owner` fold, Q47).
    pub slot_detached: bool,
    /// The stream's classification: `sync` | `manual` | `csv` | `api`.
    pub classification: String,
    /// A curve an operator picked by hand produced the corrected value.
    pub has_standard_curve: bool,
    /// A calibration window produced the corrected value.
    pub has_calibration: bool,
    /// A deployment covers the instant, so the instrument comes from the deployment history.
    pub has_deployment: bool,
    pub is_flagged: bool,
    pub withdrawn: bool,
    pub unverified: bool,
}

/// What may be done to one row, given what produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditOption {
    /// Reopen the run in its tool with the stored inputs loaded, edit and re-save (M4).
    ReopenRun,
    /// Take the output slot at this visit away from its calculation (admin only, M63).
    Detach,
    /// Give a detached output slot back to its calculation (admin only).
    Return,
    /// Correct the measurement in place.
    ValueCorrection,
    /// Choose a different hand-picked standard curve.
    Curve,
    /// The instrument comes from a deployment, so the fix is to the deployment, not the row.
    EditDeployment,
    /// The corrected value comes from a calibration window, so the fix is to the window.
    EditCalibration,
    Flag,
    Unflag,
    Withdraw,
    Reassert,
    Verify,
    Reject,
}

impl EditOption {
    /// The capability the option needs. Curation of the measurement is `write_data`; attribution
    /// is `manage_sensors`; taking a slot off its calculation is the Administrator's.
    #[must_use]
    pub fn capability(self) -> crate::common::authz::Capability {
        use crate::common::authz::Capability;
        match self {
            Self::Curve | Self::EditDeployment | Self::EditCalibration => Capability::ManageSensors,
            Self::Detach | Self::Return => Capability::Admin,
            _ => Capability::WriteData,
        }
    }
}

/// The decision an edit request carries: what to do, and the one value it needs.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditDecision {
    /// `value_correction` | `flag` | `unflag` | `withdraw` | `reassert` | `curve` |
    /// `verify` | `reject`.
    pub kind: String,
    /// The corrected raw value, for `value_correction`.
    #[serde(default)]
    pub value: Option<f64>,
    /// The curve, calibration or instrument the decision names.
    #[serde(default)]
    pub target_id: Option<Uuid>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl EditDecision {
    /// The assertion the set records, given what the selection carries. A correction naming a
    /// value per key has no single value to assert at set level: the values are on the rows, and
    /// the set records only that a correction was made over them.
    pub(super) fn assertion_over(
        &self,
        kind: Kind,
        selection: &Selection,
    ) -> AppResult<(serde_json::Value, EditOption)> {
        if kind == Kind::ValueCorrection
            && crate::routes::private::readings::service::keyed_corrections(selection)?.is_some()
            && self.value.is_none()
        {
            return Ok((serde_json::json!({}), EditOption::ValueCorrection));
        }
        self.assertion(kind)
    }

    pub(super) fn parsed(&self) -> AppResult<Kind> {
        let kind = Kind::parse(&self.kind)
            .ok_or_else(|| AppError::BadRequest(format!("unknown edit kind '{}'", self.kind)))?;
        if !matches!(
            kind,
            Kind::ValueCorrection
                | Kind::Flag
                | Kind::Unflag
                | Kind::Withdraw
                | Kind::Reassert
                | Kind::Curve
                | Kind::Verify
                | Kind::Reject
        ) {
            return Err(AppError::BadRequest(format!(
                "'{}' is not an edit; it is recorded by the path that owns it",
                self.kind
            )));
        }
        Ok(kind)
    }

    /// The `new` payload the decision records, and the option it corresponds to.
    pub(super) fn assertion(&self, kind: Kind) -> AppResult<(serde_json::Value, EditOption)> {
        let target = || {
            self.target_id.ok_or_else(|| {
                AppError::BadRequest(format!("a {} names the row it belongs to", self.kind))
            })
        };
        Ok(match kind {
            Kind::ValueCorrection => {
                let value = self.value.ok_or_else(|| {
                    AppError::BadRequest("a value correction carries the corrected value".into())
                })?;
                (
                    serde_json::json!({ "raw_value": value }),
                    EditOption::ValueCorrection,
                )
            }
            Kind::Flag => (
                serde_json::json!({ "reason": self.reason.clone().unwrap_or_default() }),
                EditOption::Flag,
            ),
            Kind::Unflag => (serde_json::json!({}), EditOption::Unflag),
            Kind::Withdraw | Kind::Reject => (
                serde_json::json!({ "reason": self.reason.clone().unwrap_or_default() }),
                if kind == Kind::Reject {
                    EditOption::Reject
                } else {
                    EditOption::Withdraw
                },
            ),
            Kind::Reassert => (serde_json::json!({}), EditOption::Reassert),
            Kind::Verify => (
                serde_json::json!({ "unverified": false }),
                EditOption::Verify,
            ),
            Kind::Curve => (
                serde_json::json!({ "standard_curve_id": target()? }),
                EditOption::Curve,
            ),
            other => {
                return Err(AppError::BadRequest(format!(
                    "'{}' is not an edit",
                    other.as_str()
                )));
            }
        })
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub selection: Selection,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InspectedRow {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicate_index: i16,
    pub raw_value: f64,
    pub provenance: RowProvenance,
    pub options: Vec<EditOption>,
    /// The run to reopen, when the route is the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tool_run_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InspectResponse {
    pub rows: Vec<InspectedRow>,
}

/// One row of [`ROW_SQL`], decoded by the derive rather than column by column.
#[derive(FromQueryResult)]
pub(super) struct StoredRow {
    pub(super) stream_id: Uuid,
    pub(super) time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) replicate_index: i16,
    pub(super) raw_value: f64,
    pub(super) site_id: Option<Uuid>,
    pub(super) parameter_id: Option<Uuid>,
    pub(super) run_id: Option<String>,
    pub(super) has_curve: bool,
    pub(super) has_calibration: bool,
    pub(super) has_deployment: bool,
    pub(super) is_flagged: bool,
    pub(super) withdrawn: bool,
    pub(super) unverified: bool,
    pub(super) source_system: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    pub selection: Selection,
    pub decision: EditDecision,
    /// Required on a commit: the id the preview returned. A commit of anything else is refused.
    #[serde(default)]
    pub preview_id: Option<Uuid>,
}

/// One replicate as the preview reports it: before and after.
#[derive(Debug, Serialize, ToSchema)]
pub struct MovedRow {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicate_index: i16,
    /// The columns this row moves, keyed by column name.
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub before: serde_json::Value,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub after: serde_json::Value,
}

/// One group's statistics, before and after.
#[derive(Debug, Serialize, ToSchema)]
pub struct MovedSample {
    pub sample_id: Uuid,
    /// The statistics this group moves, keyed by column name.
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub before: serde_json::Value,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub after: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewResponse {
    pub preview_id: Uuid,
    /// The decision's effect on each row, read back from the write itself.
    pub rows: Vec<MovedRow>,
    /// The statistics the samples trigger recomputed for the groups the rows belong to.
    pub samples: Vec<MovedSample>,
    /// The calculations the touched parameters feed, in the order the chain would run them.
    pub calculations: Vec<crate::routes::private::tools::models::CalculationImpact>,
    /// Everything the preview does not compute, named rather than left to be assumed.
    pub not_previewed: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EditResponse {
    /// The decisions this edit recorded, which is what a rollback names.
    pub rows_decided: u64,
    pub decision_ids: Vec<Uuid>,
    /// The set the decisions were recorded under, rolled back as one act.
    pub set_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackResponse {
    pub rollback_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackSetResponse {
    pub set_id: Uuid,
    pub rolled_back: usize,
}

/// One reading's servedness, for the states a decision moves between.
#[derive(FromQueryResult)]
pub(super) struct StateRow {
    pub(super) stream_id: Uuid,
    pub(super) time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) replicate_index: i16,
    pub(super) state: serde_json::Value,
}

/// One sample's statistics, as the preview compares them before and after.
#[derive(FromQueryResult)]
pub(super) struct SampleStatsRow {
    pub(super) id: Uuid,
    pub(super) stats: serde_json::Value,
}

#[derive(FromQueryResult)]
pub(super) struct ParameterRow {
    pub(super) parameter_id: Uuid,
}

/// One decision id, for a read that wants the ledger keys and nothing else.
#[derive(FromQueryResult)]
pub(super) struct DecisionIdRow {
    pub(super) id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReloadResponse {
    pub tool: String,
    /// The calculate body that reproduces the run: its stored inputs plus the calculation context.
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub body: serde_json::Value,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub constants: serde_json::Value,
    #[schema(value_type = Vec<std::collections::HashMap<String, serde_json::Value>>)]
    pub curves: Vec<serde_json::Value>,
}

/// What a decision did, per proposal.
#[derive(Debug, Serialize, ToSchema)]
pub struct DecideResponse {
    pub accepted: usize,
    pub rejected: usize,
    /// Ids that were not decided, each with why.
    pub refused: Vec<(Uuid, String)>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DecideRequest {
    pub ids: Vec<Uuid>,
    /// `accept` writes the proposed value as a value correction; `reject` records the refusal
    /// against this exact source value.
    pub decision: String,
    pub reason: Option<String>,
}

#[derive(FromQueryResult)]
pub(super) struct Pending {
    pub(super) id: Uuid,
    pub(super) stream_id: Uuid,
    pub(super) time: DateTime<Utc>,
    pub(super) replicate_index: i16,
    pub(super) proposed_raw_value: f64,
    pub(super) proposed_standard_curve_id: Option<Uuid>,
    pub(super) stored_standard_curve_id: Option<Uuid>,
}

#[derive(FromQueryResult)]
pub(super) struct SourceCount {
    pub(super) source_system: String,
    pub(super) n: i64,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ListQuery {
    /// `pending` (the default view of the queue), `accepted` or `rejected`.
    pub status: Option<String>,
    pub stream_id: Option<Uuid>,
}

/// One instant of one series, addressed either by the readings PK's stream half or by the slot a
/// chart knows. The whole replicate group at the instant is the record.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ProvenanceQuery {
    /// The instant (exact reading timestamp).
    pub time: DateTime<Utc>,
    /// Key form 1: the stream serving the point.
    pub stream_id: Option<Uuid>,
    /// Key form 2: the site half of the slot (with `parameter_id`).
    pub site_id: Option<Uuid>,
    /// Key form 2: the parameter half of the slot (with `site_id`).
    pub parameter_id: Option<Uuid>,
    /// Narrow key form 2 to one cadence ('continuous' matches rows stored as NULL).
    pub measurement_type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProvenanceResponse {
    pub time: DateTime<Utc>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    /// What the instant measured, named. The record was serving bare numbers under a bare uuid.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_name: Option<String>,
    /// The slot's unit when it declares one, the catalog default otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub units: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decimal_places: Option<i16>,
    /// More than one stream serves this (site, parameter) at this instant.
    pub duplicate_slot: bool,
    /// One record per stream serving the instant.
    pub records: Vec<ProvenanceRecord>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProvenanceRecord {
    pub origin: OriginInfo,
    pub readings: Vec<ReadingFacet>,
    pub chain: ChainInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub event: Option<EventRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub computation: Option<ComputationInfo>,
    /// The formula that produced a derived value, the counterpart of a tool run's record.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calculation: Option<CalculationInfo>,
    /// The values the formula producing this parameter read at this instant, one hop up the
    /// chain. Each names the key its own record is read by.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub inputs: Vec<InputRef>,
    /// Every enabled formula reading this parameter, one hop down the chain, with its output's
    /// value at this instant where one exists.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub consumers: Vec<ConsumerRef>,
    /// What the calculation that made this record actually read, captured at the read (Q215),
    /// each input beside what its source holds now. Empty on a record nothing computed, and on
    /// one computed before the capture existed, whose inputs are therefore unknown.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub consumed: Vec<ConsumedRef>,
    pub holds: Vec<HoldRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OriginInfo {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub source_name: Option<String>,
    /// 'sync' | 'manual' | 'csv' | 'api', from the stream's source system.
    pub classification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub paired_at: Option<DateTime<Utc>>,
    /// Latest first-arrival stamp in the replicate group: when these rows first existed. NULL
    /// means they predate tracking. Nothing moves it, so a corrected row still reports its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub ingested_at: Option<DateTime<Utc>>,
    /// When the value the group currently serves arrived: the latest live value correction's `at`
    /// where one exists, and the first arrival otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value_arrived_at: Option<DateTime<Utc>>,
    /// The latest windowed-ingest pass whose claimed window covers the instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub receipt: Option<ReceiptSummary>,
    /// The portal function that computed this column and the columns it read, as the source
    /// declared on the stream. Absent for a column the source stores as entered.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub portal_calculation: Option<PortalCalculation>,
}

/// The portal function a synced column was computed by, as the source declared it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PortalCalculation {
    /// The source's own function name, verbatim (`calcPCO2`).
    pub function: String,
    /// The columns it reads, in the order the source lists them.
    pub inputs: Vec<PortalInput>,
}

/// One column a portal calculation reads.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PortalInput {
    pub column: String,
    /// The record it opens, where a stream of the same source at the same site holds it at the
    /// same instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub point: Option<SlotRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptSummary {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    #[schema(required)]
    pub window_from: Option<DateTime<Utc>>,
    #[schema(required)]
    pub window_to: Option<DateTime<Utc>>,
    pub submitted: i32,
    pub new_rows: i32,
    pub changed: i32,
    pub unchanged: i32,
    pub withdrawn: i32,
    pub rejected_total: i32,
    pub braked: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadingFacet {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibrated_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub measurement_type: Option<String>,
    pub is_flagged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub flag_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_reason: Option<String>,
    /// A pending entry: stored and shown here, never published (Q18, Q21).
    pub unverified: bool,
    /// When this row first existed. Nothing moves it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub ingested_at: Option<DateTime<Utc>>,
    /// When the value this row currently serves arrived: its latest live value correction's `at`,
    /// else its first arrival.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value_arrived_at: Option<DateTime<Utc>>,
    /// Where this value came from, one of `PROVENANCE_KINDS`. A `sync` or `derived` row's story is
    /// resolved from the stream, the receipt and the definition; the others carry a stored blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub provenance_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibration: Option<CalibrationRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub standard_curve: Option<CurveRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationRef {
    pub id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub valid_until: Option<DateTime<Utc>>,
    /// Set when the curve has been retired: the reading keeps the value it produced, and no new
    /// measurement resolves it (M146).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub retired_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveRef {
    pub id: Uuid,
    /// The lab instrument the curve belongs to, which is where its record lives.
    pub sensor_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    /// Set when the lab has taken the curve out of circulation (M147). The value stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub retired_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChainInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor: Option<SensorRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub deployment: Option<DeploymentRef>,
    /// Live instrument or calibration pins on the group: attribution a person decided, which
    /// reprocess leaves alone.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub pins: Vec<PinRef>,
}

#[derive(FromQueryResult)]
pub(super) struct ArrivalRow {
    pub(super) stream_id: Uuid,
    pub(super) replicate_index: i16,
    pub(super) at: DateTime<chrono::FixedOffset>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PinRef {
    pub decision_id: Uuid,
    /// `instrument_pin` | `calibration_pin`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_index: Option<i16>,
    #[schema(value_type = Object)]
    pub target: serde_json::Value,
    pub actor: String,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub set_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorRef {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub model: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeploymentRef {
    pub id: Uuid,
    pub site_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub site_name: Option<String>,
    pub deployed_from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub deployed_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventRef {
    pub id: Uuid,
    pub collected_at: DateTime<Utc>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
}

/// The formula behind a derived value: the calculation it belongs to and the version it was made
/// with. A row naming no version reports no formula, because the text that produced it is not
/// recoverable from the definition's current one (M134).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CalculationInfo {
    pub definition_id: Uuid,
    /// The calculation the formula belongs to.
    pub tool_script_id: Uuid,
    pub code: String,
    pub name: String,
    /// The version the stored value names, absent when the value predates versioning.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub version_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub version_no: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub formula: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub content_hash: Option<String>,
    /// The calculation's newest version, so a value made by an older one is visible as such.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub active_version_no: Option<i32>,
    /// The calculation's decommission, read as it stands now, absent while it is live.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decommissioned: Option<Decommission>,
}

/// A calculation's decommission: when, by whom and why (Q272).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Decommission {
    pub at: DateTime<Utc>,
    pub by: String,
    pub reason: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ComputationInfo {
    /// The statistics row, when the instant carries two or more replicates.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sample_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub notes: Option<String>,
    /// The server-built tool-run blob stored on the reading, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<HashMap<String, serde_json::Value>>)]
    pub provenance: Option<serde_json::Value>,
    /// The run's minting path: 'interactive' | 'csv_import' | 'chain'.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub run_source: Option<String>,
    /// The decommission of the calculation the run executed, read as it stands now: the blob is
    /// frozen at run time and cannot carry it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decommissioned: Option<Decommission>,
    /// The group's statistics, the numbers the chart plotted and drew its bar from. Without these
    /// the record shows the replicates and never what was served.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub max: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HoldRef {
    pub id: Uuid,
    pub kind: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    /// The calculation a chain finding is against, so its chip opens that calculation.
    #[schema(required)]
    pub tool: Option<String>,
}

/// One value a formula read at the record's instant. A parameter input carries the slot key
/// (`parameter_id` with the record's site and time) that resolves its own record; a site property
/// carries the column it was read from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InputRef {
    pub definition_id: Uuid,
    /// The formula's code, so two formulas producing one parameter stay apart.
    pub formula_code: String,
    pub variable_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub site_property: Option<String>,
    /// Set when the formula evaluates per replicate and this is the variable it iterates: the
    /// value at this index fed the output at the same index.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_index: Option<i16>,
    /// How the value was read: `replicate` (the row at the index), `mean` (the family's sample
    /// statistic), `reading` (the single row at the instant), `site` (the site row's column).
    /// `missing` when nothing at the instant answers.
    pub served_as: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value: Option<f64>,
}

/// One reading a calculation read, as the row and the revision it stood at (Q215). `revision` is
/// the newest `reading_decisions.seq` at the key when the value was read; null is the arrival
/// state, a row no decision had touched yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ConsumedReading {
    pub stream_id: Uuid,
    pub time: DateTime<Utc>,
    pub replicate_index: i16,
    #[schema(required)]
    pub revision: Option<i64>,
    #[schema(required)]
    pub value: Option<f64>,
}

/// One input a calculation consumed, captured when it was read (Q215): what it was bound to and
/// the exact revision of every row behind it, so a later reader can say whether the source has
/// moved since. `kind` is `reading` (one row), `mean` (the sample statistic over `members`),
/// `replicates` (a family, one member per index, a gap as a member with no value), `site` (a
/// column of the site row), `constant`, `curve` (a catalog curve, or entered coefficients with no
/// subject), `computed` (the number a step of the same set produced, under the step's own code)
/// or `step` (a formula of the pinned set). An entity input names its `change_audit` subject and
/// the newest `seq` for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ConsumedInput {
    pub variable: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub property: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub revision: Option<i64>,
    /// How the reading it names was reached (Q230): absent where it was read at the instant the
    /// run computed, `hold` where it is the last value measured at or before it, which a
    /// calculation on a stream does for an input the lab measures at a visit. Without it a held
    /// value reads as a mis-stamped one, and a replay cannot tell which rule bound it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub alignment: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ConsumedReading>,
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
}

/// Where a consumed reading opens: the site page's point record for its slot.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SlotRef {
    pub site_id: Uuid,
    pub site_parameter_id: Uuid,
    pub time: DateTime<Utc>,
    /// `spot` or `continuous`, the cadence arm the record is read by.
    pub measurement_type: String,
}

/// One reading a calculation consumed, beside what its key holds now (Q215).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConsumedMemberRef {
    pub stream_id: Uuid,
    pub time: DateTime<Utc>,
    pub replicate_index: i16,
    #[schema(required)]
    pub revision: Option<i64>,
    #[schema(required)]
    pub value: Option<f64>,
    #[schema(required)]
    pub current_revision: Option<i64>,
    #[schema(required)]
    pub current_value: Option<f64>,
    /// `changed`, `unchanged` or `unknown`.
    pub state: String,
    /// The record this member opens, absent while its stream is unpaired.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub point: Option<SlotRef>,
}

/// One input of the calculation that made this record, as it was consumed and as its source
/// stands now (Q215). `kind`, `subject` and `property` are the captured binding.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConsumedRef {
    pub variable: String,
    pub kind: String,
    /// How the reading it names was reached (Q230), as the capture recorded it: absent where it
    /// was read at the instant computed, `hold` where it is the last value measured at or before
    /// it. Without it a held member reads as one stamped at the wrong instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub alignment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub property: Option<String>,
    #[schema(required)]
    pub revision: Option<i64>,
    #[schema(required)]
    pub current_revision: Option<i64>,
    #[schema(value_type = serde_json::Value)]
    pub value: serde_json::Value,
    /// What the source holds now, where the input reads one row. A statistic over several
    /// readings is not recomputed here: its members carry their own current values.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<serde_json::Value>)]
    pub current_value: Option<serde_json::Value>,
    /// `changed`, `unchanged` or `unknown`.
    pub state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub members: Vec<ConsumedMemberRef>,
}

/// One formula reading the record's parameter, with its output at the instant where one exists.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConsumerRef {
    pub definition_id: Uuid,
    pub formula_code: String,
    pub formula_name: String,
    /// The calculation the formula belongs to, absent on a shared step, which belongs to none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calculation: Option<String>,
    pub variable_name: String,
    /// The slot key of the output's own record. Absent on an intermediate, which mints none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub output_parameter_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub output_parameter_code: Option<String>,
    /// Set when the formula evaluates per replicate over this parameter: the output at this index
    /// came from the record's value at the same index.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_index: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value: Option<f64>,
}

#[derive(Debug, FromQueryResult)]
pub struct RawRow {
    pub stream_id: Uuid,
    pub(super) replicate_index: i16,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub(super) raw_value: f64,
    pub(super) calibrated_value: Option<f64>,
    pub(super) sensor_id: Option<Uuid>,
    pub(super) calibration_id: Option<Uuid>,
    pub(super) standard_curve_id: Option<Uuid>,
    pub(super) deployment_id: Option<Uuid>,
    pub(super) measurement_type: Option<String>,
    pub(super) is_flagged: Option<bool>,
    pub(super) flag_reason: Option<String>,
    pub sample_id: Option<Uuid>,
    pub collection_event_id: Option<Uuid>,
    pub(super) withdrawn_at: Option<DateTime<Utc>>,
    pub(super) unverified: Option<bool>,
    pub(super) withdrawn_reason: Option<String>,
    pub(super) ingested_at: Option<DateTime<Utc>>,
    pub(super) provenance_kind: Option<String>,
    pub provenance: Option<serde_json::Value>,
    pub(super) derived_version_id: Option<Uuid>,
    pub(super) label: Option<String>,
    pub(super) notes: Option<String>,
    pub(super) created_by: Option<String>,
}

#[derive(FromQueryResult)]
pub(super) struct CoveringReceipt {
    pub(super) stream_id: Uuid,
    pub(super) id: Uuid,
    pub(super) at: Option<DateTime<chrono::FixedOffset>>,
    pub(super) window_from: Option<DateTime<chrono::FixedOffset>>,
    pub(super) window_to: Option<DateTime<chrono::FixedOffset>>,
    pub(super) submitted: i32,
    pub(super) new_rows: i32,
    pub(super) changed: i32,
    pub(super) unchanged: i32,
    pub(super) withdrawn: i32,
    pub(super) rejected_total: i32,
    pub(super) braked: bool,
}

/// A hold as its queries select it, with the key column each of the two shapes carries.
#[derive(FromQueryResult)]
pub(super) struct HoldRow {
    pub(super) stream_id: Option<Uuid>,
    pub(super) parameter_id: Option<Uuid>,
    pub(super) id: Uuid,
    pub(super) kind: String,
    pub(super) status: String,
    pub(super) created_at: DateTime<chrono::FixedOffset>,
    pub(super) tool: Option<String>,
}

impl From<&HoldRow> for HoldRef {
    fn from(row: &HoldRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind.clone(),
            status: row.status.clone(),
            created_at: row.created_at.with_timezone(&Utc),
            tool: row.tool.clone(),
        }
    }
}

#[derive(FromQueryResult)]
pub(super) struct LinkRow {
    pub(super) id: Uuid,
    pub(super) code: String,
    pub(super) name: String,
    pub(super) per_replicate: Option<String>,
    pub(super) output_parameter_id: Option<Uuid>,
    pub(super) output_parameter_code: Option<String>,
    pub(super) calculation: Option<String>,
    pub(super) enabled: bool,
}

#[derive(FromQueryResult)]
pub(super) struct SourceRow {
    pub(super) derived_definition_id: Uuid,
    pub(super) variable_name: String,
    pub(super) parameter_id: Option<Uuid>,
    pub(super) site_property: Option<String>,
    pub(super) parameter_code: Option<String>,
}

#[derive(FromQueryResult)]
pub(super) struct ServedRow {
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
    pub(super) replicate_index: i16,
    pub(super) value: f64,
    pub(super) live: bool,
    pub(super) measurement_type: Option<String>,
    pub(super) stream_id: Uuid,
    pub(super) input_value: f64,
    pub(super) from_mean: bool,
    /// The newest ledger sequence at the row's key, or `None` at its arrival state (Q215).
    pub(super) revision: Option<i64>,
}

#[derive(FromQueryResult)]
pub(super) struct SiteRow {
    pub(super) id: Uuid,
    pub(super) row: serde_json::Value,
}

#[derive(FromQueryResult)]
pub(super) struct SlotRow {
    pub(super) code: String,
    pub(super) name: String,
    pub(super) units: Option<String>,
    pub(super) decimal_places: Option<i16>,
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
    pub calculations: Vec<crate::routes::private::tools::models::CalculationImpact>,
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

/// How to handle readings that collide with an existing (stream_id, time, replicate_index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictMode {
    /// Keep the existing row, drop the incoming one.
    #[default]
    Skip,
    /// Replace the stored values with the incoming ones.
    Overwrite,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchReadingsRequest {
    pub readings: Vec<ReadingInput>,
    /// Behaviour on (stream_id, time, replicate_index) collisions. Defaults to `skip`.
    #[serde(default)]
    pub conflict: ConflictMode,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadingInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    /// The standard curve the value was corrected with, for a caller replaying grabs that carried
    /// one. Ordinary batch inserts leave it unset. The reading must be a spot measurement on the
    /// instrument the curve was fitted on, and the server recomputes `calibrated_value` from the
    /// curve, so a submitted one is not what gets stored.
    #[serde(default)]
    pub standard_curve_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
    #[serde(default)]
    pub sample_id: Option<Uuid>,
    /// Per-reading override ('continuous' | 'spot' | 'derived'). Omit to resolve from the
    /// resolved sensor's data_frequency (lab CSV uploads should pass 'spot' explicitly).
    #[serde(default)]
    pub measurement_type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BatchReadingsResponse {
    pub inserted: usize,
    /// Stored rows whose value this write changed under `conflict = overwrite`; a re-sent
    /// identical row is not counted. Always 0 in `skip` mode.
    pub overwritten: usize,
    /// The calculations the spot parameters this batch landed feed, and what each rewrites at
    /// the visits touched. Their recompute is enqueued when this is not empty.
    #[serde(default)]
    pub calculations: Vec<crate::routes::private::tools::models::CalculationImpact>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestReadingsRequest {
    pub stream_id: Uuid,
    pub readings: Vec<IngestReading>,
    /// Update existing rows at the same (stream, time, replicate) key instead of skipping
    /// them, so source-side corrections propagate on re-sync. Sync-service callers only;
    /// flag state and sample links on the existing row are preserved.
    #[serde(default)]
    pub overwrite: bool,
    /// The writer declaring these readings collection events: spot replicate groups sharing an
    /// instant form a `samples` row from the first reading, like `/grab_samples`. Sync-service
    /// callers only; without it a group still forms a sample once it carries two replicates.
    #[serde(default)]
    pub collection: bool,
    /// Per-instant expectations from the source portal's own precomputed statistics. Each audited
    /// group's mean/sd is recomputed over the values about to be stored and compared; a
    /// disagreeing group is admitted and recorded as a `replicate_audit_holds` row for review
    /// (`pending` when the stream is paired, `deferred` until pairing otherwise). Sync-service
    /// callers only.
    #[serde(default)]
    pub audit: Option<Vec<crate::routes::private::sync::models::GroupAudit>>,
    /// A completeness claim: these readings are the source's complete content for this stream
    /// over `[from, to)`. The server diffs stored content against the payload and converges:
    /// new rows insert, changed values correct in place, rows absent at source are stamped
    /// withdrawn (never deleted). Sync-service callers only; spot streams only. Without it the
    /// request is a bare append, exactly the old semantics.
    #[serde(default)]
    pub window: Option<SourceWindow>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestResponse {
    pub inserted: usize,
    /// Readings dropped by admission (out-of-window timestamp, non-finite value, unknown
    /// measurement_type, unknown calibration_id). Reported rather than raised so a cursor-driven
    /// caller can advance past them, and counted so the loss is never silent.
    #[serde(default)]
    pub skipped: usize,
    /// One entry per kind of rejection with its count, never one per reading, so the response
    /// stays a fixed size however large the batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_reasons: Vec<String>,
    /// Windowed diff: stored keys the source has moved since river-data stored them. Nothing is
    /// written for them (Q84); `proposed` says how many are waiting for a person.
    #[serde(default)]
    pub changed: usize,
    /// Windowed diff: changed keys recorded as proposals this pass, each awaiting a decision.
    #[serde(default)]
    pub proposed: usize,
    /// Windowed diff: stored rows absent from the claimed window, stamped withdrawn.
    #[serde(default)]
    pub withdrawn: usize,
    /// Windowed diff: stored rows the payload re-sent unchanged.
    #[serde(default)]
    pub unchanged: usize,
    /// Windowed diff: stored rows the funnel refused or the backend dropped, left untouched.
    #[serde(default)]
    pub retained: usize,
    /// The window the server accepted, echoed so a connector can detect an API image that
    /// silently ignored the claim (which would downgrade the source to append mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub accepted_window: Option<SourceWindow>,
    pub stream_id: Uuid,
    pub paired: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestStatusEventsRequest {
    pub stream_id: Uuid,
    pub events: Vec<IngestStatusEvent>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestStatusEventsResponse {
    pub inserted: usize,
    /// Events dropped because their timestamp is outside the admissible window. Counted rather
    /// than raised, for the same reason `/ingest` counts its skipped readings.
    #[serde(default)]
    pub skipped: usize,
    /// Events dropped for repeating the stream's latest value: the series keeps its first
    /// value and its transitions.
    #[serde(default)]
    pub deduplicated: usize,
    pub stream_id: Uuid,
    pub paired: bool,
}

/// Stream-based status event ingestion (non-numeric device states like "low_battery").
/// Hypertable inserts keyed by stream_id. Requires `write_data`.
/// The newest status event stored for a stream, which decides what is new and what repeats.
#[derive(FromQueryResult)]
pub(super) struct StatusTip {
    pub(super) time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) value: Option<String>,
}

/// The replicate audit: recompute each audited group's statistics over the stored replicates,
/// compare against the portal's claim, and admit the group either way. Served statistics are
/// trigger-computed from the stored replicates, so a disagreement questions the portal's
/// aggregate cells, not the data, and withholding would only hide measurements from the people
/// waiting on them. A mismatch records a hold for review (pending when paired, deferred until
/// pairing); a group that matches again at source supersedes its open hold; a group an operator
/// already ruled on (acknowledged or remediated) is left alone unless the portal's expected
/// statistics have moved since the ruling, which opens a fresh hold.
/// One stored replicate an audit expectation is compared against.
#[derive(FromQueryResult)]
pub(super) struct StoredReplicate {
    pub(super) time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) replicate_index: i16,
    pub(super) value: f64,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrabSampleRequest {
    pub site_id: Uuid,
    /// Stamped onto the samples rows this request creates or reuses.
    pub label: Option<String>,
    pub notes: Option<String>,
    /// `replace` atomically rewrites every replicate group this request names. Without it, a
    /// group that is already stored refuses the write with a 409 describing what is there.
    #[serde(default)]
    pub mode: Option<GrabWriteMode>,
    /// Compute the preview and report existing groups without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Set by the chain when the inputs it computed from are themselves pending, so the outputs
    /// inherit that state. Never deserialized: a client cannot claim it or clear it.
    #[serde(skip)]
    pub pending_inputs: bool,
    /// The `tool_runs` row these readings came from (returned by `/tools/{name}/calculate` as
    /// `run_id`). The server builds the provenance blob from that row, so the blob's inputs,
    /// constants, curves and outputs are what the engine resolved, its actor is the calculating
    /// user, and a save cannot claim a run it did not make: every reading must name one of the
    /// run's outputs and carry that output's value.
    #[serde(default)]
    pub tool_run_id: Option<Uuid>,
    /// A `seasonal_checks` row (from `/readings/seasonal_check`) covering this save's values.
    /// When present, every reading's (parameter, value) must have been screened by that check:
    /// the portal's "any edit resets Check", enforced server-side. The check itself is advisory;
    /// naming a check that does not cover the values is refused.
    #[serde(default)]
    pub check_id: Option<Uuid>,
    /// What the client believes each group it replaces already holds. A replace retracts the
    /// stored replicates it does not carry, so a save built from a stale read would retract a
    /// repeat somebody else added in the meantime. Naming the indexes refuses that with a 409
    /// describing what is there instead; omitting the field writes without the check.
    #[serde(default)]
    pub expected_replicates: Option<Vec<ExpectedGroup>>,
    pub readings: Vec<GrabSampleReading>,
}

/// The replicate indexes a client read for one group before it built its save.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpectedGroup {
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicate_indices: Vec<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GrabWriteMode {
    Replace,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrabSampleReading {
    pub parameter_id: Uuid,
    pub sensor_id: Option<Uuid>,
    pub value: f64,
    pub time: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
    /// The named output of the referenced tool run this reading stores. With `tool_run_id` every
    /// reading names either an `output` or an `input`, and is refused otherwise; the reading's
    /// `value` must be the run's value for that output.
    #[serde(default)]
    pub output: Option<String>,
    /// The named input of the referenced tool run this reading stores: the measured value the run
    /// consumed at `replicate_index`, stored raw. A curve the run applied is not a correction of
    /// this row, so `standard_curve_id` is admitted here and the database applies it (ADR 0003).
    /// A `replicates` input stores one reading per position; a numeric input the manifest binds to
    /// a visit parameter (`event_inputs`) stores one at replicate 0, correcting what the visit
    /// holds. A numeric input the manifest binds to nothing is a run-only setting and is refused.
    #[serde(default)]
    pub input: Option<String>,
    /// The standard curve the operator fitted for this measurement, typically per microplate. It is
    /// applied on top of the instrument's base calibration, which the server resolves from the
    /// sensor's windows at `time`. The stored row carries the measured `raw_value`, both curve
    /// references and the value they produce together, so a recorded identity base and an
    /// unrecorded one stay distinguishable.
    #[serde(default)]
    pub standard_curve_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GrabSampleResponse {
    pub inserted: usize,
    pub samples_created: usize,
    /// The samples this request created, as opposed to reused. A caller that wants to act on
    /// exactly the rows it just wrote (the bot flags field submissions for review) can key on these
    /// rather than re-selecting by slot and time, which would also catch a concurrent write.
    #[serde(default)]
    pub created_sample_ids: Vec<Uuid>,
    /// True when nothing was written.
    pub dry_run: bool,
    /// Rows `mode: replace` rewrote in place. Only the grab stream's own rows at the instant are
    /// candidates; rows another source wrote at the same slot and time are untouched.
    pub replaced: usize,
    /// Curated rows on the grab stream that `mode: replace` left in place: flagged, withdrawn, or
    /// carrying a standard curve the request did not supply. Each group with one raises a
    /// `source_modified` hold, and the value entered at that replicate index is not written.
    ///
    /// `serde(default)` reads a response body recorded before the field existed; every save
    /// answers with it, which is what `schema(required)` says.
    #[serde(default)]
    #[schema(required)]
    pub kept_curated: usize,
    /// Replicates stored at the instant that `mode: replace` withdrew because the save no longer
    /// carries them: a cleared cell, or a pasted block narrower than what is there. The value
    /// stays readable and the stamp is reversible; nothing deletes.
    #[serde(default)]
    #[schema(required)]
    pub withdrawn: usize,
    /// What each reading stores: the measured value, the curves that apply and the value they
    /// produce together, computed by the code the write itself uses.
    pub preview: Vec<GrabPreview>,
    /// Replicate groups already stored at the requested (parameter, time) keys, as found before
    /// this request wrote anything.
    pub existing_groups: Vec<ExistingGroup>,
    /// The calculations the saved parameters feed and the output parameters each rewrites at the
    /// visit, in run order. Reported on `dry_run` too, so the consequence is known before the
    /// write; the save enqueues the visit's recompute when this is not empty.
    #[serde(default)]
    pub calculations: Vec<crate::routes::private::tools::models::CalculationImpact>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveApplication {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    pub equation: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GrabPreview {
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicate_index: i16,
    pub raw_value: f64,
    /// The instrument's windowed calibration covering `time`, applied first.
    #[schema(required)]
    pub base_calibration: Option<CurveApplication>,
    /// The operator's hand-picked curve, applied to the base's output.
    #[schema(required)]
    pub standard_curve: Option<CurveApplication>,
    /// Both curves folded into one line, present when both apply.
    #[schema(required)]
    pub composed_equation: Option<String>,
    #[schema(required)]
    pub calibrated_value: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema, FromQueryResult)]
pub struct ExistingReplicate {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[schema(required)]
    pub calibrated_value: Option<f64>,
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
}

/// A replicate a replace kept, and why curation kept it.
#[derive(FromQueryResult)]
pub(super) struct KeptRow {
    pub(super) replicate_index: i16,
    pub(super) reason: String,
}

/// The curation facts a stored group already carries, which a replace keeps.
#[derive(FromQueryResult)]
pub(super) struct PriorFactsRow {
    pub(super) label: Option<String>,
    pub(super) notes: Option<String>,
    pub(super) created_by: Option<String>,
    pub(super) provenance: Option<serde_json::Value>,
    pub(super) provenance_kind: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExistingGroup {
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicates: Vec<ExistingReplicate>,
}

/// What the file's numbers are, deciding whether the import may claim a correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CsvValueState {
    /// Uncorrected instrument output: the deployment's calibration covering each row's time is
    /// stamped and applied, exactly as if the rows had arrived through `/ingest`.
    Raw,
    /// Already-processed numbers (a result sheet, a portal export). Stored as served: no
    /// calibration id is stamped and nothing recomputes them, because a stored calibration id
    /// claims `raw_value` is the uncorrected input, which these rows are not. The default:
    /// an uncorrected import left uncorrected is visible and repairable, a corrected import
    /// corrected again is silent corruption.
    #[default]
    Corrected,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportCsvRequest {
    /// Target site, by UUID or case-insensitive name. With `site_column`, the site a row whose
    /// cell is empty belongs to.
    pub site: String,
    /// The column naming each row's site, for a file covering several sites: the portals'
    /// high-frequency exports are one wide file per resolution with a `Site_ID` column. Each cell
    /// is resolved like `site`, by UUID or case-insensitive name. Omitted, a header named
    /// `site_id` or `site` is taken as one.
    #[serde(default)]
    pub site_column: Option<String>,
    /// Wide CSV text: a `DateTime`, `Date` or `Time` column plus one column per parameter.
    /// Optional when `session_id` references a previously uploaded CSV.
    #[serde(default)]
    pub csv: Option<String>,
    /// Reference to a staged CSV from a prior dry_run. When present without `csv`, the server
    /// retrieves the cached CSV text instead of requiring a re-upload.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// Optional explicit column → parameter mapping. The value is a parameter name or UUID;
    /// `null` skips the column. Overrides automatic resolution.
    #[serde(default)]
    pub mapping: Option<HashMap<String, Option<String>>>,
    /// When true, resolve and report the plan (and overlap diff) without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Behaviour on (stream_id, time, replicate_index) collisions. Defaults to `skip`.
    #[serde(default)]
    pub conflict: ConflictMode,
    /// Timezone offset (hours) of the source timestamps relative to UTC. The server subtracts
    /// this offset to convert to UTC, e.g. `2.0` for CEST (UTC+02:00).
    #[serde(default)]
    pub tz_offset_hours: Option<f64>,
    /// measurement_type stamped on every imported reading ('continuous' | 'spot' | 'derived').
    /// Use 'spot' for lab/campaign result sheets. Omit to resolve per row from the stream's
    /// declaration, then the owning sensor's data_frequency.
    #[serde(default)]
    pub measurement_type: Option<String>,
    /// Whether the file holds raw instrument output or already-processed values. Defaults to
    /// `corrected`: no calibration is stamped or applied unless the caller declares the rows raw.
    #[serde(default)]
    pub values: CsvValueState,
    /// Import the file as tool entry (S4a): each row's columns are inputs of this tool, the tool
    /// runs over every row, and the outputs are saved through the grab write path with the same
    /// server-built provenance a typed entry gets (`source: csv_import`). Without it, columns are
    /// catalog parameters and values import as-is.
    #[serde(default)]
    pub tool: Option<String>,
    /// With `tool`: the standard curve each of the tool's curve slots takes, by slot name, for
    /// every row. A CSV column headed with a slot's name overrides it row by row with the curve
    /// id in the cell (a blank cell falls back to this). The run applies the curve, so outputs
    /// are saved corrected and carry no curve id; the replicates it consumed are saved raw
    /// carrying the curve and its instrument, and the database corrects them.
    #[serde(default)]
    pub curves: Option<HashMap<String, Uuid>>,
    /// The seasonal check a `dry_run` of this file returned (`check.check_id`). A spot or tool
    /// file is screened against the site's seasonal distribution; a commit naming the check is
    /// held to exactly the values it screened, and a commit naming none is refused when the
    /// screen finds a value outside the recorded range.
    #[serde(default)]
    pub check_id: Option<Uuid>,
}

/// One slice of a file too large for a single request. The portals' `10min_data.csv` is 474 MB, an
/// order of magnitude over the import body limit, so it arrives as appends against one staging
/// session and is imported by naming that session.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportChunkRequest {
    /// The session to append to. Omitted, a session is opened and its id returned; pass that id
    /// on every later chunk and to the import itself.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// The next slice of the file, in file order. The first chunk carries the header row, and a
    /// slice ends where the caller chose: the session holds text, so a row split across two
    /// chunks is rejoined by the append.
    pub chunk: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportChunkResponse {
    /// The session the file is accumulating in. Name it on the next chunk and on the import.
    pub session_id: Uuid,
    /// Bytes the session now holds.
    pub bytes: usize,
}

/// One site's share of a multi-site import: what it took and the job that writes it. The scalar
/// fields beside it are the file's totals.
#[derive(Debug, Serialize, ToSchema)]
pub struct SiteImportOutcome {
    pub site_id: Uuid,
    pub site_name: String,
    pub row_count: usize,
    pub inserted_total: usize,
    #[schema(required)]
    pub derived_job_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCsvResponse {
    pub site_id: Uuid,
    pub site_name: String,
    pub dry_run: bool,
    /// Staging session ID. Returned on every request; pass it back on subsequent requests
    /// (re-analyze, import) to avoid re-uploading the CSV.
    #[schema(required)]
    pub session_id: Option<Uuid>,
    /// Header → resolved catalog parameter name, for columns that will be ingested.
    pub mapped_columns: HashMap<String, String>,
    /// Columns intentionally not ingested: derived outputs (recomputed) or explicitly skipped.
    pub skipped_columns: Vec<String>,
    /// Columns that could not be resolved to a parameter.
    pub unmapped_columns: Vec<String>,
    /// Non-fatal notes (e.g. a mapped parameter is not assigned to the site).
    pub warnings: Vec<String>,
    /// Data rows parsed from the CSV.
    pub row_count: usize,
    /// (parameter, timestamp) groups holding more than one value in a 'spot' file; each is stored
    /// as one replicate set behind a single served point.
    pub replicate_groups: usize,
    /// Readings inserted (0 for `dry_run`).
    pub inserted_total: usize,
    #[schema(required)]
    pub earliest: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(required)]
    pub latest: Option<chrono::DateTime<chrono::Utc>>,
    /// Background reprocessing job recomputing derived parameters + refreshing aggregates over the
    /// imported range. Poll `GET /api/reprocessing_jobs/{id}` for progress. `null` when nothing was
    /// inserted (idempotent re-import) or on `dry_run`.
    #[schema(required)]
    pub derived_job_id: Option<Uuid>,
    /// Distinct timestamps queued for derived recompute by that job.
    pub derived_timestamps: usize,
    /// Readings skipped because they already existed (idempotent re-import). 0 for `dry_run`.
    pub duplicates: usize,
    /// Incoming readings whose (stream, time) already exists with the same stored value.
    pub overlaps_identical: usize,
    /// Incoming readings whose (stream, time) already exists with a different stored value.
    /// In `overwrite` mode these are the rows that would be (or were) replaced.
    pub overlaps_differing: usize,
    /// Stored rows whose value this import changed under `conflict = overwrite`; an identical
    /// overlap is not counted. Always 0 in `skip` mode or `dry_run`.
    pub overwritten: usize,
    /// Up to 20 differing overlaps, so the UI can preview what an overwrite would change.
    pub overlap_sample: Vec<OverlapDiff>,
    /// Per-row problems (bad timestamp / non-numeric value): the offending row or cell is skipped
    /// and the rest import, so the operator can fix the source and re-import. Truncated for very
    /// large files (see `error_count`).
    pub errors: Vec<RowError>,
    /// Total number of row problems (may exceed `errors.len()` when the list is truncated).
    pub error_count: usize,
    /// `tool_runs` rows minted by a tool-entry import (one per data row that ran). Always 0 for a
    /// plain import.
    #[serde(default)]
    pub tool_runs_created: usize,
    /// One entry per curve slot of the tool (tool entry only): where its curve comes from.
    #[serde(default)]
    pub curves: Vec<ImportCurve>,
    /// The seasonal screen over the file's cells. `null` for a continuous file, which the check
    /// does not apply to (its history is spot readings).
    #[schema(required)]
    pub check: Option<ImportCheck>,
    /// One entry per site a `site_column` file landed on, in the order they were imported. Empty
    /// for a single-site file, whose totals are the scalar fields.
    #[serde(default)]
    pub site_imports: Vec<SiteImportOutcome>,
}

/// The seasonal Check gate's CSV arm: every cell the file will store as a spot reading, screened
/// against the site's seasonal distribution for its own month.
#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCheck {
    /// The stored check a commit must name. Set by `dry_run`; echoed by a commit that named one;
    /// `null` on a commit that needed none (nothing outside the range).
    #[schema(required)]
    pub check_id: Option<Uuid>,
    /// Cells screened.
    pub screened: usize,
    /// Cells outside the recorded seasonal range.
    pub warnings: usize,
    /// The cells that warn, in file order, capped at `IMPORT_CHECK_FINDINGS_CAP`.
    pub findings: Vec<ScreenedCell>,
    pub method: SeasonalMethod,
}

/// How a tool-entry import fills one of the tool's curve slots.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ImportCurve {
    pub slot: String,
    pub label: String,
    pub required: bool,
    /// The CSV column supplying a curve id per row, when one is headed with the slot's name.
    #[schema(required)]
    pub column: Option<String>,
    /// The request-level curve every row takes unless its column cell names another.
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
    #[schema(required)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OverlapDiff {
    pub time: chrono::DateTime<chrono::Utc>,
    /// Parameter the differing value belongs to.
    pub parameter_id: Uuid,
    /// Currently stored value.
    pub existing: f64,
    /// Value from the imported CSV.
    pub incoming: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RowError {
    /// 1-based CSV line number (the header is line 1).
    pub row: usize,
    pub message: String,
}

/// The queries the importer's column resolution and overlap check make, each as the row its
/// SELECT returns.
#[derive(FromQueryResult)]
pub(super) struct SlotColumnRow {
    pub(super) parameter_id: Uuid,
    pub(super) sp_name: Option<String>,
    pub(super) param_name: Option<String>,
    pub(super) aliases: Option<Vec<String>>,
}

#[derive(FromQueryResult)]
pub(super) struct FamilyKeyRow {
    pub(super) id: Uuid,
    pub(super) source_key: String,
}

#[derive(FromQueryResult)]
pub(super) struct SlotStreamRow {
    pub(super) parameter_id: Uuid,
    pub(super) id: Uuid,
}

#[derive(FromQueryResult)]
pub(super) struct StoredValueRow {
    pub(super) parameter_id: Uuid,
    pub(super) time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) val: f64,
    pub(super) stream_id: Option<Uuid>,
}

/// One stored row of the window, as the diff reads it.
#[derive(FromQueryResult)]
pub(super) struct StoredWindowRow {
    pub(super) time: sea_orm::prelude::DateTimeWithTimeZone,
    pub(super) replicate_index: i16,
    pub(super) raw_value: f64,
    pub(super) standard_curve_id: Option<Uuid>,
    pub(super) withdrawn: bool,
    pub(super) judgements: serde_json::Value,
    pub(super) touched: bool,
}

/// One screening of entered values against the site's seasonal distribution: the portal's Check
/// gate. Minted by `POST /readings/seasonal_check` and consumed by the grab save that names it,
/// which is held to exactly the values screened here. Never listed, so no router.
pub mod seasonal_check {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "seasonal_checks")]
    #[crudcrate(
        api_struct = "SeasonalCheck",
        name_singular = "seasonal_check",
        name_plural = "seasonal_checks"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        pub site_id: Uuid,
        /// The instant the screened values were entered at.
        pub checked_time: chrono::DateTime<chrono::Utc>,
        /// The screened `(parameter, value)` set, as `SeasonalCheckValue` rows. A save naming this
        /// check is refused for any pair absent here.
        #[sea_orm(column_type = "JsonBinary")]
        pub entries: serde_json::Value,
        pub created_by: Option<String>,
        #[crudcrate(exclude(create, update))]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::sites::models::Entity",
            from = "Column::SiteId",
            to = "crate::routes::private::sites::models::Column::Id"
        )]
        Site,
    }

    impl Related<crate::routes::private::sites::models::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Site.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// One set-level curation decision: an edit over a selection, materialised as the per-row
/// `reading_decisions` rows that carry its `set_id`. A set is reached through those rows and by the
/// rollback that names it, never listed, so no router.
/// A value the source changed after river-data stored it, awaiting a person (Q84). One live row
/// per `(stream_id, time, replicate_index)`, which is the table's unique key rather than its
/// primary one.
///
/// No router: whether the review queue becomes this entity's generated list or keeps its
/// three-table join is Q150, and C215 is that work.
/// One parsed CSV row, held only for the length of an import. `(import_token, seq)` is the key:
/// the importer numbers `seq` from zero within each token and the worker reads the set back in
/// that order.
///
/// No router: the table is the import job's own scratch space, written by the upload handler and
/// deleted by the job when it finishes or fails.
pub mod import_staging {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "csv_import_staging")]
    #[crudcrate(
        api_struct = "CsvImportStagingRow",
        name_singular = "csv_import_staging_row",
        name_plural = "csv_import_staging_rows"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key)]
        pub import_token: Uuid,
        /// The row's position in the uploaded file, which is what makes replicate numbering
        /// deterministic.
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key)]
        pub seq: i64,
        pub stream_id: Uuid,
        pub site_id: Option<Uuid>,
        pub parameter_id: Option<Uuid>,
        pub time: chrono::DateTime<chrono::FixedOffset>,
        pub raw_value: f64,
        pub sensor_id: Option<Uuid>,
        pub calibration_id: Option<Uuid>,
        pub deployment_id: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Where a chunked upload accumulates before the import parses it: one row per chunk, in the order
/// they arrived. Durable and shared, so a session survives an eviction and a chunk that reaches
/// another replica appends to the same file.
pub mod import_chunk {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "csv_import_chunks")]
    #[crudcrate(
        api_struct = "CsvImportChunk",
        name_singular = "csv_import_chunk",
        name_plural = "csv_import_chunks"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key)]
        pub session_id: Uuid,
        /// The chunk's position in the upload, which is the order the file is reassembled in.
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key)]
        pub seq: i32,
        pub chunk: String,
        /// The caller who opened the upload, as `AuthContext::label` names them; nobody else may
        /// append to it or import it.
        pub opened_by: String,
        #[crudcrate(exclude(create, update))]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod change_proposal {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "reading_change_proposals")]
    #[crudcrate(
        api_struct = "ReadingChangeProposal",
        name_singular = "reading_change_proposal",
        name_plural = "reading_change_proposals",
        generate_router,
        routes(read),
        operations = crate::routes::private::readings::service::ChangeProposalOperations
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub stream_id: Uuid,
        #[crudcrate(sortable)]
        pub time: chrono::DateTime<chrono::Utc>,
        pub replicate_index: i16,
        pub proposed_raw_value: f64,
        pub proposed_standard_curve_id: Option<Uuid>,
        pub stored_raw_value: f64,
        pub stored_standard_curve_id: Option<Uuid>,
        /// `pending` | `accepted` | `rejected`, a table CHECK.
        #[crudcrate(filterable)]
        pub status: String,
        #[crudcrate(exclude(create, update), sortable)]
        pub first_seen_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update), sortable)]
        pub last_seen_at: chrono::DateTime<chrono::Utc>,
        pub decided_by: Option<String>,
        pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
        /// The stream's own naming, and the slot its pairing places it in. Filled from the
        /// pairing on read; a proposal against an unpaired stream carries only the source half.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update))]
        pub source_system: Option<String>,
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update))]
        pub source_key: Option<String>,
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update))]
        pub site_id: Option<Uuid>,
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update))]
        pub site_name: Option<String>,
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update))]
        pub parameter_id: Option<Uuid>,
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update))]
        pub parameter_code: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::data_streams::Entity",
            from = "Column::StreamId",
            to = "crate::routes::private::data_streams::Column::Id"
        )]
        DataStream,
    }

    impl Related<crate::routes::private::data_streams::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::DataStream.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod decision_set {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "reading_decision_sets")]
    #[crudcrate(
        api_struct = "ReadingDecisionSet",
        name_singular = "reading_decision_set",
        name_plural = "reading_decision_sets"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        /// The decision kind, as `decisions::Kind` names it.
        pub kind: String,
        /// The `Selection` the set was recorded over, as stored JSON. A rollback re-derives the
        /// rows from this, so it is read back by the same type that wrote it.
        #[sea_orm(column_type = "JsonBinary")]
        pub selection: serde_json::Value,
        /// What the decision set the columns to.
        #[sea_orm(column_type = "JsonBinary")]
        pub new: serde_json::Value,
        pub actor: String,
        #[crudcrate(exclude(create, update))]
        pub at: chrono::DateTime<chrono::Utc>,
        pub reason: Option<String>,
        /// How many rows the set decided, written when it closes.
        pub rows_decided: i64,
        pub rolled_back_at: Option<chrono::DateTime<chrono::Utc>>,
        pub rolled_back_by: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// One reading whose curation columns are not the fold of its live decisions.
#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct CurationDriftRow {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::FixedOffset>,
    pub replicate_index: i16,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    /// The curation columns the reading holds.
    #[schema(value_type = Object)]
    pub stored: serde_json::Value,
    /// What the reading's live decisions fold to. A column absent from it is one no decision
    /// asserts, which is not a disagreement.
    #[schema(value_type = Object)]
    pub folded: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurationDriftResponse {
    /// Every disagreeing reading, not only the ones listed below.
    pub total: i64,
    /// The first `limit` of them, newest first, so a person can open one.
    pub rows: Vec<CurationDriftRow>,
}

/// How many drift rows to list beside the count.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct CurationDriftQuery {
    pub limit: Option<u32>,
}

/// The calibration a listed reading was corrected with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReadingCalibrationRef {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[schema(required)]
    pub valid_until: Option<DateTime<Utc>>,
}

/// The standard curve a listed reading names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReadingCurveRef {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
}
