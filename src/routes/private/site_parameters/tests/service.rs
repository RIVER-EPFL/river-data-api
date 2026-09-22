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
fn resolve_all_keeps_input_order_and_tolerates_a_missing_catalog_entry() {
    let known = slot();
    let mut unknown = slot();
    unknown.id = Uuid::from_u128(9);
    unknown.parameter_id = Uuid::from_u128(99);

    let mut map = HashMap::new();
    map.insert(known.parameter_id, catalog("mm"));

    let resolved = SlotDescriptor::resolve_all(&[known.clone(), unknown.clone()], &map);
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].id, known.id);
    assert_eq!(resolved[0].units.as_deref(), Some("mm"));
    assert_eq!(resolved[1].id, unknown.id);
    assert_eq!(resolved[1].units, None);
}

/// Expected behaviour: the scope resolves through `site_parameters` whichever way the caller named
/// the slot, and it reads `samples` unaliased so it composes into a find and an update_many alike.
#[test]
fn the_slot_scope_reaches_a_slot_by_id_and_through_a_stream_pairing() {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};

    let mut q = Query::select();
    q.expr(sea_orm::sea_query::Expr::val(1))
        .from(crate::routes::private::readings::samples::Entity)
        .and_where(super::slot_scope(
            &[Uuid::from_u128(1)],
            &[Uuid::from_u128(2)],
        ));
    let sql = q.to_string(PostgresQueryBuilder);

    assert!(
        sql.contains(r#"EXISTS(SELECT 1 FROM "site_parameters" AS "sp""#),
        "the scope resolves through site_parameters: {sql}"
    );
    assert!(
        sql.contains(r#""sp"."site_id" = "samples"."site_id""#)
            && sql.contains(r#""sp"."parameter_id" = "samples"."parameter_id""#),
        "the slot is joined to the sample by site and parameter: {sql}"
    );
    assert!(
        sql.contains(r#"EXISTS(SELECT 1 FROM "data_streams" AS "ds""#)
            && sql.contains(r#""ds"."site_parameter_id" = "sp"."id""#),
        "a stream reaches its slot by its pairing: {sql}"
    );
    assert!(
        !sql.contains(r#"AS "s""#),
        "samples is read unaliased, so an update_many with no alias composes: {sql}"
    );
}

/// A retag that names no stream still scopes by slot id, and one that names no slot still reaches
/// the streams' slots: neither empty list may widen the scope to every sample.
#[test]
fn an_empty_id_list_narrows_the_scope_rather_than_widening_it() {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};

    let render = |slots: &[Uuid], streams: &[Uuid]| {
        let mut q = Query::select();
        q.expr(sea_orm::sea_query::Expr::val(1))
            .from(crate::routes::private::readings::samples::Entity)
            .and_where(super::slot_scope(slots, streams));
        q.to_string(PostgresQueryBuilder)
    };

    let no_streams = render(&[Uuid::from_u128(1)], &[]);
    assert!(
        no_streams.contains(r#""sp"."id" IN ('00000000-0000-0000-0000-000000000001')"#),
        "the slot id is still matched: {no_streams}"
    );
    let no_slots = render(&[], &[Uuid::from_u128(2)]);
    assert!(
        no_slots.contains(r#""ds"."id" IN ('00000000-0000-0000-0000-000000000002')"#),
        "the stream id is still matched: {no_slots}"
    );
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

    let partition = super::partition_calculation(&[doc.clone(), a254.clone()], &[suva.clone()], &held);
    assert_eq!(partition.inputs_present, vec![doc.clone(), a254.clone()]);
    assert!(partition.inputs_missing.is_empty());
    assert!(partition.outputs_existing.is_empty());
    assert_eq!(partition.outputs_to_create, vec![suva.clone()]);

    let without_a254 = [doc.0].into_iter().collect();
    let partition = super::partition_calculation(&[doc.clone(), a254.clone()], &[suva.clone()], &without_a254);
    assert_eq!(partition.inputs_missing, vec![a254.clone()]);
    assert_eq!(partition.outputs_to_create, vec![suva.clone()]);

    let with_suva = [doc.0, a254.0, suva.0].into_iter().collect();
    let partition = super::partition_calculation(&[doc, a254], &[suva.clone()], &with_suva);
    assert_eq!(partition.outputs_existing, vec![suva]);
    assert!(partition.outputs_to_create.is_empty());
}

/// Expected behaviour: a parameter a manifest names twice is partitioned once, so the apply does
/// not try to mint one slot twice in the same transaction.
#[test]
fn test_partition_calculation_counts_a_repeated_parameter_once() {
    let suva = (Uuid::from_u128(12), "suva".to_string());
    let partition =
        super::partition_calculation(&[], &[suva.clone(), suva.clone()], &std::collections::HashSet::new());
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

/// Scenario: a calculation reads the oxygen stream at the instant and a lab alkalinity held from
/// the last visit (Q230). The site declares the first high and the second low.
///
/// Expected behaviour: the arm the outputs run on is decided over the inputs read at the instant
/// alone, so the set publishes on the stream. Counting the held one would put it wholly on the
/// visit arm, which is the case the hold exists for.
#[test]
fn test_a_held_input_does_not_decide_the_arm() {
    let oxygen = (Uuid::from_u128(1), "Dissolved_O2".to_string());
    let alkalinity = (Uuid::from_u128(2), "Alkalinity".to_string());
    let inputs = [oxygen.clone(), alkalinity.clone()];

    let deciding = super::cadence_deciding(&inputs, &["alkalinity".to_string()]);
    assert_eq!(deciding, vec![&oxygen], "the code matches whatever its case");

    assert_eq!(super::cadence_deciding(&inputs, &[]).len(), 2);
    assert!(
        super::cadence_deciding(&inputs, &["dissolved_o2".to_string(), "alkalinity".to_string()])
            .is_empty(),
        "a set holding everything it reads decides nothing, and takes the visit arm"
    );
}
