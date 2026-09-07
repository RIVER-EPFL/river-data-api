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

use std::collections::HashSet;

use async_trait::async_trait;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult, Statement,
};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::grab_samples::{
    GrabSampleReading, GrabSampleRequest, GrabWriteMode, insert_grab_samples,
};
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use crate::routes::private::sync::replicate_audit as audit;

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
    use crate::routes::private::sites::parameters::model as site_parameters;
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

/// Whether a calculation applies at a site: the site holds a slot for at least one of its outputs.
/// One is enough because a tool whose outputs are partly declared is a tool the site wants and a
/// slot it is missing, which the save reports; none at all is a tool nobody asked for here.
#[must_use]
pub fn applies_at_site(saved_outputs: &[(String, Uuid)], declared: &HashSet<Uuid>) -> bool {
    saved_outputs.iter().any(|(_, id)| declared.contains(id))
}

/// Order tools so producers run before consumers: an edge A→B exists when one of A's outputs
/// resolves to the catalog parameter one of B's `event_inputs` reads. A cycle is refused naming
/// its members — two tools feeding each other have no runnable order.
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
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!("SELECT {} AS value", engine::served_spot_value_sql("$2")),
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
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT provenance FROM readings
             WHERE site_id = $1 AND time = $2 AND provenance ->> 'tool' = $3
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
        for s in &tool.manifest.site_inputs {
            body.remove(s.target());
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
    /// Open missing/stale findings closed because this run rewrote their output.
    pub findings_closed: usize,
    /// Stored outputs withdrawn because the script computed them as NA (M23): the portal blanked
    /// the column, and here the stamp is reversible.
    pub readings_withdrawn: usize,
    pub skipped: Vec<(String, String)>,
    /// `skipped_output` findings raised because a step did not run and its outputs are absent.
    pub findings_raised: usize,
    /// Tools the site never declared: their output slots are not configured here, so they do not
    /// apply at this site at all (Q98). Distinct from `skipped`, which is an input that did not
    /// resolve on a tool that does apply.
    pub not_applicable: Vec<String>,
    /// Tools whose prior run at this event consumed exactly what a fresh run would, under the
    /// same script version, with its outputs still served: left alone, no run minted.
    pub unchanged: Vec<String>,
}

