//! Batch reading insert, and the write rules the other reading paths share.
//!
//! `admission` holds what every stored reading must satisfy, [`admit_standard_curves`] holds who
//! may name a hand-picked standard curve, and `readings_upsert` holds what an upsert may replace on
//! the row it collides with. `/ingest`, `/grab_samples` and the CSV importer call into them, so the
//! five write paths cannot drift apart on what they accept or overwrite.

use axum::{Json, extract::State};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, ExprTrait, QueryFilter, Set,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::service::get_or_create_api_stream;
use crate::routes::private::readings;
use crate::routes::private::readings::tail;
use crate::routes::private::sensors::calibrations;
use crate::routes::private::sensors::models::{ResolvedOwner};
use crate::routes::private::sensors::service::{resolve_slot_owner_for_times};
use crate::routes::private::sensors::standard_curves;

/// What a reading must satisfy to be stored, whichever path it arrived on.
///
/// The rules are here rather than in each handler because they were in each handler: the timestamp
/// bound existed on `/readings/batch` alone, no path rejected a non-finite value, and the
/// missing-value sentinel was recognised by its spelling in one branch of one importer.
pub mod admission {
    use chrono::{DateTime, Duration, Utc};
    use std::sync::LazyLock;

    use crate::error::{AppError, AppResult};
    use crate::routes::private::readings::measurement::measurement_type_rejection;

    /// Absolute, so an archive series does not age out of admissibility as the clock moves.
    const MIN_READING_TIME: &str = "2000-01-01T00:00:00Z";
    /// Logger clock skew. `last_data_time` only moves forward, so a far-future timestamp admitted
    /// here would latch a stream's cursor until wall-clock caught up.
    const MAX_LEAD_DAYS: i64 = 1;

    /// Missing-value marker the loggers and the portal exports write in place of a measurement.
    /// Compared numerically, so `-9999`, `-9999.0` and `-9999.00` are one marker.
    pub const MISSING_SENTINEL: f64 = -9999.0;
    const SENTINEL_TOLERANCE: f64 = 1e-9;

    /// The floor, parsed once. It is a compile-time literal, so parsing it per reading is pure
    /// overhead on a thousand-reading batch.
    static MIN_TIME: LazyLock<DateTime<Utc>> = LazyLock::new(|| {
        DateTime::parse_from_rfc3339(MIN_READING_TIME)
            .expect("MIN_READING_TIME is a literal RFC 3339 timestamp")
            .with_timezone(&Utc)
    });

