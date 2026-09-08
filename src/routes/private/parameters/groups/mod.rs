//! Parameter groups: the portal's categories, their members and the document a form renders from.
//!
//! **Presentation precedence.** Every field a form shows resolves member over catalog, one
//! `COALESCE` each (`definition.rs`):
//!
//! - `label`: the member's, else `parameters.name`
//! - `units`: the member's, else `parameters.default_units` (empty string reads as absent)
//! - `description`: the member's, else `parameters.description`
//!
//! `decimal_places` is the exception and resolves nothing from the member, because a group does not
//! declare one (Q120): it is `site_parameters.decimal_places` when the caller names a site, else the
//! platform default of 2. Rounding is presentation and full resolution is what is stored, so a slot
//! that wants more or fewer places declares them, and the number a form shows is the number the
//! public API rounds with. `site_parameters` also supplies `sd_estimator`, and only when the caller
//! names a site: the divisor is declared per slot and is never inferred.

pub mod definition;
pub mod intermediates;
pub mod group_model;
pub mod member_model;
pub mod operations;
pub mod ordering;
pub mod rules;
