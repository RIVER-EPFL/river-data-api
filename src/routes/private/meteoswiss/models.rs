//! The shapes the MeteoSwiss feed is read into: one parsed interval, what a file yielded, and a
//! site that has declared which station reports for it.

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

/// A site that has declared which station reports for it.
#[derive(Debug, FromQueryResult)]
pub struct Subscriber {
    pub site_id: Uuid,
    pub site_name: String,
    pub station: String,
}
