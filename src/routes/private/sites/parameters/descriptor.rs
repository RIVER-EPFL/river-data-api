//! The per-slot header block every site-scoped endpoint reports alongside a series.
//!
//! One `(site, parameter)` slot must describe itself identically whichever endpoint is asked, so
//! the catalog fallbacks (units, name, sensor type) are resolved here and nowhere else. Endpoints
//! differ in the JSON field names they publish, not in the values, so this type exposes every
//! resolved value and each response struct copies the ones it publishes.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use uuid::Uuid;

use super::model::Model as SiteParameterModel;
use crate::error::AppResult;
use crate::routes::private::parameters;

/// The global catalog row a slot points at.
#[derive(Debug, Clone)]
pub struct CatalogParameter {
    pub code: String,
    pub name: String,
    pub default_units: String,
}

/// Load the catalog rows for a set of parameter ids, keyed by parameter id.
pub async fn catalog_map(
    db: &DatabaseConnection,
    parameter_ids: impl IntoIterator<Item = Uuid>,
) -> AppResult<HashMap<Uuid, CatalogParameter>> {
    let ids: Vec<Uuid> = parameter_ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(ids))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            (
                p.id,
                CatalogParameter {
                    code: p.code,
                    name: p.name,
                    default_units: p.default_units,
                },
            )
        })
        .collect())
}

/// Everything an endpoint needs to label one slot, with the catalog fallbacks already applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDescriptor {
    /// `site_parameters.id`.
    pub id: Uuid,
    /// Global catalog parameter id.
    pub parameter_id: Uuid,
    /// Catalog `code`, empty when the catalog row is missing.
    pub code: String,
    /// Catalog `name`, absent when the catalog row is missing. Published as `display_name` by the
    /// series endpoints.
    pub catalog_name: Option<String>,
    /// The slot's own `name`. Published as `name` by the series endpoints.
    pub slot_name: String,
    /// Catalog name falling back to the slot name. Published as `name` by the site detail and
    /// parameter-list projections.
    pub name: String,
    /// Slot `sensor_type`, falling back to the slot name when unset.
    pub sensor_type: String,
    /// Site override falling back to the catalog `default_units`. An empty catalog value is no
    /// units at all, so it resolves to `None` rather than an empty string.
    pub units: Option<String>,
    /// The site override alone, unresolved, for clients that distinguish an override from a
    /// fallback.
    pub display_units: Option<String>,
    /// Display precision the client formats with. The served values keep full precision.
    pub decimal_places: Option<i16>,
}

impl SlotDescriptor {
    /// Resolve one slot against its catalog row (`None` when the parameter is missing from the
    /// catalog, which leaves the code empty and the units unresolved).
    pub fn resolve(slot: &SiteParameterModel, catalog: Option<&CatalogParameter>) -> Self {
        let catalog_name = catalog.map(|c| c.name.clone());
        let units = slot.display_units.clone().or_else(|| {
            catalog
                .map(|c| c.default_units.clone())
                .filter(|u| !u.is_empty())
        });
        Self {
            id: slot.id,
            parameter_id: slot.parameter_id,
            code: catalog.map(|c| c.code.clone()).unwrap_or_default(),
            name: catalog_name.clone().unwrap_or_else(|| slot.name.clone()),
            catalog_name,
            slot_name: slot.name.clone(),
            sensor_type: if slot.sensor_type.is_empty() {
                slot.name.clone()
            } else {
                slot.sensor_type.clone()
            },
            units,
            display_units: slot.display_units.clone(),
            decimal_places: slot.decimal_places,
        }
    }

