use axum::{Json, extract::State};
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::batch::{
    CurveClaim, Replace, admission, admit_standard_curves, readings_upsert,
};
use crate::routes::private::{
    data_streams, readings, readings::sample_groups, readings::samples,
    readings::sd_estimator, sensors::calibrations, sites, sites::parameters as site_parameters,
};

/// Grabs are spot measurements by definition: a bottle, not a logger cadence.
const GRAB_MEASUREMENT_TYPE: &str = "spot";

/// Posting to this endpoint is the declaration that a collection event happened.
const GRAB_IS_A_COLLECTION_EVENT: bool = true;

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
    /// Rows removed by `mode: replace` before the insert.
    pub replaced: usize,
    /// What each reading stores: the measured value, the curves that apply and the value they
    /// produce together, computed by the code the write itself uses.
    pub preview: Vec<GrabPreview>,
    /// Replicate groups already stored at the requested (parameter, time) keys, as found before
    /// this request wrote anything.
    pub existing_groups: Vec<ExistingGroup>,
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
            .query_all(sea_orm::Statement::from_sql_and_values(
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

    Ok(stream.id)
}

/// What a request puts on the `samples` rows it creates or reuses, as opposed to what identifies
/// them. Grouped so the two find-or-create helpers take the identity and the contents separately
/// rather than a dozen positional arguments.
struct SampleFacts<'a> {
    created_by: Option<&'a str>,
    label: Option<&'a str>,
    notes: Option<&'a str>,
    provenance: Option<&'a serde_json::Value>,
}

/// The samples row for this collection event, created if it is not already there, with its label and
/// notes refreshed when the request carries them.
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
    facts: &SampleFacts<'_>,
    estimator: sd_estimator::Resolved,
) -> Result<(Uuid, bool), AppError> {
    let SampleFacts {
        created_by,
        label,
        notes,
        provenance,
    } = *facts;
    let candidate = samples::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        collected_at: Set(time),
        label: Set(label.map(String::from)),
        notes: Set(notes.map(String::from)),
        created_by: Set(created_by.map(String::from)),
        provenance: Set(provenance.cloned()),
        created_at: Set(Some(chrono::Utc::now())),
        mean: Set(None),
        stdev: Set(None),
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
    let sample_id = existing.id;

    if !inserted && (label.is_some() || notes.is_some() || provenance.is_some()) {
        let mut active: samples::ActiveModel = existing.into();
        if let Some(l) = label {
            active.label = Set(Some(l.to_string()));
        }
        if let Some(n) = notes {
            active.notes = Set(Some(n.to_string()));
        }
        // A re-post carrying provenance is a new run over this collection event; the blob
        // follows the numbers being written, never the ones being replaced.
        if let Some(p) = provenance {
            active.provenance = Set(Some(p.clone()));
        }
        active.updated_at = Set(Some(chrono::Utc::now()));
        active.update(txn).await?;
    }

    Ok((sample_id, inserted))
}

/// Create a `samples` row per (parameter_id, time) group. Returns the group-to-sample map and how
/// many rows were created.
async fn auto_create_samples(
    txn: &sea_orm::DatabaseTransaction,
    readings: &[GrabSampleReading],
    site_id: Uuid,
    facts: &SampleFacts<'_>,
    requested_estimator: Option<&'static str>,
    fixed_estimators: &HashMap<Uuid, &'static str>,
) -> Result<
    (
        HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Uuid>,
        Vec<Uuid>,
    ),
    AppError,
