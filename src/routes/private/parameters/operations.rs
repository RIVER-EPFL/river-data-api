use crudcrate::CRUDOperations;

use super::model::Parameter;

/// No custom CRUD hooks.
///
/// Alarm thresholds are intentionally NOT auto-created from a parameter's `default_*` columns when
/// the defaults change. Alarm evaluation already falls back to those defaults (priority 3) whenever
/// no `alarm_thresholds` row exists, so materializing default-valued rows is redundant — and a
/// site-specific copy would silently shadow a global threshold an operator set. A threshold row
/// exists only when a user explicitly creates one via the editor.
pub struct ParameterOperations;

impl CRUDOperations for ParameterOperations {
    type Resource = Parameter;
}
