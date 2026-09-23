//! Running a tool and storing what it produced: the interactive save, the event
//! recompute chain and the two jobs that drive it.

use async_trait::async_trait;
use sea_orm::sea_query::{Alias, Expr, Order, Query};
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult,
    QueryFilter, Set, Statement,
};
use std::collections::HashSet;
use uuid::Uuid;

use super::models::{
    ActiveTool, AuditCounts, Engine, EventAudit, EventContext, EventPreview, EventRecompute,
    MissingConstant, PreviewedValue, RecomputeOutcome, RecomputeScope, RunOutcome, RunTrace,
    ToolCalculation, ToolResult, parse_manifest, run,
};
use super::service::{
    ParameterCatalog, ResolvedRun, VisitContext, build, evaluate_with_trace, execute_resolved,
    formula_bindings, list_active_tools, load_parameter_catalog, numbers_by_name, parse_pinned,
    resolve_event_inputs, resolve_replicate_inputs, resolve_run, resolve_site_inputs,
    run_active_tool, run_fingerprint, runner_runtime, served_spot_value_expr, stored_curves,
};
use super::staged::{StagedCell, StagedVisit};
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::models as collection_events;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::models::{
    GrabSampleReading, GrabSampleRequest, GrabWriteMode,
};
use crate::routes::private::readings::views::insert_grab_samples;
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};
use crate::routes::private::sync::hold_model;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;
use crate::routes::private::sync::service as audit;

/// Run a tool and store the `tool_runs` row that a later save references. Every path that
/// executes a tool for keeps goes through here — the interactive calculate endpoint, the CSV
/// tool-entry import, the chain executor — so the stored record has one shape.
///
/// The run row is written before the results are handed out: a save references the row, the
/// provenance blob is built from it, and every claim in the blob predates the save. The stored
/// inputs are the effective inputs the runner received (request values plus defaults and the
/// resolved site/event inputs), and `context` records where each resolved value came from.
pub async fn execute_and_store_run(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
    actor: &str,
    source: &str,
) -> AppResult<ToolResult> {
    let outcome = run_active_tool(state, tool, body).await?;
    store_run(state, tool, outcome, actor, source).await
}

/// [`execute_and_store_run`] for a run the caller has already resolved (and, say, compared
/// against a prior run before deciding to execute it).
pub async fn execute_and_store_resolved(
    state: &AppState,
    tool: &ActiveTool,
    resolved: ResolvedRun,
    actor: &str,
    source: &str,
) -> AppResult<ToolResult> {
    let outcome = execute_resolved(state, tool, resolved).await?;
    store_run(state, tool, outcome, actor, source).await
}

/// Run a tool and return what it computed, storing nothing: no `tool_runs` row and no `run_id`,
/// so a preview is never something a save can name (Q212).
pub async fn preview_run(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
) -> AppResult<ToolCalculation> {
    let outcome = run_active_tool(state, tool, body).await?;
    let runtime = runner_runtime(state).await;
    Ok(calculation_of(tool, runtime.as_ref(), outcome))
}

fn calculation_of(
    tool: &ActiveTool,
    runtime: Option<&super::models::RunnerRuntime>,
    outcome: RunOutcome,
) -> ToolCalculation {
    ToolCalculation {
        tool: tool.name.clone(),
        results: serde_json::Value::Object(outcome.results),
        cleared: outcome.cleared,
        skipped: outcome.skipped,
        refused: outcome.refused,
        inputs_used: outcome.inputs_used,
        inputs_ignored: outcome.inputs_ignored,
        constants: serde_json::Value::Object(outcome.constants),
        curves: outcome.curves,
        site_inputs: outcome.site_inputs,
        event_inputs: outcome.event_inputs,
        tool_version: tool.version_ref(runtime),
        trace: outcome.trace,
        consumed: outcome.consumed,
    }
}

pub(super) async fn store_run(
    state: &AppState,
    tool: &ActiveTool,
    outcome: RunOutcome,
    actor: &str,
    source: &str,
) -> AppResult<ToolResult> {
    let runtime = runner_runtime(state).await;
    let site_id = outcome.site_id;
    let collected_at = outcome.collected_at;
    let inputs = serde_json::Value::Object(outcome.inputs.clone());
    let calculation = calculation_of(tool, runtime.as_ref(), outcome);
    // The run records what the script produced, cleared outputs included: an explicit null is
    // what says the value was computed and is not a number, as against never computed at all.
    let stored_outputs = {
        let mut map = calculation.results.as_object().cloned().unwrap_or_default();
        for key in &calculation.cleared {
            map.insert(key.clone(), serde_json::Value::Null);
        }
        serde_json::Value::Object(map)
    };

    let context = if site_id.is_some()
        || !calculation.site_inputs.is_empty()
        || !calculation.event_inputs.is_empty()
        || !calculation.skipped.is_empty()
        || !calculation.consumed.is_empty()
    {
        serde_json::json!({
            "site_id": site_id,
            "collected_at": collected_at,
            "site_inputs": calculation.site_inputs,
            "event_inputs": calculation.event_inputs,
            "skipped": calculation.skipped,
            "consumed": calculation.consumed,
        })
    } else {
        serde_json::Value::Null
    };

    let run_id = Uuid::new_v4();
    run::ActiveModel {
        id: Set(run_id),
        tool_name: Set(tool.name.clone()),
        tool_version: Set(
            serde_json::to_value(&calculation.tool_version).unwrap_or(serde_json::Value::Null)
        ),
        inputs: Set(inputs),
        constants: Set(calculation.constants.clone()),
        curves: Set(serde_json::to_value(&calculation.curves).unwrap_or(serde_json::Value::Null)),
        outputs: Set(stored_outputs),
        created_by: Set(actor.to_string()),
        context: Set((!context.is_null()).then_some(context)),
        site_id: Set(site_id),
        collected_at: Set(collected_at),
        source: Set(source.to_string()),
        ..Default::default()
    }
    .insert(&state.db)
    .await?;

    Ok(ToolResult {
        calculation,
        run_id,
    })
}

/// Relative tolerance for the stale comparison. A recompute under the pinned version with the
/// stored inputs reproduces the value bit-for-bit; anything beyond float-noise means an input
/// (usually a re-resolved event input) or the stored value moved.
pub(super) const STALE_REL_TOL: f64 = 1e-9;

pub async fn load_event(db: &DatabaseConnection, id: Uuid) -> AppResult<EventContext> {
    let row = crate::routes::private::collection_events::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
    Ok(EventContext {
        id,
        site_id: row.site_id,
        collected_at: row.collected_at.with_timezone(&chrono::Utc),
    })
}

