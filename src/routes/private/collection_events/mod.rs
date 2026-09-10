//! Collection events: one row per (site, staged timestamp) visit — the portal's wide `data` row
//! as an entity (D7). Readings attach through `readings.collection_event_id`; the attach helper
//! in [`service`] is the one place that link is written.

pub mod flows;
pub mod models;
pub mod service;
pub mod views;
pub use models::*;
