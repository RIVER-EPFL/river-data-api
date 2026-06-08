use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

use super::model::{self, ApiToken};
use super::services::mint_api_token;

pub struct ApiTokenOperations;

#[async_trait]
impl CRUDOperations for ApiTokenOperations {
    type Resource = ApiToken;

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
