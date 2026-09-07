use axum::{Json, extract::State};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Set,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::routes::private::sync::replicate_audit as audit;
use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::recompute;
use crate::routes::private::readings::batch::{
    CurveClaim, Replace, admission, admit_standard_curves, readings_upsert,
};
use crate::routes::private::readings::decisions;
use crate::routes::private::readings::tail;
use crate::routes::private::{
    data_streams, readings, readings::sample_groups, readings::samples, readings::sd_estimator,
    sensors::calibrations, sites, sites::parameters as site_parameters,
};

/// Grabs are spot measurements by definition: a bottle, not a logger cadence.
const GRAB_MEASUREMENT_TYPE: &str = "spot";

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrabSampleRequest {
    pub site_id: Uuid,
    pub created_by: Option<String>,
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
    /// When present, every reading's (parameter, value) must have been screened by that check —
    /// the portal's "any edit resets Check", enforced server-side. The check itself is advisory;
    /// naming a check that does not cover the values is refused.
    #[serde(default)]
    pub check_id: Option<Uuid>,
    /// Which divisor the samples this request creates compute their standard deviation with:
    /// `sample` (n-1) or `population` (n). Present when a tool's manifest fixes it or its operator
    /// chose one; omitted, the slot's declaration decides, and absent that the group is recorded
    /// undeclared. It never changes a group that already exists: the estimator a stored sample was
    /// computed with is changed through the audit resolution or the retag job, where the decision
    /// is recorded.
    #[serde(default)]
    pub sd_estimator: Option<String>,
    pub readings: Vec<GrabSampleReading>,
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
    /// The named `replicates` input of the referenced tool run this reading stores: the measured
    /// value the run consumed at `replicate_index`, stored raw. A curve the run applied is not a
    /// correction of this row, so `standard_curve_id` is admitted here and the database applies
    /// it (ADR 0003).
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
    /// Rows removed by `mode: replace` before the insert. Only the grab stream's own rows at the
    /// instant are candidates; rows another source wrote at the same slot and time are untouched.
    pub replaced: usize,
    /// Curated rows on the grab stream that `mode: replace` left in place: flagged, withdrawn, or
    /// carrying a standard curve the request did not supply. Each group with one raises a
    /// `source_modified` hold, and the value entered at that replicate index is not written.
    #[serde(default)]
    pub kept_curated: usize,
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
    pub calculations: Vec<crate::routes::private::tools::closure::CalculationImpact>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveApplication {
    pub id: Uuid,
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
    pub base_calibration: Option<CurveApplication>,
    /// The operator's hand-picked curve, applied to the base's output.
    pub standard_curve: Option<CurveApplication>,
    /// Both curves folded into one line, present when both apply.
    pub composed_equation: Option<String>,
    pub calibrated_value: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExistingReplicate {
    pub replicate_index: i16,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub standard_curve_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExistingGroup {
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub replicates: Vec<ExistingReplicate>,
}

/// The line as the operator reads it, sign folded into the operator: `y = 2x - 3`.
fn equation(slope: f64, intercept: f64) -> String {
    if intercept < 0.0 {
        format!("y = {slope}x - {}", -intercept)
    } else {
        format!("y = {slope}x + {intercept}")
    }
}

/// Replicate indices per (parameter, time) group: either every index in a group is explicit and
/// unique, or none is and the group numbers from 0. A mix would renumber around the explicit rows
/// and a duplicate would silently drop a measurement, so both are refused. Once written an index
/// is never renumbered.
fn assign_replicate_indices(readings: &[GrabSampleReading]) -> Result<Vec<i16>, AppError> {
    let mut groups: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Vec<usize>> = HashMap::new();
    for (i, r) in readings.iter().enumerate() {
        groups.entry((r.parameter_id, r.time)).or_default().push(i);
    }

    let mut indices = vec![0i16; readings.len()];
    for ((parameter_id, time), members) in groups {
        let explicit: Vec<Option<i16>> = members
            .iter()
            .map(|&i| readings[i].replicate_index)
            .collect();
        if explicit.iter().all(Option::is_some) {
            let mut seen = std::collections::HashSet::new();
            for (&i, idx) in members.iter().zip(&explicit) {
                let idx = idx.expect("all explicit");
                if !seen.insert(idx) {
                    return Err(AppError::Conflict(format!(
                        "Replicate index {idx} appears twice for parameter {parameter_id} at {time}"
                    )));
                }
                indices[i] = idx;
            }
        } else if explicit.iter().all(Option::is_none) {
            for (n, &i) in members.iter().enumerate() {
                indices[i] = i16::try_from(n).map_err(|_| {
                    AppError::BadRequest(format!(
                        "Too many replicates for parameter {parameter_id} at {time}"
                    ))
                })?;
            }
        } else {
            return Err(AppError::BadRequest(format!(
                "Replicate indices for parameter {parameter_id} at {time} mix explicit and \
                 automatic; send all of them or none"
            )));
        }
    }
    Ok(indices)
}

/// The spot rows already stored at each requested (parameter, time), across every stream feeding
/// the slot, so a CSV-imported grab and a hand-entered one count as the same group.
async fn fetch_existing_groups(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    groups: &[(Uuid, chrono::DateTime<chrono::Utc>)],
) -> Result<Vec<ExistingGroup>, AppError> {
    let mut out = Vec::new();
    for (parameter_id, time) in groups {
        let rows = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT replicate_index, raw_value, calibrated_value, standard_curve_id
                  FROM readings
                  WHERE site_id = $1 AND parameter_id = $2 AND time = $3
                    AND measurement_type = 'spot'
                  ORDER BY replicate_index",
                [site_id.into(), (*parameter_id).into(), (*time).into()],
            ))
            .await?;
        if rows.is_empty() {
            continue;
        }
        let replicates = rows
            .iter()
            .map(|row| {
                Ok(ExistingReplicate {
                    replicate_index: row.try_get("", "replicate_index")?,
                    raw_value: row.try_get("", "raw_value")?,
                    calibrated_value: row.try_get("", "calibrated_value")?,
                    standard_curve_id: row.try_get("", "standard_curve_id")?,
                })
            })
            .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
        out.push(ExistingGroup {
            parameter_id: *parameter_id,
            time: *time,
            replicates,
        });
    }
    Ok(out)
}

/// Get or create a "grab_sample" stream for a given (site_id, parameter_id) pair.
async fn get_or_create_grab_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    site_parameter_id: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        // Auto-pair existing unpaired stream
        if stream.site_parameter_id.is_none()
            && let Some(sp_id) = site_parameter_id
        {
            let mut active: data_streams::ActiveModel = stream.clone().into();
            active.site_parameter_id = Set(Some(sp_id));
            active.paired_at = Set(Some(chrono::Utc::now().into()));
            active.updated_at = Set(chrono::Utc::now().into());
            active.update(db).await?;
        }
        crate::routes::private::sensors::operations::ensure_channel_instrument(
            db,
            &stream,
            site_id,
            parameter_id,
            "grab entry",
        )
        .await?;
        return Ok(stream.id);
    }

    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let model = data_streams::ActiveModel {
        id: Set(id),
        source_system: Set("grab_sample".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some("Grab sample".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        site_parameter_id: Set(site_parameter_id),
        sensor_id: Set(None),
        measurement_type: Set(Some("spot".to_string())),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(site_parameter_id.map(|_| now.into())),
        last_data_time: Set(None),
        last_window_digest: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    data_streams::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                data_streams::Column::SourceSystem,
                data_streams::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    let stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(format!("{site_id}:{parameter_id}")))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create grab sample stream".to_string()))?;

    crate::routes::private::sensors::operations::ensure_channel_instrument(
        db,
        &stream,
        site_id,
        parameter_id,
        "grab entry",
    )
    .await?;

    Ok(stream.id)
}

/// What a request records about the measurement itself, as opposed to its value: who entered it,
/// what they called it, what they wrote about it, and the server-built blob saying what produced
/// the number. Every one of these is a property of the reading, so they are written onto each row
/// the request lands rather than onto the statistics row its replicates may or may not form.
struct GrabFacts<'a> {
    created_by: Option<&'a str>,
    label: Option<&'a str>,
    notes: Option<&'a str>,
    provenance: Option<&'a serde_json::Value>,
}

