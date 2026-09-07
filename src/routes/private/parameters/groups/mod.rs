//! Parameter groups: the portal's categories, their members and the document a form renders from.
//!
//! **Presentation precedence.** Every field a form shows resolves member over catalog, one
//! `COALESCE` each (`definition.rs`):
//!
//! - `label`: the member's, else `parameters.name`
//! - `units`: the member's, else `parameters.default_units` (empty string reads as absent)
//! - `description`: the member's, else `parameters.description`
//! - `decimal_places`: the member's, else none
//!
//! `site_parameters` enters only for `sd_estimator`, and only when the caller names a site: the
//! divisor is declared per slot and is never inferred. The site's own `decimal_places`, which is
//! what the public API rounds with, is not in the chain, so a member declaring none carries none
//! and a consumer that rounds does it from `/sites/{id}/parameters`.

pub mod definition;
pub mod intermediates;
pub mod group_model;
pub mod member_model;
pub mod operations;
pub mod ordering;
pub mod rules;
