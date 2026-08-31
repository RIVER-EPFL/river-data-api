//! The dependency-ordered chain executor and the missing/stale event audit (D6).
//!
//! The portal's `runGlobalCalculations` recomputed the whole table on demand; here the same job
//! is scoped to one collection event and driven by the manifests: tool A feeds tool B when one of
//! A's outputs resolves to the catalog parameter a B `event_input` reads. The executor runs every
//! tool whose inputs resolve at the event, in that order, and saves through the ordinary grab
//! write path, so every recomputed value carries a fresh server-built provenance blob.
//!
//! The audit is the executor's read-only twin: it reports outputs missing where the declared
//! inputs exist, and outputs that disagree with a recompute under their pinned script version.
//! Findings land in the review queue (`replicate_audit_holds`, event kinds); the auditor never
//! writes a value.

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::grab_samples::{
    GrabSampleReading, GrabSampleRequest, GrabWriteMode, insert_grab_samples,
};
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::lifecycle::JobContext;

use super::engine::{self, ActiveTool, ParameterCatalog};

/// Relative tolerance for the stale comparison. A recompute under the pinned version with the
/// stored inputs reproduces the value bit-for-bit; anything beyond float-noise means an input
/// (usually a re-resolved event input) or the stored value moved.
const STALE_REL_TOL: f64 = 1e-9;

pub struct EventContext {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
}

pub async fn load_event(db: &DatabaseConnection, id: Uuid) -> AppResult<EventContext> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT site_id, collected_at FROM collection_events WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
    Ok(EventContext {
        id,
        site_id: row.try_get("", "site_id")?,
        collected_at: row
            .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "collected_at")?
            .with_timezone(&chrono::Utc),
    })
}

/// Order tools so producers run before consumers: an edge A→B exists when one of A's outputs
/// resolves to the catalog parameter one of B's `event_inputs` reads. A cycle is refused naming
/// its members — two tools feeding each other have no runnable order.
pub fn dependency_order(
    tools: &[ActiveTool],
    catalog: &ParameterCatalog,
) -> AppResult<Vec<usize>> {
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
    let consumed_codes: Vec<Vec<String>> = tools
        .iter()
        .map(|t| {
            t.manifest
                .event_inputs
                .iter()
                .map(|e| e.parameter_code.to_lowercase())
                .collect()
        })
        .collect();

    let n = tools.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n]; // deps[b] = producers b waits on
    for b in 0..n {
        for (a, produced) in produced_codes.iter().enumerate() {
            if a != b && consumed_codes[b].iter().any(|code| produced.contains(code)) {
                deps[b].push(a);
            }
        }
    }

    let mut order = Vec::with_capacity(n);
    let mut placed = vec![false; n];
    loop {
        let mut progressed = false;
        // Stable by declaration order (list_active_tools orders by name).
        for i in 0..n {
            if !placed[i] && deps[i].iter().all(|&d| placed[d]) {
                placed[i] = true;
                order.push(i);
                progressed = true;
            }
        }
        if order.len() == n {
            return Ok(order);
        }
        if !progressed {
            let cycle: Vec<&str> = (0..n)
                .filter(|&i| !placed[i])
                .map(|i| tools[i].name.as_str())
                .collect();
            return Err(AppError::Conflict(format!(
                "Tool event_inputs form a dependency cycle: {}",
                cycle.join(", ")
            )));
        }
    }
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
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COALESCE(
                (SELECT smp.mean FROM samples smp
                  WHERE smp.site_id = $1 AND smp.parameter_id = $2 AND smp.collected_at = $3),
                (SELECT COALESCE(r.calibrated_value, r.raw_value) FROM readings r
                  WHERE r.site_id = $1 AND r.parameter_id = $2 AND r.time = $3
                    AND r.measurement_type = 'spot' AND r.is_flagged IS NOT TRUE
                    AND r.withdrawn_at IS NULL
                  ORDER BY r.replicate_index LIMIT 1)
             ) AS value",
            [
                site_id.into(),
                parameter_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
            ],
        ))
        .await?;
    match row {
        Some(r) => Ok(r.try_get("", "value")?),
        None => Ok(None),
    }
}

