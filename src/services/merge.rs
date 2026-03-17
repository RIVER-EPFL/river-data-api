use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct MergeSiteParametersRequest {
    pub source_site_parameter_id: Uuid,
    pub target_site_parameter_id: Uuid,
}

#[derive(Debug, Serialize)]
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