/// The catalog parameters a site holds a slot for. The set a calculation's applicability is read
/// against, and the only thing that declares it.
pub async fn declared_parameters(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashSet<Uuid>> {
    use crate::routes::private::site_parameters::models as site_parameters;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
    Ok(site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .select_only()
        .column(site_parameters::Column::ParameterId)
        .into_tuple::<Uuid>()
        .all(db)
        .await?
        .into_iter()
        .collect())
}

/// Whether a calculation applies at a site (Q193): the site declares every parameter the
/// calculation reads, so declaring the inputs declares the outputs and the run mints the output
/// slot it needs. Q98's test stands beside it rather than being dropped: a site that already
/// holds one of the outputs keeps the calculation, which is what a calculation reading nothing
/// but site properties and constants has.
#[must_use]
pub fn applies_at_site(
    read_inputs: &[Uuid],
    saved_outputs: &[(String, Uuid)],
    declared: &HashSet<Uuid>,
) -> bool {
    if !read_inputs.is_empty() && read_inputs.iter().all(|id| declared.contains(id)) {
        return true;
    }
    saved_outputs.iter().any(|(_, id)| declared.contains(id))
}

/// Why the chain leaves one of a run's outputs alone at a visit, or `None` when it writes it.
///
/// A slot an admin detached at this visit is a manual value until an input moves or it is
/// returned (Q40, Q47). A slot the site declares high cadence is the stream engine's (Q234):
/// `readings` holds one row per slot instant, so a visit that wrote it would overwrite the value
/// and provenance the stream pass put there. A two-output calculation may have one output on each
/// arm, which is why the question is asked per output rather than per run.
#[must_use]
pub fn output_skip_reason(
    key: &str,
    owner: crate::routes::private::readings::models::Owner,
    cadence: Option<&str>,
) -> Option<String> {
    if owner == crate::routes::private::readings::models::Owner::Manual {
        return Some(format!("output {key} is detached at this visit"));
    }
    if cadence == Some("high") {
        return Some(format!(
            "output {key} is a high-cadence slot at this site, computed on its stream"
        ));
    }
    None
}

/// The catalog ids of the parameters a calculation reads at a visit, for the applicability test.
/// A code the catalog does not hold resolves to nothing and is left out: it is a read the site
/// cannot declare, and `resolve_run` reports it as the skip it is.
#[must_use]
pub fn read_inputs(tool: &ActiveTool, catalog: &ParameterCatalog) -> Vec<Uuid> {
    read_input_members(tool, catalog)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// The same reads, carrying the code each resolved from, for the flows that name a missing input
/// back to a person rather than testing a set.
#[must_use]
pub fn read_input_members(tool: &ActiveTool, catalog: &ParameterCatalog) -> Vec<(Uuid, String)> {
    tool.manifest
        .read_codes()
        .iter()
        .filter_map(|code| catalog.resolve_code(code).map(|p| (p.id, p.code)))
        .collect()
}

/// Order tools so producers run before consumers: an edge A→B exists when one of A's outputs
/// resolves to a catalog parameter B reads, whether as an event input or as a replicate family.
/// A cycle is refused naming its members: two tools feeding each other have no runnable order.
pub fn dependency_order(tools: &[ActiveTool], catalog: &ParameterCatalog) -> AppResult<Vec<usize>> {
    let produced_codes: Vec<Vec<String>> = tools
        .iter()
        .map(|t| {
            t.manifest
                .outputs
                .iter()
                .filter_map(|o| catalog.resolve(o).map(|p| p.code.to_lowercase()))
                .collect()
        })
        .collect();
    let consumed_codes: Vec<Vec<String>> = tools.iter().map(|t| t.manifest.read_codes()).collect();

    let n = tools.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n]; // deps[b] = producers b waits on
    for b in 0..n {
        for (a, produced) in produced_codes.iter().enumerate() {
            if a != b && consumed_codes[b].iter().any(|code| produced.contains(code)) {
                deps[b].push(a);
            }
        }
    }

    crate::common::dependency::order(&deps).map_err(|cycle| {
        let members: Vec<&str> = cycle.iter().map(|&i| tools[i].name.as_str()).collect();
        AppError::Conflict(format!(
            "Tool event_inputs form a dependency cycle: {}",
            members.join(", ")
        ))
    })
}

/// The served spot value at one (site, parameter, instant): the sample mean, else the lowest
/// unflagged replicate. `None` when nothing is stored there.
pub async fn served_spot_value(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> AppResult<Option<f64>> {
    let row = db
        .query_one_raw(build(
            &Query::select()
                .expr_as(
                    served_spot_value_expr(
                        Expr::val(site_id),
                        Expr::val(parameter_id),
                        Expr::val(sea_orm::prelude::DateTimeWithTimeZone::from(at)),
                    ),
                    Alias::new("value"),
                )
                .to_owned(),
        ))
        .await?;
    match row {
        Some(r) => Ok(r.try_get("", "value")?),
        None => Ok(None),
    }
}

/// The latest provenance blob a given tool stored at this event, if any.
pub(super) async fn blob_at_event(
    db: &DatabaseConnection,
    event: &EventContext,
    tool: &str,
) -> AppResult<Option<serde_json::Value>> {
    use sea_orm::sea_query::ExprTrait;
    let row = db
        .query_one_raw(build(
            &Query::select()
                .column(readings::Column::Provenance)
                .from(readings::Entity)
                .and_where(Expr::col(readings::Column::SiteId).eq(event.site_id))
                .and_where(Expr::col(readings::Column::Time).eq(event.collected_at))
                .and_where(Expr::cust_with_values("provenance ->> 'tool' = $1", [tool]))
                .order_by_expr(Expr::cust("provenance ->> 'saved_at'"), Order::Desc)
                .limit(1)
                .to_owned(),
        ))
        .await?;
    // The column is nullable, so a row with no blob is None; a row that will not decode is an
    // error, because the chain reads this to decide whether a run already stands.
    Ok(row
        .map(|r| r.try_get::<Option<serde_json::Value>>("", "provenance"))
        .transpose()?
        .flatten())
}

/// The slot value a body carries to name the curve a stored snapshot recorded: the catalog curve
/// by id where the run chose one, so a later edit of its coefficients is picked up, else the
/// coefficients the run was given by hand.
fn curve_slot_value(curve: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(id) = curve.get("standard_curve_id").and_then(|v| v.as_str()) {
        return Some(serde_json::json!({ "standard_curve_id": id }));
    }
    let slope = curve.get("slope")?.as_f64()?;
    let intercept = curve.get("intercept")?.as_f64()?;
    Some(serde_json::json!({ "slope": slope, "intercept": intercept }))
}

/// The request body for a run at this event: the prior run's stored inputs when one exists (minus
/// the params the context re-resolves, so upstream changes propagate) and the curve each slot was
/// filled with, plus the context fields.
pub(super) fn body_for_run(
    tool: &ActiveTool,
    event: &EventContext,
    prior_blob: Option<&serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut body = serde_json::Map::new();
    if let Some(inputs) = prior_blob
        .and_then(|b| b.get("inputs"))
        .and_then(serde_json::Value::as_object)
    {
        body = inputs.clone();
        for e in &tool.manifest.event_inputs {
            body.remove(&e.param);
        }
        // A replicate family is re-read from the store too: keeping the prior run's list would
        // recompute the same numbers after the visit's repeats moved.
        for p in tool
            .manifest
            .params
            .iter()
            .filter(|p| p.kind == "replicates")
        {
            body.remove(&p.name);
        }
        for s in &tool.manifest.site_inputs {
            body.remove(s.target());
        }
    }
    // The curves live beside the inputs in the blob, never in them, so a recompute that carried
    // the inputs alone left every curve slot unsupplied and skipped the formula reading it.
    if let Some(snapshots) = prior_blob
        .and_then(|b| b.get("curves"))
        .and_then(serde_json::Value::as_array)
    {
        for snapshot in snapshots {
            let Some(name) = snapshot.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !tool.manifest.curves.iter().any(|c| c.name == name) {
                continue;
            }
            if let Some(value) = snapshot.get("curve").and_then(curve_slot_value) {
                body.insert(name.to_string(), value);
            }
        }
    }
    body.insert("site_id".into(), serde_json::json!(event.site_id));
    body.insert(
        "collected_at".into(),
        serde_json::json!(
            event
                .collected_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
    );
    body
}

/// Why one tool was not executed at one event. Recorded, never fatal: the chain runs what it can.
pub(super) fn skip_reason(e: &AppError) -> Option<String> {
    match e {
        AppError::BadRequest(msg) => Some(msg.clone()),
        AppError::ToolScriptError { message, .. } => Some(format!("script error: {message}")),
        _ => None,
    }
}

/// The fingerprint of the run a provenance blob records, comparable with a fresh
/// [`ResolvedRun::fingerprint`]. `None` when the blob pins no stored script version.
pub(super) fn blob_fingerprint(blob: &serde_json::Value) -> Option<String> {
    let version_id = blob
        .get("tool_version")
        .and_then(|v| v.get("script_version_id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())?;
    let empty_object = serde_json::json!({});
    let empty_array = serde_json::json!([]);
    Some(run_fingerprint(
        version_id,
        blob.get("inputs").unwrap_or(&empty_object),
        blob.get("constants").unwrap_or(&empty_object),
        blob.get("curves").unwrap_or(&empty_array),
    ))
}

/// Whether every output the prior run saved is still served at its value. A value someone put
/// in the slot since is superseded by a recompute, however unchanged the inputs are.
pub(super) async fn outputs_still_served(
    db: &DatabaseConnection,
    event: &EventContext,
    blob: &serde_json::Value,
) -> AppResult<bool> {
    let Some(saved) = blob.get("saved").and_then(serde_json::Value::as_object) else {
        return Ok(false);
    };
    for (output, parameter) in saved {
        let Some(parameter_id) = parameter.as_str().and_then(|s| s.parse::<Uuid>().ok()) else {
            return Ok(false);
        };
        let Some(produced) = blob
            .get("outputs")
            .and_then(|o| o.get(output))
            .and_then(serde_json::Value::as_f64)
        else {
            return Ok(false);
        };
        let Some(served) =
            served_spot_value(db, event.site_id, parameter_id, event.collected_at).await?
        else {
            return Ok(false);
        };
        if disagrees(served, produced) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a stored value and a recomputed one disagree, at the audit's relative tolerance. The
/// scale floor is what keeps a pair straddling zero from dividing by nothing.
pub(super) fn disagrees(stored: f64, recomputed: f64) -> bool {
    let scale = stored.abs().max(recomputed.abs()).max(1e-12);
    (stored - recomputed).abs() / scale > STALE_REL_TOL
}

/// The readings one output of a run stores. A per-replicate output is an array, one entry per
/// index of the variable it evaluated over, and each entry is a reading at that index; a gap
/// stays a gap rather than closing up the indexes after it.
pub(super) fn readings_for_output(
    key: &str,
    parameter_id: Uuid,
    value: &serde_json::Value,
    time: chrono::DateTime<chrono::Utc>,
) -> Vec<GrabSampleReading> {
    let reading = |value: f64, replicate_index: Option<i16>| GrabSampleReading {
        input: None,
        parameter_id,
        sensor_id: None,
        value,
        time,
        replicate_index,
        output: Some(key.to_string()),
        standard_curve_id: None,
    };
    match value {
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                Some(reading(item.as_f64()?, Some(i16::try_from(index).ok()?)))
            })
            .collect(),
        other => other
            .as_f64()
            .map(|v| reading(v, None))
            .into_iter()
            .collect(),
    }
}

/// Run every active tool whose inputs resolve at this event, in dependency order, saving the
/// outputs through the grab write path. Each executed tool mints a real `tool_runs` row
/// (`source = 'chain'`), so the recomputed values carry the same verified provenance a hand save
/// gets.
pub async fn recompute_event(
    state: &AppState,
    event_id: Uuid,
    actor: &str,
) -> AppResult<RecomputeOutcome> {
    let event = load_event(&state.db, event_id).await?;
    let mut staged = StagedVisit::default();
    Ok(
        walk_event(state, &event, &mut staged, &Pass::Save { actor })
            .await?
            .outcome,
    )
}

/// The same chain over the same visit, run against what the operator has typed and not saved, and
/// storing none of it: no `tool_runs` row, no reading, no decision, no finding, no output slot and
/// no job, so nothing a save could cite as provenance (Q212). Save is the only writer, and it runs
/// the same walk.
pub async fn preview_event(
    state: &AppState,
    event_id: Uuid,
    cells: &[StagedCell],
) -> AppResult<EventPreview> {
    let event = load_event(&state.db, event_id).await?;
    let mut staged =
        StagedVisit::resolve(&state.db, event.site_id, event.collected_at, cells).await?;
    let pass = walk_event(state, &event, &mut staged, &Pass::Preview).await?;
    Ok(EventPreview {
        site_id: event.site_id,
        collected_at: event.collected_at,
        outputs: pass.outputs,
        calculations: pass.calculations,
        skipped: pass.outcome.skipped,
        not_applicable: pass.outcome.not_applicable,
        unchanged: pass.outcome.unchanged,
    })
}

/// What a pass of the chain does with what each run produces. Everything before that point is the
/// same decision on both paths: the dependency order, applicability, the prior run's inputs, the
/// pinned version, constants, curves, slot ownership, and every skip and refusal.
pub(super) enum Pass<'a> {
    /// For keeps: mint the run, save the outputs, raise and close the findings.
    Save { actor: &'a str },
    /// For the screen: keep the outputs in the staged overlay alone, so the tools downstream read
    /// them, and write nothing.
    Preview,
}

/// What one pass of the chain over a visit did.
pub(super) struct ChainPass {
    outcome: RecomputeOutcome,
    /// One entry per tool that ran, in the order it ran.
    calculations: Vec<ToolCalculation>,
    /// The values the pass produced, by output slot and replicate.
    outputs: Vec<PreviewedValue>,
}

async fn walk_event(
    state: &AppState,
    event: &EventContext,
    staged: &mut StagedVisit,
    pass: &Pass<'_>,
) -> AppResult<ChainPass> {
    let event = event.clone();
    let tools = list_active_tools(&state.db).await?;
    let catalog = load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest)).await?;
    let order = dependency_order(&tools, &catalog)?;
    let actor = match pass {
        Pass::Save { actor } => *actor,
        Pass::Preview => "",
    };
    let mut calculations: Vec<ToolCalculation> = Vec::new();
    let mut previewed: Vec<PreviewedValue> = Vec::new();

    let mut outcome = RecomputeOutcome {
        readings_withdrawn: 0,
        tools_run: 0,
        readings_written: 0,
        findings_closed: 0,
        skipped: Vec::new(),
        findings_raised: 0,
        not_applicable: Vec::new(),
        slots_minted: 0,
        unchanged: Vec::new(),
    };

    // A calculation applies at a site when the site declares what it reads (Q193, narrowing Q98):
    // the site parameters are still the declaration, so the calculation set is filtered by them
    // before the dependency order is walked, rather than every enabled tool being run wherever
    // its inputs happen to resolve. The output slot follows from the inputs, minted by the run.
    let declared = declared_parameters(&state.db, event.site_id).await?;

    for i in order {
        let tool = &tools[i];
        let saved_outputs: Vec<(String, Uuid)> = tool
            .manifest
            .outputs
            .iter()
            .filter_map(|o| catalog.resolve(o).map(|p| (o.key.clone(), p.id)))
            .collect();
        if saved_outputs.is_empty() {
            continue;
        }
        if !applies_at_site(&read_inputs(tool, &catalog), &saved_outputs, &declared) {
            outcome.not_applicable.push(tool.name.clone());
            continue;
        }

        let prior = blob_at_event(&state.db, &event, &tool.name).await?;
        let body = body_for_run(tool, &event, prior.as_ref());
        let body_bytes = serde_json::to_vec(&serde_json::Value::Object(body))
            .map_err(|e| AppError::Internal(e.to_string()))?;

        let resolved = match resolve_run(
            state,
            tool,
            &body_bytes,
            None,
            MissingConstant::Refuse,
            Some(&*staged),
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(e) => match skip_reason(&e) {
                Some(reason) => {
                    outcome.findings_raised +=
                        note_skip(state, pass, &event, &tool.name, &saved_outputs, &reason).await?;
                    outcome.skipped.push((tool.name.clone(), reason));
                    continue;
                }
                None => return Err(e),
            },
        };
        // A run that would consume exactly what the prior run consumed, under the same script
        // version, produces the same outputs: nothing to mint, nothing to rewrite.
        if let Some(blob) = prior.as_ref()
            && blob_fingerprint(blob).as_deref()
                == Some(resolved.fingerprint(tool.version_id).as_str())
            && outputs_still_served(&state.db, &event, blob).await?
        {
            outcome.unchanged.push(tool.name.clone());
            continue;
        }

        let (calculation, run_id) = match execute_pass(state, pass, tool, resolved).await {
            Ok(result) => result,
            Err(e) => match skip_reason(&e) {
                Some(reason) => {
                    outcome.findings_raised +=
                        note_skip(state, pass, &event, &tool.name, &saved_outputs, &reason).await?;
                    outcome.skipped.push((tool.name.clone(), reason));
                    continue;
                }
                None => return Err(e),
            },
        };
        calculations.push(calculation.clone());

        // The outputs the run produced, saved to their resolved parameters. A per-replicate
        // (array) output is saved one reading per index, inheriting the position of the variable
        // it evaluated over.
        let mut owned_outputs: Vec<(String, Uuid)> = Vec::with_capacity(saved_outputs.len());
        for (key, parameter_id) in &saved_outputs {
            let owner = crate::routes::private::readings::service::output_owner(
                &state.db,
                event.site_id,
                *parameter_id,
                event.collected_at,
            )
            .await?;
            let cadence = crate::routes::private::site_parameters::service::slot_cadence(
                &state.db,
                event.site_id,
                *parameter_id,
            )
            .await?;
            match output_skip_reason(key, owner, cadence.as_deref()) {
                Some(reason) => outcome.skipped.push((tool.name.clone(), reason)),
                None => owned_outputs.push((key.clone(), *parameter_id)),
            }
        }
        // An output whose formula produced a number that is not finite is refused, not cleared
        // (Q172): the value stored at this visit stands and stays served, so the finding is the
        // only thing that says the calculation divided by zero.
        for (key, parameter_id) in &owned_outputs {
            if !calculation.refused.iter().any(|r| r == key) {
                continue;
            }
            let reason = skipped_reason(&calculation.skipped, key)
                .unwrap_or_else(|| "the result is not a finite number".to_string());
            if let Pass::Save { .. } = pass {
                raise_skip(&state.db, &event, &tool.name, key, *parameter_id, &reason).await?;
                outcome.findings_raised += 1;
            }
            outcome
                .skipped
                .push((tool.name.clone(), format!("{key}: {reason}")));
        }

        // An output the script computed as NA is a request to blank the column, so the stored
        // value is withdrawn rather than left standing beside a run that did not produce it. A
        // person's ruling on the row is not overridden: those keep their value and their hold.
        for (key, parameter_id) in &owned_outputs {
            if !calculation.cleared.iter().any(|c| c == key) {
                continue;
            }
            if let Pass::Preview = pass {
                staged.retract(*parameter_id);
                previewed.push(PreviewedValue {
                    output: key.clone(),
                    parameter_id: *parameter_id,
                    replicate_index: None,
                    value: None,
                });
                continue;
            }
            let withdrawn = crate::common::bulk_write::guarded(&state.db, async |txn| {
                crate::routes::private::readings::service::record_many(
                    txn,
                    crate::routes::private::readings::models::Kind::Withdraw,
                    {
                        use crate::routes::private::collection_events::flows::row;
                        use crate::routes::private::readings::models::Column;
                        use sea_orm::ExprTrait as _;
                        sea_orm::Condition::all()
                            .add(row(Column::SiteId).eq(event.site_id))
                            .add(row(Column::ParameterId).eq(*parameter_id))
                            .add(row(Column::Time).eq(
                                sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at),
                            ))
                            .add(row(Column::MeasurementType).eq("spot"))
                            .add(row(Column::WithdrawnAt).is_null())
                            // A row somebody has ruled on is not the recompute's to retract.
                            .add(crate::routes::private::readings::service::unjudged("r"))
                    },
                    crate::routes::private::readings::service::NewValue::Literal(
                        serde_json::json!({ "reason": "the calculation now yields no value" }),
                    ),
                    actor,
                    Some("computed as NA by the recompute"),
                    crate::routes::private::readings::models::Origin::Chain,
                    None,
                )
                .await
            })
            .await?;
            outcome.readings_withdrawn += usize::try_from(withdrawn.rows).unwrap_or(0);
        }

        let readings: Vec<GrabSampleReading> = owned_outputs
            .iter()
            .flat_map(|(key, parameter_id)| match calculation.results.get(key) {
                Some(value) => readings_for_output(key, *parameter_id, value, event.collected_at),
                None => Vec::new(),
            })
            .collect();
        if readings.is_empty() {
            let reason = "run produced no savable output".to_string();
            // A refused output already carries the arithmetic that stopped it; the generic reason
            // would replace it with a vaguer one.
            let unexplained: Vec<(String, Uuid)> = saved_outputs
                .iter()
                .filter(|(key, _)| !calculation.refused.iter().any(|r| r == key))
                .cloned()
                .collect();
            outcome.findings_raised +=
                note_skip(state, pass, &event, &tool.name, &unexplained, &reason).await?;
            outcome.skipped.push((tool.name.clone(), reason));
            continue;
        }

        // The preview stops here: the values it produced go into the overlay, so the tools
        // downstream read what this run made rather than what the store still holds, and nothing
        // is written. Everything below is the save.
        if let Pass::Preview = pass {
            for (key, parameter_id) in &owned_outputs {
                let produced = readings
                    .iter()
                    .filter(|r| r.output.as_deref() == Some(key.as_str()))
                    .map(|r| (r.replicate_index.unwrap_or(0), r.value))
                    .collect::<Vec<_>>();
                if produced.is_empty() {
                    continue;
                }
                staged.take_output(*parameter_id, &produced);
                previewed.extend(produced.iter().map(|(index, value)| PreviewedValue {
                    output: key.clone(),
                    parameter_id: *parameter_id,
                    replicate_index: Some(*index),
                    value: Some(*value),
                }));
            }
            outcome.tools_run += 1;
            continue;
        }

        // A value computed from a pending measurement is pending too (M62): whatever an intern
        // entered at this visit carries into everything the chain derives from it.
        let inputs_pending: bool = state
            .db
            .query_one_raw(pending_inputs_at(event.site_id, event.collected_at))
            .await?
            .map(|r| r.try_get("", "p"))
            .transpose()?
            .unwrap_or(false);

        // The site declared the inputs, so it gets the column the run publishes (Q193). The slot
        // is minted needing review, so a manager confirms it from the site's Parameters tab.
        for (key, parameter_id) in &owned_outputs {
            if declared.contains(parameter_id) || calculation.results.get(key).is_none() {
                continue;
            }
            if crate::routes::private::site_parameters::service::mint_tool_slot(
                &state.db,
                event.site_id,
                *parameter_id,
            )
            .await?
            .is_some()
            {
                outcome.slots_minted += 1;
            }
        }

        let auth = crate::common::middleware::AuthContext::Keycloak {
            roles: Vec::new(),
            sub: actor.to_string(),
            email: None,
            email_verified: false,
            grants: std::sync::Arc::new(std::collections::HashSet::new()),
        };
        let request = GrabSampleRequest {
            expected_replicates: None,
            site_id: event.site_id,
            pending_inputs: inputs_pending,
            created_by: Some(actor.to_string()),
            label: None,
            notes: None,
            mode: Some(GrabWriteMode::Replace),
            dry_run: false,
            tool_run_id: run_id,
            check_id: None,
            // The tool's manifest is read by the save path itself; nothing here overrides
            // the slot's declaration.
            readings,
        };
        let saved = insert_grab_samples(
            axum::extract::State(state.clone()),
            axum::Extension(auth),
            crate::common::middleware::ProjectScope(
                crate::common::authz::AccessScope::Unrestricted,
            ),
            axum::Json(request),
        )
        .await?;
        outcome.tools_run += 1;
        outcome.readings_written += saved.0.inserted;
        // The value the finding reported is gone: the finding is closed by the repair itself,
        // not left for the next audit to notice.
        for (_, parameter_id) in &saved_outputs {
            outcome.findings_closed +=
                supersede_findings(&state.db, &event, *parameter_id).await? as usize;
        }
        // A set whose other outputs saved takes none of the whole-tool skip arms, so the outputs
        // the engine refused are filed here, one per slot, under the reason it gave.
        for skip in &calculation.skipped {
            let Some((output, reason)) = skipped_entry(skip) else {
                continue;
            };
            let Some((_, parameter_id)) = saved_outputs.iter().find(|(code, _)| code == output)
            else {
                continue;
            };
            outcome.findings_raised += note_skip(
                state,
                pass,
                &event,
                &tool.name,
                &[(output.to_string(), *parameter_id)],
                reason,
            )
            .await?;
            outcome
                .skipped
                .push((tool.name.clone(), reason.to_string()));
        }
    }

    Ok(ChainPass {
        outcome,
        calculations,
        outputs: previewed,
    })
}

/// Execute the resolved run under this pass. The save mints the `tool_runs` row the saved
/// readings cite; the preview mints none, so no value it returns can be cited as provenance
/// (Q212).
async fn execute_pass(
    state: &AppState,
    pass: &Pass<'_>,
    tool: &ActiveTool,
    resolved: ResolvedRun,
) -> AppResult<(ToolCalculation, Option<Uuid>)> {
    match pass {
        Pass::Save { actor } => {
            let result = execute_and_store_resolved(state, tool, resolved, actor, "chain").await?;
            Ok((result.calculation, Some(result.run_id)))
        }
        Pass::Preview => {
            let outcome = execute_resolved(state, tool, resolved).await?;
            let runtime = runner_runtime(state).await;
            Ok((calculation_of(tool, runtime.as_ref(), outcome), None))
        }
    }
}

/// File the finding a skip owes, where the pass is the one that writes. A preview reports the same
/// skip and files nothing.
async fn note_skip(
    state: &AppState,
    pass: &Pass<'_>,
    event: &EventContext,
    tool: &str,
    outputs: &[(String, Uuid)],
    reason: &str,
) -> AppResult<usize> {
    match pass {
        Pass::Save { .. } => record_skip(&state.db, event, tool, outputs, reason).await,
        Pass::Preview => Ok(0),
    }
}

pub(super) async fn events_in_scope(
    db: &DatabaseConnection,
    scope: &RecomputeScope,
) -> Result<Vec<Uuid>, DbErr> {
    let (sql, binds) = scope.events_sql();
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            binds,
        ))
        .await?;
    rows.iter().map(|r| r.try_get("", "id")).collect()
}