/// The latest provenance blob a given tool stored at this event, if any.
async fn blob_at_event(
    db: &DatabaseConnection,
    event: &EventContext,
    tool: &str,
) -> AppResult<Option<serde_json::Value>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT provenance FROM samples
             WHERE site_id = $1 AND collected_at = $2 AND provenance ->> 'tool' = $3
             ORDER BY provenance ->> 'saved_at' DESC LIMIT 1",
            [
                event.site_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
                tool.into(),
            ],
        ))
        .await?;
    Ok(row.and_then(|r| r.try_get("", "provenance").ok()))
}

/// The request body for a run at this event: the prior run's stored inputs when one exists (minus
/// the params the context re-resolves, so upstream changes propagate), plus the context fields.
fn body_for_run(
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
        for s in &tool.manifest.station_inputs {
            body.remove(s.target());
        }
    }
    body.insert("site_id".into(), serde_json::json!(event.site_id));
    body.insert(
        "collected_at".into(),
        serde_json::json!(event.collected_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    );
    body
}

/// Why one tool was not executed at one event. Recorded, never fatal: the chain runs what it can.
fn skip_reason(e: &AppError) -> Option<String> {
    match e {
        AppError::BadRequest(msg) => Some(msg.clone()),
        AppError::ToolScriptError { message, .. } => Some(format!("script error: {message}")),
        _ => None,
    }
}

pub struct RecomputeOutcome {
    pub tools_run: usize,
    pub readings_written: usize,
    pub skipped: Vec<(String, String)>,
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
    let tools = engine::list_active_tools(&state.db).await?;
    let catalog =
        engine::load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest)).await?;
    let order = dependency_order(&tools, &catalog)?;

    let mut outcome = RecomputeOutcome {
        tools_run: 0,
        readings_written: 0,
        skipped: Vec::new(),
    };

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

        let prior = blob_at_event(&state.db, &event, &tool.name).await?;
        let body = body_for_run(tool, &event, prior.as_ref());
        let body_bytes = serde_json::to_vec(&serde_json::Value::Object(body))
            .map_err(|e| AppError::Internal(e.to_string()))?;

        let result =
            match super::execute_and_store_run(state, tool, &body_bytes, actor, "chain").await {
                Ok(result) => result,
                Err(e) => match skip_reason(&e) {
                    Some(reason) => {
                        outcome.skipped.push((tool.name.clone(), reason));
                        continue;
                    }
                    None => return Err(e),
                },
            };

        // Scalar outputs the run produced, saved to their resolved parameters. A per-replicate
        // (array) output stays unsaved here: replicate identity is the source's column position,
        // which a recompute has no authority to assign (Phase 5 carries intermediaries).
        let readings: Vec<GrabSampleReading> = saved_outputs
            .iter()
            .filter_map(|(key, parameter_id)| {
                result.results.get(key).and_then(serde_json::Value::as_f64).map(|value| {
                    GrabSampleReading {
                        input: None,
                        parameter_id: *parameter_id,
                        sensor_id: None,
                        value,
                        time: event.collected_at,
                        replicate_index: None,
                        output: Some(key.clone()),
                        standard_curve_id: None,
                    }
                })
            })
            .collect();
        if readings.is_empty() {
            outcome
                .skipped
                .push((tool.name.clone(), "run produced no savable output".to_string()));
            continue;
        }

        let auth = crate::common::middleware::AuthContext::Keycloak {
            roles: Vec::new(),
            sub: actor.to_string(),
            email: None,
            email_verified: false,
            grants: std::sync::Arc::new(std::collections::HashSet::new()),
        };
        let request = GrabSampleRequest {
            site_id: event.site_id,
            created_by: Some(actor.to_string()),
            label: None,
            notes: None,
            mode: Some(GrabWriteMode::Replace),
            dry_run: false,
                tool_run_id: Some(result.run_id),
            check_id: None,
            // The tool's manifest is read by the save path itself; nothing here overrides
            // the slot's declaration.
            sd_estimator: None,
            readings,
        };
        let saved = insert_grab_samples(
            axum::extract::State(state.clone()),
            axum::Extension(auth),
            crate::common::middleware::ProjectScope(crate::common::authz::AccessScope::Unrestricted),
            axum::Json(request),
        )
        .await?;
        outcome.tools_run += 1;
        outcome.readings_written += saved.0.inserted;
    }

    Ok(outcome)
}