/// The same four facts as they are stored on a reading, owned.
#[derive(Default)]
struct StoredFacts {
    created_by: Option<String>,
    label: Option<String>,
    notes: Option<String>,
    provenance: Option<serde_json::Value>,
}

impl StoredFacts {
    fn is_empty(&self) -> bool {
        self.created_by.is_none()
            && self.label.is_none()
            && self.notes.is_none()
            && self.provenance.is_none()
    }
}

impl GrabFacts<'_> {
    /// This request's facts over what the group already carried, field by field: a rewrite that
    /// says nothing about the label keeps the one the group was entered under.
    fn over(&self, prior: Option<&StoredFacts>) -> StoredFacts {
        StoredFacts {
            created_by: self
                .created_by
                .map(String::from)
                .or_else(|| prior.and_then(|p| p.created_by.clone())),
            label: self
                .label
                .map(String::from)
                .or_else(|| prior.and_then(|p| p.label.clone())),
            notes: self
                .notes
                .map(String::from)
                .or_else(|| prior.and_then(|p| p.notes.clone())),
            provenance: self
                .provenance
                .cloned()
                .or_else(|| prior.and_then(|p| p.provenance.clone())),
        }
    }
}

/// The statistics row for this group, created if it is not already there.
///
/// Returns the row's id and whether this call is the one that created it.
///
/// The insert yields to a concurrent one rather than testing for the row first: two field entries
/// for the same (site, parameter, time) both see nothing, both insert, and the unique index on
/// those three columns then fails one of them, losing an entire grab to a 500. `DO NOTHING` is what
/// every other writer of `samples` does, and the read below picks up whichever row won.
async fn find_or_create_sample(
    txn: &sea_orm::DatabaseTransaction,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    estimator: sd_estimator::Resolved,
) -> Result<(Uuid, bool), AppError> {
    let candidate = samples::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        collected_at: Set(time),
        created_at: Set(Some(chrono::Utc::now())),
        mean: Set(None),
        stdev: sea_orm::ActiveValue::NotSet,
        stdev_sample: Set(None),
        stdev_population: Set(None),
        median: Set(None),
        n: Set(0),
        min_value: Set(None),
        max_value: Set(None),
        updated_at: Set(None),
        sd_estimator: Set(estimator.estimator.to_string()),
        sd_estimator_source: Set(estimator.source.as_str().to_string()),
    };
    let inserted = match samples::Entity::insert(candidate)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                samples::Column::SiteId,
                samples::Column::ParameterId,
                samples::Column::CollectedAt,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(txn)
        .await
    {
        Ok(rows) => rows > 0,
        // A conflict that inserted nothing is the expected outcome of re-posting a grab, not a
        // failure.
        Err(sea_orm::DbErr::RecordNotInserted) => false,
        Err(e) => return Err(AppError::Database(e)),
    };

    let existing = samples::Entity::find()
        .filter(samples::Column::SiteId.eq(site_id))
        .filter(samples::Column::ParameterId.eq(parameter_id))
        .filter(samples::Column::CollectedAt.eq(time))
        .one(txn)
        .await?
        .ok_or_else(|| {
            AppError::Internal("Failed to record the sample for this grab".to_string())
        })?;

    Ok((existing.id, inserted))
}

/// Form the `samples` row for every group this request left with two or more stored spot readings,
/// and stamp the group's readings with it. Returns the group-to-sample map and the rows created.
///
/// Counted over what is stored rather than over the request: a second replicate entered later joins
/// the instant's group, and a single measurement re-posted alone never mints a row of one.
async fn materialise_grab_samples(
    txn: &sea_orm::DatabaseTransaction,
    groups: &[(Uuid, chrono::DateTime<chrono::Utc>)],
    site_id: Uuid,
    requested_estimator: Option<&'static str>,
    fixed_estimators: &HashMap<Uuid, &'static str>,
) -> Result<Vec<Uuid>, AppError> {
    let mut created: Vec<Uuid> = Vec::new();
    for (parameter_id, time) in groups {
        let stored: i64 = txn
            .query_one_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT COUNT(*)::bigint AS n FROM readings
                  WHERE site_id = $1 AND parameter_id = $2 AND time = $3
                    AND measurement_type = 'spot'",
                [site_id.into(), (*parameter_id).into(), (*time).into()],
            ))
            .await?
            .map_or(Ok(0), |row| row.try_get::<i64>("", "n"))?;
        if !sample_groups::forms_sample(usize::try_from(stored).unwrap_or(0)) {
            continue;
        }
        // Each group resolves its own estimator: one request can span several parameters, and the
        // declaration is per slot. A manifest that fixes one for this output outranks the
        // request's, which is the operator's choice for a `selectable` output; both are stamped
        // `tool`, and neither is invented here.
        let explicit = fixed_estimators
            .get(parameter_id)
            .copied()
            .or(requested_estimator);
        let estimator = sd_estimator::resolve(
            txn,
            site_id,
            *parameter_id,
            explicit.map(|e| (e, sd_estimator::Source::Tool)),
            None,
        )
        .await?;
        // Re-posting the same grab must reuse its sample, not accumulate empty duplicates.
        let (sample_id, is_new) =
            find_or_create_sample(txn, site_id, *parameter_id, *time, estimator).await?;
        if is_new {
            created.push(sample_id);
        }
        // Scoped to spot readings: a sonde reading sharing the grab's snapped timestamp must not be
        // adopted into the sample, or the trigger folds sensor data into the grab statistics.
        txn.execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET sample_id = $1
              WHERE site_id = $2 AND parameter_id = $3 AND time = $4 AND sample_id IS NULL
                AND measurement_type = 'spot'",
            [
                sample_id.into(),
                site_id.into(),
                (*parameter_id).into(),
                (*time).into(),
            ],
        ))
        .await?;
    }

    Ok(created)
}

