//! Barometric pressure from the MeteoSwiss Open Government Data SMN feed.
//!
//! The oxygen-saturation procedure the public API publishes takes a pressure series matched to the
//! water temperature stamps; without one, saturation is computed against a fixed sea-level pressure
//! and carries an elevation-dependent bias at every alpine site.
//!
//! The source is public and needs no credentials, so it is a scheduled job here rather than a sync
//! microservice: there is no enrollment to hold, no cursor the source can re-time, and nothing to
//! deploy alongside the API. Which station serves a site is a property of the site
//! (`sites.meteoswiss_station_abbr`), so the mapping is filled in by an operator and never coded.

pub mod parse;
pub mod sync;