    /// Resolve a batch of slots against one catalog map, preserving input order.
    pub fn resolve_all(
        slots: &[SiteParameterModel],
        catalog: &HashMap<Uuid, CatalogParameter>,
    ) -> Vec<Self> {
        slots
            .iter()
            .map(|s| Self::resolve(s, catalog.get(&s.parameter_id)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> SiteParameterModel {
        SiteParameterModel {
            id: Uuid::from_u128(1),
            site_id: Uuid::from_u128(2),
            parameter_id: Uuid::from_u128(3),
            name: "Slot name".to_string(),
            sensor_type: "sonde".to_string(),
            display_units: None,
            units_name: None,
            units_min: None,
            units_max: None,
            decimal_places: None,
            channel_id: None,
            sample_interval_sec: None,
            is_active: Some(true),
            is_public: Some(false),
            is_derived: Some(false),
            derived_definition_id: None,
            variable_mappings: None,
            created_at: None,
            updated_at: None,
            discovered_at: None,
            parameter: Vec::new(),
            derived_definition: None,
        }
    }

    fn catalog(default_units: &str) -> CatalogParameter {
        CatalogParameter {
            code: "DOuM".to_string(),
            name: "Dissolved Oxygen".to_string(),
            default_units: default_units.to_string(),
        }
    }

    #[test]
    fn site_override_wins_over_the_catalog_default() {
        let mut s = slot();
        s.display_units = Some("K".to_string());
        let d = SlotDescriptor::resolve(&s, Some(&catalog("uM")));
        assert_eq!(d.units.as_deref(), Some("K"));
        assert_eq!(d.display_units.as_deref(), Some("K"));
    }

    #[test]
    fn a_slot_without_an_override_reports_the_catalog_default() {
        let d = SlotDescriptor::resolve(&slot(), Some(&catalog("uM")));
        assert_eq!(d.units.as_deref(), Some("uM"));
        assert_eq!(d.display_units, None);
    }

    #[test]
    fn an_empty_catalog_default_is_no_units() {
        let d = SlotDescriptor::resolve(&slot(), Some(&catalog("")));
        assert_eq!(d.units, None);
    }

    #[test]
    fn a_missing_catalog_row_leaves_the_code_empty_and_names_fall_back_to_the_slot() {
        let d = SlotDescriptor::resolve(&slot(), None);
        assert_eq!(d.code, "");
        assert_eq!(d.catalog_name, None);
        assert_eq!(d.name, "Slot name");
        assert_eq!(d.units, None);
    }

    #[test]
    fn an_empty_sensor_type_falls_back_to_the_slot_name() {
        let mut s = slot();
        s.sensor_type = String::new();
        let d = SlotDescriptor::resolve(&s, Some(&catalog("uM")));
        assert_eq!(d.sensor_type, "Slot name");
    }

    #[test]
    fn catalog_and_slot_names_are_both_reported() {
        let d = SlotDescriptor::resolve(&slot(), Some(&catalog("uM")));
        assert_eq!(d.name, "Dissolved Oxygen");
        assert_eq!(d.catalog_name.as_deref(), Some("Dissolved Oxygen"));
        assert_eq!(d.slot_name, "Slot name");
    }

    #[test]
    fn decimal_places_travels_with_the_slot() {
        let mut s = slot();
        s.decimal_places = Some(1);
        let d = SlotDescriptor::resolve(&s, Some(&catalog("uM")));
        assert_eq!(d.decimal_places, Some(1));
    }

    #[test]
    fn resolve_all_keeps_input_order_and_tolerates_a_missing_catalog_entry() {
        let mut known = slot();
        known.display_units = Some("mm".to_string());
        let mut unknown = slot();
        unknown.id = Uuid::from_u128(9);
        unknown.parameter_id = Uuid::from_u128(99);

        let mut map = HashMap::new();
        map.insert(known.parameter_id, catalog("uM"));

        let resolved = SlotDescriptor::resolve_all(&[known.clone(), unknown.clone()], &map);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].id, known.id);
        assert_eq!(resolved[0].units.as_deref(), Some("mm"));
        assert_eq!(resolved[1].id, unknown.id);
        assert_eq!(resolved[1].units, None);
    }
}
