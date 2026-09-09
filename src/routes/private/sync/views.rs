use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors;

use super::operator;
use axum::routing::patch;

/// Sync admin views are split by required authorization so the unified `/api/` router
/// can layer the right middleware per group without route-level overrides.
///
/// Group membership:
/// - `read_routes`: list/get operations, fine for any read_metadata caller.
/// - `write_routes`: operator actions such as issuing sync commands and pairing workflows.
///   Same gate as other entity mutations (Keycloak admin or write_metadata token).
/// - `manage_routes`: the replicate audit review surface and non-destructive reconciliation.
///   Manager-level humans and above (or a write_metadata token); interns and plain members never
///   see the audit backlog.
/// - `destructive_routes`: the reconciliation delete job, which removes streams and readings, so
///   it takes the same gate as stream deletion on CRUD (Keycloak admin or write_metadata token).
/// - `admin_routes`: credential listing, creation and revoke, these mint full-permission
///   sync session tokens, so they're Keycloak-admin only (no API token can pass). The listing
///   is admin-gated alongside them, matching `sync_service_credentials` CRUD, so a leaked token
///   cannot enumerate which credentials exist.
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/services", get(operator::list_services))
        .route("/services/{id}", get(operator::get_service))
        .route("/commands", get(operator::list_commands))
        .route("/commands/{id}", get(operator::get_command))
        .route("/events", get(operator::list_sync_events))
        .route("/pairing-plans", get(list_pairing_plans))
        .route("/pairing-plans/{id}", get(get_pairing_plan))
        .route("/pairing-plans/{id}/site-metadata", get(plan_site_metadata))
        .route("/pairing-plans/{id}/instruments", get(plan_instruments))
        .route("/unpaired-summary", get(unpaired_summary))
}

pub fn write_routes() -> Router<AppState> {
    Router::new()
        .route("/services/{id}", patch(operator::update_service))
        .route("/services/{id}/commands", post(operator::issue_command))
        .route("/services/{id}/revoke", post(operator::revoke_service))
        .route("/pairing-plans", post(create_pairing_plan))
        .route("/pairing-plans/{id}", patch(update_pairing_plan))
        .route("/pairing-plans/{id}/apply", post(apply_pairing_plan))
        .route(
            "/pairing-plans/{id}/supersede",
            post(supersede_pairing_plan),
        )
        .route("/pairing-plans/{id}/revert", post(revert_pairing_plan))
}

pub fn manage_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/replicate_audit_holds",
            get(super::replicate_audit::list_holds),
        )
        .route(
            "/replicate_audit_holds/{id}/acknowledge",
            post(super::replicate_audit::acknowledge_hold),
        )
        .route(
            "/replicate_audit_holds/{id}/resolve",
            post(super::replicate_audit::resolve_hold),
        )
        .route(
            "/replicate_audit_holds/{id}/reopen",
            post(super::replicate_audit::reopen_hold),
        )
        .route(
            "/replicate_audit_holds/acknowledge_bulk",
            post(super::replicate_audit::acknowledge_holds_bulk),
        )
        .route(
            "/change_proposals",
            get(crate::routes::private::readings::proposals::list_proposals),
        )
        .route(
            "/change_proposals/decide",
            post(crate::routes::private::readings::proposals::decide_proposals),
        )
        .route(
            "/replicate_reconciliation/duplicate_slots",
            get(super::replicate_reconciliation::duplicate_slots),
        )
        .route(
            "/replicate_reconciliation/candidates",
            get(super::replicate_reconciliation::reconciliation_candidates),
        )
        .route(
            "/replicate_reconciliation",
            post(super::replicate_reconciliation::start_reconciliation),
        )
}

/// Destructive reconciliation: the delete job removes obsolete streams and their readings, so it
/// sits behind the same gate as stream deletion on CRUD (Keycloak Administrator or a
/// write_metadata token), not the manager review layer the non-destructive endpoints use.
pub fn destructive_routes() -> Router<AppState> {
    Router::new().route(
        "/replicate_reconciliation/delete",
        post(super::replicate_reconciliation::start_reconciliation_delete),
    )
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/credentials",
            get(operator::list_credentials).post(operator::create_credential),
        )
        .route(
            "/credentials/{id}/revoke",
            post(operator::revoke_credential),
        )
}

#[derive(Deserialize)]
pub struct CreatePairingPlanRequest {
    source_system: String,
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

/// How much of one source system is paired, the dashboard's "needs attention" count.
#[derive(Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct UnpairedSummaryRow {
    pub source_system: String,
    pub unpaired: i64,
    pub paired: i64,
}

/// One site's metadata as the plan's streams carry it: every field is text in `metadata`, so the
/// row reads them as text and the parses below turn them into what the response holds.
#[derive(sea_orm::FromQueryResult)]
struct PlanSiteMetadataRow {
    site_name: Option<String>,
    latitude: Option<String>,
    longitude: Option<String>,
    altitude_m: Option<String>,
    glacier_name: Option<String>,
    glacier_rgi: Option<String>,
    location_type: Option<String>,
    catchment: Option<String>,
    full_name: Option<String>,
    elevation: Option<String>,
    channel_id: Option<String>,
    sample_interval_sec: Option<String>,
}

