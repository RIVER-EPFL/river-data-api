use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::aggregates::{self, Window};
use crate::common::bulk_write::{self, TouchedRange};
use crate::error::{AppError, AppResult};
use crate::routes::private::alarms::models as alarm_thresholds;
use crate::routes::private::data_streams::models::{MoveScope, SlotMove};
use crate::routes::private::data_streams::service::{move_slot_rows, slot_move_collisions};
use crate::routes::private::parameters::derived::{definition_model, source_model};
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::sensors::calibrations::model as sensor_calibrations;
use crate::routes::private::sensors::deployments::model as sensor_deployments;
use crate::routes::private::sites::parameters::models as site_parameters;

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
async fn refuse_on_collision<C: ConnectionTrait>(
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
async fn row_snapshot<C: ConnectionTrait>(
    txn: &C,
    table: &str,
    id: Uuid,
) -> AppResult<serde_json::Value> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!("SELECT to_jsonb(t) AS row FROM {table} t WHERE t.id = $1"),
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
async fn record_merge<C: ConnectionTrait>(
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
    txn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO change_audit (subject, change, changed_by, old_value, new_value) \
         VALUES ($1, $2, $3, $4::jsonb, $5::jsonb)",
        [
            format!("{subject}:{target_id}").into(),
            format!("{subject}_merge").into(),
            actor.into(),
            serde_json::Value::Object(absorbed).to_string().into(),
            target.to_string().into(),
        ],
    ))
    .await
    .map_err(AppError::Database)?;
    Ok(())
}

pub async fn merge_site_parameters(
    db: &DatabaseConnection,
    req: &MergeSiteParametersRequest,
    actor: &str,
    origin: crate::routes::private::readings::decisions::Origin,
) -> AppResult<MergeSiteParametersResponse> {
    let source_id = req.source_site_parameter_id;
    let target_id = req.target_site_parameter_id;

    if source_id == target_id {
        return Err(AppError::BadRequest(
            "Source and target must be different".to_string(),
        ));
    }

    let (response, touched) = bulk_write::guarded(db, async |txn| {
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
        ))
    })
    .await?;

    refresh_moved_rollups(db, touched).await?;
    Ok(response)
}

/// The rollups group by `parameter_id`, so recomputing the buckets the moved readings occupy
/// rebuilds both the survivor's series and the absorbed one's in the same pass.
async fn refresh_moved_rollups(db: &DatabaseConnection, touched: TouchedRange) -> AppResult<()> {
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

async fn update_data_streams<C: ConnectionTrait>(
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

async fn delete_source<C: ConnectionTrait>(
    db: &C,
    source_id: Uuid,
    site_id: Uuid,
    source_param_id: Uuid,
    target_param_id: Uuid,
) -> AppResult<()> {
    // Backstop: the move above re-points every slot-keyed row, so these match nothing unless a row
    // was written between the two statements. Nothing deletes a reading: a straggler is re-pointed
    // like the rest, so the site_parameter can go without leaving a row attributed to it.
    let sql = "UPDATE readings SET parameter_id = $3 WHERE site_id = $1 AND parameter_id = $2";
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![
            site_id.into(),
            source_param_id.into(),
            target_param_id.into(),
        ],
    ))
    .await
    .map_err(AppError::Database)?;

    let sql = "UPDATE status_events SET parameter_id = $3 WHERE site_id = $1 AND parameter_id = $2";
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![
            site_id.into(),
            source_param_id.into(),
            target_param_id.into(),
        ],
    ))
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
    let formulas = definition_model::Entity::find()
        .filter(definition_model::Column::OutputParameterId.is_not_null())
        .all(conn)
        .await?;
    let sources: Vec<(Uuid, Option<Uuid>)> = source_model::Entity::find()
        .select_only()
        .columns([
            source_model::Column::DerivedDefinitionId,
            source_model::Column::ParameterId,
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
    origin: crate::routes::private::readings::decisions::Origin,
) -> AppResult<MergeParametersResponse> {
    let source_id = req.source_parameter_id;
    let target_id = req.target_parameter_id;

    if source_id == target_id {
        return Err(AppError::BadRequest(
            "Source and target must be different".to_string(),
        ));
    }

    let (response, touched) = bulk_write::guarded(db, async |txn| {
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

        Ok((
            MergeParametersResponse {
                sites_merged,
                sites_reassigned,
                readings_moved: moved.readings + swept.readings,
                streams_updated: moved.streams,
                source_deleted: true,
            },
            moved.touched.merge(swept.touched),
        ))
    })
    .await?;

    refresh_moved_rollups(db, touched).await?;
    Ok(response)
}

/// Totals accumulated across the sites a catalog-level merge walks.
#[derive(Default)]
struct MergeTotals {
    readings: u64,
    /// Streams re-pointed at a surviving site_parameter.
    streams: u64,
    touched: TouchedRange,
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
    origin: crate::routes::private::readings::decisions::Origin,
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
    origin: crate::routes::private::readings::decisions::Origin,
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
    let already_reading_target: Vec<Uuid> = source_model::Entity::find()
        .filter(source_model::Column::ParameterId.eq(target_id))
        .all(txn)
        .await
        .map_err(AppError::Database)?
        .into_iter()
        .map(|s| s.derived_definition_id)
        .collect();
    source_model::Entity::delete_many()
        .filter(source_model::Column::ParameterId.eq(source_id))
        .filter(source_model::Column::DerivedDefinitionId.is_in(already_reading_target))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;
    source_model::Entity::update_many()
        .col_expr(source_model::Column::ParameterId, to_target(target_id))
        .filter(source_model::Column::ParameterId.eq(source_id))
        .exec(txn)
        .await
        .map_err(AppError::Database)?;

    // What a formula produces moves with what it reads. Without this the delete below raises the
    // output foreign key, and the merge fails on a constraint name rather than doing its job; a
    // formula whose output would close a loop is already refused before any of this runs.
    definition_model::Entity::update_many()
        .col_expr(
            definition_model::Column::OutputParameterId,
            to_target(target_id),
        )
        .filter(definition_model::Column::OutputParameterId.eq(source_id))
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
