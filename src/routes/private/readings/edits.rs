//! The edit primitive (Q8, M60): the one place a stored measurement is changed.
//!
//! Four steps over one selection, so every correction is provenance-routed, previewed, reversible
//! and told in full:
//!
//! - **inspect** says which route each row takes and what may be done to it. A value whose slot a
//!   calculation still owns is never edited in place: it is reopened in its tool, edited there and
//!   re-saved, which supersedes the outputs with fresh provenance. A value nothing computed, or
//!   one whose slot has been detached, is corrected here.
//! - **preview** applies the decision inside a transaction, reads back the numbers it moved, and
//!   rolls the transaction back, so what is shown is the write's own arithmetic rather than a
//!   second implementation of it.
//! - **commit** applies it for real, held to the previewed selection.
//! - **rollback** inverts the decision, because nothing here deletes.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::decisions::{self, Kind, Origin, Owner, Selection};
use crate::common::actor::label;

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
            Self::Curve | Self::EditDeployment | Self::EditCalibration => {
                Capability::ManageSensors
            }
            Self::Detach | Self::Return => Capability::Admin,
            _ => Capability::WriteData,
        }
    }
}

/// The options a row's provenance permits, in the order the surface offers them.
///
/// The rule is Q8's: the provenance already stored decides which path a cell takes. What decides
/// is ownership rather than the presence of a run, so a tool-run value offers no in-place
/// correction while its calculation owns the slot, and offers one once the slot is detached,
/// which is the single override Q117 kept.
#[must_use]
pub fn edit_options(p: &RowProvenance) -> Vec<EditOption> {
    let mut options = Vec::new();
    if p.has_tool_run && !p.slot_detached {
        options.push(EditOption::ReopenRun);
        options.push(EditOption::Detach);
    } else {
        if p.slot_detached {
            options.push(EditOption::Return);
        }
        options.push(EditOption::ValueCorrection);
        if p.has_standard_curve {
            options.push(EditOption::Curve);
        }
        // Attribution is never stamped on a row (Q117): a wrong instrument is a wrong deployment
        // and a wrong corrected value is a wrong calibration window, and the reprocess carries the
        // correction through. So the surface points at the record that decides, and offers nothing
        // where there is no such record to correct.
        if p.has_calibration {
            options.push(EditOption::EditCalibration);
        }
        // With no deployment the fix is to create one, so the surface points at the deployment
        // either way rather than falling silent where a pin used to be offered.
        options.push(EditOption::EditDeployment);
    }
    options.push(if p.is_flagged {
        EditOption::Unflag
    } else {
        EditOption::Flag
    });
    options.push(if p.withdrawn {
        EditOption::Reassert
    } else {
        EditOption::Withdraw
    });
    if p.unverified {
        options.push(EditOption::Verify);
        options.push(EditOption::Reject);
    }
    options
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
    fn assertion_over(
        &self,
        kind: Kind,
        selection: &Selection,
    ) -> AppResult<(serde_json::Value, EditOption)> {
        if kind == Kind::ValueCorrection
            && decisions::keyed_corrections(selection)?.is_some()
            && self.value.is_none()
        {
            return Ok((serde_json::json!({}), EditOption::ValueCorrection));
        }
        self.assertion(kind)
    }

    fn parsed(&self) -> AppResult<Kind> {
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
    fn assertion(&self, kind: Kind) -> AppResult<(serde_json::Value, EditOption)> {
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

/// The id a preview and its commit share.
///
/// It is a digest of the selection and the decision, not a stored row: a commit naming a preview
/// of a different selection or a different decision cannot produce the same id, which is exactly
/// what holding the commit to the preview means. Nothing expires, because nothing is stored.
fn preview_id(selection: &Selection, decision: &EditDecision) -> AppResult<Uuid> {
    let canonical = serde_json::json!({
        "selection": serde_json::to_value(selection)
            .map_err(|e| AppError::Internal(e.to_string()))?,
        "decision": serde_json::to_value(decision)
            .map_err(|e| AppError::Internal(e.to_string()))?,
    });
    Ok(Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        canonical.to_string().as_bytes(),
    ))
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

const ROW_SQL: &str = "SELECT r.stream_id, r.time, r.replicate_index, r.raw_value,
        r.site_id, r.parameter_id,
        r.provenance ->> 'run_id' AS run_id,
        r.standard_curve_id IS NOT NULL AS has_curve,
        r.calibration_id IS NOT NULL AS has_calibration,
        r.deployment_id IS NOT NULL AS has_deployment,
        COALESCE(r.is_flagged, false) AS is_flagged,
        r.withdrawn_at IS NOT NULL AS withdrawn,
        COALESCE(r.unverified, false) AS unverified,
        ds.source_system
   FROM readings r JOIN data_streams ds ON ds.id = r.stream_id";

/// One row of [`ROW_SQL`], decoded by the derive rather than column by column.
#[derive(FromQueryResult)]
struct StoredRow {
    stream_id: Uuid,
    time: sea_orm::prelude::DateTimeWithTimeZone,
    replicate_index: i16,
    raw_value: f64,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    run_id: Option<String>,
    has_curve: bool,
    has_calibration: bool,
    has_deployment: bool,
    is_flagged: bool,
    withdrawn: bool,
    unverified: bool,
    source_system: String,
}

fn classification(source_system: &str) -> String {
    match source_system {
        "grab_sample" => "manual",
        "api" => "api",
        "csv_import" => "csv",
        _ => "sync",
    }
    .to_string()
}

async fn inspect_rows<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
) -> AppResult<Vec<InspectedRow>> {
    let (predicate, binds) = selection.predicate()?;
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ROW_SQL} WHERE {predicate} ORDER BY r.time, r.stream_id, r.replicate_index"),
            binds,
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    let mut owners: HashMap<(Uuid, Uuid, chrono::DateTime<chrono::Utc>), Owner> = HashMap::new();
    for row in &rows {
        let row = StoredRow::from_query_result(row, "")?;
        // The run id is stored inside the provenance blob, so it arrives as text and is a run
        // reference only if it parses as one.
        let tool_run_id = row.run_id.as_deref().and_then(|s| s.parse::<Uuid>().ok());
        let time = row.time.with_timezone(&chrono::Utc);
        // Ownership is a fold over the slot's decisions, so it is read once per slot instant
        // rather than once per replicate.
        let mut owner = Owner::Tool;
        if let (Some(site), Some(parameter)) = (row.site_id, row.parameter_id) {
            owner = match owners.entry((site, parameter, time)) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => {
                    *e.insert(decisions::output_owner(conn, site, parameter, time).await?)
                }
            };
        }
        let provenance = RowProvenance {
            has_tool_run: tool_run_id.is_some(),
            slot_detached: owner == Owner::Manual,
            classification: classification(&row.source_system),
            has_standard_curve: row.has_curve,
            has_calibration: row.has_calibration,
            has_deployment: row.has_deployment,
            is_flagged: row.is_flagged,
            withdrawn: row.withdrawn,
            unverified: row.unverified,
        };
        out.push(InspectedRow {
            stream_id: row.stream_id,
            time,
            replicate_index: row.replicate_index,
            raw_value: row.raw_value,
            options: edit_options(&provenance),
            provenance,
            tool_run_id,
        });
    }
    Ok(out)
}