pub(super) struct FindingPayload {
    expected: serde_json::Value,
    computed: serde_json::Value,
    delta: serde_json::Value,
}

pub(super) async fn upsert_finding(
    db: &DatabaseConnection,
    kind: HoldKind,
    event: &EventContext,
    parameter_id: Uuid,
    tool: &str,
    payload: FindingPayload,
) -> AppResult<()> {
    audit::upsert_hold(
        db,
        &audit::Hold {
            key: audit::HoldKey::Slot {
                site_id: event.site_id,
                parameter_id,
                group_time: event.collected_at,
            },
            kind,
            expected: payload.expected,
            computed: payload.computed,
            delta: payload.delta,
            status: HoldStatus::Pending,
            tool: Some(tool),
        },
    )
    .await
}

/// Why the run reported this output as a step that did not run.
fn skipped_reason(skipped: &[serde_json::Value], output: &str) -> Option<String> {
    skipped.iter().find_map(|entry| {
        (entry.get("output")?.as_str()? == output)
            .then(|| entry.get("reason")?.as_str().map(str::to_string))
            .flatten()
    })
}

/// A step that did not run is a fact about the visit, not only about the run that skipped it: the
/// outputs it would have produced are absent, and the reason belongs where it outlives the job
/// row the counts are pruned with. An output some other path already filled is not reported.
pub(super) async fn record_skip(
    db: &DatabaseConnection,
    event: &EventContext,
    tool: &str,
    saved_outputs: &[(String, Uuid)],
    reason: &str,
) -> AppResult<usize> {
    let mut raised = 0;
    for (output, parameter_id) in saved_outputs {
        if served_spot_value(db, event.site_id, *parameter_id, event.collected_at)
            .await?
            .is_some()
        {
            continue;
        }
        raise_skip(db, event, tool, output, *parameter_id, reason).await?;
        raised += 1;
    }
    Ok(raised)
}

