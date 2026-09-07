//! The rules the generated CRUD cannot state: the name a tool is reached by, and the two counts a
//! reader wants beside a calculation.

use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    Statement,
};
use uuid::Uuid;

use super::engine;
use super::script_model::ToolScript;
use super::version_model::{ToolScriptVersion, ToolScriptVersionList};

/// A tool's name is a path segment (`/tools/{name}/calculate`) and a manifest key, so it is
/// lower-cased and refused unless it is `[a-z0-9_]`.
pub(crate) fn normalise_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ApiError::bad_request(
            "tool name must be non-empty [a-z0-9_]".to_string(),
        ));
    }
    Ok(name.to_lowercase())
}

/// `script` or `formula`, or nothing at all.
pub(crate) fn check_engine(engine: &str) -> Result<(), ApiError> {
    if engine::Engine::parse(engine).is_none() {
        return Err(ApiError::bad_request(format!(
            "engine {engine} is not script or formula"
        )));
    }
    Ok(())
}

/// The version count and the live version's number, for a page of calculations.
async fn counts(
    db: &DatabaseConnection,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Option<i32>, i64)>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT s.id, av.version_no AS active_version_no,
                     (SELECT count(*) FROM tool_script_versions v
                       WHERE v.tool_script_id = s.id) AS version_count
                FROM tool_scripts s
                LEFT JOIN tool_script_versions av ON av.id = s.active_version_id
               WHERE s.id = ANY($1)",
            [ids.to_vec().into()],
        ))
        .await
        .map_err(ApiError::database)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push((
            row.try_get("", "id").map_err(ApiError::database)?,
            row.try_get("", "active_version_no")
                .map_err(ApiError::database)?,
            row.try_get("", "version_count")
                .map_err(ApiError::database)?,
        ));
    }
    Ok(out)
}

pub struct ToolScriptOperations;

#[async_trait]
impl CRUDOperations for ToolScriptOperations {
    type Resource = ToolScript;

    /// The name is normalised before the insert rather than validated in `before_create`, which
    /// is handed the request by reference and cannot correct it.
    async fn create(
        &self,
        db: &DatabaseConnection,
        mut data: <ToolScript as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<ToolScript, ApiError> {
        data.name = normalise_name(&data.name)?;
        if let Some(engine) = data.engine.as_deref() {
            check_engine(engine)?;
        }
        let name = data.name.clone();
        self.perform_create(db, data).await.map_err(|e| {
            if e.to_string().contains("idx_tool_scripts_name") {
                ApiError::conflict(format!("a tool named '{name}' already exists"))
            } else {
                e
            }
        })
    }

    /// The name is `exclude(update)`, so an update can only reach the engine.
    async fn before_update(
        &self,
        _db: &DatabaseConnection,
        _id: Uuid,
        data: &<ToolScript as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(engine)) = data.engine.as_ref() {
            check_engine(engine)?;
        }
        Ok(())
    }

    /// The version history, newest first, and which of them is live.
    async fn after_get_one(
        &self,
        db: &DatabaseConnection,
        entity: &mut ToolScript,
    ) -> Result<(), ApiError> {
        let versions = super::version_model::Entity::find()
            .filter(super::version_model::Column::ToolScriptId.eq(entity.id))
            .order_by_desc(super::version_model::Column::VersionNo)
            .all(db)
            .await
            .map_err(ApiError::database)?;
        entity.versions = versions
            .into_iter()
            .map(|m| {
                let mut v = ToolScriptVersionList::from(ToolScriptVersion::from(m));
                v.active = entity.active_version_id == Some(v.id);
                v
            })
            .collect();
        entity.version_count = entity.versions.len() as i64;
        entity.active_version_no = entity
            .versions
            .iter()
            .find(|v| v.active)
            .map(|v| v.version_no);
        Ok(())
    }

    async fn after_get_all(
        &self,
        db: &DatabaseConnection,
        entities: &mut Vec<<ToolScript as crudcrate::CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let counted = counts(db, &ids).await?;
        for entity in entities.iter_mut() {
            if let Some((_, active_version_no, version_count)) =
                counted.iter().find(|(id, _, _)| *id == entity.id)
            {
                entity.active_version_no = *active_version_no;
                entity.version_count = *version_count;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{check_engine, normalise_name};

    #[test]
    fn test_a_tool_name_is_lower_cased_and_path_safe() {
        assert_eq!(normalise_name("  DOC  ").expect("trimmed"), "doc");
        assert_eq!(normalise_name("tss_afdm").expect("plain"), "tss_afdm");
        assert!(normalise_name("").is_err());
        assert!(normalise_name("chl a").is_err(), "a space is not a segment");
        assert!(
            normalise_name("co2/air").is_err(),
            "a slash is not a segment"
        );
    }

    #[test]
    fn test_only_the_two_engines_are_accepted() {
        assert!(check_engine("script").is_ok());
        assert!(check_engine("formula").is_ok());
        assert!(check_engine("r").is_err());
    }
}