/// Whether `value` is the output's value: the scalar itself, or one of the numeric leaves of a
/// replicate-shaped output. Exact equality on purpose: the numbers travelled through JSON at full
/// precision, so an edited value is a different number and the tool link it claims is not true.
fn output_carries_value(output: &serde_json::Value, value: f64) -> bool {
    match output {
        serde_json::Value::Number(n) => n.as_f64() == Some(value),
        serde_json::Value::Array(items) => items.iter().any(|v| output_carries_value(v, value)),
        serde_json::Value::Object(map) => map.values().any(|v| output_carries_value(v, value)),
        _ => false,
    }
}

/// The server-built provenance blob for a save that names a tool run, `None` for a manual entry.
///
/// The blob is constructed from the stored `tool_runs` row, never from the request: the run's
/// inputs, constants, curves and outputs are what the engine resolved at calculate time, the
/// calculating actor and timestamp were stamped then, and the saving actor is the caller here.
/// Fail-closed on the link itself: every reading must name one of the run's outputs and carry
/// that output's value, so a save cannot claim a run it did not use. A run that consumed a
/// standard curve produced corrected outputs, so any reading carrying `standard_curve_id` is
/// refused (ADR 0003: a stored curve id means raw in, curve out — stamping one here would apply
/// the correction twice).
/// The estimator each output of a run's tool fixes, keyed by the parameter its readings land on.
///
/// A manifest that names `sample` or `population` is stating what that output means, so the server
/// applies it rather than trusting a client to repeat it. `selectable` is the operator's choice and
/// arrives on the request instead; an output that declares neither takes the slot's declaration.
///
/// The manifest read is the run's own pinned version, not the tool's active one: a save records
/// what the run that produced it meant.
/// The manifest of the tool version a run was executed under. A save reads what the run meant,
/// never the tool's current active manifest. `None` when there is no run, no matching version, or
/// a stored manifest that no longer parses (a tool-authoring problem, not this write's).
async fn run_pinned_manifest(
    db: &DatabaseConnection,
    run_id: Uuid,
) -> Result<Option<crate::routes::private::tools::engine::Manifest>, AppError> {
    let Some(row) = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.manifest FROM tool_runs r
             JOIN tool_script_versions v
               ON v.id = (r.tool_version->>'script_version_id')::uuid
             WHERE r.id = $1",
            [run_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let manifest: serde_json::Value = row.try_get("", "manifest").map_err(AppError::Database)?;
    Ok(crate::routes::private::tools::engine::parse_manifest(&manifest).ok())
}

async fn tool_run_fixed_estimators(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
    readings: &[GrabSampleReading],
) -> Result<HashMap<Uuid, &'static str>, AppError> {
    let Some(run_id) = tool_run_id else {
        return Ok(HashMap::new());
    };
    let Some(manifest) = run_pinned_manifest(db, run_id).await? else {
        return Ok(HashMap::new());
    };
    let by_key: HashMap<&str, &'static str> = manifest
        .outputs
        .iter()
        .filter_map(|o| Some((o.key.as_str(), o.fixed_sd_estimator()?)))
        .map(|(k, e)| {
            (
                k,
                if e == "population" {
                    sd_estimator::POPULATION
                } else {
                    sd_estimator::SAMPLE
                },
            )
        })
        .collect();
    if by_key.is_empty() {
        return Ok(HashMap::new());
    }
    let mut by_parameter: HashMap<Uuid, &'static str> = HashMap::new();
    for r in readings {
        if let Some(output) = r.output.as_deref()
            && let Some(estimator) = by_key.get(output)
        {
            by_parameter.insert(r.parameter_id, estimator);
        }
    }
    Ok(by_parameter)
}