    /// The window a reading's timestamp must fall in. Only the upper bound moves with `now`.
    pub fn window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        (*MIN_TIME, now + Duration::days(MAX_LEAD_DAYS))
    }

    /// Why this timestamp is not admissible against a caller-supplied `now`, or `None` when it is.
    ///
    /// The `_at` variants exist so a caller filtering a whole batch reads the clock once and every
    /// reading in that batch is judged against the same window: two readings a microsecond apart
    /// cannot land on opposite sides of a moving bound.
    pub fn time_rejection_at(now: DateTime<Utc>, time: DateTime<Utc>) -> Option<String> {
        let (min_time, max_time) = window(now);
        if time >= min_time && time <= max_time {
            return None;
        }
        Some(format!(
            "Reading timestamp {} is outside valid range ({} to {})",
            time.to_rfc3339(),
            min_time.to_rfc3339(),
            max_time.to_rfc3339(),
        ))
    }

    /// Why this value is not admissible, or `None` when it is. `NaN` and the infinities have no
    /// meaning as a measurement and blank every aggregate bucket they reach.
    pub fn value_rejection(raw_value: f64) -> Option<String> {
        if raw_value.is_finite() {
            return None;
        }
        Some(format!("{raw_value} is not a finite number"))
    }

    /// Why this replicate index is not admissible for this cadence, or `None` when it is.
    ///
    /// Only a spot instant has replicates. Every continuous and derived reader filters
    /// `replicate_index = 0`, and so do the four continuous aggregates, so a non-zero index on a
    /// non-spot row is stored and served nowhere.
    pub fn replicate_index_rejection(
        measurement_type: Option<&str>,
        replicate_index: i16,
    ) -> Option<String> {
        if replicate_index == 0
            || measurement_type == Some(crate::routes::private::readings::sample_groups::SPOT)
        {
            return None;
        }
        Some(format!(
            "Replicate index {replicate_index} is only valid on a spot reading; {} readings are served at index 0 alone",
            measurement_type
                .unwrap_or(river_data_core::models::MeasurementType::Continuous.as_str())
        ))
    }

    pub fn admit_replicate_index(
        measurement_type: Option<&str>,
        replicate_index: i16,
    ) -> AppResult<()> {
        replicate_index_rejection(measurement_type, replicate_index)
            .map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
    }

    pub fn admit_value(raw_value: f64) -> AppResult<()> {
        value_rejection(raw_value).map_or(Ok(()), |reason| {
            Err(AppError::BadRequest(format!("Reading value {reason}")))
        })
    }

    /// Why this reading is not admissible, or `None` when it is. The rules [`admit`] enforces,
    /// reported as a value for the callers that skip the reading rather than refuse the request.
    pub fn rejection(
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<String> {
        rejection_at(Utc::now(), time, raw_value, measurement_type)
    }

    /// [`rejection`] against a caller-supplied `now`.
    pub fn rejection_at(
        now: DateTime<Utc>,
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<String> {
        measurement_type_rejection(measurement_type)
            .or_else(|| time_rejection_at(now, time))
            .or_else(|| value_rejection(raw_value).map(|reason| format!("Reading value {reason}")))
    }

    /// Why a reading cannot be stored, as a closed set. The messages [`rejection`] returns carry
    /// the offending value and so cannot be grouped; a caller summarising a batch wants the kind.
    ///
    /// `UnknownCalibration` needs the calibration rows, so [`rejection_kind`] cannot decide it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RejectionKind {
        OutOfWindow,
        NonFinite,
        UnknownMeasurementType,
        UnknownCalibration,
        /// Two payload rows at one (time, replicate_index) key under a completeness window. The
        /// last occurrence wins (the backends emit source-id order, so last is deterministic);
        /// the losers are counted here so the receipt arithmetic still closes.
        DuplicateKey,
        /// A non-zero `replicate_index` on a reading whose resolved cadence is not spot. Only a
        /// spot instant has replicates; every continuous and derived reader, and all four
        /// continuous aggregates, filter `replicate_index = 0`.
        ReplicateIndexOnNonSpot,
    }

    impl RejectionKind {
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::OutOfWindow => "timestamp outside the admissible window",
                Self::NonFinite => "value is not a finite number",
                Self::UnknownMeasurementType => "measurement_type outside the vocabulary",
                Self::UnknownCalibration => "calibration_id names no calibration",
                Self::DuplicateKey => "duplicate (time, replicate_index) key in one payload",
                Self::ReplicateIndexOnNonSpot => "replicate_index is only valid on a spot reading",
            }
        }
    }

    /// Which kind of rejection applies, or `None` when the reading is admissible. Same order of
    /// precedence as [`rejection`].
    pub fn rejection_kind(
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<RejectionKind> {
        rejection_kind_at(Utc::now(), time, raw_value, measurement_type)
    }

    /// [`rejection_kind`] against a caller-supplied `now`.
    pub fn rejection_kind_at(
        now: DateTime<Utc>,
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> Option<RejectionKind> {
        if measurement_type_rejection(measurement_type).is_some() {
            Some(RejectionKind::UnknownMeasurementType)
        } else if time_rejection_at(now, time).is_some() {
            Some(RejectionKind::OutOfWindow)
        } else if value_rejection(raw_value).is_some() {
            Some(RejectionKind::NonFinite)
        } else {
            None
        }
    }

    /// The full admission check for one reading: classification vocabulary, timestamp bound,
    /// finite value.
    pub fn admit(
        time: DateTime<Utc>,
        raw_value: f64,
        measurement_type: Option<&str>,
    ) -> AppResult<()> {
        rejection(time, raw_value, measurement_type)
            .map_or(Ok(()), |reason| Err(AppError::BadRequest(reason)))
    }

    pub fn is_missing_sentinel(value: f64) -> bool {
        (value - MISSING_SENTINEL).abs() <= SENTINEL_TOLERANCE
    }

    /// What one delimited cell resolves to.
    #[derive(Debug, PartialEq)]
    pub enum Cell {
        Value(f64),
        /// Declared missing: empty, `NaN`/`NA`, or the sentinel. Contributes no reading and is not
        /// a row error.
        Missing,
        /// Unusable: unparseable, or parseable but not finite.
        Invalid(String),
    }

    /// Classify a cell by value, not by spelling. Declared missing markers are recognised before
    /// parsing (`NaN` is a marker, not a number), and the sentinel after it, so every spelling of
    /// `-9999` lands in the same branch.
    pub fn classify_cell(cell: &str) -> Cell {
        let cell = cell.trim();
        if cell.is_empty() || cell.eq_ignore_ascii_case("nan") || cell.eq_ignore_ascii_case("na") {
            return Cell::Missing;
        }
        let Ok(value) = cell.parse::<f64>() else {
            return Cell::Invalid(format!("'{cell}' is not a number"));
        };
        if value_rejection(value).is_some() {
            return Cell::Invalid(format!("'{cell}' is not a finite number"));
        }
        if is_missing_sentinel(value) {
            return Cell::Missing;
        }
        Cell::Value(value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn declared_missing_markers_carry_no_value_and_no_error() {
            for cell in ["", "   ", "NaN", "nan", "NA", "na"] {
                assert_eq!(classify_cell(cell), Cell::Missing, "cell {cell:?}");
            }
        }

        #[test]
        fn every_spelling_of_the_sentinel_is_the_same_marker() {
            for cell in ["-9999", "-9999.0", "-9999.00", " -9999.000 ", "-9.999e3"] {
                assert_eq!(classify_cell(cell), Cell::Missing, "cell {cell:?}");
            }
            assert!(is_missing_sentinel(-9999.0));
            assert!(!is_missing_sentinel(-9998.9));
            assert!(!is_missing_sentinel(9999.0));
        }

        #[test]
        fn a_value_next_to_the_sentinel_is_a_measurement() {
            assert_eq!(classify_cell("-9999.5"), Cell::Value(-9999.5));
            assert_eq!(classify_cell("-999.9"), Cell::Value(-999.9));
        }

        #[test]
        fn non_finite_cells_are_errors_rather_than_missing_values() {
            for cell in ["Inf", "inf", "-inf", "Infinity", "-Infinity"] {
                assert!(
                    matches!(classify_cell(cell), Cell::Invalid(_)),
                    "cell {cell:?} must be a row error"
                );
            }
        }

        #[test]
        fn unparseable_cells_are_errors() {
            assert!(matches!(classify_cell("n/a"), Cell::Invalid(_)));
            assert!(matches!(classify_cell("12,5"), Cell::Invalid(_)));
        }

        #[test]
        fn ordinary_cells_parse() {
            assert_eq!(classify_cell(" 12.5 "), Cell::Value(12.5));
            assert_eq!(classify_cell("0"), Cell::Value(0.0));
            assert_eq!(classify_cell("1e3"), Cell::Value(1000.0));
        }

        #[test]
        fn non_finite_values_are_refused_on_every_path() {
            assert!(admit_value(f64::NAN).is_err());
            assert!(admit_value(f64::INFINITY).is_err());
            assert!(admit_value(f64::NEG_INFINITY).is_err());
            assert!(admit_value(0.0).is_ok());
            assert!(admit_value(MISSING_SENTINEL).is_ok());
        }

        #[test]
        fn the_timestamp_window_holds_at_its_edges_and_refuses_beyond_them() {
            let now = Utc::now();
            let (min_time, max_time) = window(now);
            assert!(time_rejection_at(now, now).is_none());
            assert!(time_rejection_at(now, min_time + Duration::minutes(1)).is_none());
            assert!(time_rejection_at(now, max_time - Duration::minutes(1)).is_none());
            assert!(time_rejection_at(now, min_time - Duration::days(1)).is_some());
            assert!(time_rejection_at(now, max_time + Duration::days(1)).is_some());
        }

        /// Expected behaviour: the lead bound moves with the clock, so a file's rows are judged
        /// against one reading of it. Judged a millisecond later, this timestamp changes answer.
        #[test]
        fn a_timestamp_just_past_the_lead_bound_turns_on_which_clock_read_judges_it() {
            let now = Utc::now();
            let just_past = now + Duration::days(MAX_LEAD_DAYS) + Duration::microseconds(500);
            assert!(time_rejection_at(now, just_past).is_some());
            assert!(time_rejection_at(now + Duration::milliseconds(1), just_past).is_none());
        }

        /// Expected behaviour: the floor is a fixed date, so a decade-old archive series stays
        /// ingestible indefinitely. A relative floor would make the same reading admissible today
        /// and refused later, which is what stalls a portal backfill.
        #[test]
        fn the_backward_bound_does_not_move_with_the_clock() {
            let (early, _) = window("2020-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
            let (late, _) = window("2099-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
            assert_eq!(early, late);

            let archive = "2016-08-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
            assert!(time_rejection_at(Utc::now(), archive).is_none());
        }

        /// Expected behaviour: `rejection` and `admit` are one implementation, so the reason a
        /// reading is skipped on `/ingest` is the reason it would be refused elsewhere.
        #[test]
        fn rejection_reports_what_admit_raises() {
            let now = Utc::now();
            let (_, max_time) = window(now);

            assert_eq!(rejection(now, 1.0, None), None);
            assert_eq!(rejection(now, MISSING_SENTINEL, Some("spot")), None);

            for (time, value, declared) in [
                (max_time + Duration::days(2), 1.0, None),
                (now, f64::NAN, None),
                (now, 1.0, Some("grab")),
            ] {
                let reason = rejection(time, value, declared);
                assert!(reason.is_some(), "expected a reason for {declared:?}");
                assert!(admit(time, value, declared).is_err());
            }
        }

        #[test]
        fn the_classification_vocabulary_is_closed() {
            let now = Utc::now();
            for declared in [None, Some("continuous"), Some("spot"), Some("derived")] {
                assert!(admit(now, 1.0, declared).is_ok(), "declared {declared:?}");
            }
            assert!(admit(now, 1.0, Some("grab")).is_err());
        }
    }
}

/// What one reading claims about the standard curve that corrected it, as its writer resolved it.
#[derive(Debug, Clone, Copy)]
pub struct CurveClaim<'a> {
    pub standard_curve_id: Uuid,
    /// The instrument the reading is attributed to, after slot-owner resolution.
    pub sensor_id: Option<Uuid>,
    /// The reading's classification, after the resolution chain.
    pub measurement_type: &'a str,
}

/// Classification a hand-picked curve belongs to: a curve is fitted for one measurement, and a
/// logger cadence has no such measurement to pick it for.
const CURVE_MEASUREMENT_TYPE: &str = river_data_core::models::MeasurementType::Spot.as_str();

/// The standard curves a request names, refused unless every reading naming one may carry it.
///
/// A curve is fitted on one instrument and chosen by hand for one measurement, so a reading may
/// name it only when the reading is that instrument's own spot measurement. Four claims are
/// refused: an id no curve carries, a curve fitted on a different instrument, a reading that names
/// no instrument at all, and a reading classified as anything but spot. The reference is not
/// decoration: it freezes the curve against edits and deletion, and it is what a served value
/// claims to have been corrected by.
///
/// Returns the curves so the caller computes the corrected value from the coefficients. A submitted
/// `calibrated_value` cannot be checked against a curve, only recomputed from it, so no path trusts
/// one alongside a curve reference.
pub async fn admit_standard_curves(
    db: &DatabaseConnection,
    claims: &[CurveClaim<'_>],
) -> AppResult<HashMap<Uuid, standard_curves::Model>> {
    if claims.is_empty() {
        return Ok(HashMap::new());
    }

    let ids: Vec<Uuid> = claims.iter().map(|c| c.standard_curve_id).collect();
    let curves: HashMap<Uuid, standard_curves::Model> = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();

    for claim in claims {
        let id = claim.standard_curve_id;
        let Some(curve) = curves.get(&id) else {
            return Err(AppError::BadRequest(format!(
                "Standard curve {id} not found"
            )));
        };
        match claim.sensor_id {
            Some(sensor_id) if sensor_id == curve.sensor_id => {}
            Some(sensor_id) => {
                return Err(AppError::BadRequest(format!(
                    "Standard curve {id} was fitted on instrument {}, not on {sensor_id}",
                    curve.sensor_id
                )));
            }
            None => {
                return Err(AppError::BadRequest(format!(
                    "Standard curve {id} was fitted on instrument {}, which this reading does not \
                     name",
                    curve.sensor_id
                )));
            }
        }
        if claim.measurement_type != CURVE_MEASUREMENT_TYPE {
            return Err(AppError::BadRequest(format!(
                "Standard curve {id} corrects a {CURVE_MEASUREMENT_TYPE} measurement, and this \
                 reading is classified '{}'",
                claim.measurement_type
            )));
        }
    }

    Ok(curves)
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

const BATCH_SIZE: usize = 1000;

/// What an upsert may replace on the row it collides with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// Keep the stored row, drop the incoming one.
    Nothing,
    /// The measurement and its classification, ie. an operator or source correction.
    Values,
    /// The measurement plus the row's attribution, ie. a sync re-send that also re-resolves which
    /// site, parameter, sensor, calibration and deployment the row belongs to. The standard curve
    /// is never among them: it is chosen by hand per measurement and no query can recover it, so a
    /// re-send that names none leaves the stored one standing.
    ValuesAndAttribution,
}

impl From<ConflictMode> for Replace {
    fn from(mode: ConflictMode) -> Self {
        match mode {
            ConflictMode::Skip => Replace::Nothing,
            ConflictMode::Overwrite => Replace::Values,
        }
    }
}

/// Build the `ON CONFLICT` clause for the readings PK, shared by every path that upserts a
/// reading.
///
/// Operator state is never replaced: `is_flagged` and `flag_reason` are left out entirely, and the
/// sample link is written as `COALESCE(EXCLUDED.sample_id, readings.sample_id)` so a correction
/// that carries only a value keeps the reading inside its sample. Writing the incoming NULL there
/// would fire the samples refresh trigger with the replicate removed, and the refresh deletes a
/// `samples` row nothing references any more, taking its label, notes and created_by with it.
pub(crate) fn readings_upsert(replace: Replace) -> sea_orm::sea_query::OnConflict {
    let mut clause = sea_orm::sea_query::OnConflict::columns([
        readings::Column::StreamId,
        readings::Column::Time,
        readings::Column::ReplicateIndex,
    ]);
    match replace {
        Replace::Nothing => {
            clause.do_nothing();
        }
        Replace::Values | Replace::ValuesAndAttribution => {
            clause.update_columns([
                readings::Column::RawValue,
                readings::Column::CalibratedValue,
                readings::Column::MeasurementType,
            ]);
            if replace == Replace::ValuesAndAttribution {
                clause.update_columns([
                    readings::Column::SiteId,
                    readings::Column::ParameterId,
                    readings::Column::SensorId,
                    readings::Column::CalibrationId,
                    readings::Column::DeploymentId,
                ]);
                // A hand-picked curve outlives any correction that does not name one of its own.
                clause.value(
                    readings::Column::StandardCurveId,
                    sea_orm::sea_query::Expr::cust(
                        r#"COALESCE(EXCLUDED.standard_curve_id, "readings"."standard_curve_id")"#,
                    ),
                );
            }
            clause.value(
                readings::Column::SampleId,
                sea_orm::sea_query::Expr::cust(
                    r#"COALESCE(EXCLUDED.sample_id, "readings"."sample_id")"#,
                ),
            );
            // Arrival time follows the value: an overwrite that changes nothing keeps the
            // original stamp, so a full-content re-assert pass does not claim every row
            // arrived today.
            clause.value(
                readings::Column::IngestedAt,
                sea_orm::sea_query::Expr::cust(
                    r#"CASE WHEN "readings"."raw_value" IS DISTINCT FROM EXCLUDED.raw_value
                        OR "readings"."calibrated_value" IS DISTINCT FROM EXCLUDED.calibrated_value
                        THEN NOW() ELSE "readings"."ingested_at" END"#,
                ),
            );
        }
    }
    clause.to_owned()
}

/// The upsert clause for a request-level `conflict` mode.
pub(crate) fn readings_on_conflict(mode: ConflictMode) -> sea_orm::sea_query::OnConflict {
    readings_upsert(mode.into())
}

/// Batch insert readings keyed by (site_id, parameter_id). Auto-creates "api" streams when
/// a (site, parameter) pair has none. 10MB body limit. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/batch",
    request_body = BatchReadingsRequest,
    responses(
        (status = 200, description = "Inserted count", body = BatchReadingsResponse),
        (status = 400, description = "Timestamp outside the admissible window, or non-finite value"),
        (status = 413, description = "Body exceeds 10MB limit"),
    ),
    tag = "ingestion"
)]
pub async fn insert_batch_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BatchReadingsRequest>,
) -> AppResult<Json<BatchReadingsResponse>> {
    // A project-scoped token may only write to sites within its project.
    let target_sites: Vec<Uuid> = payload.readings.iter().map(|r| r.site_id).collect();
    enforce_project_scope_for_sites(&state.db, &scope, &target_sites).await?;

    for r in &payload.readings {
        admission::admit(r.time, r.raw_value, r.measurement_type.as_deref())?;
        if let Some(calibrated) = r.calibrated_value {
            admission::admit_value(calibrated)?;
        }
    }

    // Collect unique (site_id, parameter_id) pairs and resolve stream_ids
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    // Whether each pair names a slot that exists. A reading is attributed by the pairing, never by
    // the request alone: a caller naming a parameter the site has not been assigned would otherwise
    // mint attribution no `site_parameters` row backs, which every later resolution reads as wrong.
    let mut slot_exists: HashMap<(Uuid, Uuid), bool> = HashMap::new();

    for r in &payload.readings {
        let key = (r.site_id, r.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, r.site_id, r.parameter_id).await?;
            entry.insert(stream_id);
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = slot_exists.entry(key) {
            let paired = crate::routes::private::data_streams::service::site_parameter_of(
                &state.db,
                r.site_id,
                r.parameter_id,
            )
            .await?
            .is_some();
            entry.insert(paired);
        }
    }

    // Collect unique (site_id, time) pairs for derived auto-compute
    let site_timestamps_for_derived: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = {
        let mut map: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            map.entry(r.site_id).or_default().push(r.time);
        }
        for timestamps in map.values_mut() {
            timestamps.sort();
            timestamps.dedup();
        }
        map
    };

    // For rows that don't carry an explicit sensor, resolve it from the deployment window covering
    // the time so batch-inserted data lands attributed. Explicit payload values always win.
    let mut owner_map: HashMap<(Uuid, Uuid, chrono::DateTime<chrono::Utc>), ResolvedOwner> =
        HashMap::new();
    {
        let mut times_by_slot: HashMap<(Uuid, Uuid), Vec<chrono::DateTime<chrono::Utc>>> =
            HashMap::new();
        for r in &payload.readings {
            if r.sensor_id.is_none() {
                times_by_slot
                    .entry((r.site_id, r.parameter_id))
                    .or_default()
                    .push(r.time);
            }
        }
        for ((site, param), ts) in &times_by_slot {
            let resolved = resolve_slot_owner_for_times(&state.db, *site, *param, ts).await?;
            for (t, owner) in resolved {
                owner_map.insert((*site, *param, t), owner);
            }
        }
    }

    // Stream-declared defaults: a retagged "api" stream must classify batch writes the same
    // way it classifies /ingest writes. The channel's instrument travels with them: it is what a
    // row naming neither an instrument of its own nor a deployed one is attributed to, so a batch
    // write cannot store a measurement whose instrument is unknown.
    let mut stream_sensors: HashMap<Uuid, Uuid> = HashMap::new();
    let stream_defaults: HashMap<Uuid, Option<String>> = {
        let stream_ids: Vec<Uuid> = stream_cache.values().copied().collect();
        let mut map = HashMap::with_capacity(stream_ids.len());
        for stream in data_streams::Entity::find()
            .filter(data_streams::Column::Id.is_in(stream_ids))
            .all(&state.db)
            .await?
        {
            if let Some(sensor_id) = stream.sensor_id {
                stream_sensors.insert(stream.id, sensor_id);
            }
            map.insert(stream.id, stream.measurement_type);
        }
        map
    };

    // Sensor-frequency defaults for readings that don't declare a measurement_type: explicit
    // payload sensors plus slot-owner-resolved ones, one query.
    let sensor_types = {
        let mut candidate_sensors: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(owner_map.values().filter_map(|o| o.sensor_id))
            .collect();
        candidate_sensors.sort_unstable();
        candidate_sensors.dedup();
        crate::routes::private::readings::measurement::measurement_types_for_sensors(
            &state.db,
            &candidate_sensors,
        )
        .await?
    };

    // Per-reading context, resolved before the models are built: which stream the row lands on,
    // which instrument it inherits when it names none, and what it classifies as. The standard
    // curve rules below are stated over these resolved values rather than the submitted ones.
    struct Resolved {
        stream_id: Uuid,
        owner: ResolvedOwner,
        measurement_type: String,
    }
    let resolved: Vec<Resolved> = payload
        .readings
        .iter()
        .map(|r| {
            let stream_id = stream_cache[&(r.site_id, r.parameter_id)];
            let owner = if r.sensor_id.is_none() {
                owner_map
                    .get(&(r.site_id, r.parameter_id, r.time))
                    .cloned()
                    .unwrap_or_default()
            } else {
                ResolvedOwner::default()
            };
            let measurement_type =
                crate::routes::private::readings::measurement::resolve_measurement_type(
                    r.measurement_type.as_deref(),
                    stream_defaults.get(&stream_id).and_then(|d| d.as_deref()),
                    r.sensor_id.or(owner.sensor_id),
                    &sensor_types,
                );
            Resolved {
                stream_id,
                owner,
                measurement_type,
            }
        })
        .collect();

    // A caller-supplied standard curve is held to the same rule as a grab entry: the reading must
    // be that instrument's own spot measurement, and the corrected value is computed here from the
    // curve rather than taken from the request.
    let claims: Vec<CurveClaim<'_>> = payload
        .readings
        .iter()
        .zip(&resolved)
        .filter_map(|(r, res)| {
            r.standard_curve_id.map(|id| CurveClaim {
                standard_curve_id: id,
                sensor_id: r.sensor_id.or(res.owner.sensor_id),
                measurement_type: &res.measurement_type,
            })
        })
        .collect();
    let standard_curve_models = admit_standard_curves(&state.db, &claims).await?;

    // The base calibrations those rows sit on, so the stored value is the one the pair of recorded
    // curves produces: instrument correction first, hand-picked curve on its result. Rows whose
    // caller supplied no value need their base too: stamping the id while leaving the value
    // uncorrected would claim a calibration the number never went through.
    let base_calibrations: HashMap<Uuid, calibrations::service::Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .zip(&resolved)
            .filter(|(r, _)| r.standard_curve_id.is_some() || r.calibrated_value.is_none())
            .filter_map(|(r, res)| r.calibration_id.or(res.owner.calibration_id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            HashMap::new()
        } else {
            calibrations::Entity::find()
                .filter(calibrations::Column::Id.is_in(ids))
                .all(&state.db)
                .await?
                .into_iter()
                .map(|c| {
                    (
                        c.id,
                        calibrations::service::Curve {
                            id: c.id,
                            slope: c.slope,
                            intercept: c.intercept,
                        },
                    )
                })
                .collect()
        }
    };

    // The cadence a reading is stored under is resolved, not declared, so the replicate index is
    // judged against the resolved value rather than the request's.
    for (r, res) in payload.readings.iter().zip(&resolved) {
        admission::admit_replicate_index(
            Some(res.measurement_type.as_str()),
            r.replicate_index.unwrap_or(0),
        )?;
    }

    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .zip(resolved)
        .map(|(r, res)| {
            let Resolved {
                stream_id,
                owner,
                measurement_type,
            } = res;
            let calibration_id = r.calibration_id.or(owner.calibration_id);
            let attributed = slot_exists
                .get(&(r.site_id, r.parameter_id))
                .copied()
                .unwrap_or(false);
            let standard = r.standard_curve_id.map(|id| {
                let c = &standard_curve_models[&id];
                calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            let base = calibration_id.and_then(|id| base_calibrations.get(&id).copied());
            // A curve claim is computed here; a caller-supplied value is kept as claimed; a row
            // with neither gets its resolved base applied, so the stamped calibration_id is always
            // a curve the stored value went through.
            let calibrated_value = match (standard, r.calibrated_value) {
                (Some(curve), _) => Some(calibrations::service::apply_curves(
                    r.raw_value,
                    base,
                    Some(curve),
                )),
                (None, Some(value)) => Some(value),
                (None, None) => base.map(|c| c.apply(r.raw_value)),
            };
            readings::ActiveModel {
                standard_curve_id: Set(r.standard_curve_id),
                provenance_kind: Set(Some("batch".to_string())),
                site_id: Set(attributed.then_some(r.site_id)),
                parameter_id: Set(attributed.then_some(r.parameter_id)),
                calibrated_value: Set(calibrated_value),
                sensor_id: Set(r
                    .sensor_id
                    .or(owner.sensor_id)
                    .or_else(|| stream_sensors.get(&stream_id).copied())),
                calibration_id: Set(calibration_id),
                deployment_id: Set(r.deployment_id.or(owner.deployment_id)),
                measurement_type: Set(Some(measurement_type)),
                sample_id: Set(r.sample_id),
                ..readings::new(
                    stream_id,
                    r.time.into(),
                    r.replicate_index.unwrap_or(0),
                    r.raw_value,
                )
            }
        })
        .collect();

    let total = models.len();
    let inserted: usize;
    let overwritten: usize;
    let conflict = payload.conflict;

    // One guarded transaction for every chunk: an overwrite rewrites stored rows, which on a
    // hypertable older than the compression policy means decompressing them, and the per-statement
    // cap refuses that outside a transaction that lifts it. Chunking stays, so the statement size
    // is bounded; the transaction is what makes a part-written correction impossible.
    let actor = crate::common::actor::label(&auth);
    // A correction is recorded as what made it. A batch is usually a person or a script acting for
    // one; a sync service reaching the same route is recorded as sync.
    let origin = auth.origin();
    let touched_events;
    (inserted, overwritten, touched_events) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let mut inserted = 0usize;
        let mut overwritten = 0usize;
        for chunk in models.chunks(BATCH_SIZE) {
            // In overwrite mode `rows_affected` counts both inserts and updates, so the count of
            // keys already present (looked up before the write) separates the inserts. Only the
            // rows whose value the write changes count as overwritten, the same definition the
            // CSV import reports; those are exactly the value corrections recorded (ADR 0008).
            let (pre_existing, changed) = if conflict == ConflictMode::Overwrite {
                let corrections =
                    crate::routes::private::readings::decisions::record_value_corrections(
                        txn,
                        chunk,
                        &actor,
                        origin,
                    )
                    .await?;
                (
                    count_existing(txn, chunk).await?,
                    usize::try_from(corrections.rows).unwrap_or(usize::MAX),
                )
            } else {
                (0, 0)
            };

            match readings::Entity::insert_many(chunk.to_vec())
                .on_conflict(readings_on_conflict(conflict))
                .exec_without_returning(txn)
                .await
            {
                Ok(rows) => {
                    let affected = rows as usize;
                    inserted += affected.saturating_sub(pre_existing);
                    overwritten += changed;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("None of the records") {
                        // All duplicates in this chunk
                    } else {
                        tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                        return Err(crate::error::AppError::Database(e));
                    }
                }
            }
        }
        // A hand-picked curve on a batch row is a claim, recorded once (ADR 0008).
        crate::routes::private::readings::decisions::record_curve_claims(
            txn,
            &models,
            &actor,
            origin,
        )
        .await?;

        // Attributed spot rows this batch landed belong to collection events (D7). A batch caller
        // is a person or a script acting for one, so the events are manual.
        let spot_times: Vec<chrono::DateTime<chrono::FixedOffset>> = models
            .iter()
            .filter(|m| {
                matches!(&m.measurement_type,
                    sea_orm::ActiveValue::Set(Some(t)) if t == readings::sample_groups::SPOT)
            })
            .map(|m| *m.time.as_ref())
            .collect();
        let mut touched_events = Vec::new();
        if let (Some(first), Some(last)) = (
            spot_times.iter().min().copied(),
            spot_times.iter().max().copied(),
        ) {
            let mut stream_ids: Vec<Uuid> =
                models.iter().map(|m| *m.stream_id.as_ref()).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let row_predicate = "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3";
            let binds: Vec<sea_orm::Value> =
                vec![stream_ids.clone().into(), first.into(), last.into()];
            crate::routes::private::collection_events::service::attach_collection_events(
                txn,
                row_predicate,
                binds.clone(),
                crate::routes::private::collection_events::service::EventSource::Manual,
            )
            .await?;
            // A replicate landing beside one already stored makes the instant a group, whichever
            // path wrote either row, so the batch goes through the one materialiser too.
            readings::sample_groups::materialise_samples(txn, row_predicate, binds.clone()).await?;
            let mut instants = spot_times.clone();
            instants.sort_unstable();
            instants.dedup();
            touched_events =
                crate::routes::private::collection_events::flows::touched_events(
                    txn,
                    "r.stream_id = ANY($1) AND r.time = ANY($2)",
                    vec![stream_ids.into(), instants.into()],
                )
                .await?;
        }

        Ok((inserted, overwritten, touched_events))
    })
    .await?;

    // The values have landed; the calculations that read them run without anyone asking
    // (ADR 0007). What they are is reported back alongside the counts.
    let calculations = {
        let mut touched: Vec<Uuid> = touched_events
            .iter()
            .filter(|e| e.source != "portal_sync")
            .flat_map(|e| e.parameter_ids.iter().copied())
            .collect();
        touched.sort_unstable();
        touched.dedup();
        crate::routes::private::tools::service::calculations_fed_by(&state.db, &touched).await?
    };

    // An overwrite replaces the measurement, not the correction: the write keeps the stored curve
    // references, so the value is recomputed from exactly those curves, the same as the CSV
    // overwrite path. Without this the row carries a value its recorded curves did not produce
    // until the janitor sweep catches it.
    if conflict == ConflictMode::Overwrite && overwritten > 0 {
        let mut stream_ids: Vec<Uuid> = models.iter().map(|m| *m.stream_id.as_ref()).collect();
        stream_ids.sort_unstable();
        stream_ids.dedup();
        let times: Vec<chrono::DateTime<chrono::FixedOffset>> =
            models.iter().map(|m| *m.time.as_ref()).collect();
        if let (Some(first), Some(last)) = (times.iter().min(), times.iter().max()) {
            calibrations::service::recompose_from_own_curves_guarded(
                &state.db,
                "TRUE",
                "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3",
                vec![stream_ids.into(), (*first).into(), (*last).into()],
            )
            .await?;
        }
    }

    tracing::debug!(
        total,
        inserted,
        overwritten,
        "Batch readings insert complete"
    );

    let earliest = site_timestamps_for_derived
        .values()
        .flatten()
        .min()
        .copied();
    let latest = site_timestamps_for_derived
        .values()
        .flatten()
        .max()
        .copied();

    // Auto-compute derived values for affected sites, tracked as a job. Spawn-guard: keep only
    // sites with an active derived parameter, others would compute nothing.
    if inserted > 0 || overwritten > 0 {
        let mut derived_sites: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for (site_id, timestamps) in &site_timestamps_for_derived {
            if crate::routes::private::parameters::derived::janitor::site_has_active_derived(
                &state.db, *site_id,
            )
            .await
            .unwrap_or(true)
            {
                derived_sites.insert(*site_id, timestamps.clone());
            }
        }
        if !derived_sites.is_empty() {
            let site_timestamps: Vec<serde_json::Value> = derived_sites
                .iter()
                .map(|(site_id, timestamps)| {
                    serde_json::json!({ "site_id": site_id, "timestamps": timestamps })
                })
                .collect();
            crate::routes::private::reprocessing_jobs::worker::enqueue(
                &state.db,
                "batch_derived",
                None,
                None,
                &serde_json::json!({ "site_timestamps": site_timestamps }),
                None,
            )
            .await?;
        }
    }

    // An overwrite replaced values the rollups have already materialised; an insert appended past
    // them, which the next scheduled refresh covers. Best-effort: the rows are committed, so a
    // refresh losing a lock to the janitor must not report a write that happened as one that did
    // not. Episodes go to the `alarm_backfill` job because a batch can span a long window.
    let written = tail::Written::new(u64::try_from(inserted + overwritten).unwrap_or(u64::MAX))
        .over(earliest.zip(latest))
        .at(stream_cache
            .iter()
            .map(|((site_id, parameter_id), stream_id)| {
                tail::Slot::paired(*site_id, *parameter_id).through(*stream_id)
            })
            .collect())
        .touching(touched_events);
    tail::run(
        &state,
        &written,
        &tail::Axes {
            cache: tail::Cache::Sites,
            refresh: if overwritten > 0 {
                tail::Refresh::Range { fatal: false }
            } else {
                tail::Refresh::Skip
            },
            announce: true,
            reconcile_alarms: true,
            episodes: tail::Episodes::Job,
            recompute_derived: false,
            writer: crate::routes::private::collection_events::flows::Writer::Person,
        },
        &crate::common::actor::label(&auth),
    )
    .await?;

    Ok(Json(BatchReadingsResponse {
        inserted,
        overwritten,
        calculations,
    }))
}