/// What each row a selection covers may have done to it, and by which route. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/inspect",
    request_body = InspectRequest,
    responses(
        (status = 200, description = "The rows and their routes", body = InspectResponse),
        (status = 400, description = "A selection naming nothing"),
    ),
    tag = "readings"
)]
pub async fn inspect(
    State(state): State<AppState>,
    Json(req): Json<InspectRequest>,
) -> AppResult<Json<InspectResponse>> {
    Ok(Json(InspectResponse {
        rows: inspect_rows(&state.db, &req.selection).await?,
    }))
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
    pub before: serde_json::Value,
    pub after: serde_json::Value,
}

/// One group's statistics, before and after.
#[derive(Debug, Serialize, ToSchema)]
pub struct MovedSample {
    pub sample_id: Uuid,
    pub before: serde_json::Value,
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
    #[schema(value_type = Vec<Object>)]
    pub calculations: Vec<serde_json::Value>,
    /// Everything the preview does not compute, named rather than left to be assumed.
    pub not_previewed: Vec<String>,
}

const STATE_SQL: &str = "jsonb_build_object(
    'raw_value', r.raw_value, 'calibrated_value', r.calibrated_value,
    'is_flagged', COALESCE(r.is_flagged, false), 'flag_reason', r.flag_reason,
    'withdrawn_at', r.withdrawn_at, 'unverified', COALESCE(r.unverified, false),
    'standard_curve_id', r.standard_curve_id, 'calibration_id', r.calibration_id,
    'sensor_id', r.sensor_id)";