// --- The missing/stale audit -----------------------------------------------------------------

pub struct AuditCounts {
    pub events_audited: usize,
    pub missing: usize,
    pub stale: usize,
    pub superseded: usize,
}

struct FindingPayload {
    expected: serde_json::Value,
    computed: serde_json::Value,
    delta: serde_json::Value,
}

async fn upsert_finding(
    db: &DatabaseConnection,
    kind: &str,
    event: &EventContext,
    parameter_id: Uuid,
    tool: &str,
    payload: FindingPayload,
) -> AppResult<()> {
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO replicate_audit_holds
             (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending')
         ON CONFLICT (kind, site_id, parameter_id, group_time)
             WHERE stream_id IS NULL AND status = 'pending'
         DO UPDATE SET expected = EXCLUDED.expected, computed = EXCLUDED.computed,
                       delta = EXCLUDED.delta, tool = EXCLUDED.tool, created_at = NOW()",
        [
            kind.into(),
            event.site_id.into(),
            parameter_id.into(),
            sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
            tool.into(),
            payload.expected.into(),
            payload.computed.into(),
            payload.delta.into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Close open findings for slots the current audit found in agreement (or now populated).
async fn supersede_findings(
    db: &DatabaseConnection,
    event: &EventContext,
    parameter_id: Uuid,
) -> AppResult<u64> {
    let res = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE replicate_audit_holds SET status = 'superseded'
             WHERE stream_id IS NULL AND status = 'pending'
               AND site_id = $1 AND parameter_id = $2 AND group_time = $3",
            [
                event.site_id.into(),
                parameter_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected())
}

/// The pinned script version a blob names, rebuilt as a runnable tool. `None` when the blob names
/// no stored version (a draft run) or the version row is gone.
async fn pinned_tool(
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
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT tool_script_id, version_no, script, entry_function, manifest, content_hash
             FROM tool_script_versions WHERE id = $1",
            [version_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let manifest_raw: serde_json::Value = row.try_get("", "manifest")?;
    let Ok(manifest) = engine::parse_manifest(&manifest_raw) else {
        return Ok(None);
    };
    Ok(Some(ActiveTool {
        script_id: row.try_get("", "tool_script_id")?,
        name: tool_name.to_string(),
        label: manifest.label.clone(),
        description: manifest.description.clone(),
        version_id,
        version_no: row.try_get("", "version_no")?,
        script: row.try_get("", "script")?,
        entry_function: row.try_get("", "entry_function")?,
        content_hash: row.try_get("", "content_hash")?,
        manifest,
    }))
}

/// Whether every required param of a tool is answerable at the event without a person: a manifest
/// default, a resolvable station property, or a same-event value. This is "the declared inputs
/// exist" for the missing-output report.
async fn inputs_exist(
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
    let station = engine::resolve_station_inputs(
        &state.db,
        &tool.name,
        &tool.manifest,
        Some(event.site_id),
        &mut body,
    )
    .await;
    if station.is_err() {
        return Ok(false);
    }
    engine::resolve_event_inputs(
        &state.db,
        &tool.name,
        &tool.manifest,
        Some(event.site_id),
        Some(event.collected_at),
        &mut body,
    )
    .await?;
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
            let outcome = match engine::run_active_tool(state, &pinned, &body_bytes).await {
                Ok(o) => o,
                Err(e) => match skip_reason(&e) {
                    Some(_) => continue,
                    None => return Err(e),
                },
            };
            let saved_map = blob
                .get("saved")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            for (output, parameter) in &saved_map {
                let Some(parameter_id) = parameter
                    .as_str()
                    .and_then(|s| s.parse::<Uuid>().ok())
                else {
                    continue;
                };
                let stored = served_spot_value(&state.db, event.site_id, parameter_id, event.collected_at)
                    .await?;
                let recomputed = outcome.results.get(output).and_then(serde_json::Value::as_f64);
                match (stored, recomputed) {
                    (Some(stored), Some(recomputed)) => {
                        let scale = stored.abs().max(recomputed.abs()).max(1e-12);
                        if (stored - recomputed).abs() / scale > STALE_REL_TOL {
                            counts.stale += 1;
                            upsert_finding(
                                &state.db,
                                "stale_output",
                                event,
                                parameter_id,
                                &tool.name,
                                FindingPayload {
                                    expected: serde_json::json!({
                                        "value": recomputed,
                                        "output": output,
                                        "tool_version": blob.get("tool_version"),
                                    }),
                                    computed: serde_json::json!({ "value": stored }),
                                    delta: serde_json::json!({ "abs": (stored - recomputed).abs() }),
                                },
                            )
                            .await?;
                        } else {
                            counts.superseded +=
                                supersede_findings(&state.db, event, parameter_id).await? as usize;
                        }
                    }
                    (None, Some(recomputed)) => {
                        counts.missing += 1;
                        upsert_finding(
                            &state.db,
                            "missing_output",
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
                    counts.missing += 1;
                    upsert_finding(
                        &state.db,
                        "missing_output",
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

// --- Tracked jobs ----------------------------------------------------------------------------

fn as_db_err(e: AppError) -> DbErr {
    DbErr::Custom(e.to_string())
}

fn app_state() -> Result<AppState, DbErr> {
    crate::common::global_app_state()
        .ok_or_else(|| DbErr::Custom("application state is not initialised".to_string()))
}

/// `event_recompute`: the chain executor over one collection event.
pub struct EventRecompute;

#[async_trait]
impl Job for EventRecompute {
    fn name(&self) -> &'static str {
        "event_recompute"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let state = app_state()?;
        let params = ctx.params();
        let event_id = params
            .get("collection_event_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse::<Uuid>().ok())
            .ok_or_else(|| DbErr::Custom("collection_event_id missing".to_string()))?;
        let actor = params
            .get("actor")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("system")
            .to_string();

        let outcome = recompute_event(&state, event_id, &actor)
            .await
            .map_err(as_db_err)?;
        let skipped: Vec<serde_json::Value> = outcome
            .skipped
            .iter()
            .map(|(tool, reason)| serde_json::json!({ "tool": tool, "reason": reason }))
            .collect();
        ctx.set_detail(serde_json::json!({
            "scope": { "collection_event_id": event_id, "skipped": skipped },
            "counts": {
                "tools_run": outcome.tools_run,
                "readings_written": outcome.readings_written,
                "tools_skipped": outcome.skipped.len(),
            },
        }))
        .await;
        Ok(i64::try_from(outcome.readings_written).unwrap_or(i64::MAX))
    }
}

/// `event_audit`: the missing/stale report over one event, one site, or everything.
pub struct EventAudit;

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

        let tools = engine::list_active_tools(&state.db).await.map_err(as_db_err)?;
        let catalog = engine::load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest))
            .await
            .map_err(as_db_err)?;
        let order = dependency_order(&tools, &catalog).map_err(as_db_err)?;

        let mut sql = String::from("SELECT id FROM collection_events");
        let mut binds: Vec<sea_orm::Value> = Vec::new();
        if let Some(id) = event_id {
            binds.push(id.into());
            sql.push_str(" WHERE id = $1");
        } else if let Some(site) = site_id {
            binds.push(site.into());
            sql.push_str(" WHERE site_id = $1");
        }
        sql.push_str(" ORDER BY collected_at");
        let event_rows = ctx
            .db()
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                binds,
            ))
            .await?;

        let mut counts = AuditCounts {
            events_audited: 0,
            missing: 0,
            stale: 0,
            superseded: 0,
        };
        for row in &event_rows {
            if ctx.is_cancelled() {
                break;
            }
            let id: Uuid = row.try_get("", "id")?;
            let event = load_event(&state.db, id).await.map_err(as_db_err)?;
            audit_event(&state, &event, &tools, &catalog, &order, &mut counts)
                .await
                .map_err(as_db_err)?;
        }

        ctx.set_detail(serde_json::json!({
            "scope": { "site_id": site_id, "collection_event_id": event_id },
            "counts": {
                "events_audited": counts.events_audited,
                "missing_findings": counts.missing,
                "stale_findings": counts.stale,
                "superseded": counts.superseded,
            },
        }))
        .await;
        Ok(i64::try_from(counts.missing + counts.stale).unwrap_or(i64::MAX))
    }
}
