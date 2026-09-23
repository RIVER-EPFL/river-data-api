use std::collections::HashMap;

use crudcrate::ApiError;
use crudcrate::CRUDOperations;
use crudcrate::CRUDResource;
use sea_orm::ActiveModelTrait;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
use sea_orm::PaginatorTrait;
use sea_orm::QueryFilter;
use sea_orm::QuerySelect;
use sea_orm::Set;
use sea_orm::Statement;
use sea_orm::TransactionTrait;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::Query;
use sea_orm::sea_query::SimpleExpr;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::models::CatalogParameter;
use super::models::Column;
use super::models::Entity;
use super::models::GroupMember;
use super::models::Model as SiteParameterModel;
use super::models::SiteParameter;
use super::models::SlotDescriptor;
use crate::common::aggregates;
use crate::common::aggregates::Window;
use crate::common::bulk_write;
use crate::common::bulk_write::TouchedRange;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::alarms::models as alarm_thresholds;
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::models::MoveScope;
use crate::routes::private::data_streams::service::move_slot_rows;
use crate::routes::private::data_streams::service::slot_move_collisions;
use crate::routes::private::derived_parameters::models::definition as calculation_formulas;
use crate::routes::private::parameters;
use crate::routes::private::readings;
use crate::routes::private::readings::samples;
use crate::routes::private::readings::status_events::models as status_events;
use crate::routes::private::reprocessing_jobs::models::job as reprocessing_jobs;
use crate::routes::private::sensors::service::require_measuring_instrument;
use crate::routes::private::site_parameters;

/// How many readings the slot carries. Zero is a slot paired by mistake and never measured, which
/// deletes; anything else is a history the delete would unattribute.
async fn readings_held<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<u64, sea_orm::DbErr> {
    let Some(slot) = Entity::find_by_id(id).one(db).await? else {
        return Ok(0);
    };
    readings::Entity::find()
        .filter(readings::Column::SiteId.eq(slot.site_id))
        .filter(readings::Column::ParameterId.eq(slot.parameter_id))
        .count(db)
        .await
}

/// Why a delete is refused, or None where the slot has nothing to lose. Deleting a measured slot
/// nulls `site_id` and `parameter_id` on every reading it carried, which is what the Active toggle
/// exists to avoid (Q160).
fn refuse_delete_of_measured_slot(held: u64) -> Option<String> {
    (held > 0).then(|| {
        format!(
            "This parameter holds {held} readings at this site. Deleting it would leave them \
             attributed to no site and no parameter. Untick Active to retire the slot and keep \
             its history."
        )
    })
}

pub struct SiteParameterOperations;