#[derive(sea_orm::FromQueryResult)]
struct PlanSiteDeviceRow {
    site_name: Option<String>,
    serial: Option<String>,
    model: Option<String>,
    streams: i64,
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

/// Create a draft pairing plan describing a batch of intended stream-to-site_parameter
/// pairings. The plan is reviewable and applied separately. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans",
    request_body(content = Object),
    responses(
        (status = 200, description = "Created pairing plan", body = crate::routes::private::data_streams::pairing_plans::PairingPlan),
    ),
    tag = "sync"
)]
pub async fn create_pairing_plan(
    State(state): State<AppState>,
    Json(req): Json<CreatePairingPlanRequest>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::PairingPlan>> {
    let plan =
        crate::routes::private::sync::service::create_plan(&state.db, &req.source_system).await?;
    Ok(Json(plan.into()))
}

#[derive(Deserialize)]
pub struct ListPairingPlansQuery {
    #[serde(default)]
    source_system: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// A plan without its `entries` document. A CNET draft's entries are 185 kB and a NOMIS draft's
/// 2.5 MB, and the listing is a way back into a review, not a way to read every draft at once.
#[derive(Serialize, ToSchema)]
pub struct PairingPlanSummary {
    id: Uuid,
    source_system: String,
    status: String,
    created_by: Option<String>,
    #[schema(value_type = crate::routes::private::sync::service::PlanSummary)]
    summary: serde_json::Value,
    created_at: chrono::DateTime<chrono::FixedOffset>,
    applied_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    /// Streams of this source that are unpaired now and not in the plan, so a draft built while a
    /// sync service was still registering says how much of the source it leaves behind. Counted for
    /// a draft only; a plan that has been applied or superseded is history.
    uncovered_streams: Option<i64>,
}

/// List pairing plans, newest first, optionally narrowed to one source system or one status
/// (draft/applying/applied/reverting/reverted/superseded). Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans",
    params(
        ("source_system" = Option<String>, Query, description = "Only this source system"),
        ("status" = Option<String>, Query, description = "Only this status"),
    ),
    responses(
        (status = 200, description = "Array of pairing plans without their entries", body = Vec<PairingPlanSummary>),
    ),
    tag = "sync"
)]
pub async fn list_pairing_plans(
    State(state): State<AppState>,
    Query(query): Query<ListPairingPlansQuery>,
) -> AppResult<Json<Vec<PairingPlanSummary>>> {
    use crate::routes::private::data_streams::pairing_plans::{Column, Entity};
    use sea_orm::{QueryOrder, QuerySelect};

    let mut select = Entity::find();
    if let Some(source) = query.source_system.as_deref().map(str::trim)
        && !source.is_empty()
    {
        select = select.filter(Column::SourceSystem.eq(source));
    }
    if let Some(status) = query.status.as_deref().map(str::trim)
        && !status.is_empty()
    {
        select = select.filter(Column::Status.eq(status));
    }
    let rows = select
        .select_only()
        .columns([
            Column::Id,
            Column::SourceSystem,
            Column::Status,
            Column::CreatedBy,
            Column::Summary,
            Column::CreatedAt,
            Column::AppliedAt,
        ])
        .order_by_desc(Column::CreatedAt)
        .into_tuple::<(
            Uuid,
            String,
            String,
            Option<String>,
            serde_json::Value,
            chrono::DateTime<chrono::FixedOffset>,
            Option<chrono::DateTime<chrono::FixedOffset>>,
        )>()
        .all(&state.db)
        .await?;

    let mut listing = Vec::with_capacity(rows.len());
    for (id, source_system, status, created_by, summary, created_at, applied_at) in rows {
        let uncovered = if status == "draft" {
            uncovered_stream_count(&state.db, id, &source_system).await?
        } else {
            None
        };
        listing.push(PairingPlanSummary {
            id,
            source_system,
            status,
            created_by,
            summary,
            created_at,
            applied_at,
            uncovered_streams: uncovered,
        });
    }

    Ok(Json(listing))
}

/// Streams a draft does not cover: unpaired now, plannable (`create_plan` skips a legacy single
/// superseded by its `:reps` family), and named by no entry. The plan's own entries decide it, so
/// the count is exact where comparing entry totals with a stream total is not: a replicate family
/// is one entry over several source columns.
async fn uncovered_stream_count(
    db: &sea_orm::DatabaseConnection,
    plan_id: Uuid,
    source_system: &str,
) -> AppResult<Option<i64>> {
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT count(*) AS n
              FROM data_streams ds
             WHERE ds.source_system = $2
               AND ds.site_parameter_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM data_streams fam
                                WHERE fam.source_system = ds.source_system
                                  AND fam.source_key = ds.source_key || ':reps')
               AND ds.id NOT IN (SELECT (e ->> 'stream_id')::uuid
                                   FROM pairing_plans p, jsonb_array_elements(p.entries) e
                                  WHERE p.id = $1)",
            [plan_id.into(), source_system.into()],
        ))
        .await?;
    Ok(row.map(|r| r.try_get::<i64>("", "n")).transpose()?)
}

/// Mark a draft superseded, which is what Start over does to the draft it replaces: the decisions
/// stay readable and `apply` refuses it as it refuses an applied plan. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/supersede",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Plan superseded", body = PlanStatusChanged),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status"),
    ),
    tag = "sync"
)]
pub async fn supersede_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PlanStatusChanged>> {
    let status = plan_status(&state.db, id).await?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only supersede 'draft' plans"
        )));
    }
    state
        .db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE pairing_plans SET status = 'superseded' WHERE id = $1 AND status = 'draft'",
            [id.into()],
        ))
        .await?;
    Ok(Json(PlanStatusChanged {
        id,
        status: "superseded".to_string(),
    }))
}

/// Get a single pairing plan with its full pairing list. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Pairing plan with intended pairings", body = crate::routes::private::data_streams::pairing_plans::PairingPlan),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn get_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::PairingPlan>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(Json(plan.into()))
}

