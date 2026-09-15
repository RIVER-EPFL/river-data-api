use crudcrate::ApiError;
use crudcrate::CRUDOperations;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
use sea_orm::PaginatorTrait;
use sea_orm::QueryFilter;
use sea_orm::QuerySelect;
use sea_orm::TransactionTrait;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::models::Parameter;
use crate::common::bulk_write;
use crate::common::bulk_write::TouchedRange;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::alarms::flows::reconcile_all_from_hook;
use crate::routes::private::alarms::models as alarm_thresholds;
use crate::routes::private::data_streams::models::MoveScope;
use crate::routes::private::data_streams::models::SlotMove;
use crate::routes::private::data_streams::service::move_slot_rows;
use crate::routes::private::derived_parameters::models::definition;
use crate::routes::private::derived_parameters::models::source;
use crate::routes::private::parameters;
use crate::routes::private::sensor_calibrations::models as sensor_calibrations;
use crate::routes::private::sensor_deployments::models as sensor_deployments;
use crate::routes::private::site_parameters;
use crate::routes::private::site_parameters::service::delete_source;
use crate::routes::private::site_parameters::service::record_merge;
use crate::routes::private::site_parameters::service::refresh_moved_rollups;
use crate::routes::private::site_parameters::service::refuse_on_collision;
use crate::routes::private::site_parameters::service::row_snapshot;
use crate::routes::private::site_parameters::service::update_data_streams;

/// A parameter's own bounds are its global `alarm_thresholds` row, so nothing on the parameter
/// itself is read by alarm evaluation and an edit cannot change breach state. Removing the
/// parameter can: its slots and its thresholds go with it, so the delete hooks reconcile
/// immediately instead of waiting for the backstop sweep.
pub struct ParameterOperations;

impl CRUDOperations for ParameterOperations {
    type Resource = Parameter;

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

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }
}

// --- Absorbing one catalog parameter into another ---

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MergeParametersRequest {
    pub source_parameter_id: Uuid,
    pub target_parameter_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MergeParametersResponse {
    pub sites_merged: u64,
    pub sites_reassigned: u64,
    pub readings_moved: u64,
    pub streams_updated: u64,
    pub source_deleted: bool,
}

/// Merge two catalog parameters: absorb source into target at every site, then delete the source
/// row. Same guarantees as [`merge_site_parameters`]: one guarded transaction, rollups refreshed
/// after the commit.
/// Refuse a merge that would make a derived definition read what it produces.
///
/// The merge re-points `derived_parameter_sources.parameter_id` from the source onto the target
/// and leaves `output_parameter_id` where it is, so a definition producing the target and reading
/// the source comes out reading itself. An edge is created by a write that never passes through
/// the authoring validator, which is what Q96's decision names; the check is the same one, asked of
/// the graph the merge would leave.
async fn refuse_derived_cycle<C: sea_orm::ConnectionTrait>(
    conn: &C,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<()> {
    let formulas = definition::Entity::find()
        .filter(definition::Column::OutputParameterId.is_not_null())
        .all(conn)
        .await?;
    let sources: Vec<(Uuid, Option<Uuid>)> = source::Entity::find()
        .select_only()
        .columns([
            source::Column::DerivedDefinitionId,
            source::Column::ParameterId,
        ])
        .into_tuple()
        .all(conn)
        .await?;
    // One row per formula, plus one per source it reads: the shape a left join returns.
    let rows: Vec<(String, Uuid, Option<Uuid>)> = formulas
        .into_iter()
        .filter_map(|f| f.output_parameter_id.map(|out| (f.id, f.code, out)))
        .flat_map(|(id, code, out)| {
            let read: Vec<Option<Uuid>> = sources
                .iter()
                .filter(|(definition_id, _)| *definition_id == id)
                .map(|(_, parameter_id)| *parameter_id)
                .collect();
            if read.is_empty() {
                vec![(code, out, None)]
            } else {
                read.into_iter().map(|p| (code.clone(), out, p)).collect()
            }
        })
        .collect();

    // Parameters as indices, with the merge applied: everything the source named is the target.
    let resolve = |id: Uuid| if id == source_id { target_id } else { id };
    let mut index: std::collections::HashMap<Uuid, usize> = std::collections::HashMap::new();
    let mut order_of: Vec<Uuid> = Vec::new();
    let slot =
        |id: Uuid, index: &mut std::collections::HashMap<Uuid, usize>, order_of: &mut Vec<Uuid>| {
            *index.entry(id).or_insert_with(|| {
                order_of.push(id);
                order_of.len() - 1
            })
        };
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut definition_of: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    for (code, output_parameter_id, parameter_id) in rows {
        let to = slot(resolve(output_parameter_id), &mut index, &mut order_of);
        definition_of.insert(to, code);
        let Some(source) = parameter_id else {
            continue;
        };
        let from = slot(resolve(source), &mut index, &mut order_of);
        edges.push((to, from));
    }
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); order_of.len()];
    for (to, from) in edges {
        deps[to].push(from);
    }
    let Some(cycle) = crate::common::dependency::cycle(&deps) else {
        return Ok(());
    };
    let mut named: Vec<&str> = cycle
        .iter()
        .filter_map(|i| definition_of.get(i).map(String::as_str))
        .collect();
    named.sort_unstable();
    named.dedup();
    Err(AppError::BadRequest(format!(
        "Merging these parameters would make a derived calculation read what it produces: {}",
        named.join(", ")
    )))
}