impl CRUDOperations for SiteParameterOperations {
    type Resource = SiteParameter;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it (B185).
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// Retire everything the slot owns before it goes away: unattribute its readings and status
    /// events, delete the samples nothing references any more, release the streams that fed it,
    /// and rebuild the rollups. `retire_slot` also does the `data_streams` NULLing the foreign key
    /// requires, so the delete CrudCrate performs next succeeds.
    ///
    /// A slot that measured something is retired with its Active flag instead, so nothing here
    /// unattributes a history (Q160).
    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        let held = readings_held(db, id).await.map_err(ApiError::database)?;
        if let Some(refusal) = refuse_delete_of_measured_slot(held) {
            return Err(ApiError::bad_request(refusal));
        }
        crate::routes::private::data_streams::flows::retire_slot(
            db,
            crate::routes::private::data_streams::models::SlotScope::SiteParameter(id),
        )
        .await
        .map_err(|e| ApiError::internal(e.to_string(), None))?;
        Ok(())
    }

    /// A slot names the instrument that measures it, so the row it names has to be one something
    /// was measured on. A bookkeeping instrument stands in for a slot that has declared nothing,
    /// and declaring it would record the absence of an answer as an answer.
    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: &<SiteParameter as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if let Some(sensor_id) = data.instrument_sensor_id {
            require_measuring_instrument(db, sensor_id, "the instrument that measures a slot")
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }

    /// The twin of `before_create`: the declaration is patchable, and the picker that sets it is
    /// the surface an operator reaches it through.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
        data: &<SiteParameter as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(sensor_id)) = data.instrument_sensor_id {
            require_measuring_instrument(db, sensor_id, "the instrument that measures a slot")
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }

    /// The insert and the name backfill, on the transaction the orchestrator opened.
    ///
    /// `name` is the slot's fulltext and sort key, so a row must never be visible without one, and
    /// a hook cannot supply it: the create model is immutable in `before_create` and `after_create`
    /// runs after the insert. Both statements go here, and the write they make is one because the
    /// lifecycle is one transaction; opening another here would only nest a savepoint inside it.
    async fn perform_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: <SiteParameter as CRUDResource>::CreateModel,
    ) -> Result<SiteParameter, ApiError> {
        let active: <SiteParameter as CRUDResource>::ActiveModelType = data.into();
        let model = active.insert(db).await.map_err(ApiError::database)?;
        let mut entity = SiteParameter::from(model);

        // A human-readable name from the parameter when the client omitted it.
        if entity.name.trim().is_empty()
            && let Some(parameter) =
                crate::routes::private::parameters::Entity::find_by_id(entity.parameter_id)
                    .one(db)
                    .await
                    .map_err(ApiError::database)?
        {
            Entity::update_many()
                .col_expr(Column::Name, Expr::value(parameter.name.clone()))
                .filter(Column::Id.eq(entity.id))
                .exec(db)
                .await
                .map_err(ApiError::database)?;
            entity.name = parameter.name;
        }

        Ok(entity)
    }

    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        // `is_active` and `is_public` defaults live in the model's `on_create`, so an omitted
        // field is already resolved by the time this hook runs and an explicit null stays null.
        // NOTE: alarm thresholds are intentionally NOT auto-created from parameter defaults.
        // Alarm evaluation already falls back to the parameter's `default_*` columns when no
        // `alarm_thresholds` row exists, so a default-valued row is redundant, and worse, a
        // site-specific copy would silently shadow a global threshold an operator set. A
        // site-specific row is created only when a user explicitly overrides via the editor.

        // Backfill derived values for the readings already present at this site when a
        // derived site_parameter is assigned. Enqueued as a durable `derived_assignment` job on
        // the claim-based worker pool. The guard skips the enqueue only when this calculation's
        // own assignment or recompute is already in flight. An import or pairing backfill at any
        // site is not an overlap: rows it lands after this point reach the new slot through its
        // own derived cascade, and rows already present are this job's to compute.
        //
        // This stays a query rather than becoming the enqueue's `dedupe_key`: the key is released
        // when the worker claims the row, so it coalesces only while a job is queued, and the
        // skip has to hold while one is running too.
        if entity.entry_mode == "tool"
            && let Some(calculation_id) = calculation_producing(db, entity.parameter_id).await?
        {
            let site_id = entity.site_id;

            let in_flight = reprocessing_jobs::Entity::find()
                .filter(
                    reprocessing_jobs::Column::Status
                        .is_in(["queued", "pending", "running", "retrying"]),
                )
                .filter(
                    reprocessing_jobs::Column::TriggerType
                        .is_in(["derived_assignment", "derived_recompute"]),
                )
                .filter(reprocessing_jobs::Column::TriggerId.eq(calculation_id))
                .select_only()
                .column(reprocessing_jobs::Column::Id)
                .into_tuple::<Uuid>()
                .one(db)
                .await
                .map_err(ApiError::database)?;

            if in_flight.is_some() {
                tracing::info!(
                    %calculation_id, %site_id,
                    "Skipping derived assignment backfill: this calculation's backfill is already in flight"
                );
            } else {
                crate::routes::private::reprocessing_jobs::service::enqueue(
                    db,
                    "derived_assignment",
                    None,
                    Some(calculation_id),
                    &serde_json::json!({ "calculation_id": calculation_id, "site_id": site_id }),
                    None,
                )
                .await
                .map_err(ApiError::database)?;
            }
        }

        Ok(())
    }

    // Breach evaluation only considers slots whose site_parameter is active, so toggling
    // `is_active` (or removing the slot) can open or resolve alarms with no new reading.
    // Reconcile immediately instead of waiting for the backstop sweep.
    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::flows::reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::flows::reconcile_all_from_hook(db).await;
        Ok(())
    }
}

/// The calculation that produces a parameter, if one does. A formula names the parameter it
/// outputs, and an output has exactly one producer (`idx_derived_definitions_output_parameter`),
/// so the slot needs no reference of its own. `None` for a formula that belongs to no
/// calculation, which computes nothing on its own.
async fn calculation_producing<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    Ok(calculation_formulas::Entity::find()
        .filter(calculation_formulas::Column::OutputParameterId.eq(parameter_id))
        .select_only()
        .column(calculation_formulas::Column::ToolScriptId)
        .into_tuple::<Option<Uuid>>()
        .one(db)
        .await
        .map_err(ApiError::database)?
        .flatten())
}

