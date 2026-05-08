use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

use crate::entity::api_tokens::{self, ApiToken};
use crate::services::api_token::{generate_token, hash_token};

pub struct ApiTokenOperations;

#[async_trait]
impl CRUDOperations for ApiTokenOperations {
    type Resource = ApiToken;

    async fn perform_create(
        &self,
        db: &DatabaseConnection,
        data: <ApiToken as CRUDResource>::CreateModel,
    ) -> Result<ApiToken, ApiError> {
        let raw = generate_token();
        let hash = hash_token(&raw);

        let mut active_model: api_tokens::ActiveModel = data.into();
        active_model.token_hash = Set(hash);

        let model = active_model.insert(db).await.map_err(ApiError::database)?;
        let mut token = ApiToken::from(model);
        token.raw_token = Some(raw);
        Ok(token)
    }
}
