use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::DataStream;
use super::replicates;

pub struct DataStreamOperations;

/// The stream's registered replicate-family key, when it has one.
async fn family_key(db: &DatabaseConnection, id: Uuid) -> Result<Option<String>, ApiError> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_key FROM data_streams WHERE id = $1 AND metadata -> $2 IS NOT NULL",
            [id.into(), replicates::METADATA_KEY.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    row.map(|r| r.try_get::<String>("", "source_key"))
        .transpose()
        .map_err(ApiError::database)
}

#[async_trait]
impl CRUDOperations for DataStreamOperations {
    type Resource = DataStream;

    /// A replicate family stays classified 'spot' through entity CRUD as well: `/streams/register`
    /// and `/streams/retag` already refuse it, and a plain PATCH must not be the one route that
    /// can move the column. Clearing it (NULL) is refused too, the classification would then fall
    /// through to the owning sensor's data_frequency and can resolve continuous.
    async fn before_update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: &<DataStream as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(new_value) = &data.measurement_type
            && new_value.as_deref() != Some("spot")
            && let Some(key) = family_key(db, id).await?
        {
            let target = new_value.as_deref().unwrap_or("NULL");
            return Err(ApiError::bad_request(format!(
                "stream '{key}' declares a replicate family and must stay classified 'spot', \
                 not '{target}'. The continuous aggregates roll up only non-spot rows at \
                 replicate index 0, so a family outside 'spot' loses every replicate but one \
                 from every rollup"
            )));
        }
        Ok(())
    }

    /// One row at a time, through the single-row path, so a bulk edit meets the same guard.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(Uuid, <DataStream as crudcrate::CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<DataStream>, ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }
}