/// The cadence a site declares for one catalog parameter, or `None` where it holds no slot for
/// it. `high` is the stream arm, `low` the visit arm.
pub async fn slot_cadence<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<String>> {
    Ok(Entity::find()
        .filter(Column::SiteId.eq(site_id))
        .filter(Column::ParameterId.eq(parameter_id))
        .select_only()
        .column(Column::Cadence)
        .into_tuple::<String>()
        .one(db)
        .await?)
}

/// The slot a publishing run mints at a site that declared the calculation's inputs but not its
/// output (Q193). It carries `needs_review` until a manager confirms it from the site's Parameters
/// tab, it computes rather than being typed into, and it is not public. `None` when the site
/// already holds the slot.
pub async fn mint_tool_slot<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<Uuid>> {
    use crate::routes::private::parameters::models as parameters;
    let held = Entity::find()
        .filter(Column::SiteId.eq(site_id))
        .filter(Column::ParameterId.eq(parameter_id))
        .select_only()
        .column(Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await?;
    if held.is_some() {
        return Ok(None);
    }
    let parameter = parameters::Entity::find_by_id(parameter_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Parameter {parameter_id} not found")))?;
    let slot = super::models::ActiveModel {
        id: Set(Uuid::new_v4()),
        instrument_sensor_id: Set(None),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        name: Set(parameter.name),
        sensor_type: Set(String::new()),
        decimal_places: Set(None),
        sample_interval_sec: Set(None),
        is_active: Set(Some(true)),
        is_public: Set(Some(false)),
        needs_review: Set(true),
        entry_mode: Set("tool".to_string()),
        // The run that mints it is a visit's, so the slot it needs is the visit arm's.
        cadence: Set("low".to_string()),
        variable_mappings: Set(None),
        created_at: Set(Some(chrono::Utc::now())),
        updated_at: Set(Some(chrono::Utc::now())),
        discovered_at: Set(Some(chrono::Utc::now())),
    };
    Ok(Some(slot.insert(db).await?.id))
}

/// Load the catalog rows for a set of parameter ids, keyed by parameter id.
pub async fn catalog_map(
    db: &DatabaseConnection,
    parameter_ids: impl IntoIterator<Item = Uuid>,
) -> AppResult<HashMap<Uuid, CatalogParameter>> {
    let ids: Vec<Uuid> = parameter_ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(ids))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            (
                p.id,
                CatalogParameter {
                    code: p.code,
                    name: p.name,
                    default_units: p.default_units,
                },
            )
        })
        .collect())
}

impl SlotDescriptor {
    /// Resolve one slot against its catalog row (`None` when the parameter is missing from the
    /// catalog, which leaves the code empty and the units unresolved).
    pub fn resolve(slot: &SiteParameterModel, catalog: Option<&CatalogParameter>) -> Self {
        let catalog_name = catalog.map(|c| c.name.clone());
        let units = catalog
            .map(|c| c.default_units.clone())
            .filter(|u| !u.is_empty());
        Self {
            id: slot.id,
            parameter_id: slot.parameter_id,
            code: catalog.map(|c| c.code.clone()).unwrap_or_default(),
            name: catalog_name.clone().unwrap_or_else(|| slot.name.clone()),
            catalog_name,
            slot_name: slot.name.clone(),
            sensor_type: if slot.sensor_type.is_empty() {
                slot.name.clone()
            } else {
                slot.sensor_type.clone()
            },
            units,
            decimal_places: slot.decimal_places,
        }
    }

    /// Resolve a batch of slots against one catalog map, preserving input order.
    pub fn resolve_all(
        slots: &[SiteParameterModel],
        catalog: &HashMap<Uuid, CatalogParameter>,
    ) -> Vec<Self> {
        slots
            .iter()
            .map(|s| Self::resolve(s, catalog.get(&s.parameter_id)))
            .collect()
    }
}

/// The group's members and the site's slots, decided in one place so the route and its test agree
/// on what "already carried" means: a slot exists for a member when the site holds a row for that
/// catalog parameter, whatever the group says about it.
#[must_use]
pub fn partition_members(
    members: &[GroupMember],
    held: &std::collections::HashSet<Uuid>,
) -> (Vec<GroupMember>, Vec<GroupMember>) {
    let mut create = Vec::new();
    let mut existing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for member in members {
        if held.contains(&member.0) {
            existing.push(member.clone());
        } else if seen.insert(member.0) {
            create.push(member.clone());
        }
    }
    (create, existing)
}

