use super::{
    BulkWhere, EntityCatalog, InstrumentCatalog, InstrumentNameConflict, PlanCalculationRef,
    PlanEntry, PlanGroupRef, apply_bulk_action, apply_group_updates, family_parameter_suggestion,
    group_code, minted_param_needs_review, plan_calculation, proposal_conflict,
    resolve_parameter_instrument, select_entries, stream_instrument_key,
};
use crate::routes::private::sync::models::PlanEntryUpdate;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

fn catalog(entries: &[(&str, Uuid)]) -> InstrumentCatalog {
    InstrumentCatalog {
        by_id: entries
            .iter()
            .map(|(key, id)| (*id, ((*key).to_string(), Some((*key).to_string()))))
            .collect(),
        labels: vec![],
        by_source_key: entries
            .iter()
            .map(|(key, id)| ((*key).to_string(), *id))
            .collect(),
        by_name: HashMap::new(),
        by_serial: HashMap::new(),
        curves: HashMap::new(),
        defaulted: std::collections::HashSet::new(),
    }
}

/// Scenario: a stream names an instrument and the source's own curve catalog holds another
/// whose label matches the stream's curve column.
/// Expected behaviour: the instrument the stream names wins, bookkeeping row or not. Since
/// M172 registration mints nothing, so an instrument on the stream is an attribution somebody
/// made and there is no default to see through. The curve label answers only for a stream that
/// names none.
#[test]
fn test_the_instrument_a_stream_names_wins_over_a_matching_curve_label() {
    let bookkeeping = Uuid::new_v4();
    let analyser = Uuid::new_v4();
    let mut c = catalog(&[]);
    c.by_id
        .insert(bookkeeping, ("cnet DOC_avg_ppb".to_string(), None));
    c.by_id.insert(
        analyser,
        ("DOC corr".to_string(), Some("DOC corr".to_string())),
    );
    c.labels.push(("doc".to_string(), analyser));
    c.defaulted.insert(bookkeeping);

    let named = super::resolve_instrument(Some(bookkeeping), Some("doc_std_curve_id"), "cnet", &c)
        .expect("a stream instrument resolves");
    assert_eq!(named.resolved_by, "stream", "{named:?}");
    assert_eq!(named.id, Some(bookkeeping));
    assert!(named.defaulted, "{named:?}");

    let attributed =
        super::resolve_instrument(Some(analyser), Some("doc_std_curve_id"), "cnet", &c)
            .expect("a stream instrument resolves");
    assert_eq!(attributed.resolved_by, "stream", "{attributed:?}");
    assert_eq!(attributed.id, Some(analyser));

    // A stream naming none is where the curve label is read.
    let by_label = super::resolve_instrument(None, Some("doc_std_curve_id"), "cnet", &c)
        .expect("a curve column resolves");
    assert_eq!(by_label.resolved_by, "curve_label", "{by_label:?}");
    assert_eq!(by_label.id, Some(analyser));

    // With nothing to match, the question stands unanswered and the plan proposes one.
    let alone = super::resolve_instrument(None, Some("tss_std_curve_id"), "cnet", &c)
        .expect("a curve column resolves");
    assert_eq!(alone.resolved_by, "placeholder", "{alone:?}");
    assert!(alone.create, "{alone:?}");
    // Unconfirmed: the apply refuses until an operator agrees to the name (Q123).
    assert!(!alone.confirmed, "{alone:?}");

    // Nothing to resolve from at all: no instrument on the stream and no curve column.
    assert!(super::resolve_instrument(None, None, "cnet", &c).is_none());
}

#[test]
fn test_an_attributed_instrument_is_not_reported_as_a_default() {
    let attributed = Uuid::new_v4();
    let mut c = catalog(&[]);
    c.by_id.insert(
        attributed,
        ("Hach DR3900".to_string(), Some("cnet:DOC".to_string())),
    );
    c.by_source_key.insert("cnet:DOC".to_string(), attributed);

    let by_stream = super::resolve_instrument(Some(attributed), None, "cnet", &c)
        .expect("a stream instrument resolves");
    assert!(!by_stream.defaulted, "{by_stream:?}");

    let by_key = super::resolve_parameter_instrument("cnet:DOC".to_string(), "DOC", &c);
    assert_eq!(by_key.id, Some(attributed));
    assert!(!by_key.defaulted, "{by_key:?}");

    let proposed = super::resolve_parameter_instrument("cnet:TSS".to_string(), "TSS", &c);
    assert!(proposed.create, "{proposed:?}");
    assert!(!proposed.defaulted, "{proposed:?}");
}