async fn resolve_tool_run_provenance(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
    site_id: Uuid,
    readings: &[GrabSampleReading],
    saved_by: &str,
) -> Result<Option<serde_json::Value>, AppError> {
    let Some(run_id) = tool_run_id else {
        if let Some(r) = readings
            .iter()
            .find(|r| r.output.is_some() || r.input.is_some())
        {
            return Err(AppError::BadRequest(format!(
                "Reading for parameter {} names tool {} '{}' but the request carries no \
                 tool_run_id",
                r.parameter_id,
                if r.output.is_some() {
                    "output"
                } else {
                    "input"
                },
                r.output
                    .as_deref()
                    .or(r.input.as_deref())
                    .unwrap_or_default()
            )));
        }
        return Ok(None);
    };

    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT tool_name, tool_version, inputs, constants, curves, outputs, created_by, \
             created_at, context, source FROM tool_runs WHERE id = $1",
            [run_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Tool run {run_id} does not exist")))?;

    let tool_name: String = row.try_get("", "tool_name").map_err(AppError::Database)?;
    let run_source: String = row.try_get("", "source").map_err(AppError::Database)?;
    let run_context: Option<serde_json::Value> =
        row.try_get("", "context").map_err(AppError::Database)?;
    let tool_version: serde_json::Value = row
        .try_get("", "tool_version")
        .map_err(AppError::Database)?;
    let inputs: serde_json::Value = row.try_get("", "inputs").map_err(AppError::Database)?;
    let constants: serde_json::Value = row.try_get("", "constants").map_err(AppError::Database)?;
    let curves: serde_json::Value = row.try_get("", "curves").map_err(AppError::Database)?;
    let outputs: serde_json::Value = row.try_get("", "outputs").map_err(AppError::Database)?;
    let calculated_by: String = row.try_get("", "created_by").map_err(AppError::Database)?;
    let calculated_at: chrono::DateTime<chrono::Utc> = row
        .try_get::<chrono::DateTime<chrono::FixedOffset>>("", "created_at")
        .map_err(AppError::Database)?
        .with_timezone(&chrono::Utc);

    // The run resolved its station properties and same-event reads for one visit, and those
    // resolutions travel into the blob below. Saving it anywhere else would file a number computed
    // from one site's properties as another site's measurement, so the context the run recorded is
    // held to the save. A context-free run is a draft and stays saveable anywhere.
    if let Some(context) = run_context.as_ref().filter(|c| !c.is_null()) {
        if let Some(run_site) = context
            .get("site_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            && run_site != site_id
        {
            return Err(AppError::BadRequest(format!(
                "This {tool_name} run was calculated for site {run_site}; saving it at site \
                 {site_id} would file its resolved station inputs as another station's"
            )));
        }
        if let Some(run_time) = context
            .get("collected_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            && let Some(r) = readings.iter().find(|r| r.time != run_time)
        {
            return Err(AppError::BadRequest(format!(
                "This {tool_name} run was calculated for the visit at {run_time}; a reading at \
                 {} belongs to another visit",
                r.time
            )));
        }
    }

    // An output that reduces a replicates param is a display of the group the save is about to
    // write, not a measurement of its own. Storing it would put a mean in the readings the
    // `samples` trigger then takes a mean over. The manifest is the run's own pinned version.
    let aggregate_outputs: std::collections::HashSet<String> = run_pinned_manifest(db, run_id)
        .await?
        .map(|m| {
            m.outputs
                .iter()
                .filter(|o| o.aggregate_of.is_some())
                .map(|o| o.key.clone())
                .collect()
        })
        .unwrap_or_default();

    let run_applied_curves = curves.as_array().is_some_and(|c| !c.is_empty());
    let mut saved: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut saved_inputs: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for r in readings {
        if let Some(input) = r.input.as_deref() {
            if r.output.is_some() {
                return Err(AppError::BadRequest(format!(
                    "Reading for parameter {} names both an input and an output of the run",
                    r.parameter_id
                )));
            }
            let Some(index) = r.replicate_index else {
                return Err(AppError::BadRequest(format!(
                    "Reading for input '{input}' needs the replicate_index it was entered at"
                )));
            };
            let Some(values) = inputs.get(input).and_then(serde_json::Value::as_array) else {
                return Err(AppError::BadRequest(format!(
                    "'{input}' is not a replicates input of this {tool_name} run"
                )));
            };
            let recorded = usize::try_from(index)
                .ok()
                .and_then(|i| values.get(i))
                .and_then(serde_json::Value::as_f64);
            if recorded != Some(r.value) {
                return Err(AppError::BadRequest(format!(
                    "Value {} is not what this {tool_name} run consumed at replicate {index} of \
                     '{input}'",
                    r.value
                )));
            }
            match saved_inputs.get(input) {
                Some(existing) if existing != &serde_json::json!(r.parameter_id) => {
                    return Err(AppError::BadRequest(format!(
                        "Input '{input}' is saved to two different parameters in one request"
                    )));
                }
                _ => {
                    saved_inputs.insert(input.to_string(), serde_json::json!(r.parameter_id));
                }
            }
            continue;
        }
        let Some(output) = r.output.as_deref() else {
            return Err(AppError::BadRequest(format!(
                "Reading for parameter {} at {} names neither the tool output nor the input it \
                 stores; every reading of a tool-run save must",
                r.parameter_id, r.time
            )));
        };
        let Some(output_value) = outputs.get(output) else {
            return Err(AppError::BadRequest(format!(
                "'{output}' is not an output of this {tool_name} run"
            )));
        };
        if aggregate_outputs.contains(output) {
            return Err(AppError::BadRequest(format!(
                "'{output}' is a statistic of this {tool_name} run's replicates, not a \
                 measurement; save the replicates it reduces and the sample statistics follow"
            )));
        }
        if !output_carries_value(output_value, r.value) {
            return Err(AppError::BadRequest(format!(
                "Value {} is not what this {tool_name} run produced for '{output}'; the \
                 provenance would claim a number the tool did not compute",
                r.value
            )));
        }
        if run_applied_curves && r.standard_curve_id.is_some() {
            return Err(AppError::BadRequest(format!(
                "'{output}' was computed with a standard curve already applied; stamping \
                 standard_curve_id would have the correction applied twice"
            )));
        }
        match saved.get(output) {
            Some(existing) if existing != &serde_json::json!(r.parameter_id) => {
                return Err(AppError::BadRequest(format!(
                    "Output '{output}' is saved to two different parameters in one request"
                )));
            }
            _ => {
                saved.insert(output.to_string(), serde_json::json!(r.parameter_id));
            }
        }
    }

    let mut blob = serde_json::json!({
        "tool": tool_name,
        "tool_version": tool_version,
        "inputs": inputs,
        "constants": constants,
        "curves": curves,
        "outputs": outputs,
        "saved": saved,
        "saved_inputs": saved_inputs,
        "run_id": run_id,
        // D15: which path the numbers travelled. A tool linked here always actually ran; a CSV
        // that carried already-computed values gets no blob at all.
        "source": if run_source == "csv_import" { "csv_import" } else { "tool_run" },
        "calculated_by": calculated_by,
        "calculated_at": calculated_at,
        "saved_by": saved_by,
        "saved_at": chrono::Utc::now(),
    });
    // The run's resolved calculation context (station properties, same-event reads) travels into
    // the blob, so an auditor reads where each resolved value came from without the run row.
    if let Some(context) = run_context
        && !context.is_null()
        && let Some(map) = blob.as_object_mut()
    {
        map.insert("context".to_string(), context);
    }
    Ok(Some(blob))
}

/// Mint the site_parameter row a verified tool save lands on, `needs_review = TRUE`. The global
/// catalog parameter must exist; that is what keeps auto-provisioning from minting identity out
/// of a typo. Returns the new (or concurrently created) row's id.
///
/// A raw insert rather than the CRUD path: the CRUD hooks' auto-threshold guard would find no
/// default thresholds on a fresh analyte anyway, and a mechanical row must never fail the save
/// that provisions it.
async fn provision_site_parameter(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    site_name: &str,
) -> Result<Uuid, AppError> {
    let parameter = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name FROM parameters WHERE id = $1",
            [parameter_id.into()],
        ))
        .await?
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Parameter {parameter_id} is not configured for site {site_name} and is not in \
                 the parameter catalog; create the catalog parameter first"
            ))
        })?;
    let name: String = parameter.try_get("", "name").unwrap_or_default();

    db.execute_raw(sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active, \
         is_public, needs_review, created_at)
         VALUES ($1, $2, $3, $4, '', TRUE, FALSE, TRUE, NOW())
         ON CONFLICT DO NOTHING",
        [
            Uuid::new_v4().into(),
            site_id.into(),
            parameter_id.into(),
            name.into(),
        ],
    ))
    .await?;

    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM site_parameters WHERE site_id = $1 AND parameter_id = $2",
            [site_id.into(), parameter_id.into()],
        ))
        .await?
        .ok_or_else(|| {
            AppError::Internal("Failed to provision the site parameter for this save".to_string())
        })?;
    row.try_get("", "id").map_err(AppError::Database)
}

