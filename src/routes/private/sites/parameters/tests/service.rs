use std::collections::HashMap;

use uuid::Uuid;

use super::{CatalogParameter, SiteParameterModel, SlotDescriptor};

fn slot() -> SiteParameterModel {
    SiteParameterModel {
        id: Uuid::from_u128(1),
        site_id: Uuid::from_u128(2),
        parameter_id: Uuid::from_u128(3),
        instrument_sensor_id: None,
        name: "Slot name".to_string(),
        sensor_type: "sonde".to_string(),
        sd_estimator: None,
        display_units: None,
        units_name: None,
        units_min: None,
        units_max: None,
        decimal_places: None,
        channel_id: None,
        sample_interval_sec: None,
        is_active: Some(true),
        is_public: Some(false),
        needs_review: false,
        entry_mode: "manual".to_string(),
        variable_mappings: None,
        created_at: None,
        updated_at: None,
        discovered_at: None,
        parameter: Vec::new(),
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
