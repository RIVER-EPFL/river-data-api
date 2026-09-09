use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::routes::private::sync::service::{ApplyResult, PlanCurveIntents, PlanEntries, PlanSummary};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels)]
#[sea_orm(table_name = "pairing_plans")]
#[crudcrate(
    api_struct = "PairingPlan",
    name_singular = "pairing_plan",
    name_plural = "pairing_plans",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub source_system: String,
    #[crudcrate(filterable, on_create = String::from("draft"))]
    pub status: String,
    pub created_by: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub summary: PlanSummary,
    #[sea_orm(column_type = "JsonBinary")]
    pub entries: PlanEntries,
    /// Curves the review assigned to instruments this plan creates, `[{curve_id,
    /// instrument_source_key}]`, moved in the apply transaction that mints them.
    #[sea_orm(column_type = "JsonBinary")]
    #[crudcrate(exclude(create, update), on_create = PlanCurveIntents::default())]
    pub curve_assignments: PlanCurveIntents,
    /// Bumped by every edit. A PATCH or an apply names the version it read, so a second writer
    /// on one draft is refused rather than carrying the first writer's decisions away.
    #[crudcrate(exclude(create, update), on_create = 0)]
    pub version: i32,
    #[crudcrate(sortable, exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(sortable, exclude(create, update))]
    pub applied_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    #[crudcrate(exclude(create, update))]
    pub apply_result: Option<ApplyResult>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