async fn row_states<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<(Uuid, chrono::DateTime<chrono::Utc>, i16, serde_json::Value)>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT r.stream_id, r.time, r.replicate_index, {STATE_SQL} AS state
                   FROM readings r JOIN data_streams ds ON ds.id = r.stream_id
                  WHERE {predicate}
                  ORDER BY r.time, r.stream_id, r.replicate_index"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|row| {
            let row = StateRow::from_query_result(row, "")?;
            Ok((
                row.stream_id,
                row.time.with_timezone(&chrono::Utc),
                row.replicate_index,
                row.state,
            ))
        })
        .collect()
}

async fn sample_states<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<(Uuid, serde_json::Value)>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT DISTINCT s.id,
                        jsonb_build_object('mean', s.mean, 'stdev', s.stdev, 'n', s.n,
                                           'min_value', s.min_value, 'max_value', s.max_value)
                            AS stats
                   FROM readings r
                   JOIN data_streams ds ON ds.id = r.stream_id
                   JOIN samples s ON s.id = r.sample_id
                  WHERE {predicate}
                  ORDER BY s.id"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|row| {
            let row = SampleStatsRow::from_query_result(row, "")?;
            Ok((row.id, row.stats))
        })
        .collect()
}

/// The parameters a selection's rows belong to, for the calculation closure.
async fn touched_parameters<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<Uuid>> {
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT DISTINCT r.parameter_id FROM readings r
                   JOIN data_streams ds ON ds.id = r.stream_id
                  WHERE {predicate} AND r.parameter_id IS NOT NULL"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|row| Ok(ParameterRow::from_query_result(row, "")?.parameter_id))
        .collect()
}

/// Apply the decision on `conn` as one decision set, which the caller may then roll back. The
/// set is what makes a many-row edit, a visit retracted whole, one act to undo.
async fn apply<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
    decision: &EditDecision,
    actor: &str,
) -> AppResult<(Uuid, decisions::Recorded)> {
    let kind = decision.parsed()?;
    let (new, _) = decision.assertion_over(kind, selection)?;
    decisions::record_set(
        conn,
        kind,
        selection,
        new,
        actor,
        decision.reason.as_deref(),
        Origin::Manual,
    )
    .await
}

/// Refuse an edit the selected rows' provenance does not permit, naming the row that refused it.
async fn refuse_unrouted<C: ConnectionTrait>(
    conn: &C,
    selection: &Selection,
    option: EditOption,
) -> AppResult<()> {
    for row in inspect_rows(conn, selection).await? {
        if !row.options.contains(&option) {
            return Err(AppError::BadRequest(format!(
                "the reading at {} replicate {} is not corrected here: {}",
                row.time,
                row.replicate_index,
                if row.provenance.has_tool_run && !row.provenance.slot_detached {
                    "a tool run produced it, so it is reopened in its tool and saved again"
                } else {
                    "its provenance does not offer that edit"
                }
            )));
        }
    }
    Ok(())
}

