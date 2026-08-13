use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::aggregates::{self, Window};
use crate::common::bulk_write::{self, TouchedRange};
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::views::{
    MoveScope, SlotMove, move_slot_rows, slot_move_collisions,
};

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
pub async fn merge_site_parameters(
    db: &DatabaseConnection,
    req: &MergeSiteParametersRequest,
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
        let moved = move_slot_rows(txn, scope, source_param_id, target_param_id).await?;
        let streams_updated = update_data_streams(txn, source_id, target_id).await?;
        delete_source(txn, source_id, source_site_id, source_param_id).await?;

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
    let sql = "SELECT id, site_id, parameter_id FROM site_parameters WHERE id = ANY($1)";
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            vec![vec![source_id, target_id].into()],
        ))
        .await
        .map_err(AppError::Database)?;

    let mut source: Option<(Uuid, Uuid)> = None;
    let mut target: Option<(Uuid, Uuid)> = None;

    for row in &rows {
        let id: Uuid = row.try_get("", "id").map_err(AppError::Database)?;
        let site_id: Uuid = row.try_get("", "site_id").map_err(AppError::Database)?;
        let param_id: Uuid = row
            .try_get("", "parameter_id")
            .map_err(AppError::Database)?;

        if id == source_id {
            source = Some((site_id, param_id));
        }
        if id == target_id {
            target = Some((site_id, param_id));
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
        .execute(Statement::from_sql_and_values(
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
) -> AppResult<()> {
    // Backstop: the move above re-points every slot-keyed row, so these match nothing unless a row
    // was written between the two statements. Deleting the site_parameter without them would leave
    // readings attributed to a slot that no longer exists.
    let sql = "DELETE FROM readings WHERE site_id = $1 AND parameter_id = $2";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    let sql = "DELETE FROM status_events WHERE site_id = $1 AND parameter_id = $2";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    let sql = "DELETE FROM alarm_thresholds WHERE parameter_id = $1 AND site_id = $2";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![source_param_id.into(), site_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    let sql = "DELETE FROM site_parameters WHERE id = $1";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![source_id.into()],
    ))
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
pub async fn merge_parameters(
    db: &DatabaseConnection,
    req: &MergeParametersRequest,
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

        let (sites_merged, sites_reassigned, moved) =
            merge_site_parameters_per_site(txn, source_id, target_id).await?;

        let swept = reassign_parameter_references(txn, source_id, target_id).await?;
        delete_parameter(txn, source_id).await?;

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
    let pg = sea_orm::DatabaseBackend::Postgres;
    let count: i64 = txn
        .query_one(Statement::from_sql_and_values(
            pg,
            "SELECT COUNT(*) as c FROM parameters WHERE id = ANY($1)",
            vec![vec![source_id, target_id].into()],
        ))
        .await
        .map_err(AppError::Database)?
        .ok_or_else(|| AppError::NotFound("Parameter query failed".into()))?
        .try_get("", "c")
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
) -> AppResult<(u64, u64, MergeTotals)> {
    let pg = sea_orm::DatabaseBackend::Postgres;

    let source_sps = txn
        .query_all(Statement::from_sql_and_values(
            pg,
            "SELECT id, site_id FROM site_parameters WHERE parameter_id = $1",
            vec![source_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    let mut sites_merged: u64 = 0;
    let mut sites_reassigned: u64 = 0;
    let mut totals = MergeTotals::default();

    for row in &source_sps {
        let sp_id: Uuid = row.try_get("", "id").map_err(AppError::Database)?;
        let site_id: Uuid = row.try_get("", "site_id").map_err(AppError::Database)?;

        let target_sp = txn
            .query_one(Statement::from_sql_and_values(
                pg,
                "SELECT id FROM site_parameters WHERE site_id = $1 AND parameter_id = $2",
                vec![site_id.into(), target_id.into()],
            ))
            .await
            .map_err(AppError::Database)?;

        if let Some(target_row) = target_sp {
            let target_sp_id: Uuid = target_row.try_get("", "id").map_err(AppError::Database)?;
            let moved = move_slot_rows(txn, MoveScope::Site(site_id), source_id, target_id).await?;
            let streams = update_data_streams(txn, sp_id, target_sp_id).await?;
            delete_source(txn, sp_id, site_id, source_id).await?;

            totals.readings += moved.readings;
            totals.streams += streams;
            totals.touched = totals.touched.merge(moved.touched);
            sites_merged += 1;
        } else {
            txn.execute(Statement::from_sql_and_values(
                pg,
                "UPDATE site_parameters SET parameter_id = $1 WHERE id = $2",
                vec![target_id.into(), sp_id.into()],
            ))
            .await
            .map_err(AppError::Database)?;
            let moved = move_slot_rows(txn, MoveScope::Site(site_id), source_id, target_id).await?;

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
) -> AppResult<SlotMove> {
    let pg = sea_orm::DatabaseBackend::Postgres;

    // Deployments and calibrations both reference parameters(id); move them to the survivor so the
    // source parameter can be deleted (the deployment FK is RESTRICT).
    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE sensor_deployments SET parameter_id = $1 WHERE parameter_id = $2",
        vec![target_id.into(), source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;
    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE sensor_calibrations SET parameter_id = $1 WHERE parameter_id = $2",
        vec![target_id.into(), source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    // Derived parameter sources: delete conflicts, then reassign
    txn.execute(Statement::from_sql_and_values(
        pg,
        r#"DELETE FROM derived_parameter_sources WHERE parameter_id = $1
           AND derived_definition_id IN (
               SELECT derived_definition_id FROM derived_parameter_sources WHERE parameter_id = $2
           )"#,
        vec![source_id.into(), target_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;
    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE derived_parameter_sources SET parameter_id = $1 WHERE parameter_id = $2",
        vec![target_id.into(), source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "DELETE FROM alarm_thresholds WHERE parameter_id = $1",
        vec![source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    // The per-site walk covers every site with a source `site_parameter`; this catches rows at
    // sites that never had one, so the source parameter can be deleted.
    let swept = move_slot_rows(txn, MoveScope::EverySite, source_id, target_id).await?;

    // Merge aliases: target gets source's aliases + source's name as a new alias
    txn.execute(Statement::from_sql_and_values(
        pg,
        r#"UPDATE parameters SET aliases = (
            SELECT array_agg(DISTINCT a)
            FROM unnest(
                (SELECT aliases FROM parameters WHERE id = $1)
                || (SELECT aliases FROM parameters WHERE id = $2)
                || ARRAY[(SELECT code FROM parameters WHERE id = $2)]
            ) AS a WHERE a IS NOT NULL AND a != ''
        ) WHERE id = $1"#,
        vec![target_id.into(), source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    Ok(swept)
}

async fn delete_parameter(txn: &impl ConnectionTrait, source_id: Uuid) -> AppResult<()> {
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM parameters WHERE id = $1",
        vec![source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;
    Ok(())
}
