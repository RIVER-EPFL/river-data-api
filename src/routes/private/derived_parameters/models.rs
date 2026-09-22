//! The two tables a calculation's formulas are made of: the formula and its output slot, and the
//! input parameters bound to its variable names.
//!
//! One file, a module per entity, because an entity owns the names `Model`, `Entity` and
//! `Column`, plus the read shapes the calculation routes answer in.

use uuid::Uuid;

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
        /// The catalog parameter this formula published before it was ticked as a step. Ticking it
        /// back adopts this row again, so disabling publication and enabling it keeps one identity
        /// rather than minting a second parameter or refusing the code.
        #[crudcrate(exclude(create, update))]
        pub given_up_parameter_id: Option<Uuid>,
        /// The calculation this formula belongs to (M67). NULL is a shared step, owned by no
        /// calculation and declared by each that reads it (Q156).
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
        /// Why this formula's code can no longer be changed, when it cannot. The catalog code is
        /// the CSV column header and the public API's identifier, so once readings are stored under
        /// the output parameter or a project publishes it, a rename is refused (Q183). NULL means
        /// the code is still free.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update), default = None)]
        pub code_locked: Option<String>,
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

/// What a step feeds: one calculation that computes it, and the formulas inside that calculation
/// whose text reads the step's code.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct StepDependent {
    pub tool_script_id: Uuid,
    pub name: String,
    pub label: String,
    /// Whether the calculation owns the step, rather than declaring one owned by nobody.
    pub owns: bool,
    pub formulas: Vec<StepReader>,
}

/// One formula that reads the step.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct StepReader {
    pub code: String,
    pub formula: String,
}

/// Everything that reads one step, which is what its page shows before its expression is edited.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct StepDependents {
    pub formula_id: Uuid,
    pub code: String,
    /// True when the step belongs to no calculation, which is what makes it shareable (Q156).
    pub shared: bool,
    pub calculations: Vec<StepDependent>,
}

/// One calculation's declaration that it reads a shared step (Q156).
///
/// A shared step is a `calculation_formulas` row that is `intermediate` and owned by no
/// calculation, so nothing about the step says who reads it. A row here is that reading: the
/// declaring calculation evaluates the step in its own run, under the step's own code, and the
/// step's inputs become inputs of that calculation.
pub mod shared_step {
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
    #[sea_orm(table_name = "calculation_shared_steps")]
    #[crudcrate(
        api_struct = "CalculationSharedStep",
        name_singular = "calculation_shared_step",
        name_plural = "calculation_shared_steps",
        generate_router,
        operations = crate::routes::private::derived_parameters::service::SharedStepOperations
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        /// The calculation that reads the step.
        #[crudcrate(filterable, sortable)]
        pub tool_script_id: Uuid,
        /// The step it reads, which is a formula row owned by no calculation.
        #[crudcrate(filterable, sortable)]
        pub formula_id: Uuid,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::derived_parameters::models::definition::Entity",
            from = "Column::FormulaId",
            to = "crate::routes::private::derived_parameters::models::definition::Column::Id"
        )]
        CalculationFormula,
    }

    impl Related<crate::routes::private::derived_parameters::models::definition::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::CalculationFormula.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
