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
        decimal_places: None,
        sample_interval_sec: None,
        is_active: Some(true),
        is_public: Some(false),
        needs_review: false,
        entry_mode: "manual".to_string(),
        cadence: "high".to_string(),
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
fn units_are_the_catalog_default() {
    let d = SlotDescriptor::resolve(&slot(), Some(&catalog("uM")));
    assert_eq!(d.units.as_deref(), Some("uM"));
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
fn resolve_tolerates_a_slot_whose_parameter_the_catalog_map_lacks() {
    let known = slot();
    let mut unknown = slot();
    unknown.id = Uuid::from_u128(9);
    unknown.parameter_id = Uuid::from_u128(99);

    let mut map = HashMap::new();
    map.insert(known.parameter_id, catalog("mm"));

    let resolved: Vec<_> = [&known, &unknown]
        .into_iter()
        .map(|s| SlotDescriptor::resolve(s, map.get(&s.parameter_id)))
        .collect();
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].id, known.id);
    assert_eq!(resolved[0].units.as_deref(), Some("mm"));
    assert_eq!(resolved[1].id, unknown.id);
    assert_eq!(resolved[1].units, None);
}

/// Expected behaviour: a slot that measured nothing deletes, and one that holds readings is
/// refused with the count in the message. Deleting the second nulls `site_id` and `parameter_id`
/// on every reading it carried, which is what the Active toggle exists to avoid (Q160).
#[test]
fn test_only_a_slot_that_measured_nothing_can_be_deleted() {
    assert_eq!(super::refuse_delete_of_measured_slot(0), None);

    let refusal = super::refuse_delete_of_measured_slot(14_203).expect("a measured slot refuses");
    assert!(refusal.contains("14203"), "the count is named: {refusal}");
    assert!(
        refusal.contains("Active"),
        "the refusal points at the toggle that retires: {refusal}"
    );
}

/// Scenario: `suva` reads DOC and a254 and writes suva; Martigny declares DOC and a254 but not
/// suva.
///
/// Expected behaviour: both inputs are present, the output is the one slot the apply mints, and
/// an output the site already carries is left where it is.
#[test]
fn test_partition_calculation_separates_missing_inputs_from_outputs_to_mint() {
    let doc = (Uuid::from_u128(10), "DOC".to_string());
    let a254 = (Uuid::from_u128(11), "a254".to_string());
    let suva = (Uuid::from_u128(12), "suva".to_string());
    let held = [doc.0, a254.0].into_iter().collect();

    let partition = super::partition_calculation(
        &[doc.clone(), a254.clone()],
        std::slice::from_ref(&suva),
        &held,
    );
    assert_eq!(partition.inputs_present, vec![doc.clone(), a254.clone()]);
    assert!(partition.inputs_missing.is_empty());
    assert!(partition.outputs_existing.is_empty());
    assert_eq!(partition.outputs_to_create, vec![suva.clone()]);

    let without_a254 = [doc.0].into_iter().collect();
    let partition = super::partition_calculation(
        &[doc.clone(), a254.clone()],
        std::slice::from_ref(&suva),
        &without_a254,
    );
    assert_eq!(partition.inputs_missing, vec![a254.clone()]);
    assert_eq!(partition.outputs_to_create, vec![suva.clone()]);

    let with_suva = [doc.0, a254.0, suva.0].into_iter().collect();
    let partition =
        super::partition_calculation(&[doc, a254], std::slice::from_ref(&suva), &with_suva);
    assert_eq!(partition.outputs_existing, vec![suva]);
    assert!(partition.outputs_to_create.is_empty());
}

/// Expected behaviour: a parameter a manifest names twice is partitioned once, so the apply does
/// not try to mint one slot twice in the same transaction.
#[test]
fn test_partition_calculation_counts_a_repeated_parameter_once() {
    let suva = (Uuid::from_u128(12), "suva".to_string());
    let partition = super::partition_calculation(
        &[],
        &[suva.clone(), suva.clone()],
        &std::collections::HashSet::new(),
    );
    assert_eq!(partition.outputs_to_create, vec![suva]);
}

/// Expected behaviour: the output slot joins the stream arm only where every input the
/// calculation reads is a high-cadence slot at that site (Q234).
#[test]
fn test_applied_cadence_follows_the_inputs() {
    let high = || Some("high".to_string());
    let low = || Some("low".to_string());
    assert_eq!(super::applied_cadence(&[high(), high()]), "high");
    assert_eq!(super::applied_cadence(&[high(), low()]), "low");
    assert_eq!(super::applied_cadence(&[high(), None]), "low");
    assert_eq!(super::applied_cadence(&[]), "low");
}

fn member(id: Uuid, code: &str) -> super::GroupMember {
    (id, code.to_string(), code.to_string())
}

/// Scenario: a group names DOC, a254 and DOC again; the site already carries a254.
/// Expected behaviour: a254 is existing, DOC is created once.
#[test]
fn test_partition_members_splits_held_slots_from_those_to_create() {
    let doc = Uuid::new_v4();
    let a254 = Uuid::new_v4();
    let members = vec![member(doc, "DOC"), member(a254, "a254"), member(doc, "DOC")];
    let held = std::collections::HashSet::from([a254]);
    let (create, existing) = super::partition_members(&members, &held);
    assert_eq!(create, vec![member(doc, "DOC")]);
    assert_eq!(existing, vec![member(a254, "a254")]);
}

#[test]
fn test_partition_members_of_an_empty_group_is_empty() {
    let (create, existing) = super::partition_members(&[], &std::collections::HashSet::new());
    assert!(create.is_empty());
    assert!(existing.is_empty());
}

#[test]
fn test_another_sites_assignment_does_not_suppress_this_one() {
    use super::assignment_dedupe_key;

    let calculation = Uuid::from_u128(10);
    let martigny = Uuid::from_u128(20);
    let saxon = Uuid::from_u128(21);
    assert_ne!(
        assignment_dedupe_key(calculation, martigny),
        assignment_dedupe_key(calculation, saxon),
    );
    assert_eq!(
        assignment_dedupe_key(calculation, martigny),
        assignment_dedupe_key(calculation, martigny),
        "a second assignment at one site coalesces while the first waits"
    );
    assert_ne!(
        assignment_dedupe_key(calculation, martigny),
        assignment_dedupe_key(Uuid::from_u128(11), martigny),
    );
}

fn read(code: &str, cadence: Option<&str>) -> (String, Option<String>) {
    (code.to_string(), cadence.map(str::to_string))
}

/// Expected behaviour: one measured input on the stream is a stream calculation (Q252).
#[test]
fn test_stream_refusal_one_stream_input_runs() {
    assert_eq!(
        super::stream_refusal(&[read("Vaisala_CO2", Some("high"))]),
        None
    );
    assert_eq!(super::stream_refusal(&[]), None);
}

#[test]
fn test_stream_refusal_names_a_second_measured_input() {
    let refusal = super::stream_refusal(&[
        read("Vaisala_DO", Some("high")),
        read("Vaisala_Temp", Some("high")),
    ])
    .expect("refused");
    assert!(refusal.contains("Vaisala_DO, Vaisala_Temp"), "{refusal}");
}

#[test]
fn test_stream_refusal_names_a_visit_only_input() {
    let refusal = super::stream_refusal(&[read("Alkalinity", Some("low"))]).expect("refused");
    assert!(refusal.contains("Alkalinity"), "{refusal}");
    assert!(super::stream_refusal(&[read("Alkalinity", None)]).is_some());
}