/// Apply the decision, read back every number it moved, and roll it back. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/preview",
    request_body = EditRequest,
    responses(
        (status = 200, description = "What the edit would do", body = PreviewResponse),
        (status = 400, description = "An edit the rows' provenance does not permit"),
    ),
    tag = "readings"
)]
pub async fn preview(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(req): Json<EditRequest>,
) -> AppResult<Json<PreviewResponse>> {
    let actor = label(&auth);
    let kind = req.decision.parsed()?;
    let (_, option) = req.decision.assertion_over(kind, &req.selection)?;
    authorise(&auth, option)?;
    refuse_unrouted(&state.db, &req.selection, option).await?;
    let (predicate, binds) = req.selection.predicate()?;
    let parameters = touched_parameters(&state.db, &predicate, binds.clone()).await?;

    // The transaction is the preview: the decision is applied, the numbers are read back from the
    // rows the trigger just rewrote, and the whole thing is undone. Nothing recomputes the
    // arithmetic a second time, so the preview cannot disagree with the write.
    let (rows, samples) = crate::common::bulk_write::guarded_rollback(&state.db, async |txn| {
        let before_rows = row_states(txn, &predicate, binds.clone()).await?;
        let before_samples = sample_states(txn, &predicate, binds.clone()).await?;
        apply(txn, &req.selection, &req.decision, &actor).await?;
        let after_rows = row_states(txn, &predicate, binds.clone()).await?;
        let after_samples = sample_states(txn, &predicate, binds.clone()).await?;
        let rows: Vec<MovedRow> = before_rows
            .into_iter()
            .zip(after_rows)
            .map(
                |((stream_id, time, index, before), (_, _, _, after))| MovedRow {
                    stream_id,
                    time,
                    replicate_index: index,
                    before,
                    after,
                },
            )
            .collect();
        let samples: Vec<MovedSample> = before_samples
            .into_iter()
            .zip(after_samples)
            .map(|((sample_id, before), (_, after))| MovedSample {
                sample_id,
                before,
                after,
            })
            .collect();
        Ok((rows, samples))
    })
    .await?;

    let calculations =
        crate::routes::private::tools::closure::calculations_fed_by(&state.db, &parameters)
            .await?
            .into_iter()
            .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
            .collect();

    Ok(Json(PreviewResponse {
        preview_id: preview_id(&req.selection, &req.decision)?,
        rows,
        samples,
        calculations,
        not_previewed: vec![
            "continuous aggregates, which the commit refreshes over the range it moved".to_string(),
            "alarm episodes, which the commit re-evaluates for the slots it touched".to_string(),
            "the calculations listed, which run as their own job after the commit".to_string(),
        ],
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EditResponse {
    /// The decisions this edit recorded, which is what a rollback names.
    pub rows_decided: u64,
    pub decision_ids: Vec<Uuid>,
    /// The set the decisions were recorded under, rolled back as one act.
    pub set_id: Uuid,
}

/// Commit an edit, held to the selection and decision the preview covered. Requires `write_data`;
/// an attribution edit requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/readings/edits",
    request_body = EditRequest,
    responses(
        (status = 200, description = "The edit was recorded", body = EditResponse),
        (status = 400, description = "An edit the rows' provenance does not permit"),
        (status = 409, description = "The preview covered a different selection or decision"),
    ),
    tag = "readings"
)]
pub async fn commit(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(req): Json<EditRequest>,
) -> AppResult<Json<EditResponse>> {
    let actor = label(&auth);
    let kind = req.decision.parsed()?;
    let (_, option) = req.decision.assertion_over(kind, &req.selection)?;
    authorise(&auth, option)?;
    let expected = preview_id(&req.selection, &req.decision)?;
    match req.preview_id {
        Some(id) if id == expected => {}
        Some(_) => {
            return Err(AppError::Conflict(
                "that preview covered a different selection or decision; preview this one first"
                    .to_string(),
            ));
        }
        None => {
            return Err(AppError::BadRequest(
                "an edit is committed against the preview of itself; call preview first"
                    .to_string(),
            ));
        }
    }
    refuse_unrouted(&state.db, &req.selection, option).await?;

    let (set_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        apply(txn, &req.selection, &req.decision, &actor).await
    })
    .await?;

    propagate(&state, &recorded, &actor).await?;

    let (predicate, binds) = req.selection.predicate()?;
    let ids = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT d.id FROM reading_decisions d
                  WHERE d.rolled_back_by IS NULL AND d.kind = '{kind}'
                    AND EXISTS (SELECT 1 FROM readings r
                                  JOIN data_streams ds ON ds.id = r.stream_id
                                 WHERE {predicate} AND r.stream_id = d.stream_id
                                   AND r.time = d.time
                                   AND (d.replicate_index IS NULL
                                        OR d.replicate_index = r.replicate_index))
                  ORDER BY d.at DESC, d.id DESC LIMIT $%LIMIT%",
                kind = kind.as_str()
            )
            .replace("$%LIMIT%", &recorded.rows.max(1).to_string()),
            binds,
        ))
        .await?;
    Ok(Json(EditResponse {
        rows_decided: recorded.rows,
        decision_ids: ids
            .iter()
            .map(|r| r.try_get("", "id"))
            .collect::<Result<_, _>>()?,
        set_id,
    }))
}

/// The caller's standing against the option's capability.
fn authorise(auth: &AuthContext, option: EditOption) -> AppResult<()> {
    if auth.allows(option.capability()) {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "that edit requires {}",
        option.capability()
    )))
}