/// The fingerprint of the run a provenance blob records, comparable with a fresh
/// [`engine::ResolvedRun::fingerprint`]. `None` when the blob pins no stored script version.
fn blob_fingerprint(blob: &serde_json::Value) -> Option<String> {
    let version_id = blob
        .get("tool_version")
        .and_then(|v| v.get("script_version_id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())?;
    let empty_object = serde_json::json!({});
    let empty_array = serde_json::json!([]);
    Some(engine::run_fingerprint(
        version_id,
        blob.get("inputs").unwrap_or(&empty_object),
        blob.get("constants").unwrap_or(&empty_object),
        blob.get("curves").unwrap_or(&empty_array),
    ))
}

/// Whether every output the prior run saved is still served at its value. A value someone put
/// in the slot since is superseded by a recompute, however unchanged the inputs are.
async fn outputs_still_served(
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
fn disagrees(stored: f64, recomputed: f64) -> bool {
    let scale = stored.abs().max(recomputed.abs()).max(1e-12);
    (stored - recomputed).abs() / scale > STALE_REL_TOL
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
        readings_withdrawn: 0,
        tools_run: 0,
        readings_written: 0,
        findings_closed: 0,
        skipped: Vec::new(),
        findings_raised: 0,
        not_applicable: Vec::new(),
        unchanged: Vec::new(),
    };

    // A calculation applies at a site when the site holds slots for its outputs (Q98): the site
    // parameters are the declaration, so the calculation set is filtered by them before the
    // dependency order is walked, rather than every enabled tool being run wherever its inputs
    // happen to resolve.
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
        if !applies_at_site(&saved_outputs, &declared) {
            outcome.not_applicable.push(tool.name.clone());
            continue;
        }

        let prior = blob_at_event(&state.db, &event, &tool.name).await?;
        let body = body_for_run(tool, &event, prior.as_ref());
        let body_bytes = serde_json::to_vec(&serde_json::Value::Object(body))
            .map_err(|e| AppError::Internal(e.to_string()))?;

        let resolved = match engine::resolve_run(
            state,
            tool,
            &body_bytes,
            None,
            engine::MissingConstant::Refuse,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(e) => match skip_reason(&e) {
                Some(reason) => {
                    outcome.findings_raised +=
                        record_skip(&state.db, &event, &tool.name, &saved_outputs, &reason).await?;
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

        let result =
            match super::execute_and_store_resolved(state, tool, resolved, actor, "chain").await {
                Ok(result) => result,
                Err(e) => match skip_reason(&e) {
                    Some(reason) => {
                        outcome.findings_raised +=
                            record_skip(&state.db, &event, &tool.name, &saved_outputs, &reason)
                                .await?;
                        outcome.skipped.push((tool.name.clone(), reason));
                        continue;
                    }
                    None => return Err(e),
                },
            };

        // Scalar outputs the run produced, saved to their resolved parameters. A per-replicate
        // (array) output stays unsaved here: replicate identity is the source's column position,
        // which a recompute has no authority to assign (Phase 5 carries intermediaries).
        // A slot an admin detached at this visit is a manual value until an input moves or it
        // is returned (Q40, Q47): the chain leaves it alone and says so.
        let mut owned_outputs: Vec<(String, Uuid)> = Vec::with_capacity(saved_outputs.len());
        for (key, parameter_id) in &saved_outputs {
            if crate::routes::private::readings::decisions::output_owner(
                &state.db,
                event.site_id,
                *parameter_id,
                event.collected_at,
            )
            .await?
                == crate::routes::private::readings::decisions::Owner::Manual
            {
                outcome.skipped.push((
                    tool.name.clone(),
                    format!("output {key} is detached at this visit"),
                ));
            } else {
                owned_outputs.push((key.clone(), *parameter_id));
            }
        }
        // An output the script computed as NA is a request to blank the column, so the stored
        // value is withdrawn rather than left standing beside a run that did not produce it. A
        // person's ruling on the row is not overridden: those keep their value and their hold.
        for (key, parameter_id) in &owned_outputs {
            if !result.cleared.iter().any(|c| c == key) {
                continue;
            }
            let withdrawn = crate::common::bulk_write::guarded(&state.db, async |txn| {
                crate::routes::private::readings::decisions::record_many(
                    txn,
                    crate::routes::private::readings::decisions::Kind::Withdraw,
                    &format!(
                        "r.site_id = $1 AND r.parameter_id = $2 AND r.time = $3 \
                         AND r.measurement_type = 'spot' AND r.withdrawn_at IS NULL AND {free}",
                        free = crate::routes::private::readings::decisions::unjudged_sql("r")
                    ),
                    vec![
                        event.site_id.into(),
                        (*parameter_id).into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
                    ],
                    crate::routes::private::readings::decisions::NewValue::Literal(
                        serde_json::json!({ "reason": "the calculation now yields no value" }),
                    ),
                    actor,
                    Some("computed as NA by the recompute"),
                    crate::routes::private::readings::decisions::Origin::Chain,
                    None,
                )
                .await
            })
            .await?;
            outcome.readings_withdrawn += usize::try_from(withdrawn.rows).unwrap_or(0);
        }

        let readings: Vec<GrabSampleReading> = owned_outputs
            .iter()
            .filter_map(|(key, parameter_id)| {
                result
                    .results
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .map(|value| GrabSampleReading {
                        input: None,
                        parameter_id: *parameter_id,
                        sensor_id: None,
                        value,
                        time: event.collected_at,
                        replicate_index: None,
                        output: Some(key.clone()),
                        standard_curve_id: None,
                    })
            })
            .collect();
        if readings.is_empty() {
            let reason = "run produced no savable output".to_string();
            outcome.findings_raised +=
                record_skip(&state.db, &event, &tool.name, &saved_outputs, &reason).await?;
            outcome.skipped.push((tool.name.clone(), reason));
            continue;
        }

        // A value computed from a pending measurement is pending too (M62): whatever an intern
        // entered at this visit carries into everything the chain derives from it.
        let inputs_pending: bool = state
            .db
            .query_one_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT EXISTS (SELECT 1 FROM readings
                                 WHERE site_id = $1 AND time = $2 AND unverified IS TRUE) AS p",
                [
                    event.site_id.into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
                ],
            ))
            .await?
            .map(|r| r.try_get("", "p"))
            .transpose()?
            .unwrap_or(false);

        let auth = crate::common::middleware::AuthContext::Keycloak {
            roles: Vec::new(),
            sub: actor.to_string(),
            email: None,
            email_verified: false,
            grants: std::sync::Arc::new(std::collections::HashSet::new()),
        };
        let request = GrabSampleRequest {
            site_id: event.site_id,
            pending_inputs: inputs_pending,
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
    }

    Ok(outcome)
}

/// The scope a recompute job covers when it names no single event.
#[derive(Debug, Clone, Default)]
pub struct RecomputeScope {
    pub site_id: Option<Uuid>,
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    pub only_findings: bool,
}

impl RecomputeScope {
    /// A scope names a site, a range, or holds itself to open findings. Nothing else is
    /// accepted: "every visit there is" is not a repair, it is a global recompute (D6).
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.site_id.is_some() || self.start.is_some() || self.end.is_some() || self.only_findings
    }

    /// The SELECT of visit ids this scope covers, oldest first. `portal_sync` visits are never
    /// in scope (Q41); `only_findings` holds the set to visits with an open event finding.
    #[must_use]
    pub fn events_sql(&self) -> (String, Vec<sea_orm::Value>) {
        let mut sql =
            String::from("SELECT ce.id FROM collection_events ce WHERE ce.source <> 'portal_sync'");
        let mut binds: Vec<sea_orm::Value> = Vec::new();
        if let Some(site_id) = self.site_id {
            binds.push(site_id.into());
            sql.push_str(&format!(" AND ce.site_id = ${}", binds.len()));
        }
        if let Some(start) = self.start {
            binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(start).into());
            sql.push_str(&format!(" AND ce.collected_at >= ${}", binds.len()));
        }
        if let Some(end) = self.end {
            binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(end).into());
            sql.push_str(&format!(" AND ce.collected_at <= ${}", binds.len()));
        }
        if self.only_findings {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM replicate_audit_holds h \
                  WHERE h.stream_id IS NULL AND h.status = 'pending' \
                    AND h.kind IN ('missing_output', 'stale_output', 'skipped_output') \
                    AND h.site_id = ce.site_id AND h.group_time = ce.collected_at)",
            );
        }
        sql.push_str(" ORDER BY ce.collected_at");
        (sql, binds)
    }
}