/// The output and the reason a run's `skipped` entry names, as `evaluate_set` writes it.
pub(super) fn skipped_entry(entry: &serde_json::Value) -> Option<(&str, &str)> {
    Some((
        entry.get("output")?.as_str()?,
        entry.get("reason")?.as_str()?,
    ))
}

/// Report one output as a step that did not run, whatever the slot already holds.
///
/// A refused output (Q172) keeps the value stored at the visit, so the finding is the only thing
/// that says the calculation divided by zero: it is raised against a served slot too, which is
/// what separates this from [`record_skip`].
pub(super) async fn raise_skip(
    db: &DatabaseConnection,
    event: &EventContext,
    tool: &str,
    output: &str,
    parameter_id: Uuid,
    reason: &str,
) -> AppResult<()> {
    // The absence is now explained, so the audit's account of the same slot gives way to it.
    supersede(
        db,
        hold_model::of_kinds(
            hold_model::in_status(
                hold_model::slot(event.site_id, parameter_id, event.collected_at),
                HoldStatus::Pending,
            ),
            &[HoldKind::MissingOutput, HoldKind::StaleOutput],
        ),
    )
    .await?;
    upsert_finding(
        db,
        HoldKind::SkippedOutput,
        event,
        parameter_id,
        tool,
        FindingPayload {
            expected: serde_json::json!({ "output": output, "reason": reason }),
            computed: serde_json::json!({}),
            delta: serde_json::json!({}),
        },
    )
    .await
}

