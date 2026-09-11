//! The curation ledger as an entity.
//!
//! `reading_decisions` is append-only (ADR 0008): every writer is a curation path with its own
//! rules, and an `AFTER INSERT` trigger projects each row onto the reading it names, so the entity
//! mounts read only (`routes(read)`). A create route would write a decision with no projection
//! rules behind it, and an update or delete would rewrite history the ledger exists to keep.
//!
//! `kind` and `origin` stay `String` here. Their vocabularies are [`super::models::Kind`] and
//! [`super::models::Origin`], and a value outside either is a corrupt row that says so
//! (`super::service::row_from`) rather than a decode failure that hides the rest of the ledger.

use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "reading_decisions")]
#[crudcrate(
    api_struct = "ReadingDecision",
    name_singular = "reading_decision",
    name_plural = "reading_decisions",
    generate_router,
    routes(read)
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub stream_id: Uuid,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub time: DateTimeWithTimeZone,
    /// The replicate the decision names, or NULL for one taken on the whole group.
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub replicate_index: Option<i16>,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub kind: String,
    /// The projected columns as they stood before the decision.
    #[crudcrate(exclude(update, create))]
    pub old: serde_json::Value,
    /// What the decision asserts about those columns.
    #[crudcrate(exclude(update, create))]
    pub new: serde_json::Value,
    #[crudcrate(filterable, fulltext, sortable, exclude(update, create))]
    pub actor: String,
    #[crudcrate(sortable, exclude(update, create))]
    pub at: DateTimeWithTimeZone,
    #[crudcrate(fulltext, exclude(update, create))]
    pub reason: Option<String>,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub origin: String,
    /// The live decision of the same family this one replaces.
    #[crudcrate(filterable, exclude(update, create))]
    pub supersedes: Option<Uuid>,
    /// The `rollback` that inverted this decision, once one has.
    #[crudcrate(filterable, exclude(update, create))]
    pub rolled_back_by: Option<Uuid>,
    /// The set-level decision this row materialises, where it belongs to one.
    #[crudcrate(filterable, exclude(update, create))]
    pub set_id: Option<Uuid>,
    /// The tracked job that made a system change, where one did. Cleared when that job row is
    /// pruned, so an old decision keeps its record and loses only the link to the run.
    #[crudcrate(filterable, exclude(update, create))]
    pub job_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