/// A catalog holding one instrument by name and nothing else, for the collision cases.
fn catalog_named(name: &str, id: Uuid, has_readings: bool) -> InstrumentCatalog {
    let mut c = catalog(&[]);
    c.by_name.insert(
        name.to_lowercase(),
        super::InstrumentNameConflict {
            id,
            name: name.to_string(),
            source_system: Some("metalp".to_string()),
            has_readings,
        },
    );
    c
}

fn family_stream() -> super::data_streams::Model {
    let now = chrono::Utc::now().into();
    super::data_streams::Model {
        id: Uuid::new_v4(),
        source_system: "cnet".to_string(),
        source_key: "FP3:DOC_avg_ppb:reps".to_string(),
        source_name: None,
        source_path: None,
        metadata: serde_json::json!({
            "hierarchy": { "project": "CNET", "site": "FP3", "parameter": "DOC_avg_ppb" }
        }),
        site_parameter_id: None,
        sensor_id: None,
        measurement_type: None,
        is_active: true,
        discovered_at: now,
        paired_at: None,
        last_data_time: None,
        last_window_digest: None,
        pairing_plan_id: None,
        created_at: now,
        updated_at: now,
        replicates: None,
    }
}

/// The plan proposes the instrument the pairing mints, for a replicate family too: the
/// suggested parameter is a label, and keying the proposal on it mints a second row for the
/// same analyte.
#[test]
fn test_the_plan_proposes_the_key_the_pairing_mints() {
    let stream = family_stream();
    let suggestion = family_parameter_suggestion("DOC_avg_ppb");
    assert_ne!(suggestion, "DOC_avg_ppb", "the suggestion is a label");

    let proposed =
        resolve_parameter_instrument(stream_instrument_key(&stream), &suggestion, &catalog(&[]));

    assert_eq!(proposed.source_key, "cnet:DOC_avg_ppb");
}

#[test]
fn test_resolve_parameter_instrument_takes_the_source_s_own() {
    let id = Uuid::new_v4();
    let resolved = resolve_parameter_instrument(
        "cnet:NO2_mgL".to_string(),
        "NO2_mgL",
        &catalog(&[("cnet:NO2_mgL", id)]),
    );
    assert_eq!(resolved.id, Some(id));
    assert!(
        !resolved.create,
        "an instrument that exists is not created again"
    );
    assert!(resolved.confirmed);
}

/// Expected behaviour: a parameter with no instrument is proposed, not agreed. The plan suggests
/// and a person confirms, so an untouched plan creates nothing nobody looked at.
#[test]
fn test_resolve_parameter_instrument_proposes_one_nobody_has_confirmed() {
    let proposed =
        resolve_parameter_instrument("cnet:NO2_mgL".to_string(), "NO2_mgL", &catalog(&[]));
    assert_eq!(proposed.id, None);
    assert_eq!(proposed.source_key, "cnet:NO2_mgL");
    assert!(proposed.create);
    assert!(!proposed.confirmed, "a suggestion waits for a person");
    assert_eq!(
        proposed.proposed_name.as_deref(),
        Some("NO2_mgL"),
        "the name is the analyte; the source is provenance and lives in source_key"
    );
    assert_eq!(proposed.name, "NO2_mgL");
    assert!(proposed.name_conflict.is_none());
}

/// Expected behaviour: the lab's DOC analyser is one machine carried to every station, so a
/// proposal that would create a second instrument called `DOC` is a decision, not a
/// suggestion. It is reported unconfirmed with the row it collides with, and apply refuses an
/// unconfirmed proposal, so the operator has to say which they meant.
#[test]
fn test_a_proposed_name_an_instrument_already_carries_is_put_to_the_operator() {
    let existing = Uuid::new_v4();
    let proposed = resolve_parameter_instrument(
        "cnet:DOC".to_string(),
        "DOC",
        &catalog_named("DOC", existing, true),
    );
    assert!(
        proposed.create,
        "attaching is one of the two answers, not the default"
    );
    assert!(
        !proposed.confirmed,
        "a collision is never agreed to on the operator's behalf"
    );
    let conflict = proposed.name_conflict.expect("the collision is reported");
    assert_eq!(conflict.id, existing);
    assert_eq!(conflict.name, "DOC");
    assert!(
        conflict.has_readings,
        "attaching would add to readings it already holds, which is what must be said"
    );
}