#[derive(Deserialize)]
pub struct UpdatePairingPlanRequest {
    /// The version the client read. The write is refused if the plan has moved on since.
    expected_version: i32,
    /// A plan-wide decision, applied before the per-entry updates so an operator can pair what
    /// matched, or skip what did not, in one act rather than one line per entry.
    #[serde(default)]
    bulk: Option<BulkAction>,
    #[serde(default)]
    updates: Vec<PlanEntryUpdate>,
    /// Standard curves to assign to instruments this plan will create. The move happens when the
    /// plan is applied, in the transaction that mints the instrument.
    #[serde(default)]
    curves: Vec<PlanCurveUpdate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkAction {
    #[serde(default)]
    r#where: crate::routes::private::sync::service::BulkWhere,
    /// `pair` or `skip`.
    action: String,
}

#[derive(Deserialize)]
struct PlanCurveUpdate {
    curve_id: Uuid,
    /// The `source_key` of an instrument the plan proposes creating. Null clears the assignment,
    /// leaving the curve on the instrument it has.
    #[serde(default)]
    instrument_source_key: Option<String>,
}

/// Fold the review's curve assignments into the plan's list. An assignment must name a curve that
/// exists and that no reading names yet (the curve's own update route refuses a used curve the
/// same way), and an instrument some paired entry proposes creating; anything else is a 400 now
/// rather than a failed apply later.
async fn apply_curve_updates(
    db: &sea_orm::DatabaseConnection,
    entries: &[crate::routes::private::sync::service::PlanEntry],
    intents: &mut Vec<crate::routes::private::sync::service::PlanCurveIntent>,
    updates: &[PlanCurveUpdate],
) -> AppResult<()> {
    for update in updates {
        intents.retain(|i| i.curve_id != update.curve_id);
        let Some(source_key) = update
            .instrument_source_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            continue;
        };
        let proposed = entries.iter().any(|e| {
            e.action == "pair"
                && e.instrument
                    .as_ref()
                    .is_some_and(|i| i.create && i.source_key == source_key)
        });
        if !proposed {
            return Err(AppError::BadRequest(format!(
                "this plan does not create an instrument with source key '{source_key}'; a \
                 curve can only be assigned here to an instrument the plan will create, an \
                 existing instrument takes it through the curve itself"
            )));
        }
        if crate::routes::private::sensors::standard_curves::Entity::find_by_id(update.curve_id)
            .one(db)
            .await?
            .is_none()
        {
            return Err(AppError::BadRequest(format!(
                "standard curve {} does not exist",
                update.curve_id
            )));
        }
        if crate::routes::private::sensors::standard_curves::views::curve_is_used(
            db,
            update.curve_id,
        )
        .await?
        {
            return Err(AppError::BadRequest(format!(
                "standard curve {} has already been applied to readings, so its instrument is \
                 fixed. Create a new curve on the new instrument and re-enter the affected \
                 measurements against it.",
                update.curve_id
            )));
        }
        intents.push(crate::routes::private::sync::service::PlanCurveIntent {
            curve_id: update.curve_id,
            instrument_source_key: source_key.to_string(),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct PlanEntryUpdate {
    stream_id: Uuid,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    project_name: Option<String>,
    #[serde(default)]
    site_name: Option<String>,
    /// The coordinates and elevation the apply will create this site with. A site is created once,
    /// so a value the source recorded wrong (an elevation of 1 m) is corrected here rather than on
    /// the site afterwards. Ignored where the entry resolves to a site that already exists: that
    /// site's own page owns its attributes. Applied to every entry naming the same site, so the
    /// twenty-three feeds at one station do not disagree about where it is.
    #[serde(default)]
    site_latitude: Option<f64>,
    #[serde(default)]
    site_longitude: Option<f64>,
    #[serde(default)]
    site_altitude_m: Option<f64>,
    #[serde(default)]
    parameter_name: Option<String>,
    #[serde(default)]
    parameter_units: Option<String>,
    /// Human display label for the parameter. Takes effect only when apply creates the
    /// parameter (`create: true`); a matched existing parameter keeps its own name.
    #[serde(default)]
    parameter_label: Option<String>,
    /// Point this entry's curve references at an existing lab instrument instead of the resolved
    /// one. Applies to every entry sharing the same curve column, since one column is one
    /// instrument across the source.
    #[serde(default)]
    instrument_id: Option<Uuid>,
    /// Rename an instrument the plan will create. Ignored once it resolves to an existing one.
    #[serde(default)]
    instrument_name: Option<String>,
    /// Agree to creating the proposed instrument. Apply refuses while any remain unconfirmed.
    #[serde(default)]
    instrument_confirmed: Option<bool>,
    /// Detach the instrument from every entry this one groups with. Attaching is `instrument_id`;
    /// this is its inverse, since an absent `instrument_id` means "unchanged", not "none".
    #[serde(default)]
    instrument_clear: Option<bool>,
    /// Declare which divisor this slot publishes its replicate standard deviation with,
    /// `sample` or `population`. Applied to the `site_parameters` row when the plan is applied.
    /// Never inferred: absent leaves the slot undeclared and the audit gate asks later.
    #[serde(default)]
    sd_estimator: Option<String>,
    /// Record that a person looked at this entry and agreed with it, or take that back. Separate
    /// from `action`, so changing what an entry does is not the same as deciding it.
    #[serde(default)]
    acknowledged: Option<bool>,
}

/// What an instrument decision covers where the entry names no instrument yet. A curve column is
/// one instrument across the whole source, so settling it on any one entry settles every entry
/// sharing the column; where no column names a curve, the source parameter plays that role, so
/// choosing the fluorometer for `chla_acid` covers all 31 stations rather than one.
fn instrument_scope(entry: &crate::routes::private::sync::service::PlanEntry) -> String {
    match entry
        .instrument
        .as_ref()
        .and_then(|i| i.curve_column.as_deref())
    {
        Some(column) => format!("column:{column}"),
        None => format!(
            "parameter:{}",
            entry
                .parameter
                .group_key
                .as_deref()
                .unwrap_or(&entry.parameter.name)
        ),
    }
}

/// The identity an instrument decision belongs to. An entry that already names an instrument
/// belongs to that instrument, however many source columns share it: the portal's `chla acid`
/// curve corrects both `Chla_acid_ugL` and `Chla_acid_ugm2` from one lab instrument, and keying
/// those by parameter would report one instrument as two rows and move only half of it when the
/// operator repointed it. An entry with no instrument has only its scope to be keyed by.
fn instrument_key(entry: &crate::routes::private::sync::service::PlanEntry) -> String {
    match entry.instrument.as_ref() {
        Some(instrument) if !instrument.source_key.is_empty() => {
            format!("instrument:{}", instrument.source_key)
        }
        _ => instrument_scope(entry),
    }
}

/// Apply a site's coordinate edits.
///
/// Kept apart from the per-entry loop for the reason the instrument half is: where a site is
/// concerned, every entry naming it is one row to the operator. Editing the elevation on one of a
/// station's twenty-three feeds and leaving the other twenty-two at the source's value would make
/// the created site's attributes depend on which entry the apply read first.
///
/// A site the plan resolved to an existing row is left alone: its attributes are its own page's,
/// and the apply only ever backfills a coordinate such a site is missing.
fn apply_site_attribute_updates(
    entries: &mut [crate::routes::private::sync::service::PlanEntry],
    updates: &[PlanEntryUpdate],
) {
    for update in updates {
        if update.site_latitude.is_none()
            && update.site_longitude.is_none()
            && update.site_altitude_m.is_none()
        {
            continue;
        }
        let Some(target) = entries.iter().find(|e| e.stream_id == update.stream_id) else {
            continue;
        };
        if target.site.id.is_some() {
            continue;
        }
        let name = target.site.name.to_lowercase();
        for entry in entries
            .iter_mut()
            .filter(|e| e.site.id.is_none() && e.site.name.to_lowercase() == name)
        {
            if let Some(lat) = update.site_latitude {
                entry.site.latitude = Some(lat);
            }
            if let Some(lon) = update.site_longitude {
                entry.site.longitude = Some(lon);
            }
            if let Some(alt) = update.site_altitude_m {
                entry.site.altitude_m = Some(alt);
            }
        }
    }
}

/// Apply the instrument half of a plan edit.
///
/// Kept apart from the per-entry loop because an instrument decision is per instrument, not per
/// stream: one instrument serves the whole source, so confirming or repointing it on any one entry
/// settles every entry that shares it. Doing it per entry would leave 30 of 31 DOC streams still
/// asking.
async fn apply_instrument_updates(
    state: &AppState,
    source_system: &str,
    entries: &mut [crate::routes::private::sync::service::PlanEntry],
    updates: &[PlanEntryUpdate],
) -> AppResult<()> {
    for update in updates {
        if update.instrument_id.is_none()
            && update.instrument_name.is_none()
            && update.instrument_confirmed.is_none()
            && update.instrument_clear != Some(true)
        {
            continue;
        }
        let Some(target) = entries.iter().find(|e| e.stream_id == update.stream_id) else {
            continue;
        };
        let key = instrument_key(target);
        // One key in, one key out: the proposal a rename mints is derived from the entry the
        // operator edited, so a row covering several source columns stays one row.
        let proposed_source_key = match target
            .instrument
            .as_ref()
            .and_then(|i| i.curve_column.as_deref())
        {
            Some(column) => format!("{source_system}:{column}"),
            None => format!("{source_system}:{}", target.parameter.name),
        };

        // A feed the source reports as a device has its instrument already: one minted for the
        // slot it serves when the stream is paired, with that slot's deployment opened. Minting a
        // lab instrument for it instead would take both.
        if update.instrument_name.is_some() && update.instrument_id.is_none() && target.is_device {
            let named = match &target.device_serial {
                Some(serial) => format!(" (the source names device serial {serial})"),
                None => String::new(),
            };
            return Err(AppError::BadRequest(format!(
                "stream {} is reported as a device{named}, so its instrument is minted for the \
                 slot it serves when the stream is paired. Attach an existing instrument to \
                 override that, or leave it unset.",
                update.stream_id
            )));
        }

        // A repoint has to name an instrument that exists; otherwise the plan would carry an id
        // the apply cannot resolve.
        let repointed = match update.instrument_id {
            Some(id) => Some(
                sensors::Entity::find_by_id(id)
                    .one(&state.db)
                    .await?
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Instrument {id} does not exist"))
                    })?,
            ),
            None => None,
        };
        // The chosen instrument's curves travel with the entry, so the review shows what it
        // corrects with rather than only its name.
        let repointed_curves = match &repointed {
            Some(sensor) => crate::routes::private::sensors::standard_curves::Entity::find()
                .filter(
                    crate::routes::private::sensors::standard_curves::Column::SensorId
                        .eq(sensor.id),
                )
                .all(&state.db)
                .await?
                .into_iter()
                .map(|c| crate::routes::private::sync::service::PlanCurveRef {
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                })
                .collect(),
            None => Vec::new(),
        };

        for entry in entries.iter_mut().filter(|e| instrument_key(e) == key) {
            if update.instrument_clear == Some(true) {
                entry.instrument = None;
                continue;
            }
            // A stream whose source names no curve per reading has no instrument until someone
            // says which one corrected it upstream. Attaching an existing one records that, and
            // naming a new one proposes it; neither stamps, because the value already carries the
            // correction. The identity is the parameter, so every station moves together.
            if entry.instrument.is_none() {
                let proposed = update
                    .instrument_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                if repointed.is_some() || proposed.is_some() {
                    let name = proposed
                        .map(str::to_string)
                        .unwrap_or_else(|| entry.parameter.name.clone());
                    entry.instrument =
                        Some(crate::routes::private::sync::service::PlanInstrumentRef {
                            curve_column: None,
                            id: None,
                            name: name.clone(),
                            source_key: proposed_source_key.clone(),
                            resolved_by: if repointed.is_some() {
                                "manual".to_string()
                            } else {
                                "placeholder".to_string()
                            },
                            create: repointed.is_none(),
                            confirmed: repointed.is_some(),
                            stamps_readings: false,
                            curves: Vec::new(),
                            proposed_name: Some(name),
                            name_conflict: None,
                        });
                }
            }
            let Some(instrument) = entry.instrument.as_mut() else {
                continue;
            };
            if let Some(sensor) = &repointed {
                instrument.id = Some(sensor.id);
                instrument.name = sensor
                    .name
                    .clone()
                    .or_else(|| sensor.serial_number.clone())
                    .unwrap_or_else(|| sensor.id.to_string());
                instrument.source_key = sensor.source_key.clone().unwrap_or_default();
                instrument.resolved_by = "manual".to_string();
                instrument.create = false;
                instrument.confirmed = true;
                instrument.curves = repointed_curves.clone();
            }
            // Naming an instrument proposes one; picking from the inventory attaches one. So a
            // name arriving at an entry that holds an existing instrument returns it to a
            // proposal, rather than doing nothing (which is what an operator undoing a mis-click
            // used to get) or renaming the inventory row (which this route never does).
            if let Some(name) = &update.instrument_name
                && repointed.is_none()
                && !name.trim().is_empty()
            {
                let name = name.trim().to_string();
                if instrument.create {
                    instrument.name = name.clone();
                    instrument.proposed_name = Some(name);
                } else {
                    instrument.id = None;
                    instrument.name = name.clone();
                    instrument.proposed_name = Some(name);
                    instrument.source_key = proposed_source_key.clone();
                    instrument.resolved_by = "placeholder".to_string();
                    instrument.create = true;
                    instrument.confirmed = false;
                    instrument.curves = Vec::new();
                }
            }

            if let Some(confirmed) = update.instrument_confirmed {
                instrument.confirmed = confirmed;
            }
        }
    }
    Ok(())
}

/// Edit a draft pairing plan (only `draft` status allows updates). Requires `write_metadata`.
#[utoipa::path(
    patch,
    path = "/api/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body(content = Object),
    responses(
        (status = 200, description = "Updated plan", body = crate::routes::private::data_streams::pairing_plans::PairingPlan),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status, or edited since the client read it", body = Object),
    ),
    tag = "sync"
)]
pub async fn update_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdatePairingPlanRequest>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::PairingPlan>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{}', can only edit 'draft' plans",
            plan.status
        )));
    }
    if plan.version != req.expected_version {
        return Err(stale_plan(plan.version));
    }

    let mut entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        plan.entries.0.clone();

    let catalog = crate::routes::private::sync::service::load_entity_catalog(&state.db).await?;

    if let Some(bulk) = &req.bulk {
        if bulk.action != "pair" && bulk.action != "skip" {
            return Err(AppError::BadRequest(format!(
                "Bulk action must be 'pair' or 'skip', got '{}'",
                bulk.action
            )));
        }
        crate::routes::private::sync::service::apply_bulk_action(
            &mut entries,
            &bulk.r#where,
            &bulk.action,
        );
    }

    for update in &req.updates {
        if let Some(entry) = entries.iter_mut().find(|e| e.stream_id == update.stream_id) {
            if let Some(ref action) = update.action {
                entry.action = action.clone();
            }
            if let Some(ref name) = update.project_name {
                entry.project.name = name.clone();
            }
            if let Some(ref name) = update.site_name {
                entry.site.name = name.clone();
            }
            if let Some(ref name) = update.parameter_name {
                entry.parameter.name = name.clone();
            }
            if let Some(ref units) = update.parameter_units {
                entry.parameter.units = units.clone();
            }
            if let Some(ref label) = update.parameter_label {
                entry.parameter.label = Some(label.trim().to_string()).filter(|l| !l.is_empty());
            }
            if let Some(ref declared) = update.sd_estimator {
                // An empty string clears the choice, which is how the review says "leave it
                // undeclared" rather than being unable to take a decision back.
                entry.sd_estimator = if declared.trim().is_empty() {
                    None
                } else {
                    Some(
                        crate::routes::private::readings::sd_estimator::parse(declared)?
                            .to_string(),
                    )
                };
            }
            if let Some(acknowledged) = update.acknowledged {
                entry.acknowledged = acknowledged;
            }
            crate::routes::private::sync::service::reclassify_entry(entry, &catalog);
            // A renamed entry that resolves to an existing site must not carry the stream's
            // coordinates: apply would backfill them onto that unrelated site.
            if update.site_name.is_some() && entry.site.id.is_some() {
                entry.site.latitude = None;
                entry.site.longitude = None;
                entry.site.altitude_m = None;
            }
            if entry.action == "pair"
                && (entry.site.name.trim().is_empty() || entry.parameter.name.trim().is_empty())
            {
                entry.action = "skip".to_string();
                entry
                    .warnings
                    .push(crate::routes::private::sync::service::PlanWarning::empty_name());
            }
        }
    }

    apply_site_attribute_updates(&mut entries, &req.updates);
    apply_instrument_updates(&state, &plan.source_system, &mut entries, &req.updates).await?;

    let mut intents = crate::routes::private::sync::service::plan_curve_intents(&plan)?;
    apply_curve_updates(&state.db, &entries, &mut intents, &req.curves).await?;

    let summary = serde_json::to_value(crate::routes::private::sync::service::compute_summary_pub(
        &entries,
    ))
    .unwrap_or_default();

    // The write names the version it read, so a second writer who read the same document is
    // refused rather than carrying the first's entries back over the first's decisions.
    let written = state
        .db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE pairing_plans SET entries = $1, curve_assignments = $2, summary = $3, \
             version = version + 1 WHERE id = $4 AND version = $5",
            [
                serde_json::to_value(&entries).unwrap_or_default().into(),
                serde_json::to_value(&intents).unwrap_or_default().into(),
                summary.into(),
                id.into(),
                req.expected_version.into(),
            ],
        ))
        .await?;
    if written.rows_affected() == 0 {
        return Err(stale_plan(plan_version(&state.db, id).await?));
    }

    let updated = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(Json(updated.into()))
}

