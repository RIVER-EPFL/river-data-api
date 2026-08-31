use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

/// A user-declared sample groups one or more replicate readings of the same
/// parameter at a site. Aggregate columns (mean/stdev/n/min_value/max_value)
/// are maintained by a PostgreSQL trigger; application code never writes them
/// and they are excluded from create/update payloads.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "samples")]
#[crudcrate(
    api_struct = "Sample",
    name_singular = "sample",
    name_plural = "samples",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    // Identity columns are create-only: the readings a sample groups are keyed on
    // (site, parameter, collected_at), so editing them here would detach the sample
    // from its replicates while the trigger keeps refreshing the old key.
    #[crudcrate(filterable, exclude(update))]
    pub site_id: Uuid,
    #[crudcrate(filterable, exclude(update))]
    pub parameter_id: Uuid,
    #[crudcrate(filterable, sortable, exclude(update))]
    pub collected_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(fulltext, sortable)]
    pub label: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub notes: Option<String>,
    pub created_by: Option<String>,
    // Written only by the grab save path; a CRUD edit must not be able to forge or erase the
    // record of what produced the numbers.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    #[crudcrate(exclude(create, update))]
    pub provenance: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    // Aggregate columns, trigger-maintained, read-only to clients.
    #[crudcrate(exclude(create, update), sortable)]
    pub mean: Option<f64>,
    #[crudcrate(exclude(create, update))]
    pub stdev: Option<f64>,
    #[crudcrate(exclude(create, update), sortable)]
    pub n: i32,
    #[crudcrate(exclude(create, update))]
    pub min_value: Option<f64>,
    #[crudcrate(exclude(create, update))]
    pub max_value: Option<f64>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    // Which divisor produced `stdev` ('sample' = n-1, 'population' = n) and where that decision
    // came from. Set by the write paths and the retag job from the slot's declaration; a CRUD edit
    // must not be able to restate what a stored number was computed with. `sd_estimator_source`
    // 'default' means nothing was declared and the fallback applied, which is what the undeclared
    // report and the audit gate look for.
    #[crudcrate(exclude(create, update), filterable)]
    pub sd_estimator: String,
    #[crudcrate(exclude(create, update), filterable)]
    pub sd_estimator_source: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id",
        on_delete = "Cascade"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::routes::private::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