/// The comparison is on the name a person reads, so case and surrounding space are not a
/// second instrument.
#[test]
fn test_a_collision_ignores_case_and_padding() {
    let existing = Uuid::new_v4();
    let proposed = resolve_parameter_instrument(
        "cnet:doc".to_string(),
        "  doc  ",
        &catalog_named("DOC", existing, false),
    );
    assert_eq!(
        proposed.name_conflict.map(|c| c.id),
        Some(existing),
        "`doc` and `DOC` are one instrument to the person choosing"
    );
}

pub fn plan_entry(site: &str, parameter: &str, confidence: &str, warnings: usize) -> PlanEntry {
    let entry = serde_json::json!({
        "stream_id": Uuid::new_v4(),
        "source_key": format!("{site}:{parameter}"),
        "source_name": null,
        "action": "pair",
        "project": { "id": null, "name": "CNET", "create": true },
        "site": { "id": null, "name": site, "create": true,
                  "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": null, "name": parameter, "label": null, "create": true,
                       "units": "mm", "group_key": null, "original_names": [] },
        "confidence": confidence,
        "warnings": (0..warnings)
            .map(|i| serde_json::json!({ "kind": "units_mismatch", "message": i.to_string() }))
            .collect::<Vec<_>>(),
    });
    serde_json::from_value(entry).expect("a plan entry")
}

#[test]
fn test_select_entries_picks_exactly_each_predicate_s_set() {
    let entries = vec![
        plan_entry("FP1", "Depth", "exact", 0),
        plan_entry("FP1", "CDOM", "none", 1),
        plan_entry("FP2", "Depth", "none", 0),
    ];

    assert_eq!(
        select_entries(&entries, &BulkWhere::default()),
        vec![0, 1, 2]
    );
    assert_eq!(
        select_entries(
            &entries,
            &BulkWhere {
                confidence: Some("none".into()),
                ..Default::default()
            }
        ),
        vec![1, 2]
    );
    assert_eq!(
        select_entries(
            &entries,
            &BulkWhere {
                has_warnings: Some(true),
                ..Default::default()
            }
        ),
        vec![1]
    );
    assert_eq!(
        select_entries(
            &entries,
            &BulkWhere {
                site_name: Some("fp1".into()),
                ..Default::default()
            }
        ),
        vec![0, 1],
        "the site is matched case-insensitively, as the review renders it"
    );
    assert_eq!(
        select_entries(
            &entries,
            &BulkWhere {
                confidence: Some("none".into()),
                parameter_name: Some("Depth".into()),
                ..Default::default()
            }
        ),
        vec![2],
        "predicates narrow together"
    );
}

#[test]
fn test_apply_bulk_action_never_pairs_an_entry_with_no_slot() {
    let mut entries = vec![
        plan_entry("", "Depth", "none", 0),
        plan_entry("FP1", "", "none", 0),
        plan_entry("FP1", "Depth", "none", 0),
    ];
    for entry in &mut entries {
        entry.action = "skip".to_string();
    }

    let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), "pair");
    assert_eq!(changed, 1, "only the entry that names a slot moves");
    assert_eq!(entries[0].action, "skip");
    assert_eq!(entries[1].action, "skip");
    assert_eq!(entries[2].action, "pair");

    let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), "skip");
    assert_eq!(changed, 1, "and skipping is its inverse");
    assert!(entries.iter().all(|e| e.action == "skip"));
}

/// Scenario: CNET declares on the descriptor for `CO2_HS_Um_avg` the portal function that writes
/// it and the columns that function reads.
/// Expected behaviour: the plan carries both, so the member row states what the portal computed
/// and a column still waiting for a formula set is the one carrying nothing.
#[test]
fn test_plan_calculation_carries_the_declared_function_and_its_inputs() {
    let metadata = serde_json::json!({
        "parameter": {
            "column_name": "CO2_HS_Um_avg",
            "source_calculation": {
                "function": "calcPCO2",
                "inputs": ["lab_co2_co2ppm", "WTW_Temp_degC_1", "Field_BP"],
            },
        },
    });
    assert_eq!(
        plan_calculation(&metadata),
        Some(PlanCalculationRef {
            function: "calcPCO2".to_string(),
            inputs: vec![
                "lab_co2_co2ppm".to_string(),
                "WTW_Temp_degC_1".to_string(),
                "Field_BP".to_string(),
            ],
        })
    );
}