/// The refusal a writer gets when the draft has moved on: the version it should reload is in the
/// detail, so the client re-reads and re-applies rather than guessing.
fn stale_plan(current_version: i32) -> AppError {
    AppError::ConflictDetail {
        message: "The plan changed since you read it; reload it and reapply your edits".to_string(),
        detail: serde_json::json!({ "current_version": current_version }),
    }
}

/// Fetch a pairing plan's version, or 404 if unknown.
async fn plan_version(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<i32> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT version FROM pairing_plans WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(row.try_get::<i32>("", "version")?)
}

#[derive(Deserialize)]
pub struct ApplyPairingPlanRequest {
    /// The version the client read. Applying a draft someone else has edited since is refused.
    expected_version: i32,
}

/// Apply a pairing plan: execute all its pairings and backfills atomically. Marks the
/// plan as `applied`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/apply",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body(content = Object),
    responses(
        (status = 200, description = "The apply job, to be watched for its counts", body = PlanJobQueued),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan already applied or reverted, or edited since the client read it", body = Object),
    ),
    tag = "sync"
)]
pub async fn apply_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ApplyPairingPlanRequest>,
) -> AppResult<Json<PlanJobQueued>> {
    // Validate synchronously for immediate feedback, then background the heavy entity-resolution +
    // readings backfill as a tracked job so the request doesn't block. The job's `detail` carries
    // the execution counts the UI used to read from the response.
    let status = plan_status(&state.db, id).await?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only apply 'draft' plans"
        )));
    }
    let version = plan_version(&state.db, id).await?;
    if version != req.expected_version {
        return Err(stale_plan(version));
    }
    // Unconfirmed instruments are the operator's decision, so the refusal belongs in the response
    // rather than in a failed job they have to go and read. `apply_plan` checks again: the job is
    // reachable on its own.
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> = plan.entries.0;
    crate::routes::private::sync::service::refuse_unconfirmed_instruments(&entries)?;

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "plan_apply",
        None,
        Some(id),
        &serde_json::json!({ "plan_id": id }),
        None,
    )
    .await?;
    Ok(Json(PlanJobQueued {
        job_id,
        status: "queued".to_string(),
    }))
}

