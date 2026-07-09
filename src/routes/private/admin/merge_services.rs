use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

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

/// Merge two site_parameters: absorb source into target.
///
/// Moves readings, status_events, data_streams, and
/// sensor_deployments from source to target, then deletes the source.
/// Duplicate readings (same PK) are skipped via ON CONFLICT DO NOTHING.
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

    // Validate both site_parameters exist and are compatible
    let (source_site_id, source_param_id, target_site_id, target_param_id) =
        validate_merge_candidates(db, source_id, target_id).await?;

    if source_site_id != target_site_id {
        return Err(AppError::BadRequest(
            "Source and target must belong to the same site".to_string(),
        ));
    }

    // Step 1: Move readings (ON CONFLICT DO NOTHING for overlap)
    let merged_readings = move_readings(db, source_site_id, source_param_id, target_param_id).await?;

    // Step 2: Move status_events (ON CONFLICT DO NOTHING for overlap)
    let merged_status_events =
        move_status_events(db, source_site_id, source_param_id, target_param_id).await?;

    // Step 3: Update data_streams to point to target site_parameter
    let streams_updated = update_data_streams(db, source_id, target_id).await?;

    // Step 4: Move sensor_deployments to target site_parameter's parameter_id
    let deployments_moved = move_sensor_deployments(db, source_param_id, target_param_id).await?;

    // Step 5: Delete the source site_parameter (readings/status_events already moved)
    delete_source(db, source_id, source_site_id, source_param_id).await?;

    Ok(MergeSiteParametersResponse {
        merged_readings,
        merged_status_events,
        streams_updated,
        deployments_moved,
        source_deleted: true,
    })
}

