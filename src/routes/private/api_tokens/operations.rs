use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

use super::model::{self, ApiToken};
use super::service::mint_api_token;

pub struct ApiTokenOperations;

#[async_trait]
impl CRUDOperations for ApiTokenOperations {
    type Resource = ApiToken;

    /// One row at a time, through the single-row path: the crudcrate default `create_many`
    /// delegates to the resource, which delegates back here, so the default recurses; the loop
    /// also runs the single-row hooks for every item.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<ApiToken as CRUDResource>::CreateModel>,
    ) -> Result<Vec<ApiToken>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    async fn perform_create(
        &self,
        db: &DatabaseConnection,
        data: <ApiToken as CRUDResource>::CreateModel,
    ) -> Result<ApiToken, ApiError> {
        let minted = mint_api_token();

        let mut active_model: model::ActiveModel = data.into();
        active_model.token_hash = Set(minted.token_hash);
        active_model.token_prefix = Set(minted.token_prefix);

        let model = active_model.insert(db).await.map_err(ApiError::database)?;
        let mut token = ApiToken::from(model);
        // The raw secret is returned exactly once, here, and never persisted.
        token.token = Some(minted.raw_token);
        Ok(token)
    }
}