pub async fn merge_parameters(
    db: &DatabaseConnection,
    req: &MergeParametersRequest,
    actor: &str,
    origin: crate::routes::private::readings::models::Origin,
) -> AppResult<MergeParametersResponse> {
    let source_id = req.source_parameter_id;
    let target_id = req.target_parameter_id;

    if source_id == target_id {
        return Err(AppError::BadRequest(
            "Source and target must be different".to_string(),
        ));
    }

    let (response, touched, touched_events) = bulk_write::guarded(db, async |txn| {
        validate_both_parameters_exist(txn, source_id, target_id).await?;
        refuse_on_collision(txn, MoveScope::EverySite, source_id, target_id).await?;
        refuse_derived_cycle(txn, source_id, target_id).await?;

        let (sites_merged, sites_reassigned, moved) =
            merge_site_parameters_per_site(txn, source_id, target_id, actor, origin).await?;

        let swept = reassign_parameter_references(txn, source_id, target_id, actor, origin).await?;
        let source_row = row_snapshot(txn, "parameters", source_id).await?;
        delete_parameter(txn, source_id).await?;
        record_merge(
            txn,
            "parameter",
            target_id,
            source_row,
            serde_json::json!({
                "sites_merged": sites_merged,
                "sites_reassigned": sites_reassigned,
                "readings_moved": moved.readings + swept.readings,
                "streams_updated": moved.streams,
            }),
            actor,
        )
        .await?;

        let touched = moved.touched.merge(swept.touched);
        let mut touched_events = moved.touched_events;
        touched_events.extend(swept.touched_events);
        Ok((
            MergeParametersResponse {
                sites_merged,
                sites_reassigned,
                readings_moved: moved.readings + swept.readings,
                streams_updated: moved.streams,
                source_deleted: true,
            },
            touched,
            touched_events,
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

/// Totals accumulated across the sites a catalog-level merge walks.
#[derive(Default)]
struct MergeTotals {
    readings: u64,
    /// Streams re-pointed at a surviving site_parameter.
    streams: u64,
    touched: TouchedRange,
    touched_events: Vec<crate::routes::private::collection_events::flows::TouchedEvent>,
}

async fn validate_both_parameters_exist(
    txn: &impl ConnectionTrait,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<()> {
    let count = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in([source_id, target_id]))
        .count(txn)
        .await
        .map_err(AppError::Database)?;
    if count < 2 {
        return Err(AppError::NotFound(
            "One or both parameters not found".to_string(),
        ));
    }
    Ok(())
}

/// For each site_parameter on source: merge into target's existing site_parameter or reassign.
async fn merge_site_parameters_per_site(
    txn: &impl ConnectionTrait,
    source_id: Uuid,
    target_id: Uuid,
    actor: &str,
    origin: crate::routes::private::readings::models::Origin,
) -> AppResult<(u64, u64, MergeTotals)> {
    let source_sps = site_parameters::Entity::find()
        .filter(site_parameters::Column::ParameterId.eq(source_id))
        .all(txn)
        .await
        .map_err(AppError::Database)?;

    let mut sites_merged: u64 = 0;
    let mut sites_reassigned: u64 = 0;
    let mut totals = MergeTotals::default();

    for row in &source_sps {
        let sp_id = row.id;
        let site_id = row.site_id;

        let target_sp = site_parameters::Entity::find()
            .filter(site_parameters::Column::SiteId.eq(site_id))
            .filter(site_parameters::Column::ParameterId.eq(target_id))
            .one(txn)
            .await
            .map_err(AppError::Database)?;

        if let Some(target_row) = target_sp {
            let target_sp_id = target_row.id;
            let moved = move_slot_rows(
                txn,
                MoveScope::Site(site_id),
                source_id,
                target_id,
                actor,
                origin,
            )
            .await?;
            let streams = update_data_streams(txn, sp_id, target_sp_id).await?;
            delete_source(txn, sp_id, site_id, source_id, target_id).await?;

            totals.readings += moved.readings;
            totals.streams += streams;
            totals.touched = totals.touched.merge(moved.touched);
            totals.touched_events.extend(moved.touched_events);
            sites_merged += 1;
        } else {
            site_parameters::Entity::update_many()
                .col_expr(
                    site_parameters::Column::ParameterId,
                    sea_orm::sea_query::Expr::value(target_id),
                )
                .filter(site_parameters::Column::Id.eq(sp_id))
                .exec(txn)
                .await
                .map_err(AppError::Database)?;
            let moved = move_slot_rows(
                txn,
                MoveScope::Site(site_id),
                source_id,
                target_id,
                actor,
                origin,
            )
            .await?;

            totals.readings += moved.readings;
            totals.touched = totals.touched.merge(moved.touched);
            totals.touched_events.extend(moved.touched_events);
            sites_reassigned += 1;
        }
    }

    Ok((sites_merged, sites_reassigned, totals))
}

/// Move what a catalog parameter owns outside the slot tables: deployments, calibrations, derived
/// sources, thresholds and aliases, plus a final slot-table sweep for rows at sites that carry no
/// `site_parameter` row for the source (nothing walked them per site).
async fn reassign_parameter_references(
    txn: &impl ConnectionTrait,
    source_id: Uuid,
    target_id: Uuid,
    actor: &str,
    origin: crate::routes::private::readings::models::Origin,
) -> AppResult<SlotMove> {
    let to_target = |id: Uuid| sea_orm::sea_query::Expr::value(id);

    // Deployments and calibrations both reference parameters(id); move them to the survivor so the
    // source parameter can be deleted (the deployment FK is RESTRICT).
    sensor_deployments::Entity::update_many()
        .col_expr(
            sensor_deployments::Column::ParameterId,
            to_target(target_id),
        )
        .filter(sensor_deployments::Column::ParameterId.eq(source_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;
    sensor_calibrations::Entity::update_many()
        .col_expr(
            sensor_calibrations::Column::ParameterId,
            to_target(target_id),
        )
        .filter(sensor_calibrations::Column::ParameterId.eq(source_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;

    // Derived parameter sources: delete conflicts, then reassign
    let already_reading_target: Vec<Uuid> = source::Entity::find()
        .filter(source::Column::ParameterId.eq(target_id))
        .all(txn)
        .await
        .map_err(AppError::Database)?
        .into_iter()
        .map(|s| s.derived_definition_id)
        .collect();
    source::Entity::delete_many()
        .filter(source::Column::ParameterId.eq(source_id))
        .filter(source::Column::DerivedDefinitionId.is_in(already_reading_target))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;
    source::Entity::update_many()
        .col_expr(source::Column::ParameterId, to_target(target_id))
        .filter(source::Column::ParameterId.eq(source_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;

    // What a formula produces moves with what it reads. Without this the delete below raises the
    // output foreign key, and the merge fails on a constraint name rather than doing its job; a
    // formula whose output would close a loop is already refused before any of this runs.
    definition::Entity::update_many()
        .col_expr(definition::Column::OutputParameterId, to_target(target_id))
        .filter(definition::Column::OutputParameterId.eq(source_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;

    alarm_thresholds::Entity::delete_many()
        .filter(alarm_thresholds::Column::ParameterId.eq(source_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;

    // The per-site walk covers every site with a source `site_parameter`; this catches rows at
    // sites that never had one, so the source parameter can be deleted.
    let swept = move_slot_rows(
        txn,
        MoveScope::EverySite,
        source_id,
        target_id,
        actor,
        origin,
    )
    .await?;

    // Merge aliases: target gets source's aliases + source's name as a new alias.
    // `needs_review` clears with it: a merge is the adjudication that flag waits for.
    let both = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in([target_id, source_id]))
        .all(txn)
        .await
        .map_err(AppError::Database)?;
    let mut aliases: Vec<String> = both
        .iter()
        .flat_map(|p| {
            p.aliases
                .iter()
                .cloned()
                .chain(std::iter::once(p.code.clone()).filter(|_| p.id == source_id))
        })
        .filter(|a| !a.is_empty())
        .collect();
    aliases.sort_unstable();
    aliases.dedup();
    parameters::Entity::update_many()
        .col_expr(
            parameters::Column::NeedsReview,
            sea_orm::sea_query::Expr::value(false),
        )
        .col_expr(
            parameters::Column::Aliases,
            sea_orm::sea_query::Expr::value(aliases),
        )
        .filter(parameters::Column::Id.eq(target_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;

    Ok(swept)
}

async fn delete_parameter(txn: &impl ConnectionTrait, source_id: Uuid) -> AppResult<()> {
    parameters::Entity::delete_by_id(source_id)
        .exec(txn)
        .await
        .map_err(AppError::Database)?;
    Ok(())
}