> {
    let mut groups: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), usize> = HashMap::new();
    for r in readings {
        *groups.entry((r.parameter_id, r.time)).or_default() += 1;
    }

    let mut sample_map = HashMap::new();
    let mut created: Vec<Uuid> = Vec::new();
    for ((parameter_id, time), count) in groups {
        // A grab request is an operator recording a collection event, so every group is a sample
        // whether or not it was measured twice. Views that read grabs, the sensor-vs-grab export
        // and the curve filter among them, join through `samples`, so a grab without a row there
        // is invisible to them.
        if !sample_groups::forms_sample(GRAB_IS_A_COLLECTION_EVENT, count) {
            continue;
        }
        // Each group resolves its own estimator: one request can span several parameters, and the
        // declaration is per slot. A manifest that fixes one for this output outranks the
        // request's, which is the operator's choice for a `selectable` output; both are stamped
        // `tool`, and neither is invented here.
        let explicit = fixed_estimators
            .get(&parameter_id)
            .copied()
            .or(requested_estimator);
        let estimator = sd_estimator::resolve(
            txn,
            site_id,
            parameter_id,
            explicit.map(|e| (e, sd_estimator::Source::Tool)),
            None,
        )
        .await?;
        // Re-posting the same grab must reuse its sample, not accumulate empty duplicates.
        let (sample_id, is_new) =
            find_or_create_sample(txn, site_id, parameter_id, time, facts, estimator).await?;
        sample_map.insert((parameter_id, time), sample_id);
        if is_new {
            created.push(sample_id);
        }
    }

    Ok((sample_map, created))
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
async fn tool_run_fixed_estimators(
    db: &DatabaseConnection,
    tool_run_id: Option<Uuid>,
    readings: &[GrabSampleReading],
) -> Result<HashMap<Uuid, &'static str>, AppError> {
    let Some(run_id) = tool_run_id else {
        return Ok(HashMap::new());
    };
    let row = db
        .query_one(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.manifest FROM tool_runs r
             JOIN tool_script_versions v
               ON v.id = (r.tool_version->>'script_version_id')::uuid
             WHERE r.id = $1",
            [run_id.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(HashMap::new());
    };
    let manifest: serde_json::Value = row.try_get("", "manifest").map_err(AppError::Database)?;
    let Ok(manifest) = crate::routes::private::tools::engine::parse_manifest(&manifest) else {
        // A stored manifest that no longer parses is the tool authoring path's problem, not this
        // write's: the slot's declaration still applies.
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
    readings: &[GrabSampleReading],
    saved_by: &str,
) -> Result<Option<serde_json::Value>, AppError> {
    let Some(run_id) = tool_run_id else {
        if let Some(r) = readings.iter().find(|r| r.output.is_some() || r.input.is_some()) {
            return Err(AppError::BadRequest(format!(
                "Reading for parameter {} names tool {} '{}' but the request carries no \
                 tool_run_id",
                r.parameter_id,
                if r.output.is_some() { "output" } else { "input" },
                r.output.as_deref().or(r.input.as_deref()).unwrap_or_default()
            )));
        }
        return Ok(None);
    };

    let row = db
        .query_one(sea_orm::Statement::from_sql_and_values(
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
    let tool_version: serde_json::Value =
        row.try_get("", "tool_version").map_err(AppError::Database)?;
    let inputs: serde_json::Value = row.try_get("", "inputs").map_err(AppError::Database)?;
    let constants: serde_json::Value = row.try_get("", "constants").map_err(AppError::Database)?;
    let curves: serde_json::Value = row.try_get("", "curves").map_err(AppError::Database)?;
    let outputs: serde_json::Value = row.try_get("", "outputs").map_err(AppError::Database)?;
    let calculated_by: String = row.try_get("", "created_by").map_err(AppError::Database)?;
    let calculated_at: chrono::DateTime<chrono::Utc> = row
        .try_get::<chrono::DateTime<chrono::FixedOffset>>("", "created_at")
        .map_err(AppError::Database)?
        .with_timezone(&chrono::Utc);

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
        .query_one(sea_orm::Statement::from_sql_and_values(
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

    db.execute(sea_orm::Statement::from_sql_and_values(
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
        .query_one(sea_orm::Statement::from_sql_and_values(
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
    path = "/grab_samples",
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
            let sp_id =
                provision_site_parameter(&state.db, site.id, pid, &site.name).await?;
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

    if payload.dry_run {
        return Ok(Json(GrabSampleResponse {
            inserted: 0,
            samples_created: 0,
            created_sample_ids: vec![],
            dry_run: true,
            replaced: 0,
            preview,
            existing_groups,
        }));
    }

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

    // Window-aware attribution for grabs that name a sensor: which deployment the instrument was on
    // at the grab time (site-fixed to payload.site_id), instead of writing NULL. Grabs without a
    // sensor_id keep NULL deployment (manual lab values with no instrument).
    let grab_slots = {
        use crate::routes::private::sensors::operations::{
            ResolvedSlot, resolve_windows_for_times,
        };
        let mut times_by_sensor: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            if let Some(sid) = r.sensor_id {
                times_by_sensor.entry(sid).or_default().push(r.time);
            }
        }
        let mut slots: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), ResolvedSlot> =
            HashMap::new();
        for (sid, times) in &times_by_sensor {
            let resolved = resolve_windows_for_times(&state.db, *sid, Some(payload.site_id), times)
                .await
                .unwrap_or_default();
            for (t, slot) in resolved {
                slots.insert((*sid, t), slot);
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

    // The blob is stamped on every samples row this request touches; a run_id groups them back
    // into one tool run when the caller did not mint one itself.

    // One guarded transaction: a replace on a compressed chunk must not fail on the cap, and the
    // delete, the sample rows and the insert land together or not at all.
    let (inserted, replaced, created_sample_ids) =
        crate::common::bulk_write::guarded(&state.db, async |txn| {
            // Deleting a group's last replicate reaps its samples row through the trigger, so the
            // row's label, notes and authorship are captured first and restored onto the recreated
            // row wherever the request does not carry its own.
            let mut prior_samples: HashMap<
                (Uuid, chrono::DateTime<chrono::Utc>),
                (Option<String>, Option<String>, Option<String>),
            > = HashMap::new();
            let replaced: usize = if payload.mode == Some(GrabWriteMode::Replace) {
                for (parameter_id, time) in &groups {
                    if let Some(row) = txn
                        .query_one(sea_orm::Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            r"SELECT label, notes, created_by FROM samples
                              WHERE site_id = $1 AND parameter_id = $2 AND collected_at = $3",
                            [
                                payload.site_id.into(),
                                (*parameter_id).into(),
                                (*time).into(),
                            ],
                        ))
                        .await?
                    {
                        prior_samples.insert(
                            (*parameter_id, *time),
                            (
                                row.try_get("", "label").unwrap_or(None),
                                row.try_get("", "notes").unwrap_or(None),
                                row.try_get("", "created_by").unwrap_or(None),
                            ),
                        );
                    }
                }
                let mut removed: u64 = 0;
                for (parameter_id, time) in &groups {
                    let res = txn
                        .execute(sea_orm::Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            r"DELETE FROM readings
                              WHERE site_id = $1 AND parameter_id = $2 AND time = $3
                                AND measurement_type = 'spot'",
                            [
                                payload.site_id.into(),
                                (*parameter_id).into(),
                                (*time).into(),
                            ],
                        ))
                        .await?;
                    removed += res.rows_affected();
                }
                usize::try_from(removed).unwrap_or(usize::MAX)
            } else {
                0
            };

            // One samples row per (parameter, time) group in the request.
            let (sample_map, created_sample_ids) = auto_create_samples(
                txn,
                &payload.readings,
                payload.site_id,
                &SampleFacts {
                    created_by: payload.created_by.as_deref(),
                    label: payload.label.as_deref(),
                    notes: payload.notes.as_deref(),
                    provenance: provenance.as_ref(),
                },
                requested_estimator,
                &fixed_estimators,
            )
            .await?;

            for (group, sample_id) in &sample_map {
                let Some((label, notes, created_by)) = prior_samples.get(group) else {
                    continue;
                };
                txn.execute(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"UPDATE samples SET label = COALESCE(label, $2),
                                         notes = COALESCE(notes, $3),
                                         created_by = COALESCE(created_by, $4)
                      WHERE id = $1",
                    [
                        (*sample_id).into(),
                        label.clone().into(),
                        notes.clone().into(),
                        created_by.clone().into(),
                    ],
                ))
                .await?;
            }

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
                    sensor_id: Set(r.sensor_id),
                    calibration_id: Set(p.base_calibration.as_ref().map(|c| c.id)),
                    deployment_id: Set(r.sensor_id.and_then(|sid| {
                        grab_slots.get(&(sid, r.time)).and_then(|s| s.deployment_id)
                    })),
                    logged: Set(Some(true)),
                    measurement_type: Set(Some(GRAB_MEASUREMENT_TYPE.to_string())),
                    is_flagged: Set(Some(false)),
                    flag_reason: Set(None),
                    sample_id: Set(sample_map.get(&(r.parameter_id, r.time)).copied()),
                })
                .collect();

            let inserted = match readings::Entity::insert_many(models)
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

            // Readings the insert skipped on conflict still belong to the group's sample; linking
            // them fires the aggregate trigger so the stats cover every replicate. Scoped to spot
            // readings: a sonde reading sharing the grab's snapped timestamp must not be adopted
            // into the sample, or the trigger folds sensor data into the grab statistics.
            for ((parameter_id, time), sample_id) in &sample_map {
                txn.execute(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"UPDATE readings SET sample_id = $1
                      WHERE site_id = $2 AND parameter_id = $3 AND time = $4 AND sample_id IS NULL
                        AND measurement_type = 'spot'",
                    [
                        (*sample_id).into(),
                        payload.site_id.into(),
                        (*parameter_id).into(),
                        (*time).into(),
                    ],
                ))
                .await?;
            }

            // Every attributed spot instant this request touched belongs to a collection event
            // (D7); a hand-entered grab is a manual visit.
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
            }

            Ok((inserted, replaced, created_sample_ids))
        })
        .await?;

    // Event-driven open-alarm reconcile for the sampled slots (error-safe; backstop covers it),
    // plus historical episode reconstruction per slot so back-dated grabs land in alarm_events
    // like the batch/import paths. Inline rather than a tracked job to avoid one
    // reprocessing_jobs row per field campaign entry.
    if inserted > 0 || replaced > 0 {
        let alarm_slots: Vec<(Uuid, Uuid)> = stream_cache
            .keys()
            .map(|pid| (payload.site_id, *pid))
            .collect();
        crate::routes::private::alarms::sweeper::reconcile_and_notify(
            &state.db,
            &state.events,
            &alarm_slots,
        )
        .await;

        for (pid, (lo, hi)) in alarm_windows {
            if let Err(e) = crate::routes::private::alarms::episodes::evaluate_alarm_episodes(
                &state.db,
                payload.site_id,
                pid,
                lo,
                hi,
            )
            .await
            {
                tracing::warn!(error = %e, site_id = %payload.site_id, parameter_id = %pid, "alarm episode reconstruction failed");
            }
        }
    }

    if inserted > 0 || replaced > 0 {
        let site_id = payload.site_id;
        crate::common::cache::invalidate_prefix(&state, &format!("readings:{site_id}")).await;
        crate::common::cache::invalidate_prefix(&state, &format!("aggregates:{site_id}")).await;
    }

    let samples_created = created_sample_ids.len();
    tracing::info!(total, inserted, replaced, samples_created, site = %site.name, "Grab samples inserted");
    Ok(Json(GrabSampleResponse {
        inserted,
        samples_created,
        created_sample_ids,
        dry_run: false,
        replaced,
        preview,
        existing_groups,
    }))
}
