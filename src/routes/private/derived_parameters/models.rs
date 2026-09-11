//! The three tables a calculation is made of: the formula and its output slot, the input
//! parameters bound to its variable names, and the version history of the formula text.
//!
//! One file, three modules, because each is a SeaORM entity and an entity owns the names `Model`,
//! `Entity` and `Column`.

pub mod definition {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    use super::super::service::CalculationFormulaOperations;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "calculation_formulas")]
    #[crudcrate(
        api_struct = "CalculationFormula",
        name_singular = "calculation_formula",
        name_plural = "calculation_formulas",
        generate_router,
        operations = CalculationFormulaOperations,
        derive_partial_eq
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[sea_orm(unique)]
        #[crudcrate(filterable, fulltext, sortable)]
        pub code: String,
        #[crudcrate(fulltext)]
        pub name: String,
        pub units: String,
        pub formula: String,
        pub description: Option<String>,
        #[crudcrate(exclude(create, update))]
        pub output_parameter_id: Option<Uuid>,
        /// The calculation this formula belongs to (M67). NULL is a standalone derived parameter, the
        /// per-reading continuous kind the derived job and janitor serve.
        #[crudcrate(filterable)]
        pub tool_script_id: Option<Uuid>,
        /// Evaluation order inside the calculation.
        #[crudcrate(filterable, sortable, on_create = 0)]
        pub ordinal: i32,
        /// The curve slot this formula corrects with (M106). Its coefficients reach the formula as
        /// `curve_slope` and `curve_intercept`, so two outputs of one calculation may take two curves.
        #[crudcrate(filterable)]
        pub curve_slot: Option<String>,
        /// The variable whose replicate vector this formula evaluates over (M107), one reading per
        /// index under its output parameter. NULL is a formula producing one number. The output's
        /// replicate identity is the named input's, so nothing here assigns a column position.
        #[crudcrate(filterable)]
        pub per_replicate: Option<String>,
        /// A step of the calculation rather than a measurement of anything (M180): it mints no catalog
        /// parameter, is not saved and is not a manifest output, and its value is handed to the
        /// formulas after it and reported in the run under this formula's own code.
        #[crudcrate(filterable, on_create = false)]
        pub intermediate: bool,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: Option<chrono::DateTime<chrono::Utc>>,
        #[sea_orm(ignore)]
        #[crudcrate(
            non_db_attr = true,
            exclude(create, update),
            join(one, all, depth = 1, fk_column = "DerivedDefinitionId")
        )]
        pub sources:
            Vec<crate::routes::private::derived_parameters::models::source::DerivedParameterSource>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(has_many = "crate::routes::private::derived_parameters::models::source::Entity")]
        DerivedParameterSources,
    }

    impl Related<crate::routes::private::derived_parameters::models::source::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::DerivedParameterSources.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod source {
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
    #[sea_orm(table_name = "derived_parameter_sources")]
    #[crudcrate(
        api_struct = "DerivedParameterSource",
        name_singular = "derived_parameter_source",
        name_plural = "derived_parameter_sources",
        generate_router,
        derive_partial_eq
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub derived_definition_id: Uuid,
        /// The catalog parameter read into the variable. NULL on a site source, which names a column
        /// of the site's own row instead; a row carries exactly one of the two (DB CHECK).
        #[crudcrate(filterable)]
        pub parameter_id: Option<Uuid>,
        /// The column of `sites` read into the variable, for a property of the station rather than a
        /// measurement taken at the visit. NULL on a parameter source.
        #[crudcrate(filterable)]
        pub site_property: Option<String>,
        pub variable_name: String,
        #[crudcrate(exclude(create, update))]
        pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::derived_parameters::models::definition::Entity",
            from = "Column::DerivedDefinitionId",
            to = "crate::routes::private::derived_parameters::models::definition::Column::Id"
        )]
        CalculationFormula,
        #[sea_orm(
            belongs_to = "crate::routes::private::parameters::Entity",
            from = "Column::ParameterId",
            to = "crate::routes::private::parameters::Column::Id"
        )]
        Parameter,
    }

    impl Related<crate::routes::private::derived_parameters::models::definition::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::CalculationFormula.def()
        }
    }

    impl Related<crate::routes::private::parameters::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Parameter.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod version {
    //! A frozen formula, as an entity.
    //!
    //! An edit to a standalone derived definition is a new calculation rather than a correction of the
    //! old one (Q89), so each text is kept under its own version number and a reading names the version
    //! it was made with. Rows are append-only and no route lists them: a version is reached through its
    //! definition, so the entity carries no router.

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
    #[sea_orm(table_name = "derived_parameter_definition_versions")]
    #[crudcrate(
        api_struct = "DerivedDefinitionVersion",
        name_singular = "derived_definition_version",
        name_plural = "derived_definition_versions"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub definition_id: Uuid,
        /// One-based and per definition, so `(definition_id, version_no)` is unique.
        #[crudcrate(filterable, sortable)]
        pub version_no: i32,
        pub formula: String,
        /// The hash the migration computes for the same text, so a re-save of an unchanged formula
        /// mints no version.
        #[crudcrate(filterable)]
        pub content_hash: String,
        pub created_by: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::definition::Entity",
            from = "Column::DefinitionId",
            to = "super::definition::Column::Id"
        )]
        Definition,
    }

    impl Related<super::definition::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Definition.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