/// A half-declaration is not a calculation anybody can read back, so none of these is carried.
#[test]
fn test_plan_calculation_refuses_a_declaration_missing_either_half() {
    let cases = [
        serde_json::json!({ "parameter": { "column_name": "WTW_pH_1" } }),
        serde_json::json!({ "parameter": { "source_calculation": null } }),
        serde_json::json!({ "parameter": { "source_calculation": { "inputs": ["a"] } } }),
        serde_json::json!({
            "parameter": { "source_calculation": { "function": "  ", "inputs": ["a"] } }
        }),
        serde_json::json!({
            "parameter": { "source_calculation": { "function": "calcPCO2", "inputs": [] } }
        }),
        serde_json::json!({
            "parameter": { "source_calculation": { "function": "calcPCO2", "inputs": ["", " "] } }
        }),
    ];
    for metadata in cases {
        assert_eq!(plan_calculation(&metadata), None, "{metadata}");
    }
}

/// Scenario: the source's register offers a probe the inventory already holds, under the same
/// name in one case and under the same serial in another.
/// Expected behaviour: the row names the instrument it would duplicate, so the review can attach
/// instead of leaving two rows for one probe (Q76). A name match is answered first: it is what a
/// person reads in the inventory.
#[test]
fn test_a_register_row_names_the_instrument_it_would_duplicate() {
    let named = Uuid::new_v4();
    let serialled = Uuid::new_v4();
    let mut c = catalog(&[]);
    c.by_name.insert(
        "doc".to_string(),
        InstrumentNameConflict {
            id: named,
            name: "DOC".to_string(),
            source_system: Some("cnet".to_string()),
            has_readings: true,
        },
    );
    c.by_serial.insert(
        "SN-42".to_string(),
        InstrumentNameConflict {
            id: serialled,
            name: "Probe 42".to_string(),
            source_system: None,
            has_readings: false,
        },
    );

    assert_eq!(
        proposal_conflict("doc", None, &c).map(|h| h.id),
        Some(named),
        "the name is matched case-insensitively"
    );
    assert_eq!(
        proposal_conflict("Anything", Some(" SN-42 "), &c).map(|h| h.id),
        Some(serialled),
        "a serial another instrument carries is the same probe"
    );
    assert_eq!(
        proposal_conflict("doc", Some("SN-42"), &c).map(|h| h.id),
        Some(named),
        "the name answers first"
    );
    assert!(proposal_conflict("Fresh", Some("SN-99"), &c).is_none());
    assert!(
        proposal_conflict("Fresh", Some("   "), &c).is_none(),
        "a blank serial matches nothing"
    );
}

/// A plan entry placed in a category the source declares.
fn grouped_entry(parameter: &str, group_label: &str) -> PlanEntry {
    let mut entry = plan_entry("Site 1", parameter, "high", 0);
    entry.parameter.group = Some(PlanGroupRef {
        id: None,
        code: group_code(group_label),
        label: group_label.to_string(),
        ordinal: 0,
        description: None,
        create: true,
    });
    entry
}

fn update_group(
    stream_id: Uuid,
    label: Option<&str>,
    description: Option<&str>,
) -> PlanEntryUpdate {
    serde_json::from_value(serde_json::json!({
        "stream_id": stream_id,
        "group_label": label,
        "group_description": description,
    }))
    .expect("a plan entry update")
}

/// Scenario: the lab wants the portal's "Field data" category to read "Field measurements" before
/// the plan is applied, and edits it on one of the columns it holds.
///
/// Expected behaviour: every column of that category is renamed, and the code follows the label so
/// the apply creates one group under the new name.
#[test]
fn test_renaming_a_proposed_group_renames_every_column_of_its_category() {
    let mut entries = vec![
        grouped_entry("WTW_pH_1", "Field data"),
        grouped_entry("Field_BP", "Field data"),
        grouped_entry("DOC_ppb", "DOM"),
    ];
    let update = update_group(entries[1].stream_id, Some("Field measurements"), None);
    apply_group_updates(&mut entries, &[update], &EntityCatalog::default());

    let groups: Vec<(String, String)> = entries
        .iter()
        .map(|e| {
            let g = e.parameter.group.as_ref().expect("a group");
            (g.label.clone(), g.code.clone())
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (
                "Field measurements".to_string(),
                "field_measurements".to_string()
            ),
            (
                "Field measurements".to_string(),
                "field_measurements".to_string()
            ),
            ("DOM".to_string(), "dom".to_string()),
        ]
    );
}

/// Renaming onto the label of a group the database already holds joins that group instead of
/// proposing a second one under a name it already carries.
#[test]
fn test_renaming_a_proposed_group_onto_an_existing_one_joins_it() {
    let existing = Uuid::new_v4();
    let catalog = EntityCatalog {
        groups: vec![(existing, "dom".to_string())],
        ..EntityCatalog::default()
    };
    let mut entries = vec![grouped_entry("DOC_ppb", "Dissolved organics")];
    let update = update_group(entries[0].stream_id, Some("DOM"), None);
    apply_group_updates(&mut entries, &[update], &catalog);

    let group = entries[0].parameter.group.as_ref().expect("a group");
    assert_eq!(group.id, Some(existing));
    assert!(!group.create, "it joins the group that exists");
}

