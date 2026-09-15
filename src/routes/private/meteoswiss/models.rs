//! The shapes the MeteoSwiss feed is read into: one parsed interval, what a file yielded, what a
//! conditional fetch returned, the subscription a site holds, that subscription as the sync reads
//! it, and the published station list a subscription names a station out of.

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

/// What a conditional fetch returned: the body, or the source saying it holds what we hold.
pub enum Fetched {
    Body(String),
    Unchanged,
}

/// How a parameter the site does not measure itself is attributed wherever it is read. The
/// MeteoSwiss terms ask for the attribution on anything published from the feed, so the line is
/// built here rather than composed by each reader.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct ExternalSource {
    /// The feed, as its rows are keyed (`meteoswiss`).
    pub system: String,
    /// The station the values are read from.
    pub station: String,
    /// The attribution line to show.
    pub attribution: String,
}

/// One enabled subscription, with the site it feeds.
#[derive(Debug, FromQueryResult)]
pub struct Subscriber {
    pub subscription_id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub station: String,
    pub variable: String,
    pub parameter_id: Uuid,
}

pub mod subscription {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    use super::super::service::MeteoswissSubscriptionOperations;

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
        generate_router,
        operations = MeteoswissSubscriptionOperations
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
        /// The catalog parameter the variable lands on, minted with the subscription.
        #[crudcrate(filterable, exclude(create, update))]
        pub parameter_id: Uuid,
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

/// One row of the published station metadata, as the list is maintained from it.
#[derive(Debug, Clone, PartialEq)]
pub struct StationRow {
    pub abbr: String,
    pub name: String,
    pub data_since: Option<chrono::NaiveDate>,
    pub height_masl: Option<f64>,
    /// Blank for a station that reports no pressure, which is 19 of the 158 published.
    pub height_barometer_masl: Option<f64>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// One station offered to a picker: what the list holds, plus how far it is from the site being
/// configured where that site has coordinates.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StationCandidate {
    pub station_abbr: String,
    pub name: String,
    #[schema(required)]
    pub data_since: Option<chrono::NaiveDate>,
    /// The station's own elevation, which every published station carries.
    #[schema(required)]
    pub height_masl: Option<f64>,
    /// The elevation the barometer sits at, which a station reporting no pressure leaves empty.
    #[schema(required)]
    pub height_barometer_masl: Option<f64>,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    /// Great-circle distance from the site, absent where either end has no coordinates.
    #[schema(required)]
    pub distance_km: Option<f64>,
}

pub mod station {
    use sea_orm::entity::prelude::*;

    /// One SMN station, maintained from `ogd-smn_meta_stations.csv`. The abbreviation is the
    /// source's own key and the one a subscription names.
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
    #[sea_orm(table_name = "meteoswiss_stations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub station_abbr: String,
        pub name: String,
        pub data_since: Option<chrono::NaiveDate>,
        pub height_masl: Option<f64>,
        pub height_barometer_masl: Option<f64>,
        pub latitude: Option<f64>,
        pub longitude: Option<f64>,
        pub updated_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod fetch_state {
    use sea_orm::entity::prelude::*;

    /// What the last fetch of one URL returned. The ETag is the source's, echoed back on the next
    /// request as `If-None-Match`.
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
    #[sea_orm(table_name = "meteoswiss_fetch_state")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub url: String,
        pub etag: Option<String>,
        pub fetched_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
