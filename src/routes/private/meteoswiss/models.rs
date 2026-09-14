//! The shapes the MeteoSwiss feed is read into: one parsed interval, what a file yielded, the
//! subscription a site holds, and that subscription as the sync reads it.

use chrono::{DateTime, Utc};
use sea_orm::FromQueryResult;
use uuid::Uuid;

/// One parsed interval: the instant and the value of the requested variable.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub time: DateTime<Utc>,
    pub value: f64,
}

/// What one file yielded. `blank` and `unreadable` are counted rather than raised so a station that
/// stops reporting one variable does not stop the sync for the rest.
#[derive(Debug, Default, PartialEq)]
pub struct Series {
    pub points: Vec<Point>,
    /// Rows whose variable cell was empty.
    pub blank: usize,
    /// Rows whose timestamp or value could not be read.
    pub unreadable: usize,
}

/// One enabled subscription, with the site it feeds.
#[derive(Debug, FromQueryResult)]
pub struct Subscriber {
    pub subscription_id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub station: String,
    pub variable: String,
    pub parameter_id: Option<Uuid>,
}

pub mod subscription {
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
    #[sea_orm(table_name = "meteoswiss_subscriptions")]
    #[crudcrate(
        api_struct = "MeteoswissSubscription",
        name_singular = "meteoswiss_subscription",
        name_plural = "meteoswiss_subscriptions",
        generate_router
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub site_id: Uuid,
        /// The SMN station abbreviation (`MOB`, `SIO`, ...) reporting for the site.
        #[crudcrate(filterable, sortable, fulltext)]
        pub station_abbr: String,
        /// The SMN variable the station is read for, `prestas0` for station-level pressure.
        #[crudcrate(filterable, sortable)]
        pub variable: String,
        /// The catalog parameter the variable lands on. Null lands nothing: a tick says so and
        /// moves to the next subscription.
        #[crudcrate(filterable)]
        pub parameter_id: Option<Uuid>,
        /// A subscription switched off keeps the station and the variable and stops the fetch.
        #[crudcrate(filterable, sortable, on_create = true)]
        pub enabled: bool,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
