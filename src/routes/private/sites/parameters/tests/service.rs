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