async fn validate_merge_candidates(
    db: &DatabaseConnection,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<(Uuid, Uuid, Uuid, Uuid)> {
    use sea_orm::{ConnectionTrait, Statement};

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
        let site_id: Uuid = row
            .try_get("", "site_id")
            .map_err(AppError::Database)?;
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

    let (source_site_id, source_param_id) = source
        .ok_or_else(|| AppError::NotFound("Source site_parameter not found".to_string()))?;
    let (target_site_id, target_param_id) = target
        .ok_or_else(|| AppError::NotFound("Target site_parameter not found".to_string()))?;

    Ok((source_site_id, source_param_id, target_site_id, target_param_id))
}

async fn move_readings(
    db: &DatabaseConnection,
    site_id: Uuid,
    source_param_id: Uuid,
    target_param_id: Uuid,
) -> AppResult<u64> {
    use sea_orm::{ConnectionTrait, Statement};

    // Update parameter_id on readings from source to target
    let sql = r#"
        UPDATE readings SET parameter_id = $1
        WHERE site_id = $2 AND parameter_id = $3
    "#;

    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            vec![target_param_id.into(), site_id.into(), source_param_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    Ok(result.rows_affected())
}

async fn move_status_events(
    db: &DatabaseConnection,
    site_id: Uuid,
    source_param_id: Uuid,
    target_param_id: Uuid,
) -> AppResult<u64> {
    use sea_orm::{ConnectionTrait, Statement};

    let sql = r#"
        UPDATE status_events SET parameter_id = $1
        WHERE site_id = $2 AND parameter_id = $3
    "#;

    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            vec![target_param_id.into(), site_id.into(), source_param_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    Ok(result.rows_affected())
}

async fn update_data_streams(
    db: &DatabaseConnection,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<u64> {
    use sea_orm::{ConnectionTrait, Statement};

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

async fn move_sensor_deployments(
    _db: &DatabaseConnection,
    _source_param_id: Uuid,
    _target_param_id: Uuid,
) -> AppResult<u64> {
    // Sensor deployments link sensor to site, not to parameter.
    // After merge, the readings already have the right sensor_id.
    // Since both site_parameters are on the same site, deployments don't need moving.
    Ok(0)
}

async fn delete_source(
    db: &DatabaseConnection,
    source_id: Uuid,
    site_id: Uuid,
    source_param_id: Uuid,
) -> AppResult<()> {
    use sea_orm::{ConnectionTrait, Statement};

    // Delete remaining readings for source parameter (already moved, these are the originals)
    let sql = "DELETE FROM readings WHERE site_id = $1 AND parameter_id = $2";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    // Delete remaining status_events for source parameter
    let sql = "DELETE FROM status_events WHERE site_id = $1 AND parameter_id = $2";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    // Delete alarm_thresholds for source
    let sql = "DELETE FROM alarm_thresholds WHERE parameter_id = $1 AND site_id = $2";
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        vec![source_param_id.into(), site_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    // Delete the source site_parameter
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

    let txn = db.begin().await.map_err(AppError::Database)?;
    validate_both_parameters_exist(&txn, source_id, target_id).await?;

    let (sites_merged, sites_reassigned, total_readings, total_streams) =
        merge_site_parameters_per_site(&txn, source_id, target_id).await?;

    reassign_parameter_references(&txn, source_id, target_id).await?;
    delete_parameter(&txn, source_id).await?;

    txn.commit().await.map_err(AppError::Database)?;

    Ok(MergeParametersResponse {
        sites_merged,
        sites_reassigned,
        readings_moved: total_readings,
        streams_updated: total_streams,
        source_deleted: true,
    })
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
) -> AppResult<(u64, u64, u64, u64)> {
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
    let mut total_readings: u64 = 0;
    let mut total_streams: u64 = 0;

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
            let (readings, streams) = merge_conflicting_site_param(
                txn, sp_id, target_sp_id, site_id, source_id, target_id,
            ).await?;
            total_readings += readings;
            total_streams += streams;
            sites_merged += 1;
        } else {
            let readings = reassign_site_param(txn, sp_id, site_id, source_id, target_id).await?;
            total_readings += readings;
            sites_reassigned += 1;
        }
    }

    Ok((sites_merged, sites_reassigned, total_readings, total_streams))
}

/// Both source and target have a site_parameter at this site -- absorb source into target.
async fn merge_conflicting_site_param(
    txn: &impl ConnectionTrait,
    source_sp_id: Uuid,
    target_sp_id: Uuid,
    site_id: Uuid,
    source_param_id: Uuid,
    target_param_id: Uuid,
) -> AppResult<(u64, u64)> {
    let pg = sea_orm::DatabaseBackend::Postgres;

    let r = txn
        .execute(Statement::from_sql_and_values(
            pg,
            "UPDATE readings SET parameter_id = $1 WHERE site_id = $2 AND parameter_id = $3",
            vec![target_param_id.into(), site_id.into(), source_param_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    let readings_moved = r.rows_affected();

    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE status_events SET parameter_id = $1 WHERE site_id = $2 AND parameter_id = $3",
        vec![target_param_id.into(), site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    let s = txn
        .execute(Statement::from_sql_and_values(
            pg,
            "UPDATE data_streams SET site_parameter_id = $1 WHERE site_parameter_id = $2",
            vec![target_sp_id.into(), source_sp_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    let streams_moved = s.rows_affected();

    delete_site_param_remnants(txn, source_sp_id, site_id, source_param_id).await?;

    Ok((readings_moved, streams_moved))
}

/// No conflict at this site -- just reassign the site_parameter's parameter_id.
async fn reassign_site_param(
    txn: &impl ConnectionTrait,
    sp_id: Uuid,
    site_id: Uuid,
    source_param_id: Uuid,
    target_param_id: Uuid,
) -> AppResult<u64> {
    let pg = sea_orm::DatabaseBackend::Postgres;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE site_parameters SET parameter_id = $1 WHERE id = $2",
        vec![target_param_id.into(), sp_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    let r = txn
        .execute(Statement::from_sql_and_values(
            pg,
            "UPDATE readings SET parameter_id = $1 WHERE site_id = $2 AND parameter_id = $3",
            vec![target_param_id.into(), site_id.into(), source_param_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE status_events SET parameter_id = $1 WHERE site_id = $2 AND parameter_id = $3",
        vec![target_param_id.into(), site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    Ok(r.rows_affected())
}

async fn delete_site_param_remnants(
    txn: &impl ConnectionTrait,
    sp_id: Uuid,
    site_id: Uuid,
    source_param_id: Uuid,
) -> AppResult<()> {
    let pg = sea_orm::DatabaseBackend::Postgres;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "DELETE FROM alarm_thresholds WHERE parameter_id = $1 AND site_id = $2",
        vec![source_param_id.into(), site_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "DELETE FROM readings WHERE site_id = $1 AND parameter_id = $2",
        vec![site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "DELETE FROM status_events WHERE site_id = $1 AND parameter_id = $2",
        vec![site_id.into(), source_param_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "DELETE FROM site_parameters WHERE id = $1",
        vec![sp_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    Ok(())
}

/// Move deployments, calibrations, derived sources, alarms, annotations, and samples from source to
/// target parameter. (Sensors no longer carry a parameter — it lives on the deployment /
/// calibration.)
async fn reassign_parameter_references(
    txn: &impl ConnectionTrait,
    source_id: Uuid,
    target_id: Uuid,
) -> AppResult<()> {
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

    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE annotations SET parameter_id = $1 WHERE parameter_id = $2",
        vec![target_id.into(), source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

    txn.execute(Statement::from_sql_and_values(
        pg,
        "UPDATE samples SET parameter_id = $1 WHERE parameter_id = $2",
        vec![target_id.into(), source_id.into()],
    ))
    .await
    .map_err(AppError::Database)?;

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

    Ok(())
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