/// Fetch a pairing plan's status, or 404 if unknown.
async fn plan_status(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<String> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT status FROM pairing_plans WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(row.try_get::<String>("", "status")?)
}

/// Revert an applied pairing plan: unpair every stream it touched, restoring the prior
/// state. Marks the plan as `reverted`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/revert",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "The revert job, to be watched for its counts", body = PlanJobQueued),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in applied status"),
    ),
    tag = "sync"
)]
pub async fn revert_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PlanJobQueued>> {
    let status = plan_status(&state.db, id).await?;
    if status != "applied" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only revert 'applied' plans"
        )));
    }
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "plan_revert",
        None,
        Some(id),
        &serde_json::json!({ "plan_id": id }),
        None,
    )
    .await?;
    Ok(Json(PlanJobQueued {
        job_id,
        status: "queued".to_string(),
    }))
}

/// Aggregate summary of unpaired streams grouped by source system. Used by the dashboard
/// to surface streams needing attention. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/unpaired-summary",
    responses(
        (status = 200, description = "Counts of unpaired streams by source_system", body = Vec<UnpairedSummaryRow>),
    ),
    tag = "sync"
)]
pub async fn unpaired_summary(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<UnpairedSummaryRow>>> {
    use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_system, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NULL) as unpaired, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NOT NULL) as paired \
         FROM data_streams GROUP BY source_system ORDER BY source_system"
                .to_owned(),
        ))
        .await?;

    let result = rows
        .iter()
        .map(|row| UnpairedSummaryRow::from_query_result(row, ""))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(result))
}