/// Whether the executor already reported this slot as a step that did not run. The skip carries
/// the reason, so the audit adds nothing by also calling the output absent.
pub(super) async fn has_pending_skip(
    db: &DatabaseConnection,
    event: &EventContext,
    parameter_id: Uuid,
) -> AppResult<bool> {
    let found = hold_model::Entity::find()
        .filter(hold_model::of_kinds(
            hold_model::in_status(
                hold_model::slot(event.site_id, parameter_id, event.collected_at),
                HoldStatus::Pending,
            ),
            &[HoldKind::SkippedOutput],
        ))
        .one(db)
        .await?;
    Ok(found.is_some())
}

/// Mark every finding the condition selects superseded, and say how many moved.
async fn supersede(db: &DatabaseConnection, condition: sea_orm::Condition) -> AppResult<u64> {
    let res = hold_model::Entity::update_many()
        .col_expr(
            hold_model::Column::Status,
            Expr::value(HoldStatus::Superseded.as_str()),
        )
        .filter(condition)
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// Close open findings for slots the current audit found in agreement (or now populated).
pub(super) async fn supersede_findings(
    db: &DatabaseConnection,
    event: &EventContext,
    parameter_id: Uuid,
) -> AppResult<u64> {
    supersede(
        db,
        hold_model::in_status(
            hold_model::slot(event.site_id, parameter_id, event.collected_at),
            HoldStatus::Pending,
        ),
    )
    .await
}

/// The pinned script version a blob names, rebuilt as a runnable tool. `None` when the blob names
/// no stored version (a draft run) or the version row is gone.
pub(super) async fn pinned_tool(
    db: &DatabaseConnection,
    tool_name: &str,
    blob: &serde_json::Value,
) -> AppResult<Option<ActiveTool>> {
    let Some(version_id) = blob
        .get("tool_version")
        .and_then(|v| v.get("script_version_id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return Ok(None);
    };
    let Some(row) = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.tool_script_id, v.version_no, v.script, v.entry_function, v.manifest,
                    v.content_hash, s.engine
             FROM tool_script_versions v
             JOIN tool_scripts s ON s.id = v.tool_script_id
             WHERE v.id = $1",
            [version_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let row = PinnedVersionRow::from_query_result(&row, "")?;
    let Ok(manifest) = parse_manifest(&row.manifest) else {
        return Ok(None);
    };
    let engine = Engine::parse(&row.engine).unwrap_or(Engine::Script);
    let formulas = if engine == Engine::Formula {
        match parse_pinned(&row.script) {
            Ok(formulas) => formulas,
            Err(_) => return Ok(None),
        }
    } else {
        Vec::new()
    };
    Ok(Some(ActiveTool {
        script_id: row.tool_script_id,
        name: tool_name.to_string(),
        label: manifest.label.clone(),
        description: manifest.description.clone(),
        version_id,
        version_no: row.version_no,
        script: row.script,
        entry_function: row.entry_function,
        content_hash: row.content_hash,
        manifest,
        engine,
        // The version body is the formula set, so a recompute under a pinned version runs the
        // formulas that version holds rather than the definitions as they stand today.
        formulas,
    }))
}

/// One stored run replayed under the version it pinned: each formula with the values it read and
/// the number it produced.
///
/// The version body is the formula set and its identity is a content hash, so re-evaluating the
/// run's own inputs, constants and curves under it reproduces what the run returned rather than
/// what the calculation would produce today.
pub async fn replay_trace(db: &DatabaseConnection, run: &run::Model) -> AppResult<RunTrace> {
    let blob = serde_json::json!({ "tool_version": run.tool_version });
    let tool = pinned_tool(db, &run.tool_name, &blob)
        .await?
        .ok_or_else(|| {
            AppError::Conflict(format!(
                "The version this {} run pinned is no longer stored, so it cannot be replayed",
                run.tool_name
            ))
        })?;
    if tool.engine != Engine::Formula {
        return Err(AppError::Conflict(format!(
            "'{}' runs on the {} engine, which records no formulas to replay",
            run.tool_name,
            tool.engine.as_str()
        )));
    }
    let inputs = run.inputs.as_object().cloned().unwrap_or_default();
    let constants = run.constants.as_object().cloned().unwrap_or_default();
    let (numbers, replicates) = formula_bindings(&inputs);
    let (_, trace) = evaluate_with_trace(
        &tool.formulas,
        &numbers,
        &replicates,
        &numbers_by_name(&constants),
        &stored_curves(&run.curves),
    )
    .map_err(|message| AppError::Conflict(format!("{}: {message}", run.tool_name)))?;
    let context = run.context.clone().unwrap_or(serde_json::Value::Null);
    let from_context = |field: &str| {
        context
            .get(field)
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    Ok(RunTrace {
        run_id: run.id,
        tool: run.tool_name.clone(),
        label: tool.label,
        version_no: tool.version_no,
        site_id: context
            .get("site_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok()),
        collected_at: context
            .get("collected_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc)),
        event_inputs: from_context("event_inputs"),
        site_inputs: from_context("site_inputs"),
        constants: serde_json::Value::Object(constants),
        trace,
        results: run.outputs.clone(),
        skipped: from_context("skipped"),
        curves: run.curves.clone(),
        manifest: serde_json::to_value(&tool.manifest).unwrap_or(serde_json::Value::Null),
    })
}

/// A pinned tool version as the chain reads it back. The manifest and the formula body are parsed
/// from the derived row rather than decoded here: a stored version outside either vocabulary is a
/// version this executor cannot run, not a decode failure.
#[derive(FromQueryResult)]
pub(super) struct PinnedVersionRow {
    tool_script_id: Uuid,
    version_no: i32,
    script: String,
    entry_function: String,
    content_hash: String,
    manifest: serde_json::Value,
    engine: String,
}

/// Whether every required param of a tool is answerable at the event without a person: a manifest
/// default, a resolvable site property, or a same-event value. This is "the declared inputs
/// exist" for the missing-output report.
pub(super) async fn inputs_exist(
    state: &AppState,
    tool: &ActiveTool,
    event: &EventContext,
) -> AppResult<bool> {
    if tool.manifest.event_inputs.is_empty() {
        // Without event inputs the tool's inputs are typed by a person; their absence is not a
        // reportable state of the event.
        return Ok(false);
    }
    let mut probe = serde_json::Map::new();
    probe.insert("site_id".into(), serde_json::json!(event.site_id));
    probe.insert(
        "collected_at".into(),
        serde_json::json!(event.collected_at.to_rfc3339()),
    );
    // Reuse the engine's own resolution by dry-probing requiredness: resolve context fills, then
    // check every required param is present or defaulted.
    let mut body = probe.clone();
    body.remove("site_id");
    body.remove("collected_at");
    let mut consumed = Vec::new();
    let site = resolve_site_inputs(
        &state.db,
        &tool.name,
        &tool.manifest,
        Some(event.site_id),
        &mut body,
        &mut consumed,
    )
    .await;
    if site.is_err() {
        return Ok(false);
    }
    let visit = VisitContext {
        site_id: Some(event.site_id),
        collected_at: Some(event.collected_at),
        staged: None,
    };
    resolve_event_inputs(
        &state.db,
        &tool.name,
        &tool.manifest,
        visit,
        &mut body,
        &mut consumed,
    )
    .await?;
    resolve_replicate_inputs(&state.db, &tool.manifest, visit, &mut body, &mut consumed).await?;
    for p in &tool.manifest.params {
        if !p.required {
            continue;
        }
        let present = body.get(&p.name).is_some_and(|v| !v.is_null());
        if !present && p.default.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Audit one event: report missing and stale outputs into the review queue. Never writes a value.
pub async fn audit_event(
    state: &AppState,
    event: &EventContext,
    tools: &[ActiveTool],
    catalog: &ParameterCatalog,
    order: &[usize],
    counts: &mut AuditCounts,
) -> AppResult<()> {
    // The report covers what the repair covers: a calculation the site declares nothing of,
    // neither what it reads nor what it writes, does not apply here, so its absent output is not
    // a finding (Q98, narrowed by Q193).
    let declared = declared_parameters(&state.db, event.site_id).await?;
    for &i in order {
        let tool = &tools[i];
        let saved_outputs: Vec<(String, Uuid)> = tool
            .manifest
            .outputs
            .iter()
            .filter_map(|o| catalog.resolve(o).map(|p| (o.key.clone(), p.id)))
            .collect();
        if saved_outputs.is_empty() {
            continue;
        }
        if !applies_at_site(&read_inputs(tool, catalog), &saved_outputs, &declared) {
            continue;
        }

        let prior = blob_at_event(&state.db, event, &tool.name).await?;
        if let Some(blob) = prior {
            // Stale check under the pinned version, with the stored inputs and freshly resolved
            // context — an upstream correction shows up as a disagreement here.
            let Some(pinned) = pinned_tool(&state.db, &tool.name, &blob).await? else {
                continue;
            };
            let body = body_for_run(&pinned, event, Some(&blob));
            let body_bytes = serde_json::to_vec(&serde_json::Value::Object(body))
                .map_err(|e| AppError::Internal(e.to_string()))?;
            let outcome = match run_active_tool(state, &pinned, &body_bytes).await {
                Ok(o) => o,
                Err(e) => match skip_reason(&e) {
                    Some(_) => continue,
                    None => return Err(e),
                },
            };
            // A run judged only under its own version agrees with itself after its calculation is
            // edited, so an edit would never be reported. When the calculation has activated a
            // different version since, the same body is run again under that one: the stored value
            // is then compared against what the calculation says today. Values an edit did not move
            // are not reported, which is what keeps a version bump over a set of formulas from
            // raising a finding on every output in it.
            let current = if pinned.version_id == tool.version_id {
                None
            } else {
                let body = body_for_run(tool, event, Some(&blob));
                let body_bytes = serde_json::to_vec(&serde_json::Value::Object(body))
                    .map_err(|e| AppError::Internal(e.to_string()))?;
                match run_active_tool(state, tool, &body_bytes).await {
                    Ok(o) => Some(o),
                    Err(e) => match skip_reason(&e) {
                        Some(_) => None,
                        None => return Err(e),
                    },
                }
            };
            let saved_map = blob
                .get("saved")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            for (output, parameter) in &saved_map {
                let Some(parameter_id) = parameter.as_str().and_then(|s| s.parse::<Uuid>().ok())
                else {
                    continue;
                };
                let stored =
                    served_spot_value(&state.db, event.site_id, parameter_id, event.collected_at)
                        .await?;
                let recomputed = outcome
                    .results
                    .get(output)
                    .and_then(serde_json::Value::as_f64);
                match (stored, recomputed) {
                    (Some(stored), Some(recomputed)) => {
                        if disagrees(stored, recomputed) {
                            counts.stale += 1;
                            upsert_finding(
                                &state.db,
                                HoldKind::StaleOutput,
                                event,
                                parameter_id,
                                &tool.name,
                                FindingPayload {
                                    expected: serde_json::json!({
                                        "value": recomputed,
                                        "output": output,
                                        "reason": "inputs",
                                        "tool_version": blob.get("tool_version"),
                                    }),
                                    computed: serde_json::json!({ "value": stored }),
                                    delta: serde_json::json!({ "abs": (stored - recomputed).abs() }),
                                },
                            )
                            .await?;
                        } else if let Some(under_active) = current
                            .as_ref()
                            .and_then(|o| o.results.get(output).and_then(serde_json::Value::as_f64))
                            && disagrees(stored, under_active)
                        {
                            counts.stale += 1;
                            upsert_finding(
                                &state.db,
                                HoldKind::StaleOutput,
                                event,
                                parameter_id,
                                &tool.name,
                                FindingPayload {
                                    expected: serde_json::json!({
                                        "value": under_active,
                                        "output": output,
                                        "reason": "calculation",
                                        "tool_version": blob.get("tool_version"),
                                        "active_version_id": tool.version_id,
                                    }),
                                    computed: serde_json::json!({ "value": stored }),
                                    delta: serde_json::json!({ "abs": (stored - under_active).abs() }),
                                },
                            )
                            .await?;
                        } else {
                            counts.superseded +=
                                supersede_findings(&state.db, event, parameter_id).await? as usize;
                        }
                    }
                    (None, Some(recomputed)) => {
                        if has_pending_skip(&state.db, event, parameter_id).await? {
                            continue;
                        }
                        counts.missing += 1;
                        upsert_finding(
                            &state.db,
                            HoldKind::MissingOutput,
                            event,
                            parameter_id,
                            &tool.name,
                            FindingPayload {
                                expected: serde_json::json!({ "value": recomputed, "output": output }),
                                computed: serde_json::json!({}),
                                delta: serde_json::json!({}),
                            },
                        )
                        .await?;
                    }
                    _ => {}
                }
            }
        } else if inputs_exist(state, tool, event).await? {
            // The tool never ran here although the event holds everything it needs: report each
            // absent output. Present outputs (hand-entered) are left alone.
            for (output, parameter_id) in &saved_outputs {
                let stored =
                    served_spot_value(&state.db, event.site_id, *parameter_id, event.collected_at)
                        .await?;
                if stored.is_none() {
                    if has_pending_skip(&state.db, event, *parameter_id).await? {
                        continue;
                    }
                    counts.missing += 1;
                    upsert_finding(
                        &state.db,
                        HoldKind::MissingOutput,
                        event,
                        *parameter_id,
                        &tool.name,
                        FindingPayload {
                            expected: serde_json::json!({ "output": output, "inputs_present": true }),
                            computed: serde_json::json!({}),
                            delta: serde_json::json!({}),
                        },
                    )
                    .await?;
                } else {
                    counts.superseded +=
                        supersede_findings(&state.db, event, *parameter_id).await? as usize;
                }
            }
        }
    }
    counts.events_audited += 1;
    Ok(())
}

pub(super) fn as_db_err(e: AppError) -> DbErr {
    DbErr::Custom(e.to_string())
}

pub(super) fn app_state() -> Result<AppState, DbErr> {
    crate::common::global_app_state()
        .ok_or_else(|| DbErr::Custom("application state is not initialised".to_string()))
}

#[async_trait]
impl Job for EventRecompute {
    fn name(&self) -> &'static str {
        "event_recompute"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let state = app_state()?;
        let params = ctx.params();
        let actor = params
            .get("actor")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("system")
            .to_string();
        let as_uuid = |key: &str| {
            params
                .get(key)
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse::<Uuid>().ok())
        };
        let as_time = |key: &str| {
            params
                .get(key)
                .and_then(serde_json::Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
        };

        if let Some(event_id) = as_uuid("collection_event_id") {
            let outcome = recompute_event(&state, event_id, &actor)
                .await
                .map_err(as_db_err)?;
            let skipped: Vec<serde_json::Value> = outcome
                .skipped
                .iter()
                .map(|(tool, reason)| serde_json::json!({ "tool": tool, "reason": reason }))
                .collect();
            ctx.report(
                JobReport::new()
                    .scope("collection_event_id", event_id.to_string())
                    .scope("skipped", skipped)
                    .scope("unchanged", outcome.unchanged.clone())
                    .count("tools_run", outcome.tools_run)
                    .count("readings_written", outcome.readings_written)
                    .count("readings_withdrawn", outcome.readings_withdrawn)
                    .count("tools_skipped", outcome.skipped.len())
                    .count("findings_raised", outcome.findings_raised)
                    .count("slots_minted", outcome.slots_minted)
                    .count("tools_unchanged", outcome.unchanged.len())
                    .count("findings_closed", outcome.findings_closed),
            )
            .await;
            return Ok(i64::try_from(outcome.readings_written).unwrap_or(i64::MAX));
        }

        // The scoped apply: every manual visit the scope selects, one after another, each
        // through the same executor a single recompute uses.
        let scope = RecomputeScope {
            site_id: as_uuid("site_id"),
            start: as_time("start"),
            end: as_time("end"),
            only_findings: params
                .get("only_findings")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            calculation: params
                .get("calculation")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            version: as_uuid("version"),
            constant: params
                .get("constant")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        };
        if !scope.is_bounded() {
            return Err(DbErr::Custom(
                "event_recompute needs an event or a scope".to_string(),
            ));
        }
        let events = events_in_scope(ctx.db(), &scope).await?;
        let mut events_recomputed = 0usize;
        let mut tools_run = 0usize;
        let mut readings_written = 0usize;
        let mut readings_withdrawn = 0usize;
        let mut tools_unchanged = 0usize;
        let mut tools_skipped = 0usize;
        let mut findings_raised = 0usize;
        let mut findings_closed = 0usize;
        let mut slots_minted = 0usize;
        for (walked, event_id) in events.iter().enumerate() {
            if ctx.is_cancelled() {
                break;
            }
            ctx.set_step(walked + 1, events.len()).await;
            let outcome = recompute_event(&state, *event_id, &actor)
                .await
                .map_err(as_db_err)?;
            events_recomputed += 1;
            tools_run += outcome.tools_run;
            readings_written += outcome.readings_written;
            readings_withdrawn += outcome.readings_withdrawn;
            tools_unchanged += outcome.unchanged.len();
            tools_skipped += outcome.skipped.len();
            findings_raised += outcome.findings_raised;
            findings_closed += outcome.findings_closed;
            slots_minted += outcome.slots_minted;
            for (tool, reason) in &outcome.skipped {
                ctx.log(
                    "info",
                    &format!("{tool} skipped at {event_id}: {reason}"),
                    serde_json::json!({ "collection_event_id": event_id, "tool": tool }),
                )
                .await;
            }
        }
        ctx.report(
            JobReport::new()
                .scope_opt("site_id", scope.site_id.map(|id| id.to_string()))
                .scope_opt("start", scope.start.map(|t| t.to_rfc3339()))
                .scope_opt("end", scope.end.map(|t| t.to_rfc3339()))
                .scope("only_findings", scope.only_findings)
                .scope_opt("calculation", scope.calculation.clone())
                .scope_opt("constant", scope.constant.clone())
                .count("events_in_scope", events.len())
                .count("events_recomputed", events_recomputed)
                .count("tools_run", tools_run)
                .count("readings_written", readings_written)
                .count("readings_withdrawn", readings_withdrawn)
                .count("tools_skipped", tools_skipped)
                .count("findings_raised", findings_raised)
                .count("slots_minted", slots_minted)
                .count("tools_unchanged", tools_unchanged)
                .count("findings_closed", findings_closed),
        )
        .await;
        Ok(i64::try_from(readings_written).unwrap_or(i64::MAX))
    }
}

/// Whether anything stored at this visit is still awaiting verification.
fn pending_inputs_at(site_id: Uuid, collected_at: chrono::DateTime<chrono::Utc>) -> Statement {
    use sea_orm::sea_query::ExprTrait;

    let unverified = Query::select()
        .expr(Expr::cust("1"))
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::SiteId).eq(site_id))
        .and_where(Expr::col(readings::Column::Time).eq(collected_at))
        .and_where(Expr::cust(r#""unverified" IS TRUE"#))
        .to_owned();
    build(
        &Query::select()
            .expr_as(Expr::exists(unverified), Alias::new("p"))
            .to_owned(),
    )
}

/// The events one audit run covers, most specific scope first. A `constant` or `calculation` scope
/// narrows to the visits whose stored provenance names it, so editing one audits what that edit
/// could have changed rather than every visit ever recorded; a visit where the tool never ran
/// carries no provenance and no stale output, which is the finding such an edit cannot produce.
pub(super) fn audit_event_set(
    event_id: Option<Uuid>,
    site_id: Option<Uuid>,
    constant: Option<&str>,
    calculation: Option<&str>,
) -> Statement {
    use sea_orm::sea_query::ExprTrait;

    let events = Alias::new("collection_events");
    let r = Alias::new("r");
    // A visit whose stored provenance names the scope. The correlation is on the event id, so the
    // subquery is one index probe per visit.
    let names_in_provenance = |predicate: Expr| {
        Expr::exists(
            Query::select()
                .expr(Expr::cust("1"))
                .from_as(readings::Entity, r.clone())
                .and_where(
                    Expr::col((r.clone(), readings::Column::CollectionEventId))
                        .equals((events.clone(), collection_events::Column::Id)),
                )
                .and_where(predicate)
                .to_owned(),
        )
    };

    let mut query = Query::select();
    query
        .column(collection_events::Column::Id)
        .from(collection_events::Entity)
        // A synced visit is the portal's, and the repair refuses one (Q41), so the audit that
        // would raise findings against it covers the same set the recompute does (Q175).
        .and_where(
            Expr::col(collection_events::Column::Source)
                .ne(crate::routes::private::collection_events::service::PORTAL_SYNC),
        );
    if let Some(id) = event_id {
        query.and_where(Expr::col(collection_events::Column::Id).eq(id));
    } else if let Some(site) = site_id {
        query.and_where(Expr::col(collection_events::Column::SiteId).eq(site));
    }
    if let Some(name) = constant {
        query.and_where(names_in_provenance(Expr::cust_with_values(
            r#"jsonb_exists("r"."provenance" -> 'constants', $1)"#,
            [name],
        )));
    }
    if let Some(name) = calculation {
        query.and_where(names_in_provenance(Expr::cust_with_values(
            r#""r"."provenance" ->> 'tool' = $1"#,
            [name],
        )));
    }
    query.order_by(collection_events::Column::CollectedAt, Order::Asc);
    build(&query.to_owned())
}

#[async_trait]
impl Job for EventAudit {
    fn name(&self) -> &'static str {
        "event_audit"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let state = app_state()?;
        let params = ctx.params();
        let event_id = params
            .get("collection_event_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse::<Uuid>().ok());
        let site_id = params
            .get("site_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse::<Uuid>().ok());

        let tools = list_active_tools(&state.db).await.map_err(as_db_err)?;
        let catalog = load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest))
            .await
            .map_err(as_db_err)?;
        let order = dependency_order(&tools, &catalog).map_err(as_db_err)?;

        let constant = params
            .get("constant")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let calculation = params
            .get("calculation")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let event_rows = ctx
            .db()
            .query_all_raw(audit_event_set(
                event_id,
                site_id,
                constant.as_deref(),
                calculation.as_deref(),
            ))
            .await?;

        ctx.info(&format!(
            "Auditing {} visits against {} calculations",
            event_rows.len(),
            tools.len()
        ))
        .await;
        let mut counts = AuditCounts {
            events_audited: 0,
            missing: 0,
            stale: 0,
            superseded: 0,
        };
        for (walked, row) in event_rows.iter().enumerate() {
            if ctx.is_cancelled() {
                break;
            }
            ctx.set_step(walked + 1, event_rows.len()).await;
            let id: Uuid = row.try_get("", "id")?;
            let event = load_event(&state.db, id).await.map_err(as_db_err)?;
            audit_event(&state, &event, &tools, &catalog, &order, &mut counts)
                .await
                .map_err(as_db_err)?;
        }

        ctx.info(&format!(
            "Audited {} visits: {} missing outputs, {} stale, {} findings superseded",
            counts.events_audited, counts.missing, counts.stale, counts.superseded
        ))
        .await;
        ctx.report(
            JobReport::new()
                .scope_opt("site_id", site_id.map(|id| id.to_string()))
                .scope_opt("collection_event_id", event_id.map(|id| id.to_string()))
                .scope_opt("constant", constant.clone())
                .count("events_audited", counts.events_audited)
                .count("missing_findings", counts.missing)
                .count("stale_findings", counts.stale)
                .count("superseded", counts.superseded),
        )
        .await;
        Ok(i64::try_from(counts.missing + counts.stale).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
#[path = "tests/chain_applicability_tests.rs"]
mod chain_applicability_tests;

#[cfg(test)]
#[path = "tests/chain_audit_scope_tests.rs"]
mod chain_audit_scope_tests;

#[cfg(test)]
#[path = "tests/chain_order_tests.rs"]
mod chain_order_tests;

#[cfg(test)]
#[path = "tests/chain_replay_tests.rs"]
mod chain_replay_tests;

#[cfg(test)]
#[path = "tests/chain_scope_tests.rs"]
mod chain_scope_tests;

#[cfg(test)]
#[path = "tests/chain_skip_reason_tests.rs"]
mod chain_skip_reason_tests;

#[cfg(test)]
#[path = "tests/chain_output_readings_tests.rs"]
mod chain_output_readings_tests;