/// Insert field-collected grab sample readings (manual measurements with replicate sets).
/// Each request creates one Sample aggregate per parameter and uses dedicated "grab_sample"
/// streams. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/grab_samples",
    request_body = GrabSampleRequest,
    responses(
        (status = 200, description = "Counts of inserted readings and created Sample rows", body = GrabSampleResponse),
        (status = 400, description = "Empty readings, parameter not configured for site, or other validation"),
    ),
    tag = "ingestion"
)]
pub async fn insert_grab_samples(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<GrabSampleRequest>,
) -> AppResult<Json<GrabSampleResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings provided".to_string()));
    }

    // A project-scoped token may only write to a site within its project.
    enforce_project_scope_for_sites(&state.db, &scope, &[payload.site_id]).await?;

    for r in &payload.readings {
        admission::admit(r.time, r.value, Some(GRAB_MEASUREMENT_TYPE))?;
    }

    // Refused at the edge rather than falling back to a divisor: a stored estimator is a
    // specification, so an unrecognised one is a request to reject, not a value to guess.
    let requested_estimator = sd_estimator::parse_opt(payload.sd_estimator.as_deref())?;

    // Validate site exists
    let site = sites::Entity::find_by_id(payload.site_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {} not found", payload.site_id)))?;

    // Validate all parameter_ids exist for this site
    let param_ids: Vec<Uuid> = payload.readings.iter().map(|r| r.parameter_id).collect();
    let site_params = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::ParameterId.is_in(param_ids.clone()))
        .all(&state.db)
        .await?;

    let mut valid_param_ids: std::collections::HashSet<Uuid> =
        site_params.iter().map(|sp| sp.parameter_id).collect();
    let mut sp_lookup: HashMap<Uuid, Uuid> = site_params
        .iter()
        .map(|sp| (sp.parameter_id, sp.id))
        .collect();

    // A verified tool save provisions the slot it lands on (D10): the output's identity is the
    // run's, not the client's, so a catalog parameter the site does not carry yet gets its
    // site_parameter minted here, flagged needs_review for an operator's look. The global
    // parameter must already exist; a save naming an unknown parameter stays refused.
    if payload.tool_run_id.is_some() {
        let missing: Vec<Uuid> = param_ids
            .iter()
            .filter(|pid| !valid_param_ids.contains(pid))
            .copied()
            .collect();
        for pid in missing {
            if valid_param_ids.contains(&pid) {
                continue;
            }
            let sp_id = provision_site_parameter(&state.db, site.id, pid, &site.name).await?;
            valid_param_ids.insert(pid);
            sp_lookup.insert(pid, sp_id);
        }
    }

    for r in &payload.readings {
        if !valid_param_ids.contains(&r.parameter_id) {
            return Err(AppError::BadRequest(format!(
                "Parameter {} is not configured for site {}",
                r.parameter_id, site.name
            )));
        }
    }

    // A save that names a seasonal check is held to it: every (parameter, value) pair must have
    // been screened by exactly that check.
    if let Some(check_id) = payload.check_id {
        let pairs: Vec<(Uuid, f64)> = payload
            .readings
            .iter()
            .map(|r| (r.parameter_id, r.value))
            .collect();
        readings::checks::validate_check_claim(&state.db, check_id, site.id, &pairs).await?;
    }

    // Replicate indices, both curves and the served value are computed before anything is
    // written, so the same numbers serve the dry-run preview, the conflict report and the write.
    let indices = assign_replicate_indices(&payload.readings)?;

    let provenance = resolve_tool_run_provenance(
        &state.db,
        payload.tool_run_id,
        site.id,
        &payload.readings,
        &crate::routes::private::tools::scripts::actor_label(&auth),
    )
    .await?;

    let fixed_estimators =
        tool_run_fixed_estimators(&state.db, payload.tool_run_id, &payload.readings).await?;

    // The chosen standard curves, admitted by the one rule every writer of `standard_curve_id`
    // uses. A grab is spot by construction, so the only claims this path can be refused for are an
    // unknown id, a curve fitted on another instrument, and a curve on a grab that names no
    // instrument at all.
    let claims: Vec<CurveClaim<'_>> = payload
        .readings
        .iter()
        .filter_map(|r| {
            r.standard_curve_id.map(|id| CurveClaim {
                standard_curve_id: id,
                sensor_id: r.sensor_id,
                measurement_type: GRAB_MEASUREMENT_TYPE,
            })
        })
        .collect();
    let standard_curves = admit_standard_curves(&state.db, &claims).await?;

    // The base calibration covering each grab that names an instrument, ranked by the one resolver
    // the ingest and reprocess paths use. Resolving it here is what lets the row carry both the id
    // and the value that id produced: a stamped calibration the stored value was never corrected by
    // is provenance that reads as true and is not.
    let base_curves = {
        let requests: Vec<(Uuid, Option<Uuid>, chrono::DateTime<chrono::Utc>)> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id.map(|sid| (sid, Some(r.parameter_id), r.time)))
            .collect();
        calibrations::resolver::resolve_many(&state.db, &requests).await?
    };

    let preview: Vec<GrabPreview> = payload
        .readings
        .iter()
        .zip(&indices)
        .map(|(r, &replicate_index)| {
            let base = r
                .sensor_id
                .and_then(|sid| base_curves.get(&(sid, Some(r.parameter_id), r.time)))
                .copied();
            let standard = r.standard_curve_id.map(|cid| {
                let c = &standard_curves[&cid];
                calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            // Both corrections, in the one order the arithmetic is defined in: the instrument's
            // base calibration, then the operator's standard curve on that result. A grab that
            // resolves neither is stored uncorrected, and `calibrated_value` stays NULL so a null
            // still means "no curve was applied" rather than "a curve happened to be identity".
            let calibrated_value = (base.is_some() || standard.is_some())
                .then(|| calibrations::service::apply_curves(r.value, base, standard));
            let composed_equation = match (base, standard) {
                (Some(b), Some(s)) => Some(equation(
                    s.slope * b.slope,
                    s.slope * b.intercept + s.intercept,
                )),
                _ => None,
            };
            GrabPreview {
                parameter_id: r.parameter_id,
                time: r.time,
                replicate_index,
                raw_value: r.value,
                base_calibration: base.map(|c| CurveApplication {
                    id: c.id,
                    name: None,
                    slope: c.slope,
                    intercept: c.intercept,
                    equation: equation(c.slope, c.intercept),
                }),
                standard_curve: standard.map(|c| CurveApplication {
                    id: c.id,
                    name: standard_curves[&c.id].name.clone(),
                    slope: c.slope,
                    intercept: c.intercept,
                    equation: equation(c.slope, c.intercept),
                }),
                composed_equation,
                calibrated_value,
            }
        })
        .collect();

    let groups: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = {
        let mut seen = std::collections::HashSet::new();
        payload
            .readings
            .iter()
            .filter(|r| seen.insert((r.parameter_id, r.time)))
            .map(|r| (r.parameter_id, r.time))
            .collect()
    };
    let existing_groups = fetch_existing_groups(&state.db, payload.site_id, &groups).await?;

    // Which calculations this save feeds, known before anything is written. The chain's own save
    // is the recompute: it reports nothing and enqueues nothing.
    let writer = match tool_run_source(&state.db, payload.tool_run_id)
        .await?
        .as_deref()
    {
        Some("chain") => recompute::Writer::Chain,
        _ => recompute::Writer::Person,
    };
    let calculations = if writer == recompute::Writer::Chain {
        Vec::new()
    } else {
        let mut touched: Vec<Uuid> = payload.readings.iter().map(|r| r.parameter_id).collect();
        touched.sort_unstable();
        touched.dedup();
        crate::routes::private::tools::closure::calculations_fed_by(&state.db, &touched).await?
    };

    if payload.dry_run {
        return Ok(Json(GrabSampleResponse {
            inserted: 0,
            samples_created: 0,
            created_sample_ids: vec![],
            dry_run: true,
            replaced: 0,
            kept_curated: 0,
            preview,
            existing_groups,
            calculations,
        }));
    }

    // An intern enters measurements; a stored value is someone else's to change (Q21).
    if decisions::entry_state(auth.highest_role().as_ref()).is_some()
        && payload.mode == Some(GrabWriteMode::Replace)
    {
        return Err(AppError::Forbidden(
            "An intern's entry cannot replace stored values; a manager rewrites them".to_string(),
        ));
    }
    // A value computed from a pending measurement is pending too (M62): the chain says so.
    let entry_state = if payload.pending_inputs {
        Some(decisions::Kind::UnverifiedEntry)
    } else {
        decisions::entry_state(auth.highest_role().as_ref())
    };

    if !existing_groups.is_empty() && payload.mode != Some(GrabWriteMode::Replace) {
        let detail = serde_json::to_value(&existing_groups)
            .map_err(|e| AppError::Internal(e.to_string()))?;
        return Err(AppError::ConflictDetail {
            message: format!(
                "{} replicate group(s) are already stored at the requested times; pass mode \
                 \"replace\" to rewrite them",
                existing_groups.len()
            ),
            detail,
        });
    }

    // Resolve stream_ids for each unique (site_id, parameter_id)
    let mut stream_cache: HashMap<Uuid, Uuid> = HashMap::new();
    for r in &payload.readings {
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(r.parameter_id)
        {
            let sp_id = sp_lookup.get(&r.parameter_id).copied();
            let stream_id =
                get_or_create_grab_stream(&state.db, payload.site_id, r.parameter_id, sp_id)
                    .await?;
            entry.insert(stream_id);
        }
    }

    // The channel instrument each parameter's grab stream carries, which a reading naming no
    // instrument of its own is attributed to. A hand-entered value still records what produced it.
    let stream_sensors: HashMap<Uuid, Uuid> = {
        let ids: Vec<Uuid> = stream_cache.values().copied().collect();
        let mut map = HashMap::new();
        for row in state
            .db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id, sensor_id FROM data_streams WHERE id = ANY($1) AND sensor_id IS NOT NULL",
                [ids.into()],
            ))
            .await?
        {
            map.insert(row.try_get::<Uuid>("", "id")?, row.try_get::<Uuid>("", "sensor_id")?);
        }
        map
    };

    // Window-aware attribution for grabs that name a sensor: which deployment the instrument was on
    // at the grab time (site-fixed to payload.site_id), instead of writing NULL. Grabs without a
    // sensor_id keep NULL deployment (manual lab values with no instrument).
    let grab_slots = {
        use crate::routes::private::sensors::operations::{
            ResolvedSlot, resolve_windows_for_times,
        };
        let mut times_by_channel: HashMap<(Uuid, Uuid), Vec<chrono::DateTime<chrono::Utc>>> =
            HashMap::new();
        for r in &payload.readings {
            if let Some(sid) = r.sensor_id {
                times_by_channel
                    .entry((sid, r.parameter_id))
                    .or_default()
                    .push(r.time);
            }
        }
        let mut slots: HashMap<(Uuid, Uuid, chrono::DateTime<chrono::Utc>), ResolvedSlot> =
            HashMap::new();
        for ((sid, pid), times) in &times_by_channel {
            let resolved = resolve_windows_for_times(
                &state.db,
                *sid,
                Some(payload.site_id),
                Some(*pid),
                times,
            )
            .await
            .unwrap_or_default();
            for (t, slot) in resolved {
                slots.insert((*sid, *pid, t), slot);
            }
        }
        slots
    };

    // Per-parameter time windows for the alarm episode reconstruction below.
    let mut alarm_windows: HashMap<
        Uuid,
        (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
    > = HashMap::new();
    for r in &payload.readings {
        alarm_windows
            .entry(r.parameter_id)
            .and_modify(|(lo, hi)| {
                *lo = (*lo).min(r.time);
                *hi = (*hi).max(r.time);
            })
            .or_insert((r.time, r.time));
    }

    let total = payload.readings.len();

    // One guarded transaction: a replace on a compressed chunk must not fail on the cap, and the
    // delete, the sample rows and the insert land together or not at all.
    let actor = crate::routes::private::tools::scripts::actor_label(&auth);
    let (inserted, replaced, kept_curated, created_sample_ids, touched_events) =
        crate::common::bulk_write::guarded(&state.db, async |txn| {
            // A replace deletes the rows carrying the group's label, notes, authorship and blob,
            // so they are captured first and restored onto the rewritten rows wherever the request
            // does not carry its own.
            let mut prior_facts: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), StoredFacts> =
                HashMap::new();
            let (replaced, kept_curated): (usize, usize) =
                if payload.mode == Some(GrabWriteMode::Replace) {
                    // What the replace rewrites is decided before the rows go: a person's
                    // correction of a stored value, or the chain superseding an output with a
                    // fresh run (ADR 0008). Rows whose value does not change decide nothing.
                    let (kind, origin, reason) = match writer {
                        recompute::Writer::Chain => (
                            decisions::Kind::Chain,
                            decisions::Origin::Chain,
                            "superseded by a recompute",
                        ),
                        recompute::Writer::Person => (
                            decisions::Kind::ValueCorrection,
                            decisions::Origin::Manual,
                            "replaced by a new entry",
                        ),
                    };
                    for (parameter_id, time) in &groups {
                        let rows: Vec<(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)> =
                            payload
                                .readings
                                .iter()
                                .zip(&preview)
                                .filter(|(r, _)| r.parameter_id == *parameter_id && r.time == *time)
                                .map(|(_, p)| {
                                    let new = match (writer, payload.tool_run_id) {
                                        (recompute::Writer::Chain, Some(run_id)) => {
                                            serde_json::json!({ "run_id": run_id })
                                        }
                                        _ => serde_json::json!({ "raw_value": p.raw_value }),
                                    };
                                    (*time, p.replicate_index, new)
                                })
                                .collect();
                        // Only the rows the replace rewrites are decided: a flagged, withdrawn
                        // or hand-curved row stays as it is (SB5) and gets a hold, not a
                        // correction. The curve rule is the delete's own, per group.
                        let supplies_curve = payload.readings.iter().zip(&preview).any(|(r, p)| {
                            r.parameter_id == *parameter_id
                                && r.time == *time
                                && p.standard_curve.is_some()
                        });
                        let guard = if supplies_curve {
                            "r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL"
                        } else {
                            "r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL \
                             AND r.standard_curve_id IS NULL"
                        };
                        decisions::record_keyed(
                            txn,
                            kind,
                            stream_cache[parameter_id],
                            &rows,
                            &actor,
                            Some(reason),
                            origin,
                            decisions::Keyed::Changed,
                            Some(guard),
                        )
                        .await?;
                    }
                    for (parameter_id, time) in &groups {
                        if let Some(row) = txn
                            .query_one_raw(sea_orm::Statement::from_sql_and_values(
                                sea_orm::DatabaseBackend::Postgres,
                                r"SELECT label, notes, created_by, provenance FROM readings
                              WHERE site_id = $1 AND parameter_id = $2 AND time = $3
                                AND measurement_type = 'spot'
                                AND (label IS NOT NULL OR notes IS NOT NULL
                                     OR created_by IS NOT NULL OR provenance IS NOT NULL)
                              ORDER BY replicate_index LIMIT 1",
                                [
                                    payload.site_id.into(),
                                    (*parameter_id).into(),
                                    (*time).into(),
                                ],
                            ))
                            .await?
                        {
                            prior_facts.insert(
                                (*parameter_id, *time),
                                StoredFacts {
                                    label: row.try_get("", "label").unwrap_or(None),
                                    notes: row.try_get("", "notes").unwrap_or(None),
                                    created_by: row.try_get("", "created_by").unwrap_or(None),
                                    provenance: row.try_get("", "provenance").unwrap_or(None),
                                },
                            );
                        }
                    }
                    // The delete is scoped to the grab stream: another source's rows at the same
                    // instant are not this request's to rewrite. Curation wins, as in the windowed
                    // diff: a flagged, withdrawn or hand-curved row stays and the disagreement
                    // lands in the review queue.
                    let mut removed: u64 = 0;
                    let mut kept_total: usize = 0;
                    for (parameter_id, time) in &groups {
                        let stream_id = stream_cache[parameter_id];
                        let supplies_curve = payload.readings.iter().zip(&preview).any(|(r, p)| {
                            r.parameter_id == *parameter_id
                                && r.time == *time
                                && p.standard_curve.is_some()
                        });
                        let kept = txn
                            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                                sea_orm::DatabaseBackend::Postgres,
                                r"SELECT replicate_index,
                                     CASE WHEN is_flagged IS TRUE THEN 'flagged'
                                          WHEN withdrawn_at IS NOT NULL THEN 'withdrawn'
                                          ELSE 'standard_curve' END AS reason
                              FROM readings
                              WHERE stream_id = $1 AND time = $2 AND measurement_type = 'spot'
                                AND (is_flagged IS TRUE OR withdrawn_at IS NOT NULL
                                     OR (NOT $3 AND standard_curve_id IS NOT NULL))
                              ORDER BY replicate_index",
                                [stream_id.into(), (*time).into(), supplies_curve.into()],
                            ))
                            .await?;
                        if !kept.is_empty() {
                            let entries = kept
                            .iter()
                            .map(|row| {
                                Ok(serde_json::json!({
                                    "replicate_index": row.try_get::<i16>("", "replicate_index")?,
                                    "reason": row.try_get::<String>("", "reason")?,
                                }))
                            })
                            .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
                            super::reconcile::upsert_source_modified_hold(
                                txn,
                                stream_id,
                                *time,
                                serde_json::json!({ "claim": "replaced", "kept": entries }),
                                serde_json::json!({ "kept": true }),
                                "pending",
                            )
                            .await?;
                            kept_total += kept.len();
                        }
                        let res = txn
                            .execute_raw(sea_orm::Statement::from_sql_and_values(
                                sea_orm::DatabaseBackend::Postgres,
                                r"DELETE FROM readings
                              WHERE stream_id = $1 AND time = $2 AND measurement_type = 'spot'
                                AND is_flagged IS NOT TRUE AND withdrawn_at IS NULL
                                AND ($3 OR standard_curve_id IS NULL)",
                                [stream_id.into(), (*time).into(), supplies_curve.into()],
                            ))
                            .await?;
                        removed += res.rows_affected();
                    }
                    (usize::try_from(removed).unwrap_or(usize::MAX), kept_total)
                } else {
                    (0, 0)
                };

            // What each row records about the measurement, request first and the rewritten group's
            // own prior values where the request is silent.
            let facts = GrabFacts {
                created_by: payload.created_by.as_deref(),
                label: payload.label.as_deref(),
                notes: payload.notes.as_deref(),
                provenance: provenance.as_ref(),
            };
            let stored_facts: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), StoredFacts> = groups
                .iter()
                .map(|group| (*group, facts.over(prior_facts.get(group))))
                .collect();

            let models: Vec<readings::ActiveModel> = payload
                .readings
                .iter()
                .zip(&preview)
                .map(|(r, p)| readings::ActiveModel {
                    standard_curve_id: Set(p.standard_curve.as_ref().map(|c| c.id)),
                    collection_event_id: Set(None),
                    withdrawn_at: Set(None),
                    withdrawn_reason: Set(None),
                    ingested_at: sea_orm::ActiveValue::NotSet,
                    stream_id: Set(stream_cache[&r.parameter_id]),
                    site_id: Set(Some(payload.site_id)),
                    parameter_id: Set(Some(r.parameter_id)),
                    time: Set(r.time.into()),
                    replicate_index: Set(p.replicate_index),
                    raw_value: Set(r.value),
                    calibrated_value: Set(p.calibrated_value),
                    sensor_id: Set(r
                        .sensor_id
                        .or_else(|| stream_sensors.get(&stream_cache[&r.parameter_id]).copied())),
                    calibration_id: Set(p.base_calibration.as_ref().map(|c| c.id)),
                    deployment_id: Set(r.sensor_id.and_then(|sid| {
                        grab_slots
                            .get(&(sid, r.parameter_id, r.time))
                            .and_then(|s| s.deployment_id)
                    })),
                    logged: Set(Some(true)),
                    measurement_type: Set(Some(GRAB_MEASUREMENT_TYPE.to_string())),
                    is_flagged: Set(Some(false)),
                    flag_reason: Set(None),
                    sample_id: Set(None),
                    label: Set(stored_facts[&(r.parameter_id, r.time)].label.clone()),
                    notes: Set(stored_facts[&(r.parameter_id, r.time)].notes.clone()),
                    created_by: Set(stored_facts[&(r.parameter_id, r.time)].created_by.clone()),
                    provenance: Set(stored_facts[&(r.parameter_id, r.time)].provenance.clone()),
                })
                .collect();

            let inserted = match readings::Entity::insert_many(models.clone())
                .on_conflict(readings_upsert(Replace::Nothing))
                .exec_without_returning(txn)
                .await
            {
                Ok(rows) => rows as usize,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("None of the records") {
                        0
                    } else {
                        return Err(AppError::Database(e));
                    }
                }
            };
            // A curve chosen with the entry is a claim, recorded once (ADR 0008).
            decisions::record_curve_claims(txn, &models, &actor, decisions::Origin::Manual).await?;

            // An intern's entry lands pending: the record carries it, the columns project it and
            // the review queue lists it until a manager verifies or rejects (Q21, M44).
            if entry_state == Some(decisions::Kind::UnverifiedEntry) {
                decisions::record_unverified_entries(
                    txn,
                    &models,
                    &actor,
                    decisions::Origin::Manual,
                )
                .await?;
                open_unverified_holds(txn, payload.site_id, &groups, &actor).await?;
            }

            // A re-post is the same measurement recorded again: the rows the insert skipped on
            // conflict still take this request's story, so a second run's blob does not sit behind
            // the value it produced. Keyed on the rows this request wrote, so a curated row a
            // replace left in place keeps the provenance of the run that made it.
            for (r, p) in payload.readings.iter().zip(&preview) {
                let stored = &stored_facts[&(r.parameter_id, r.time)];
                if stored.is_empty() {
                    continue;
                }
                txn.execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"UPDATE readings
                         SET label = COALESCE($4, label),
                             notes = COALESCE($5, notes),
                             created_by = COALESCE($6, created_by),
                             provenance = COALESCE($7, provenance)
                       WHERE stream_id = $1 AND time = $2 AND replicate_index = $3",
                    [
                        stream_cache[&r.parameter_id].into(),
                        r.time.into(),
                        p.replicate_index.into(),
                        stored.label.clone().into(),
                        stored.notes.clone().into(),
                        stored.created_by.clone().into(),
                        stored.provenance.clone().into(),
                    ],
                ))
                .await?;
            }

            // The statistics row, for the groups that now hold two or more replicates.
            let created_sample_ids = materialise_grab_samples(
                txn,
                &groups,
                payload.site_id,
                requested_estimator,
                &fixed_estimators,
            )
            .await?;

            // Every attributed spot instant this request touched belongs to a collection event
            // (D7); a hand-entered grab is a manual visit.
            let mut touched_events = Vec::new();
            if let (Some(lo), Some(hi)) = (
                groups.iter().map(|(_, t)| *t).min(),
                groups.iter().map(|(_, t)| *t).max(),
            ) {
                crate::routes::private::collection_events::attach::attach_collection_events(
                    txn,
                    "r.site_id = $1 AND r.time >= $2 AND r.time <= $3",
                    vec![
                        payload.site_id.into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(lo).into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(hi).into(),
                    ],
                    crate::routes::private::collection_events::attach::EventSource::Manual,
                )
                .await?;
                let mut instants: Vec<sea_orm::prelude::DateTimeWithTimeZone> = groups
                    .iter()
                    .map(|(_, t)| sea_orm::prelude::DateTimeWithTimeZone::from(*t))
                    .collect();
                instants.sort_unstable();
                instants.dedup();
                touched_events = recompute::touched_events(
                    txn,
                    "r.site_id = $1 AND r.time = ANY($2)",
                    vec![payload.site_id.into(), instants.into()],
                )
                .await?;
            }

            Ok((
                inserted,
                replaced,
                kept_curated,
                created_sample_ids,
                touched_events,
            ))
        })
        .await?;

    // The value has landed: the calculations that read it run without anyone asking (ADR 0007),
    // the sampled slots are reconciled and their episodes rebuilt inline (one `reprocessing_jobs`
    // row per field campaign entry would be the noise), and the site's cached responses go. Grabs
    // are excluded from the rollups, so there is nothing to refresh.
    let written = tail::Written::new(u64::try_from(inserted + replaced).unwrap_or(u64::MAX))
        .over(
            alarm_windows
                .values()
                .copied()
                .reduce(|(lo, hi), (a, b)| (lo.min(a), hi.max(b))),
        )
        .at(stream_cache
            .keys()
            .map(|pid| tail::Slot::paired(payload.site_id, *pid))
            .collect())
        .touching(touched_events);
    tail::run(
        &state,
        &written,
        &tail::Axes {
            cache: tail::Cache::Sites,
            refresh: tail::Refresh::Skip,
            announce: false,
            reconcile_alarms: true,
            episodes: tail::Episodes::Inline,
            writer,
        },
        &crate::routes::private::tools::scripts::actor_label(&auth),
    )
    .await?;

    let samples_created = created_sample_ids.len();
    tracing::info!(total, inserted, replaced, kept_curated, samples_created, site = %site.name, "Grab samples inserted");
    Ok(Json(GrabSampleResponse {
        inserted,
        samples_created,
        created_sample_ids,
        dry_run: false,
        replaced,
        kept_curated,
        preview,
        existing_groups,
        calculations,
    }))
}