async fn events_in_scope(
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
            status: "pending",
            tool: Some(tool),
        },
    )
    .await
}

/// A step that did not run is a fact about the visit, not only about the run that skipped it: the
/// outputs it would have produced are absent, and the reason belongs where it outlives the job
/// row the counts are pruned with. An output some other path already filled is not reported.
async fn record_skip(
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
        // The absence is now explained, so the audit's account of the same slot gives way to it.
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE replicate_audit_holds SET status = 'superseded'
             WHERE stream_id IS NULL AND status = 'pending'
               AND kind IN ('missing_output', 'stale_output')
               AND site_id = $1 AND parameter_id = $2 AND group_time = $3",
            [
                event.site_id.into(),
                (*parameter_id).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
            ],
        ))
        .await?;
        upsert_finding(
            db,
            "skipped_output",
            event,
            *parameter_id,
            tool,
            FindingPayload {
                expected: serde_json::json!({ "output": output, "reason": reason }),
                computed: serde_json::json!({}),
                delta: serde_json::json!({}),
            },
        )
        .await?;
        raised += 1;
    }
    Ok(raised)
}

/// Whether the executor already reported this slot as a step that did not run. The skip carries
/// the reason, so the audit adds nothing by also calling the output absent.
async fn has_pending_skip(
    db: &DatabaseConnection,
    event: &EventContext,
    parameter_id: Uuid,
) -> AppResult<bool> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 AS found FROM replicate_audit_holds
             WHERE stream_id IS NULL AND status = 'pending' AND kind = 'skipped_output'
               AND site_id = $1 AND parameter_id = $2 AND group_time = $3",
            [
                event.site_id.into(),
                parameter_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(event.collected_at).into(),
            ],
        ))
        .await?;
    Ok(row.is_some())
}