/// Count how many of the chunk's (stream_id, time, replicate_index) keys already exist, so the
/// caller can split `rows_affected` into inserts vs overwrites in `overwrite` mode.
async fn count_existing<C: ConnectionTrait>(
    db: &C,
    chunk: &[readings::ActiveModel],
) -> AppResult<usize> {
    use sea_orm::{ColumnTrait, Condition, QueryFilter, QuerySelect, sea_query::Expr};

    if chunk.is_empty() {
        return Ok(0);
    }

    let mut condition = Condition::any();
    for m in chunk {
        let (
            sea_orm::ActiveValue::Set(stream_id),
            sea_orm::ActiveValue::Set(time),
            sea_orm::ActiveValue::Set(rep),
        ) = (
            m.stream_id.clone(),
            m.time.clone(),
            m.replicate_index.clone(),
        )
        else {
            continue;
        };
        condition = condition.add(
            Condition::all()
                .add(readings::Column::StreamId.eq(stream_id))
                .add(readings::Column::Time.eq(time))
                .add(readings::Column::ReplicateIndex.eq(rep)),
        );
    }

    let count = readings::Entity::find()
        .select_only()
        .column_as(Expr::col(readings::Column::StreamId).count(), "n")
        .filter(condition)
        .into_tuple::<i64>()
        .one(db)
        .await?
        .unwrap_or(0);

    Ok(usize::try_from(count).unwrap_or(0))
}

#[cfg(test)]
#[path = "tests/batch.rs"]
mod upsert_rules;
