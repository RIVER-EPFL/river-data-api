use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    DeriveEntityModel,
    serde::Serialize,
    serde::Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "projects")]
#[crudcrate(
    api_struct = "Project",
    name_singular = "project",
    name_plural = "projects",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    #[crudcrate(filterable)]
    pub data_source: Option<String>,
    pub description: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(filterable, on_create = false)]
    pub is_public: bool,
    #[crudcrate(filterable)]
    pub public_code: Option<String>,
    pub public_api_title: Option<String>,
    pub public_api_description: Option<String>,
    pub public_api_version: Option<String>,
    pub public_contact_email: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::sites::Entity")]
    Sites,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sites.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub mod subprojects {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "subprojects")]
    #[crudcrate(
        api_struct = "Subproject",
        name_singular = "subproject",
        name_plural = "subprojects",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub project_id: Uuid,
        #[crudcrate(filterable, fulltext, sortable)]
        pub name: String,
        #[sea_orm(column_type = "Text", nullable)]
        pub description: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::projects::Entity",
            from = "Column::ProjectId",
            to = "crate::routes::private::projects::Column::Id"
        )]
        Project,
        #[sea_orm(has_many = "crate::routes::private::sites::Entity")]
        Sites,
    }

    impl Related<crate::routes::private::projects::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Project.def()
        }
    }

    impl Related<crate::routes::private::sites::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Sites.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