/// One review-queue row per slot instant an intern entered, so a manager sees the pending entry
/// beside every other finding. Keyed on (site, parameter, instant): a re-entry at the same slot
/// refreshes the open hold rather than filing a second one.
pub async fn open_unverified_holds<C: sea_orm::ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    groups: &[(Uuid, chrono::DateTime<chrono::Utc>)],
    actor: &str,
) -> AppResult<()> {
    for (parameter_id, at) in groups {
        let binds = [
            site_id.into(),
            (*parameter_id).into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(*at).into(),
            serde_json::json!({ "state": "unverified", "entered_by": actor }).into(),
        ];
        let updated = conn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE replicate_audit_holds SET computed = $4, created_at = NOW() \
                 WHERE site_id = $1 AND parameter_id = $2 AND group_time = $3 \
                   AND kind = 'unverified_entry' AND status IN ('pending', 'deferred')",
                binds,
            ))
            .await?
            .rows_affected();
        if updated > 0 {
            continue;
        }
        // Two saves at one slot instant see no row to update and both insert; the conflict
        // clause makes the loser refresh the hold instead of failing its whole save.
        audit::upsert_hold(
            conn,
            &audit::Hold {
                key: audit::HoldKey::Slot {
                    site_id,
                    parameter_id: *parameter_id,
                    group_time: *at,
                },
                kind: "unverified_entry",
                expected: serde_json::json!({ "state": "verified" }),
                computed: serde_json::json!({ "state": "unverified", "entered_by": actor }),
                delta: serde_json::json!({}),
                status: "pending",
                tool: None,
            },
        )
        .await?;
    }
    Ok(())
}