/// What every edit owes after its transaction commits, forward or inverted: the rollups over the
/// span it moved, and the calculations at the visits it touched.
async fn propagate(
    state: &AppState,
    recorded: &decisions::Recorded,
    actor: &str,
) -> AppResult<()> {
    if let Some((lo, hi)) = recorded.span
        && let Err(e) = crate::common::aggregates::refresh(
            &state.db,
            crate::common::aggregates::Window::Range(lo, hi),
        )
        .await
    {
        tracing::warn!(error = %e, "edit: aggregate refresh failed");
    }
    crate::routes::private::collection_events::recompute::enqueue_for(
        &state.db,
        &recorded.touched_events,
        actor,
        crate::routes::private::collection_events::recompute::Writer::Person,
    )
    .await?;
    Ok(())
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackResponse {
    pub rollback_id: Uuid,
}

/// Invert one edit, restoring exactly the state its decision recorded. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/{id}/rollback",
    params(("id" = Uuid, Path, description = "The decision the edit recorded")),
    responses(
        (status = 200, description = "Inverted", body = RollbackResponse),
        (status = 404, description = "No such decision"),
        (status = 409, description = "Already rolled back, or a decision that projects nothing"),
    ),
    tag = "readings"
)]
pub async fn rollback(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> AppResult<Json<RollbackResponse>> {
    let actor = label(&auth);
    let (rollback_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        decisions::rollback(txn, id, &actor, Some("edit rolled back")).await
    })
    .await?;
    propagate(&state, &recorded, &actor).await?;
    // Inverting a pin changes what the window resolves for that reading, and only the reprocess
    // writes it. The forward path enqueues the same job.
    decisions::enqueue_pin_reprocess_for_decision(&state.db, id).await?;
    Ok(Json(RollbackResponse { rollback_id }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackSetResponse {
    pub set_id: Uuid,
    pub rolled_back: usize,
}

/// Invert every live decision an edit's set recorded, restoring exactly the state each one
/// recorded. This is how a visit retracted whole is put back. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/sets/{set_id}/rollback",
    params(("set_id" = Uuid, Path, description = "The set the edit recorded")),
    responses(
        (status = 200, description = "Inverted", body = RollbackSetResponse),
        (status = 404, description = "No such set"),
        (status = 409, description = "Already rolled back"),
    ),
    tag = "readings"
)]
pub async fn rollback_edit_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::extract::Path(set_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<RollbackSetResponse>> {
    let actor = label(&auth);
    let (rolled_back, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        decisions::rollback_set(txn, set_id, &actor, Some("edit set rolled back")).await
    })
    .await?;
    propagate(&state, &recorded, &actor).await?;
    decisions::enqueue_pin_reprocess_for_set(&state.db, set_id).await?;
    Ok(Json(RollbackSetResponse {
        set_id,
        rolled_back,
    }))
}

/// One reading's servedness, for the states a decision moves between.
#[derive(FromQueryResult)]
struct StateRow {
    stream_id: Uuid,
    time: sea_orm::prelude::DateTimeWithTimeZone,
    replicate_index: i16,
    state: serde_json::Value,
}

/// One sample's statistics, as the preview compares them before and after.
#[derive(FromQueryResult)]
struct SampleStatsRow {
    id: Uuid,
    stats: serde_json::Value,
}

#[derive(FromQueryResult)]
struct ParameterRow {
    parameter_id: Uuid,
}

/// The stored run a reopened calculation is rebuilt from.
#[derive(FromQueryResult)]
struct StoredRun {
    tool_name: String,
    inputs: serde_json::Value,
    constants: serde_json::Value,
    curves: serde_json::Value,
    context: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReloadResponse {
    pub tool: String,
    /// The calculate body that reproduces the run: its stored inputs plus the calculation context.
    #[schema(value_type = Object)]
    pub body: serde_json::Value,
    #[schema(value_type = Object)]
    pub constants: serde_json::Value,
    #[schema(value_type = Vec<Object>)]
    pub curves: Vec<serde_json::Value>,
}

/// The stored run in the shape `/tools/{name}/calculate` takes, so a value a tool produced is
/// corrected by reopening it, editing an input and saving again (Q8, M4). Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/tool_runs/{id}/reload",
    params(("id" = Uuid, Path, description = "Tool run id")),
    responses(
        (status = 200, description = "The run's own inputs and context", body = ReloadResponse),
        (status = 404, description = "No such run"),
    ),
    tag = "tools"
)]
pub async fn reload_run(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> AppResult<Json<ReloadResponse>> {
    let row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT tool_name, inputs, constants, curves, context FROM tool_runs WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Tool run {id} not found")))?;
    let run = StoredRun::from_query_result(&row, "")?;
    let mut body = run.inputs.as_object().cloned().unwrap_or_default();
    // The reserved context fields the calculate body takes, so the reopened run resolves its
    // station and event inputs at the same visit rather than at whatever the browser last saw.
    for field in ["site_id", "collected_at"] {
        if let Some(value) = run.context.get(field)
            && !value.is_null()
        {
            body.insert(field.to_string(), value.clone());
        }
    }
    Ok(Json(ReloadResponse {
        tool: run.tool_name,
        body: serde_json::Value::Object(body),
        constants: run.constants,
        curves: run.curves.as_array().cloned().unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{EditOption, RowProvenance, edit_options};

    fn manual() -> RowProvenance {
        RowProvenance {
            classification: "manual".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_tool_run_value_is_reopened_in_its_tool_and_never_corrected_in_place() {
        let p = RowProvenance {
            has_tool_run: true,
            ..manual()
        };
        let options = edit_options(&p);
        assert!(options.contains(&EditOption::ReopenRun));
        assert!(options.contains(&EditOption::Detach));
        assert!(
            !options.contains(&EditOption::ValueCorrection),
            "correcting it here would leave the run beside a number it did not produce"
        );
        // The measurement's own state is still a person's to rule on.
        assert!(options.contains(&EditOption::Flag));
        assert!(options.contains(&EditOption::Withdraw));
    }

    #[test]
    fn a_detached_slot_is_corrected_in_place_and_offers_the_way_back() {
        let p = RowProvenance {
            has_tool_run: true,
            slot_detached: true,
            ..manual()
        };
        let options = edit_options(&p);
        assert!(
            options.contains(&EditOption::ValueCorrection),
            "the slot is off its calculation, so the value is a person's to write"
        );
        assert!(options.contains(&EditOption::Return));
        assert!(!options.contains(&EditOption::ReopenRun));
        assert!(
            !options.contains(&EditOption::Detach),
            "it is already detached, so detaching again is refused"
        );
    }

    #[test]
    fn a_value_no_tool_produced_is_corrected_in_place() {
        let options = edit_options(&manual());
        assert!(options.contains(&EditOption::ValueCorrection));
        assert!(!options.contains(&EditOption::ReopenRun));
        assert!(!options.contains(&EditOption::Detach));
        // Nothing corrected it, so there is no calibration window to point at; the instrument
        // still comes from a deployment, and with none covering it the fix is to create one.
        assert!(!options.contains(&EditOption::Curve));
        assert!(!options.contains(&EditOption::EditCalibration));
        assert!(options.contains(&EditOption::EditDeployment));
    }

    #[test]
    fn each_correction_offers_the_curve_that_made_it() {
        let curved = RowProvenance {
            has_standard_curve: true,
            ..manual()
        };
        assert!(edit_options(&curved).contains(&EditOption::Curve));
        let windowed = RowProvenance {
            has_calibration: true,
            has_deployment: true,
            ..manual()
        };
        let options = edit_options(&windowed);
        assert!(
            options.contains(&EditOption::EditCalibration),
            "a windowed correction is fixed in the window, not pinned on the row"
        );
        assert!(
            options.contains(&EditOption::EditDeployment),
            "a deployed instrument is fixed in the deployment, not pinned on the row"
        );
    }

    #[test]
    fn the_state_a_row_is_in_decides_which_half_of_each_pair_is_offered() {
        let flagged = RowProvenance {
            is_flagged: true,
            ..manual()
        };
        let options = edit_options(&flagged);
        assert!(options.contains(&EditOption::Unflag) && !options.contains(&EditOption::Flag));
        let withdrawn = RowProvenance {
            withdrawn: true,
            ..manual()
        };
        let options = edit_options(&withdrawn);
        assert!(
            options.contains(&EditOption::Reassert) && !options.contains(&EditOption::Withdraw)
        );
        // A pending entry is ruled on rather than edited around.
        let pending = RowProvenance {
            unverified: true,
            ..manual()
        };
        let options = edit_options(&pending);
        assert!(options.contains(&EditOption::Verify) && options.contains(&EditOption::Reject));
        assert!(!edit_options(&manual()).contains(&EditOption::Verify));
    }

    #[test]
    fn attribution_needs_more_than_curation_does() {
        use crate::common::authz::Capability;
        for option in [
            EditOption::ValueCorrection,
            EditOption::Flag,
            EditOption::Withdraw,
        ] {
            assert_eq!(option.capability(), Capability::WriteData, "{option:?}");
        }
        for option in [
            EditOption::Curve,
            EditOption::EditCalibration,
            EditOption::EditDeployment,
        ] {
            assert_eq!(option.capability(), Capability::ManageSensors, "{option:?}");
        }
        assert_eq!(EditOption::Detach.capability(), Capability::Admin);
    }
}