/// A calculation's read inputs and its outputs, partitioned against the slots the site already
/// declares. Applying is refused while `inputs_missing` is non-empty, so the same partition both
/// decides the refusal and lists what the apply would mint.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CalculationPartition {
    pub inputs_present: Vec<CalculationMember>,
    pub inputs_missing: Vec<CalculationMember>,
    pub outputs_existing: Vec<CalculationMember>,
    pub outputs_to_create: Vec<CalculationMember>,
}

/// One parameter a calculation reads or writes: the catalog id and the code a refusal names it by.
pub type CalculationMember = (Uuid, String);

/// What applying a calculation at a site would do. An input the site declares is present, one it
/// does not is missing; an output it declares is left alone, one it does not is minted. A
/// parameter named twice by the manifest is partitioned once.
#[must_use]
pub fn partition_calculation(
    read_inputs: &[CalculationMember],
    outputs: &[CalculationMember],
    held: &std::collections::HashSet<Uuid>,
) -> CalculationPartition {
    let mut partition = CalculationPartition::default();
    let mut seen = std::collections::HashSet::new();
    for member in read_inputs {
        if !seen.insert(member.0) {
            continue;
        }
        if held.contains(&member.0) {
            partition.inputs_present.push(member.clone());
        } else {
            partition.inputs_missing.push(member.clone());
        }
    }
    let mut seen = std::collections::HashSet::new();
    for member in outputs {
        if !seen.insert(member.0) {
            continue;
        }
        if held.contains(&member.0) {
            partition.outputs_existing.push(member.clone());
        } else {
            partition.outputs_to_create.push(member.clone());
        }
    }
    partition
}

/// The cadence an applied output slot takes, from the cadences the site declares for the
/// calculation's read inputs (Q234): the stream arm only where every input is on the stream arm
/// there, the visit arm otherwise. A calculation that reads nothing is a visit's.
#[must_use]
pub fn applied_cadence(input_cadences: &[Option<String>]) -> &'static str {
    let all_high = !input_cadences.is_empty()
        && input_cadences
            .iter()
            .all(|cadence| cadence.as_deref() == Some("high"));
    if all_high { "high" } else { "low" }
}

/// The inputs whose declaration decides the arm a calculation's outputs run on (Q230): the ones
/// read at the instant computed. A source held from the last visit says nothing about the arm,
/// because it stands between visits whatever the stream does; counting it would put a set that
/// mixes a stream reading and a lab value wholly on the visit arm, which is the case the hold
/// exists for.
#[must_use]
pub fn cadence_deciding<'a>(
    inputs_present: &'a [(Uuid, String)],
    held_codes: &[String],
) -> Vec<&'a (Uuid, String)> {
    inputs_present
        .iter()
        .filter(|(_, code)| {
            !held_codes
                .iter()
                .any(|held| held.eq_ignore_ascii_case(code))
        })
        .collect()
}

/// The `samples` rows a retag names: those whose slot is named by id, or reached through the
/// pairing of a named stream. Written against the unaliased `samples` table, so it composes into
/// `samples::Entity::find()` and `update_many()` alike.
#[must_use]
pub fn slot_scope(site_parameter_ids: &[Uuid], stream_ids: &[Uuid]) -> SimpleExpr {
    let sp = Alias::new("sp");
    let ds = Alias::new("ds");
    let mut streams = Query::select();
    streams
        .expr(Expr::val(1))
        .from_as(data_streams::Entity, ds.clone())
        .and_where(Expr::col((ds.clone(), data_streams::Column::Id)).is_in(stream_ids.to_vec()))
        .and_where(
            Expr::col((ds, data_streams::Column::SiteParameterId)).equals((sp.clone(), Column::Id)),
        );

    let mut slots = Query::select();
    slots
        .expr(Expr::val(1))
        .from_as(Entity, sp.clone())
        .and_where(
            Expr::col((sp.clone(), Column::SiteId))
                .equals((samples::Entity, samples::Column::SiteId)),
        )
        .and_where(
            Expr::col((sp.clone(), Column::ParameterId))
                .equals((samples::Entity, samples::Column::ParameterId)),
        )
        .and_where(
            Expr::col((sp, Column::Id))
                .is_in(site_parameter_ids.to_vec())
                .or(Expr::exists(streams)),
        );
    Expr::exists(slots)
}