/// Get site metadata enrichment for a pairing plan: latitudes, longitudes, glacier names,
/// stream counts. Used by the pairing UI to display context. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}/site-metadata",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "One row per site the plan covers", body = Vec<PlanSiteMetadata>),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn plan_site_metadata(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<PlanSiteMetadata>>> {
    use sea_orm::{FromQueryResult, Statement};

    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        plan.entries.0.clone();

    let stream_ids: Vec<Uuid> = entries.iter().map(|e| e.stream_id).collect();
    if stream_ids.is_empty() {
        return Ok(Json(vec![]));
    }

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (metadata->'hierarchy'->>'site') \
            metadata->'hierarchy'->>'site' as site_name, \
            metadata->'coordinates'->>'latitude' as latitude, \
            metadata->'coordinates'->>'longitude' as longitude, \
            metadata->'coordinates'->>'altitude_m' as altitude_m, \
            metadata->'glacier'->>'name' as glacier_name, \
            metadata->'glacier'->>'rgi_v6' as glacier_rgi, \
            metadata->'location'->>'type' as location_type, \
            metadata->'station'->>'catchment' as catchment, \
            metadata->'station'->>'full_name' as full_name, \
            metadata->'station'->>'elevation' as elevation, \
            metadata->>'channel_id' as channel_id, \
            metadata->>'sample_interval_sec' as sample_interval_sec \
         FROM data_streams WHERE id = ANY($1) \
         ORDER BY metadata->'hierarchy'->>'site'",
            [sea_orm::Value::Array(
                sea_orm::sea_query::ArrayType::Uuid,
                Some(Box::new(
                    stream_ids
                        .iter()
                        .map(|id| sea_orm::Value::Uuid(Some(*id)))
                        .collect(),
                )),
            )],
        ))
        .await?;

    // A JSON field that was never written reads as absent, and one written as the string "null"
    // or as empty says the source had nothing there, which is the same thing.
    fn present(value: Option<String>) -> Option<String> {
        value.filter(|s| s != "null" && !s.is_empty())
    }
    fn number(value: Option<String>) -> Option<f64> {
        present(value).and_then(|s| s.parse::<f64>().ok())
    }

    let mut result: Vec<PlanSiteMetadata> = rows
        .iter()
        .map(|row| {
            let r = PlanSiteMetadataRow::from_query_result(row, "")?;
            Ok(PlanSiteMetadata {
                site_name: r.site_name.unwrap_or_default(),
                latitude: number(r.latitude),
                longitude: number(r.longitude),
                altitude_m: number(r.altitude_m),
                glacier_name: present(r.glacier_name),
                glacier_rgi: present(r.glacier_rgi),
                location_type: present(r.location_type),
                catchment: present(r.catchment),
                full_name: present(r.full_name),
                elevation: number(r.elevation),
                channel_id: present(r.channel_id),
                sample_interval_sec: present(r.sample_interval_sec)
                    .and_then(|s| s.parse::<i64>().ok()),
                devices: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;

    // Devices are counted per site, not folded into the site row: a site instrumented with two
    // loggers has two, and reporting one of them names channels that belong to the other.
    let device_rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT metadata->'hierarchy'->>'site' AS site_name, \
                    metadata->'device'->>'logger_serial' AS serial, \
                    max(metadata->'device'->>'logger_device') AS model, \
                    count(*) AS streams \
             FROM data_streams \
             WHERE id = ANY($1) AND metadata->'device'->>'logger_serial' <> '' \
             GROUP BY 1, 2 ORDER BY 1, 2",
            [sea_orm::Value::Array(
                sea_orm::sea_query::ArrayType::Uuid,
                Some(Box::new(
                    stream_ids
                        .iter()
                        .map(|id| sea_orm::Value::Uuid(Some(*id)))
                        .collect(),
                )),
            )],
        ))
        .await?;
    let mut devices_by_site: std::collections::HashMap<String, Vec<PlanSiteDevice>> =
        std::collections::HashMap::new();
    for row in &device_rows {
        let r = PlanSiteDeviceRow::from_query_result(row, "")?;
        let Some(serial) = r.serial.filter(|s| !s.is_empty()) else {
            continue;
        };
        devices_by_site
            .entry(r.site_name.unwrap_or_default())
            .or_default()
            .push(PlanSiteDevice {
                serial,
                model: r.model,
                streams: r.streams,
            });
    }

    for site in &mut result {
        site.devices = devices_by_site.remove(&site.site_name).unwrap_or_default();
    }

    Ok(Json(result))
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
    /// `stream` | `curve_label` | `manual` | `placeholder`.
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
    /// The instrument this plan will move the curve onto when applied, by `source_key`, and the
    /// name the plan proposes for it. Absent when no assignment is pending.
    #[schema(required)]
    pub pending_source_key: Option<String>,
    #[schema(required)]
    pub pending_instrument_name: Option<String>,
}

/// One device-shaped feed a plan carries, and the slot it serves.
///
/// A device is not a decision the plan takes: the feed's own `(source_system, source_key)` is the
/// identity, and pairing mints the instrument for its slot and opens that slot's deployment. One
/// instrument serves one (site, parameter), so a multi-channel logger is one group per channel
/// rather than one group carrying them all; the serial it reports is displayed, never matched on.
/// It is listed so an operator can see which instrument each feed will land on, and whether it is
/// already in the inventory.
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
    pub parameters: Vec<String>,
    pub stream_count: usize,
    pub anchor_stream_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanInstrumentsResponse {
    pub groups: Vec<PlanInstrumentGroup>,
    pub unassigned: Vec<PlanUnassignedParameter>,
    pub devices: Vec<PlanDeviceGroup>,
    pub curves: Vec<PlanCurveAssignment>,
}

/// The instrument picture of a pairing plan: every instrument the plan binds, and the parameters
/// still without one. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}/instruments",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "The plan's instruments and unassigned parameters", body = PlanInstrumentsResponse),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn plan_instruments(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PlanInstrumentsResponse>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        plan.entries.0.clone();
    // Read fresh rather than from the stored plan: an instrument created since the plan was drafted
    // is exactly the collision this reports.
    let catalog = crate::routes::private::sync::service::load_instrument_catalog(
        &state.db,
        &plan.source_system,
        &[],
    )
    .await?;

    struct Acc {
        instrument: crate::routes::private::sync::service::PlanInstrumentRef,
        anchor: Uuid,
        streams: usize,
        parameters: std::collections::BTreeSet<String>,
        sites: std::collections::BTreeSet<String>,
    }
    let mut bound: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();
    let mut unassigned: std::collections::BTreeMap<String, PlanUnassignedParameter> =
        std::collections::BTreeMap::new();

    for entry in entries.iter().filter(|e| e.action == "pair") {
        let scope = instrument_key(entry);
        // A device-shaped feed is reported as its device, whether or not it already names one.
        // Listing it as a lab decision as well would put the same instrument in two places, one of
        // which offers to change it.
        if entry.is_device {
            continue;
        }
        match &entry.instrument {
            Some(instrument) => {
                let acc = bound.entry(scope).or_insert_with(|| Acc {
                    instrument: instrument.clone(),
                    anchor: entry.stream_id,
                    streams: 0,
                    parameters: std::collections::BTreeSet::new(),
                    sites: std::collections::BTreeSet::new(),
                });
                acc.streams += 1;
                acc.parameters.insert(entry.parameter.name.clone());
                acc.sites.insert(entry.site.name.clone());
            }
            None => {
                let e =
                    unassigned
                        .entry(scope.clone())
                        .or_insert_with(|| PlanUnassignedParameter {
                            scope,
                            // The parameter alone: see `resolve_parameter_instrument`. The two
                            // producers spelled the suffix differently, which made one row read
                            // as two instruments.
                            suggested_name: entry.parameter.name.clone(),
                            name_conflict: catalog.named(&entry.parameter.name),
                            parameter: entry.parameter.name.clone(),
                            stream_count: 0,
                            site_count: 0,
                            anchor_stream_id: entry.stream_id,
                        });
                e.stream_count += 1;
            }
        }
    }
    // Site breadth per unassigned parameter, counted the same way as a bound group's.
    let mut unassigned_sites: std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for entry in entries.iter().filter(|e| e.action == "pair") {
        if entry.instrument.is_none() && !entry.is_device {
            unassigned_sites
                .entry(instrument_scope(entry))
                .or_default()
                .insert(entry.site.name.clone());
        }
    }
    for (scope, sites) in unassigned_sites {
        if let Some(u) = unassigned.get_mut(&scope) {
            u.site_count = sites.len();
        }
    }

    let mut groups: Vec<PlanInstrumentGroup> = bound
        .into_iter()
        .map(|(scope, acc)| PlanInstrumentGroup {
            scope: Some(scope),
            instrument_id: acc.instrument.id,
            name: acc.instrument.name,
            source_key: acc.instrument.source_key,
            resolved_by: acc.instrument.resolved_by,
            create: acc.instrument.create,
            confirmed: acc.instrument.confirmed,
            stamps_readings: acc.instrument.stamps_readings,
            curve_column: acc.instrument.curve_column,
            stream_count: acc.streams,
            parameters: acc.parameters.into_iter().collect(),
            site_count: acc.sites.len(),
            anchor_stream_id: Some(acc.anchor),
            proposed_name: acc.instrument.proposed_name,
            name_conflict: acc.instrument.name_conflict,
            curves: acc.instrument.curves,
        })
        .collect();

    // Anything still asking first, then by breadth: the decisions come before the inventory.
    groups.sort_by(|a, b| {
        (
            a.confirmed,
            std::cmp::Reverse(a.stream_count),
            a.name.clone(),
        )
            .cmp(&(
                b.confirmed,
                std::cmp::Reverse(b.stream_count),
                b.name.clone(),
            ))
    });

    // Every curve the source replicated, with the instrument it sits on and how much data it has
    // corrected. Independent of what this plan binds: a curve on the wrong instrument is a thing to
    // fix whether or not a stream in this plan names it.
    let instrument_names: std::collections::HashMap<Uuid, String> = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(plan.source_system.clone()))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|s| {
            let name = s
                .name
                .clone()
                .or_else(|| s.serial_number.clone())
                .unwrap_or_else(|| s.id.to_string());
            (s.id, name)
        })
        .collect();
    let mut usage: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();
    for row in state
        .db
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT standard_curve_id AS id, COUNT(*) AS n FROM readings
             WHERE standard_curve_id IS NOT NULL GROUP BY standard_curve_id"
                .to_string(),
        ))
        .await?
    {
        usage.insert(row.try_get::<Uuid>("", "id")?, row.try_get::<i64>("", "n")?);
    }
    let intents = crate::routes::private::sync::service::plan_curve_intents(&plan)?;
    let proposed_names: std::collections::HashMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create)
        .map(|i| (i.source_key.as_str(), i.name.as_str()))
        .collect();
    let mut curves: Vec<PlanCurveAssignment> =
        crate::routes::private::sensors::standard_curves::Entity::find()
            .filter(
                crate::routes::private::sensors::standard_curves::Column::SourceSystem
                    .eq(plan.source_system.clone()),
            )
            .all(&state.db)
            .await?
            .into_iter()
            .map(|c| {
                let pending = intents.iter().find(|i| i.curve_id == c.id);
                PlanCurveAssignment {
                    instrument_name: instrument_names
                        .get(&c.sensor_id)
                        .cloned()
                        .unwrap_or_else(|| c.sensor_id.to_string()),
                    reading_count: usage.get(&c.id).copied().unwrap_or(0),
                    pending_source_key: pending.map(|i| i.instrument_source_key.clone()),
                    pending_instrument_name: pending.and_then(|i| {
                        proposed_names
                            .get(i.instrument_source_key.as_str())
                            .map(|n| (*n).to_string())
                    }),
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                    r_squared: c.r_squared,
                    source_key: c.source_key,
                    sensor_id: c.sensor_id,
                }
            })
            .collect();
    curves.sort_by(|a, b| {
        a.instrument_name
            .cmp(&b.instrument_name)
            .then_with(|| a.name.cmp(&b.name))
    });

    // The devices the plan's feeds name, one row per channel: a channel is an instrument, so the
    // key is the feed's own `source_key`. Grouping by serial would merge a multi-channel logger's
    // parameters into one row and then fail to resolve it, since a source-registered instrument
    // carries no serial.
    let mut device_acc: std::collections::BTreeMap<(String, String), PlanDeviceGroup> =
        std::collections::BTreeMap::new();
    for entry in entries.iter().filter(|e| e.action == "pair") {
        if !entry.is_device {
            continue;
        }
        let group = device_acc
            .entry((entry.site.name.clone(), entry.source_key.clone()))
            .or_insert_with(|| PlanDeviceGroup {
                site: entry.site.name.clone(),
                serial: entry.device_serial.clone().unwrap_or_default(),
                model: entry.device_model.clone(),
                instrument_id: None,
                instrument_name: None,
                parameters: Vec::new(),
                stream_count: 0,
                anchor_stream_id: entry.stream_id,
            });
        group.stream_count += 1;
        if !group.parameters.contains(&entry.parameter.name) {
            group.parameters.push(entry.parameter.name.clone());
        }
    }
    if !device_acc.is_empty() {
        let keys: Vec<String> = device_acc.keys().map(|(_, k)| k.clone()).collect();
        for sensor in sensors::Entity::find()
            .filter(sensors::Column::SourceSystem.eq(plan.source_system.as_str()))
            .filter(sensors::Column::SourceKey.is_in(keys))
            .all(&state.db)
            .await?
        {
            let Some(source_key) = sensor.source_key.clone() else {
                continue;
            };
            for group in device_acc
                .iter_mut()
                .filter(|((_, k), _)| *k == source_key)
                .map(|(_, g)| g)
            {
                group.instrument_id = Some(sensor.id);
                group.instrument_name = sensor.name.clone().or_else(|| sensor.source_key.clone());
            }
        }
    }

    Ok(Json(PlanInstrumentsResponse {
        groups,
        unassigned: unassigned.into_values().collect(),
        devices: device_acc.into_values().collect(),
        curves,
    }))
}

