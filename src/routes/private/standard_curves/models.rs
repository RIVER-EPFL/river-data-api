use chrono::{DateTime, Utc};
use crudcrate::EntityToModels;
use sea_orm::ExprTrait;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::service::StandardCurveOperations;

/// A lab curve applied on top of an instrument's base calibration, chosen by hand per measurement
/// (typically per microplate) rather than resolved by time. It belongs to one instrument and
/// carries no time columns at all: nothing here is ever selected by a window, which is what keeps
/// it out of the calibration chaining and reprocessing machinery.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "standard_curves")]
#[crudcrate(
    api_struct = "StandardCurve",
    name_singular = "standard_curve",
    name_plural = "standard_curves",
    generate_router,
    operations = StandardCurveOperations,
    upsert_key(source_system, source_key),
    upsert_where = Column::SourceSystem
        .is_not_null()
        .and(Column::SourceKey.is_not_null())
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub sensor_id: Uuid,
    /// Human label the operator picks the curve by, for example the plate it was fitted from.
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: Option<String>,
    /// The date the curve was fitted, which is how the lab identifies one. Defaults to the row's
    /// own creation date when nothing supplies it.
    #[crudcrate(filterable, sortable, on_create = chrono::Utc::now().date_naive())]
    pub fitted_on: Option<chrono::NaiveDate>,
    pub slope: f64,
    pub intercept: f64,
    /// Fit quality reported by whatever produced the curve; recorded, never used in arithmetic.
    pub r_squared: Option<f64>,
    pub notes: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Who entered the curve, stamped from the request that created it; an update naming it is
    /// refused.
    #[crudcrate(exclude(create), on_create = crate::common::actor::current().unwrap_or_default())]
    pub created_by: Option<String>,
    /// Sync provenance: the source a replicated curve came from (e.g. "cnet"). NULL on
    /// hand-entered curves. Written only by `/standard_curves/register`, never through CRUD.
    #[crudcrate(exclude(create, update), filterable)]
    pub source_system: Option<String>,
    /// The curve's identity within its source (e.g. "standard_curves:17"); the upsert key of
    /// `/standard_curves/register` together with `source_system`.
    #[crudcrate(exclude(create, update), filterable)]
    pub source_key: Option<String>,
    /// The curve this one was copied from, stated by whoever made the copy: the split that moves
    /// readings onto another instrument (Q112), or the copy dialog. NULL on a curve that was fitted
    /// rather than copied, and frozen once stored.
    #[crudcrate(exclude(update), filterable)]
    pub copied_from_id: Option<Uuid>,
    /// When the lab took the curve out of circulation (M147). It is no longer offered for a new
    /// measurement; the readings it corrected keep it and keep their values. Written by the retire
    /// routes, never through CRUD.
    #[crudcrate(exclude(create, update), filterable, sortable)]
    pub retired_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub retired_by: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub retired_reason: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
    #[sea_orm(
        belongs_to = "Entity",
        from = "Column::CopiedFromId",
        to = "Column::Id"
    )]
    CopiedFrom,
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// One portal standard curve to register. The curve's own fields are
/// `river_data_core::models::StandardCurveUpsert`, which the sync services build from, so a field
/// the sender gains cannot be dropped here; the API adds the source the caller is speaking for.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterStandardCurveRequest {
    /// The sync source the curve comes from, e.g. "cnet".
    pub source_system: String,
    #[serde(flatten)]
    pub curve: river_data_core::models::StandardCurveUpsert,
}

impl<'de> Deserialize<'de> for RegisterStandardCurveRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (source_system, curve) =
            crate::routes::private::wire::with_source_system(deserializer, &[])?;
        Ok(Self {
            source_system,
            curve,
        })
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterStandardCurveResponse {
    /// The stored curve, or null while it is held for a pairing plan.
    #[schema(required)]
    pub id: Option<Uuid>,
    /// The instrument the curve was fitted on, or null while it is held for a pairing plan.
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    /// True when the stored coefficients differed and the curve was already applied to readings,
    /// so a new row was minted under this provenance. History keeps the old row.
    pub superseded: bool,
    /// True when no stored curve carries this provenance, so the curve is held until a pairing
    /// plan attaches it to one of its instruments.
    pub proposed: bool,
    /// True when the review left this curve behind. It is never stored, and the source is to stop
    /// sending the readings that name it (Q220).
    pub skipped: bool,
}

/// A source's standard curve held until a pairing plan attaches it to one of its instruments.
///
/// The portal names no instrument for a curve, only its parameter cell (`label`), and a curve
/// cannot be stored without one, so nothing is created on the way past (Q195).
pub mod proposal {
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "standard_curve_proposals")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub source_system: String,
        pub source_key: String,
        /// The source's own label for the curve, as the source wrote it.
        pub label: String,
        pub name: Option<String>,
        pub slope: f64,
        pub intercept: f64,
        pub r_squared: Option<f64>,
        pub fitted_on: Option<chrono::NaiveDate>,
        pub notes: Option<String>,
        pub first_seen_at: chrono::DateTime<chrono::Utc>,
        pub last_seen_at: chrono::DateTime<chrono::Utc>,
        /// Set when the review chose to leave this curve behind. It is never stored, and the
        /// readings naming it are dropped at the source rather than arriving uncorrected (Q220).
        pub skipped_at: Option<chrono::DateTime<chrono::Utc>>,
        pub skipped_by: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct LastUsedCurveQuery {
    /// One of `parameter_id` and `parameter_code` is required.
    pub parameter_id: Option<Uuid>,
    pub parameter_code: Option<String>,
}

/// The instrument and standard curve the newest grab at a site and parameter recorded. Every
/// field but `method` is null when no grab there names either.
#[derive(Debug, Serialize, ToSchema)]
pub struct LastUsedCurveResponse {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    #[schema(required)]
    pub sensor_name: Option<String>,
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
    #[schema(required)]
    pub curve_name: Option<String>,
    #[schema(required)]
    pub curve_created_at: Option<chrono::DateTime<Utc>>,
    /// The instant of the grab the answer was read from.
    #[schema(required)]
    pub used_at: Option<chrono::DateTime<Utc>>,
    /// How the answer was decided, for the picker to show beside it.
    pub method: String,
}

pub(super) const LAST_USED_METHOD: &str = "The newest spot reading at this site and parameter that records \
    an instrument or a standard curve, withdrawn readings excluded. The instrument is the one the \
    reading names, or the curve's when the reading names none.";

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetireCurveRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetireCurveResponse {
    pub standard_curve_id: Uuid,
    #[schema(required)]
    pub retired_at: Option<DateTime<Utc>>,
    /// Readings this curve corrected. They keep it and keep their values; the count is what the
    /// surface states before the action runs.
    pub readings: i64,
}