// --- Absorbing one slot into another ---

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MergeSiteParametersRequest {
    pub source_site_parameter_id: Uuid,
    pub target_site_parameter_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MergeSiteParametersResponse {
    pub merged_readings: u64,
    pub merged_status_events: u64,
    pub streams_updated: u64,
    pub deployments_moved: u64,
    pub source_deleted: bool,
}

/// Reject a move whose slot-keyed rows would collide on the survivor's unique constraint.
///
/// Merging two collection groups would rewrite `samples.mean`/`sd`/`n` over a union of samples
/// taken separately, so the merge is refused and the operator resolves it.
pub(crate) async fn refuse_on_collision<C: ConnectionTrait>(
    conn: &C,
    scope: MoveScope,
    source_param: Uuid,
    target_param: Uuid,
) -> AppResult<()> {
    let collisions = slot_move_collisions(conn, scope, source_param, target_param).await?;
    if collisions.is_empty() {
        return Ok(());
    }
    Err(AppError::Conflict(format!(
        "Source and target already hold a row at the same instant, so the merge would have to \
         combine two separately collected groups: {}",
        collisions.join(", ")
    )))
}

/// Merge two site_parameters: absorb source into target.
///
/// Moves every slot-keyed table's rows (readings, status events, samples, annotations) plus the
/// data streams onto the target, then deletes the source. One transaction with the decompression
/// cap lifted, so it applies whole or not at all even when the readings sit in compressed chunks;
/// the rollup refresh follows the commit, since `refresh_continuous_aggregate` cannot run inside a
/// transaction block.
/// The row as it stands, for the trail to keep after the merge deletes it.
pub(crate) async fn row_snapshot<C: ConnectionTrait>(
    txn: &C,
    table: &str,
    id: Uuid,
) -> AppResult<serde_json::Value> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT to_jsonb(t) AS row FROM {table} t WHERE t.id = $1"),
            vec![id.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    Ok(row
        .map(|r| r.try_get::<serde_json::Value>("", "row"))
        .transpose()
        .map_err(AppError::Database)?
        .unwrap_or(serde_json::Value::Null))
}

/// One entry per merge, in the merge's own transaction, so a rolled-back merge leaves none.
///
/// The per-row triggers see the survivor unchanged and the source deleted, and say nothing about
/// the two being one action or about what moved between them. A merge is one operator decision over
/// many rows and the trail has to hold it as one, which is what makes it rectifiable (Q87).
pub(crate) async fn record_merge<C: ConnectionTrait>(
    txn: &C,
    subject: &str,
    target_id: Uuid,
    source: serde_json::Value,
    counts: serde_json::Value,
    actor: &str,
) -> AppResult<()> {
    let target = row_snapshot(txn, &format!("{subject}s"), target_id).await?;
    let mut absorbed = serde_json::Map::new();
    absorbed.insert("source".to_string(), source);
    absorbed.insert("counts".to_string(), counts);
    crate::routes::private::change_audit::service::record(
        txn,
        format!("{subject}:{target_id}"),
        &format!("{subject}_merge"),
        Some(actor.to_string()),
        Some(serde_json::Value::Object(absorbed)),
        Some(target.clone()),
    )
    .await?;
    Ok(())
}

pub async fn merge_site_parameters(
    db: &DatabaseConnection,
    req: &MergeSiteParametersRequest,
    actor: &str,
    origin: crate::routes::private::readings::models::Origin,
) -> AppResult<MergeSiteParametersResponse> {
    let source_id = req.source_site_parameter_id;
    let target_id = req.target_site_parameter_id;

    if source_id == target_id {
        return Err(AppError::BadRequest(
            "Source and target must be different".to_string(),
        ));
    }

    let (response, touched, touched_events) = bulk_write::guarded(db, async |txn| {
        let (source_site_id, source_param_id, target_site_id, target_param_id) =
            validate_merge_candidates(txn, source_id, target_id).await?;
        if source_site_id != target_site_id {
            return Err(AppError::BadRequest(
                "Source and target must belong to the same site".to_string(),
            ));
        }

        let scope = MoveScope::Site(source_site_id);
        refuse_on_collision(txn, scope, source_param_id, target_param_id).await?;
        let moved =
            move_slot_rows(txn, scope, source_param_id, target_param_id, actor, origin).await?;
        let streams_updated = update_data_streams(txn, source_id, target_id).await?;
        let source_row = row_snapshot(txn, "site_parameters", source_id).await?;
        delete_source(
            txn,
            source_id,
            source_site_id,
            source_param_id,
            target_param_id,
        )
        .await?;
        record_merge(
            txn,
            "site_parameter",
            target_id,
            source_row,
            serde_json::json!({
                "merged_readings": moved.readings,
                "merged_status_events": moved.status_events,
                "streams_updated": streams_updated,
            }),
            actor,
        )
        .await?;

        Ok((
            MergeSiteParametersResponse {
                merged_readings: moved.readings,
                merged_status_events: moved.status_events,
                streams_updated,
                // Deployments link a sensor to a site, and both slots are on the same site.
                deployments_moved: 0,
                source_deleted: true,
            },
            moved.touched,
            moved.touched_events,
        ))
    })
    .await?;

    refresh_moved_rollups(db, touched).await?;
    crate::routes::private::collection_events::flows::enqueue_for(
        db,
        &touched_events,
        actor,
        crate::routes::private::collection_events::flows::Writer::Person,
    )
    .await?;
    Ok(response)
}

/// The rollups group by `parameter_id`, so recomputing the buckets the moved readings occupy
/// rebuilds both the survivor's series and the absorbed one's in the same pass.
pub(crate) async fn refresh_moved_rollups(
    db: &DatabaseConnection,
    touched: TouchedRange,
) -> AppResult<()> {
    if let Some(window) = Window::touched(&touched) {
        aggregates::refresh(db, window).await?;
    }
    Ok(())
}

async fn validate_merge_candidates<C: ConnectionTrait>(
    db: &C,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<(Uuid, Uuid, Uuid, Uuid)> {
    let rows = site_parameters::Entity::find()
        .filter(site_parameters::Column::Id.is_in([source_id, target_id]))
        .all(db)
        .await
        .map_err(AppError::Database)?;

    let mut source: Option<(Uuid, Uuid)> = None;
    let mut target: Option<(Uuid, Uuid)> = None;

    for row in &rows {
        if row.id == source_id {
            source = Some((row.site_id, row.parameter_id));
        }
        if row.id == target_id {
            target = Some((row.site_id, row.parameter_id));
        }
    }

    let (source_site_id, source_param_id) =
        source.ok_or_else(|| AppError::NotFound("Source site_parameter not found".to_string()))?;
    let (target_site_id, target_param_id) =
        target.ok_or_else(|| AppError::NotFound("Target site_parameter not found".to_string()))?;

    Ok((
        source_site_id,
        source_param_id,
        target_site_id,
        target_param_id,
    ))
}

pub(crate) async fn update_data_streams<C: ConnectionTrait>(
    db: &C,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<u64> {
    let sql = "UPDATE data_streams SET site_parameter_id = $1 WHERE site_parameter_id = $2";
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            vec![target_id.into(), source_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    Ok(result.rows_affected())
}

pub(crate) async fn delete_source<C: ConnectionTrait>(
    db: &C,
    source_id: Uuid,
    site_id: Uuid,
    source_param_id: Uuid,
    target_param_id: Uuid,
) -> AppResult<()> {
    // Backstop: the move above re-points every slot-keyed row, so these match nothing unless a row
    // was written between the two statements. Nothing deletes a reading: a straggler is re-pointed
    // like the rest, so the site_parameter can go without leaving a row attributed to it.
    readings::Entity::update_many()
        .col_expr(readings::Column::ParameterId, Expr::value(target_param_id))
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.eq(source_param_id))
        .exec(db)
        .await
        .map_err(AppError::Database)?;

    status_events::Entity::update_many()
        .col_expr(
            status_events::Column::ParameterId,
            Expr::value(target_param_id),
        )
        .filter(status_events::Column::SiteId.eq(site_id))
        .filter(status_events::Column::ParameterId.eq(source_param_id))
        .exec(db)
        .await
        .map_err(AppError::Database)?;

    alarm_thresholds::Entity::delete_many()
        .filter(alarm_thresholds::Column::ParameterId.eq(source_param_id))
        .filter(alarm_thresholds::Column::SiteId.eq(site_id))
        .exec(db)
        .await
        .map_err(AppError::Database)?;

    site_parameters::Entity::delete_by_id(source_id)
        .exec(db)
        .await
        .map_err(AppError::Database)?;

    Ok(())
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
