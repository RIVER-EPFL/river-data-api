use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::operations::CalculationFormulaOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
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
        Vec<crate::routes::private::parameters::derived::source_model::DerivedParameterSource>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::parameters::derived::source_model::Entity")]
    DerivedParameterSources,
}

impl Related<crate::routes::private::parameters::derived::source_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterSources.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