/// The `source` of a stored tool run: `interactive` | `csv_import` | `chain`. `None` when the
/// request names no run.
async fn tool_run_source(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
) -> AppResult<Option<String>> {
    let Some(run_id) = tool_run_id else {
        return Ok(None);
    };
    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source FROM tool_runs WHERE id = $1",
            [run_id.into()],
        ))
        .await?;
    Ok(row.and_then(|r| r.try_get("", "source").ok()))
}

#[cfg(test)]
mod tests {
    use super::{GrabFacts, StoredFacts};

    fn stored(label: &str, notes: &str, author: &str) -> StoredFacts {
        StoredFacts {
            created_by: Some(author.to_string()),
            label: Some(label.to_string()),
            notes: Some(notes.to_string()),
            provenance: Some(serde_json::json!({ "tool": "doc" })),
        }
    }

    #[test]
    fn a_silent_field_keeps_what_the_group_carried() {
        let prior = stored("batch 7", "filtered on site", "lab");
        let request = GrabFacts {
            created_by: None,
            label: None,
            notes: Some("corrected note"),
            provenance: None,
        };
        let merged = request.over(Some(&prior));
        assert_eq!(merged.label.as_deref(), Some("batch 7"));
        assert_eq!(merged.notes.as_deref(), Some("corrected note"));
        assert_eq!(merged.created_by.as_deref(), Some("lab"));
        assert_eq!(
            merged.provenance,
            Some(serde_json::json!({ "tool": "doc" })),
            "a rewrite that names no run keeps the blob behind the value"
        );
    }

    #[test]
    fn a_first_write_carries_only_what_the_request_says() {
        let request = GrabFacts {
            created_by: Some("evan"),
            label: None,
            notes: None,
            provenance: None,
        };
        let merged = request.over(None);
        assert_eq!(merged.created_by.as_deref(), Some("evan"));
        assert_eq!(merged.label, None);
        assert!(!merged.is_empty(), "an author alone is worth storing");
        assert!(
            GrabFacts {
                created_by: None,
                label: None,
                notes: None,
                provenance: None,
            }
            .over(None)
            .is_empty(),
            "a request that records nothing writes nothing"
        );
    }
}