/// Close open findings for slots the current audit found in agreement (or now populated).
async fn supersede_findings(
    db: &DatabaseConnection,
    event: &EventContext,
    parameter_id: Uuid,
) -> AppResult<u64> {
    let res = db
        .execute_raw(Statement::from_sql_and_values(
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
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.tool_script_id, v.version_no, v.script, v.entry_function, v.manifest,
                    v.content_hash, s.engine, s.parameter_group_id
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
    let Ok(manifest) = engine::parse_manifest(&row.manifest) else {
        return Ok(None);
    };
    let engine = engine::Engine::parse(&row.engine).unwrap_or(engine::Engine::Script);
    let formulas = if engine == engine::Engine::Formula {
        match super::formula::parse_pinned(&row.script) {
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
        parameter_group_id: row.parameter_group_id,
        // The version body is the formula set, so a recompute under a pinned version runs the
        // formulas that version holds rather than the definitions as they stand today.
        formulas,
    }))
}

/// A pinned tool version as the chain reads it back. The manifest and the formula body are parsed
/// from the derived row rather than decoded here: a stored version outside either vocabulary is a
/// version this executor cannot run, not a decode failure.
#[derive(FromQueryResult)]
struct PinnedVersionRow {
    tool_script_id: Uuid,
    version_no: i32,
    script: String,
    entry_function: String,
    content_hash: String,
    manifest: serde_json::Value,
    engine: String,
    parameter_group_id: Option<Uuid>,
}

/// Whether every required param of a tool is answerable at the event without a person: a manifest
/// default, a resolvable site property, or a same-event value. This is "the declared inputs
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
    let site = engine::resolve_site_inputs(
        &state.db,
        &tool.name,
        &tool.manifest,
        Some(event.site_id),
        &mut body,
    )
    .await;
    if site.is_err() {
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
                match engine::run_active_tool(state, tool, &body_bytes).await {
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
                                "stale_output",
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
                                "stale_output",
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
                    if has_pending_skip(&state.db, event, *parameter_id).await? {
                        continue;
                    }
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
        for event_id in &events {
            if ctx.is_cancelled() {
                break;
            }
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
                .count("events_in_scope", events.len())
                .count("events_recomputed", events_recomputed)
                .count("tools_run", tools_run)
                .count("readings_written", readings_written)
                .count("readings_withdrawn", readings_withdrawn)
                .count("tools_skipped", tools_skipped)
                .count("findings_raised", findings_raised)
                .count("tools_unchanged", tools_unchanged)
                .count("findings_closed", findings_closed),
        )
        .await;
        Ok(i64::try_from(readings_written).unwrap_or(i64::MAX))
    }
}

/// `event_audit`: the missing/stale report over one event, one site, or everything.
pub struct EventAudit;

/// The events one audit run covers, most specific scope first. A `constant` or `calculation` scope
/// narrows to the visits whose stored provenance names it, so editing one audits what that edit
/// could have changed rather than every visit ever recorded; a visit where the tool never ran
/// carries no provenance and no stale output, which is the finding such an edit cannot produce.
fn audit_event_set(
    event_id: Option<Uuid>,
    site_id: Option<Uuid>,
    constant: Option<&str>,
    calculation: Option<&str>,
) -> (String, Vec<sea_orm::Value>) {
    let mut sql = String::from("SELECT id FROM collection_events");
    let mut binds: Vec<sea_orm::Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if let Some(id) = event_id {
        binds.push(id.into());
        clauses.push(format!("id = ${}", binds.len()));
    } else if let Some(site) = site_id {
        binds.push(site.into());
        clauses.push(format!("site_id = ${}", binds.len()));
    }
    if let Some(name) = constant {
        binds.push(name.into());
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM readings r \
              WHERE r.collection_event_id = collection_events.id \
                AND jsonb_exists(r.provenance -> 'constants', ${}))",
            binds.len()
        ));
    }
    if let Some(name) = calculation {
        binds.push(name.into());
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM readings r \
              WHERE r.collection_event_id = collection_events.id \
                AND r.provenance ->> 'tool' = ${})",
            binds.len()
        ));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY collected_at");
    (sql, binds)
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

        let tools = engine::list_active_tools(&state.db)
            .await
            .map_err(as_db_err)?;
        let catalog = engine::load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest))
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

        let (sql, binds) = audit_event_set(
            event_id,
            site_id,
            constant.as_deref(),
            calculation.as_deref(),
        );
        let event_rows = ctx
            .db()
            .query_all_raw(Statement::from_sql_and_values(
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
mod applicability_tests {
    use super::applies_at_site;
    use std::collections::HashSet;
    use uuid::Uuid;

    /// A site declares which calculations apply to it by holding slots for their outputs (Q98).
    #[test]
    fn a_tool_applies_where_the_site_holds_one_of_its_outputs() {
        let doc = Uuid::new_v4();
        let dom = Uuid::new_v4();
        let outputs = vec![("doc_avg".to_string(), doc), ("doc_sd".to_string(), dom)];

        let declared: HashSet<Uuid> = [doc].into_iter().collect();
        assert!(
            applies_at_site(&outputs, &declared),
            "one declared output is enough: the rest are slots the site is missing, not a tool it \
             never wanted"
        );

        let both: HashSet<Uuid> = [doc, dom].into_iter().collect();
        assert!(applies_at_site(&outputs, &both));
    }

    #[test]
    fn a_tool_the_site_declared_nothing_of_does_not_apply() {
        let outputs = vec![("doc_avg".to_string(), Uuid::new_v4())];
        assert!(!applies_at_site(&outputs, &HashSet::new()));
        assert!(!applies_at_site(
            &outputs,
            &[Uuid::new_v4()].into_iter().collect()
        ));
    }

    #[test]
    fn a_tool_that_saves_nothing_reaches_no_site() {
        assert!(!applies_at_site(
            &[],
            &[Uuid::new_v4()].into_iter().collect()
        ));
    }
}

#[cfg(test)]
mod audit_scope_tests {
    use super::audit_event_set;
    use uuid::Uuid;

    #[test]
    fn an_unscoped_audit_covers_every_event_and_reads_no_provenance() {
        let (sql, binds) = audit_event_set(None, None, None, None);
        assert_eq!(
            sql,
            "SELECT id FROM collection_events ORDER BY collected_at"
        );
        assert!(binds.is_empty());
    }

    #[test]
    fn a_constant_scope_narrows_to_the_events_whose_provenance_names_it() {
        let (sql, binds) = audit_event_set(None, None, Some("xO2"), None);
        assert!(
            sql.contains("jsonb_exists(r.provenance -> 'constants', $1)"),
            "{sql}"
        );
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn a_site_scope_and_a_constant_scope_both_apply_and_bind_in_order() {
        let site = Uuid::new_v4();
        let (sql, binds) = audit_event_set(None, Some(site), Some("xO2"), None);
        assert!(sql.contains("site_id = $1"), "{sql}");
        assert!(sql.contains(", $2)"), "{sql}");
        assert_eq!(binds.len(), 2);
    }

    /// A calculation edit audits the visits that calculation actually wrote, which is what makes
    /// the report proportionate to the edit rather than a pass over every visit ever recorded.
    #[test]
    fn a_calculation_scope_narrows_to_the_visits_it_wrote() {
        let (sql, binds) = audit_event_set(None, None, None, Some("pco2"));
        assert!(
            sql.contains("r.provenance ->> 'tool' = $1"),
            "the scope reads the stored provenance: {sql}"
        );
        assert_eq!(binds.len(), 1);
    }

    /// The two content scopes stack, so editing a constant a calculation reads audits only where
    /// both are named.
    #[test]
    fn a_constant_and_a_calculation_scope_both_apply_and_bind_in_order() {
        let (sql, binds) = audit_event_set(None, None, Some("xO2"), Some("pco2"));
        assert!(sql.contains("jsonb_exists(r.provenance -> 'constants', $1)"));
        assert!(sql.contains("r.provenance ->> 'tool' = $2"));
        assert_eq!(binds.len(), 2);
    }

    #[test]
    fn an_event_scope_outranks_a_site_scope() {
        let (sql, _) = audit_event_set(Some(Uuid::new_v4()), Some(Uuid::new_v4()), None, None);
        assert!(sql.contains("id = $1"), "{sql}");
        assert!(!sql.contains("site_id"), "{sql}");
    }
}

#[cfg(test)]
mod scope_tests {
    use super::RecomputeScope;
    use uuid::Uuid;

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn an_empty_scope_is_not_bounded_and_any_single_term_is() {
        assert!(!RecomputeScope::default().is_bounded());
        assert!(
            RecomputeScope {
                site_id: Some(Uuid::new_v4()),
                ..Default::default()
            }
            .is_bounded()
        );
        assert!(
            RecomputeScope {
                start: Some(at("2025-06-01T00:00:00Z")),
                ..Default::default()
            }
            .is_bounded()
        );
        assert!(
            RecomputeScope {
                end: Some(at("2025-06-01T00:00:00Z")),
                ..Default::default()
            }
            .is_bounded()
        );
        assert!(
            RecomputeScope {
                only_findings: true,
                ..Default::default()
            }
            .is_bounded()
        );
    }

    #[test]
    fn portal_sync_visits_are_excluded_whatever_the_scope() {
        let (sql, binds) = RecomputeScope {
            only_findings: true,
            ..Default::default()
        }
        .events_sql();
        assert!(sql.contains("ce.source <> 'portal_sync'"));
        assert!(binds.is_empty());
    }

    #[test]
    fn each_term_adds_its_clause_with_binds_in_order() {
        let scope = RecomputeScope {
            site_id: Some(Uuid::new_v4()),
            start: Some(at("2025-06-01T00:00:00Z")),
            end: Some(at("2025-06-30T00:00:00Z")),
            only_findings: true,
        };
        let (sql, binds) = scope.events_sql();
        assert!(sql.contains("ce.site_id = $1"));
        assert!(sql.contains("ce.collected_at >= $2"));
        assert!(sql.contains("ce.collected_at <= $3"));
        assert_eq!(binds.len(), 3);
        assert!(sql.contains("h.kind IN ('missing_output', 'stale_output', 'skipped_output')"));
        assert!(sql.contains("h.status = 'pending'"));
        assert!(sql.trim_end().ends_with("ORDER BY ce.collected_at"));
    }

    #[test]
    fn a_range_alone_binds_two_and_names_no_site() {
        let scope = RecomputeScope {
            start: Some(at("2025-06-01T00:00:00Z")),
            end: Some(at("2025-06-30T00:00:00Z")),
            ..Default::default()
        };
        let (sql, binds) = scope.events_sql();
        assert!(!sql.contains("ce.site_id"));
        assert!(!sql.contains("replicate_audit_holds"));
        assert!(sql.contains(">= $1") && sql.contains("<= $2"));
        assert_eq!(binds.len(), 2);
    }
}

#[cfg(test)]
mod replay_tests {
    use super::{ActiveTool, EventContext, body_for_run, disagrees};
    use crate::routes::private::tools::engine::{Engine, Manifest};
    use uuid::Uuid;

    fn tool(manifest: serde_json::Value) -> ActiveTool {
        let manifest: Manifest = serde_json::from_value(manifest).expect("manifest parses");
        ActiveTool {
            script_id: Uuid::from_u128(1),
            name: "pco2".to_string(),
            label: "pCO2".to_string(),
            description: None,
            version_id: Uuid::from_u128(2),
            version_no: 1,
            script: String::new(),
            entry_function: "tool".to_string(),
            content_hash: "hash".to_string(),
            manifest,
            engine: Engine::Script,
            parameter_group_id: None,
            formulas: Vec::new(),
        }
    }

    fn event() -> EventContext {
        EventContext {
            id: Uuid::from_u128(3),
            site_id: Uuid::from_u128(4),
            collected_at: chrono::DateTime::from_timestamp(1_772_259_000, 0)
                .expect("representable"),
        }
    }

    fn manifest_with(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "label": "pCO2",
            "params": [{ "name": "lab_temp_c", "label": "Lab temperature", "kind": "number" }],
            "outputs": [],
        });
        for (k, v) in extra.as_object().expect("object") {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn test_body_for_run_replays_the_prior_run_s_own_inputs() {
        let blob = serde_json::json!({ "inputs": { "lab_temp_c": 21.5, "mode": "db" } });
        let body = body_for_run(
            &tool(manifest_with(serde_json::json!({}))),
            &event(),
            Some(&blob),
        );
        assert_eq!(body["lab_temp_c"], 21.5);
        assert_eq!(body["mode"], "db");
    }

    // Scenario: a run made under an earlier shape is recomputed.
    // Expected behaviour: what the context resolves is dropped from the replayed inputs, so an
    // upstream value that has since changed propagates instead of the stored copy winning.
    #[test]
    fn test_body_for_run_drops_what_the_context_resolves() {
        let manifest = manifest_with(serde_json::json!({
            "params": [
                { "name": "lab_temp_c", "label": "Lab temperature", "kind": "number" },
                { "name": "water_temp_c", "label": "Water temperature", "kind": "number" },
                { "name": "elevation_m", "label": "Elevation", "kind": "number" },
            ],
            "event_inputs": [{ "param": "water_temp_c", "parameter_code": "WTW_Temp_degC_1" }],
            "site_inputs": [{ "property": "altitude_m", "param": "elevation_m" }],
        }));
        let blob = serde_json::json!({
            "inputs": { "lab_temp_c": 21.5, "water_temp_c": 4.0, "elevation_m": 1500 }
        });
        let body = body_for_run(&tool(manifest), &event(), Some(&blob));
        assert_eq!(body["lab_temp_c"], 21.5);
        assert!(
            !body.contains_key("water_temp_c"),
            "the event input is re-resolved"
        );
        assert!(!body.contains_key("elevation_m"), "so is the site input");
    }

    // A site input with no `param` fills the property's own name, and that is what has to go.
    #[test]
    fn test_body_for_run_drops_a_site_input_that_names_no_param() {
        let manifest = manifest_with(serde_json::json!({
            "params": [{ "name": "altitude_m", "label": "Altitude", "kind": "number" }],
            "site_inputs": [{ "property": "altitude_m" }],
        }));
        let blob = serde_json::json!({ "inputs": { "altitude_m": 1500 } });
        let body = body_for_run(&tool(manifest), &event(), Some(&blob));
        assert!(!body.contains_key("altitude_m"));
    }

    #[test]
    fn test_body_for_run_states_the_context_over_a_stale_stored_copy() {
        let blob = serde_json::json!({
            "inputs": {
                "site_id": "00000000-0000-0000-0000-0000000000ff",
                "collected_at": "2020-01-01T00:00:00Z"
            }
        });
        let body = body_for_run(
            &tool(manifest_with(serde_json::json!({}))),
            &event(),
            Some(&blob),
        );
        assert_eq!(body["site_id"], serde_json::json!(Uuid::from_u128(4)));
        assert_eq!(body["collected_at"], "2026-02-28T06:10:00Z");
    }

    #[test]
    fn test_body_for_run_with_no_prior_run_carries_the_context_alone() {
        let body = body_for_run(&tool(manifest_with(serde_json::json!({}))), &event(), None);
        assert_eq!(body.len(), 2);
        assert!(body.contains_key("site_id") && body.contains_key("collected_at"));
    }

    // A blob with no `inputs` object is not a replayable run: nothing is carried forward.
    #[test]
    fn test_body_for_run_ignores_a_blob_with_no_inputs() {
        let blob = serde_json::json!({ "constants": { "xO2": 0.209446 } });
        let body = body_for_run(
            &tool(manifest_with(serde_json::json!({}))),
            &event(),
            Some(&blob),
        );
        assert_eq!(body.len(), 2);
    }

    /// The audit's tolerance, at the two edges that decide whether a run is reported at all.
    #[test]
    fn test_disagrees_at_the_tolerance_and_across_zero() {
        assert!(!disagrees(1.0, 1.0));
        assert!(!disagrees(1.0, 1.0 + 1e-12));
        assert!(disagrees(1.0, 1.000_001));
        // Both sides zero is agreement, not a division by nothing.
        assert!(!disagrees(0.0, 0.0));
        assert!(disagrees(0.0, 1e-6));
    }
}

#[cfg(test)]
mod order_tests {
    use super::{blob_fingerprint, dependency_order};
    use crate::routes::private::tools::engine::{ActiveTool, Engine, ParameterCatalog};
    use uuid::Uuid;

    /// A tool that reads `reads` at the event and writes `writes`, named by catalog code.
    fn tool(name: &str, writes: &[&str], reads: &[&str]) -> ActiveTool {
        let manifest = serde_json::json!({
            "label": name,
            "params": reads.iter().map(|code| serde_json::json!({
                "name": code, "label": code, "kind": "number",
            })).collect::<Vec<_>>(),
            "outputs": writes.iter().map(|code| serde_json::json!({
                "key": code, "label": code, "suggested_parameter_code": code,
            })).collect::<Vec<_>>(),
            "event_inputs": reads.iter().map(|code| serde_json::json!({
                "param": code, "parameter_code": code,
            })).collect::<Vec<_>>(),
        });
        ActiveTool {
            script_id: Uuid::new_v4(),
            name: name.to_string(),
            label: name.to_string(),
            description: None,
            version_id: Uuid::new_v4(),
            version_no: 1,
            script: String::new(),
            entry_function: "tool".to_string(),
            content_hash: String::new(),
            manifest: crate::routes::private::tools::engine::parse_manifest(&manifest)
                .expect("the manifest parses"),
            engine: Engine::Script,
            parameter_group_id: None,
            formulas: Vec::new(),
        }
    }

    fn catalog(codes: &[&str]) -> ParameterCatalog {
        let rows: Vec<(Uuid, &str)> = codes.iter().map(|c| (Uuid::new_v4(), *c)).collect();
        ParameterCatalog::with_codes(&rows)
    }

    #[test]
    fn a_producer_orders_ahead_of_its_consumer_whatever_the_declaration_order() {
        let tools = [
            tool("pco2", &["pco2"], &["k_h"]),
            tool("henry", &["k_h"], &["water_temp"]),
        ];
        let order = dependency_order(&tools, &catalog(&["pco2", "k_h", "water_temp"]))
            .expect("the set has an order");
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn tools_that_read_nothing_of_each_other_keep_their_declaration_order() {
        let tools = [
            tool("doc", &["doc"], &["a254"]),
            tool("chla", &["chla"], &["abs_665"]),
        ];
        let order = dependency_order(&tools, &catalog(&["doc", "chla", "a254", "abs_665"]))
            .expect("the set has an order");
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn a_cycle_is_refused_naming_both_tools() {
        let tools = [
            tool("a", &["out_a"], &["out_b"]),
            tool("b", &["out_b"], &["out_a"]),
        ];
        let err = dependency_order(&tools, &catalog(&["out_a", "out_b"]))
            .expect_err("two tools feeding each other have no runnable order");
        let message = err.to_string();
        assert!(message.contains("cycle"), "{message}");
        assert!(message.contains('a') && message.contains('b'), "{message}");
    }

    #[test]
    fn a_tool_reading_its_own_output_orders_rather_than_deadlocking_on_itself() {
        // The self-edge is excluded (a != b). Refusing a calculation that reads what it writes is
        // the formula engine's job, at its own level.
        let tools = [tool("a", &["out_a"], &["out_a"])];
        assert_eq!(
            dependency_order(&tools, &catalog(&["out_a"])).expect("one tool always orders"),
            vec![0]
        );
    }

    fn blob(version: Uuid, inputs: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "tool_version": { "script_version_id": version.to_string() },
            "inputs": inputs,
            "constants": { "xO2": 1.0 },
            "curves": [],
        })
    }

    #[test]
    fn a_run_fingerprints_the_same_whatever_order_its_inputs_are_written_in() {
        let version = Uuid::new_v4();
        assert_eq!(
            blob_fingerprint(&blob(version, serde_json::json!({ "a": 1.0, "b": 2.0 }))),
            blob_fingerprint(&blob(version, serde_json::json!({ "b": 2.0, "a": 1.0 }))),
        );
    }

    #[test]
    fn a_changed_input_or_a_new_script_version_fingerprints_differently() {
        let version = Uuid::new_v4();
        let base = blob_fingerprint(&blob(version, serde_json::json!({ "a": 1.0 })));
        assert!(base.is_some());
        assert_ne!(
            base,
            blob_fingerprint(&blob(version, serde_json::json!({ "a": 1.5 })))
        );
        assert_ne!(
            base,
            blob_fingerprint(&blob(Uuid::new_v4(), serde_json::json!({ "a": 1.0 })))
        );
    }

    #[test]
    fn a_blob_naming_no_script_version_has_no_fingerprint_and_never_memoises() {
        assert_eq!(blob_fingerprint(&serde_json::json!({ "inputs": {} })), None);
        assert_eq!(
            blob_fingerprint(&serde_json::json!({
                "tool_version": { "script_version_id": "not-a-uuid" }
            })),
            None
        );
    }
}

#[cfg(test)]
mod skip_reason_tests {
    use super::skip_reason;
    use crate::error::AppError;

    /// Expected behaviour: a tool whose inputs do not resolve, and a tool whose script raises,
    /// are both skips. The chain records them and runs on; neither fails the run.
    #[test]
    fn an_unresolved_input_and_a_script_error_are_both_skips() {
        assert_eq!(
            skip_reason(&AppError::BadRequest("pa is required".to_string())),
            Some("pa is required".to_string())
        );
        assert_eq!(
            skip_reason(&AppError::ToolScriptError {
                message: "division by zero".to_string(),
                call: Some("tool(inputs, constants, curves)".to_string()),
                traceback: vec!["stop(...)".to_string()],
            }),
            Some("script error: division by zero".to_string())
        );
    }

    #[test]
    fn anything_else_fails_the_run() {
        assert!(skip_reason(&AppError::Internal("pool closed".to_string())).is_none());
        assert!(skip_reason(&AppError::NotFound("event".to_string())).is_none());
        assert!(
            skip_reason(&AppError::ServiceUnavailable("runner down".to_string())).is_none(),
            "a runner that cannot be reached is not a tool that cannot be computed"
        );
    }
}