/// An empty description clears the source's own, which is how the review takes a description back
/// rather than being unable to.
#[test]
fn test_a_blank_group_description_clears_it() {
    let mut entries = vec![grouped_entry("WTW_pH_1", "Field data")];
    entries[0].parameter.group.as_mut().unwrap().description = Some("from the portal".to_string());
    let update = update_group(entries[0].stream_id, None, Some("   "));
    apply_group_updates(&mut entries, &[update], &EntityCatalog::default());

    assert_eq!(
        entries[0].parameter.group.as_ref().unwrap().description,
        None
    );
}

/// The review's acceptance of the object is the review the flag asks for, so an accepted mint
/// lands clear and one nobody accepted stays flagged.
#[test]
fn test_minted_param_needs_review_follows_the_accepted_objects() {
    let accepted: HashSet<String> = ["parameter:Turbidity".to_string()].into_iter().collect();

    assert!(!minted_param_needs_review(&accepted, "Turbidity"));
    assert!(minted_param_needs_review(&accepted, "CDOM"));
    assert!(
        minted_param_needs_review(&accepted, "turbidity"),
        "the key is the name the review card carried, matched as it was written"
    );
    assert!(
        minted_param_needs_review(&HashSet::new(), "Turbidity"),
        "a plan with nothing accepted mints nothing reviewed"
    );
}

/// Expected behaviour: a device feed's channel is proposed as an instrument named for the slot it
/// serves, keyed on the feed itself, and waits for a person like any other creation (Q195).
#[test]
fn test_resolve_device_instrument_proposes_the_slot_unconfirmed() {
    let proposed = super::resolve_device_instrument("1270", "Martigny Water Depth", &catalog(&[]));
    assert_eq!(proposed.id, None);
    assert_eq!(proposed.source_key, "1270", "the channel is the identity");
    assert_eq!(proposed.name, "Martigny Water Depth");
    assert_eq!(
        proposed.proposed_name.as_deref(),
        Some("Martigny Water Depth")
    );
    assert!(proposed.create);
    assert!(!proposed.confirmed, "a suggestion waits for a person");
    assert!(!proposed.stamps_readings);
}

#[test]
fn test_resolve_device_instrument_takes_the_channel_s_own() {
    let id = Uuid::new_v4();
    let resolved =
        super::resolve_device_instrument("1270", "Martigny Water Depth", &catalog(&[("1270", id)]));
    assert_eq!(resolved.id, Some(id));
    assert!(
        !resolved.create,
        "a channel already in the inventory is not created again"
    );
    assert!(resolved.confirmed);
}

/// Expected behaviour: the apply refuses a device channel nobody confirmed, as it does a lab one.
#[test]
fn test_unconfirmed_instruments_counts_a_device_channel() {
    let mut entry = plan_entry("Martigny", "Water Depth", "exact", 0);
    entry.is_device = true;
    entry.instrument = Some(super::resolve_device_instrument(
        &entry.source_key,
        "Martigny Water Depth",
        &catalog(&[]),
    ));
    let source_key = entry.source_key.clone();
    let entries = vec![entry];
    assert_eq!(
        super::unconfirmed_instruments(&entries),
        vec![source_key.as_str()]
    );
    assert!(super::refuse_unconfirmed_instruments(&entries).is_err());
}

/// Expected behaviour: a channel serving an existing parameter is proposed under the inventory's
/// name for it, which is the name the slot already reads as, not the source's label.
#[test]
fn test_device_slot_name_reads_an_existing_parameter_as_the_inventory_names_it() {
    let parameter = Uuid::new_v4();
    let catalog = EntityCatalog {
        params: vec![super::CatalogParam {
            id: parameter,
            code: "water_temperature".to_string(),
            name: "Water Temperature".to_string(),
            aliases: vec![],
            units: "°C".to_string(),
            category: "measurement".to_string(),
            site_parameter_count: 0,
            reading_count: 0,
        }],
        ..EntityCatalog::default()
    };
    let mut entry = plan_entry("Martigny", "water temperature", "exact", 0);
    assert_eq!(
        super::device_slot_name(&entry, &catalog),
        "Martigny water temperature",
        "a parameter the apply creates keeps the plan's name"
    );
    entry.parameter.id = Some(parameter);
    assert_eq!(
        super::device_slot_name(&entry, &catalog),
        "Martigny Water Temperature"
    );
}