#[cfg(test)]
mod tests {
    use super::instrument_key;
    use serde_json::json;

    fn entry(
        parameter: &str,
        instrument_source_key: Option<&str>,
    ) -> crate::routes::private::sync::service::PlanEntry {
        serde_json::from_value(json!({
            "stream_id": uuid::Uuid::new_v4(),
            "source_key": format!("FP1:{parameter}"),
            "source_name": null,
            "action": "pair",
            "confidence": "none",
            "project": { "id": null, "name": "METALP", "create": true },
            "site": { "id": null, "name": "FP1", "create": true, "latitude": null, "longitude": null, "altitude_m": null },
            "parameter": { "id": null, "name": parameter, "create": false, "units": "-" },
            "instrument": instrument_source_key.map(|key| json!({
                "id": null,
                "name": "chla acid (metalp)",
                "source_key": key,
                "resolved_by": "stream",
                "create": false,
                "stamps_readings": false,
            })),
        }))
        .expect("plan entry fixture deserializes")
    }

    /// One lab instrument corrects several source columns, so the review reports it once and an
    /// edit on it moves every column it serves.
    #[test]
    fn columns_sharing_an_instrument_share_one_key() {
        let ugl = entry("Chla_acid_ugL", Some("metalp:chla acid"));
        let ugm2 = entry("Chla_acid_ugm2", Some("metalp:chla acid"));
        assert_eq!(instrument_key(&ugl), instrument_key(&ugm2));
    }

    #[test]
    fn different_instruments_stay_apart() {
        let acid = entry("Chla_acid_ugL", Some("metalp:chla acid"));
        let noacid = entry("Chla_noacid_ugL", Some("metalp:chla noacid"));
        assert_ne!(instrument_key(&acid), instrument_key(&noacid));
    }

    /// With no instrument to key on, the parameter is what the decision covers, so every station
    /// reporting it is still one row.
    #[test]
    fn an_entry_with_no_instrument_keys_on_its_parameter() {
        let a = entry("DOC_ppb", None);
        let b = entry("DOC_ppb", None);
        assert_eq!(instrument_key(&a), instrument_key(&b));
        assert_ne!(instrument_key(&a), instrument_key(&entry("NUT_P", None)));
    }
}
