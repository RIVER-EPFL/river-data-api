use super::{
    BulkWhere, EntityCatalog, InstrumentCatalog, InstrumentNameConflict, PlanCalculationRef,
    PlanEntry, PlanGroupRef, PlanInstrumentRef, apply_bulk_action, apply_group_updates,
    family_parameter_suggestion, group_code, instrument_key, join_named_proposal, linked_entity,
    minted_param_needs_review, plan_calculation, plan_slots, plan_slots_holding_readings,
    proposal_conflict, resolve_parameter_instrument, select_entries, stream_instrument_key,
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

/// Scenario: a stream names an instrument, and the source holds another whose label reads like
/// the stream's curve column.
/// Expected behaviour: the instrument the stream names wins, bookkeeping row or not. Since M172
/// registration mints nothing, so an instrument on the stream is an attribution somebody made. A
/// stream naming none is never matched by its label: words agreeing is not the source naming the
/// analyser, and the operator attaches each curve by hand (Q220).
#[test]
fn test_the_instrument_a_stream_names_wins_and_a_label_suggests_nothing() {
    let bookkeeping = Uuid::new_v4();
    let analyser = Uuid::new_v4();
    let mut c = catalog(&[]);
    c.by_id
        .insert(bookkeeping, ("cnet DOC_avg_ppb".to_string(), None));
    c.by_id.insert(
        analyser,
        ("DOC corr".to_string(), Some("DOC corr".to_string())),
    );
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

    let unnamed = super::resolve_instrument(None, Some("doc_std_curve_id"), "cnet", &c)
        .expect("a curve column resolves");
    assert_eq!(unnamed.resolved_by, "placeholder", "{unnamed:?}");
    assert_eq!(unnamed.id, None, "nothing is pre-selected: {unnamed:?}");
    assert!(unnamed.create && !unnamed.confirmed, "{unnamed:?}");

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
fn test_select_entries_picks_by_what_the_entry_is_set_to() {
    let mut entries = vec![
        plan_entry("FP1", "Depth", "exact", 0),
        plan_entry("FP2", "Depth", "none", 0),
    ];
    entries[1].action = "skip".to_string();

    let set_to = |a: &str| BulkWhere {
        action: Some(a.into()),
        ..Default::default()
    };
    assert_eq!(select_entries(&entries, &set_to("pair")), vec![0]);
    assert_eq!(select_entries(&entries, &set_to("skip")), vec![1]);
}

#[test]
fn test_apply_bulk_action_marks_reviewed_and_counts_each_entry_once() {
    let mut entries = vec![
        plan_entry("FP1", "Depth", "none", 0),
        plan_entry("FP1", "CDOM", "none", 0),
    ];
    entries[1].acknowledged = true;
    entries[1].action = "skip".to_string();

    let changed = apply_bulk_action(
        &mut entries,
        &BulkWhere::default(),
        Some("pair"),
        Some(true),
    );
    assert_eq!(changed, 2);
    assert!(entries.iter().all(|e| e.acknowledged && e.action == "pair"));

    let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), None, Some(false));
    assert_eq!(changed, 2, "and unmarking is its inverse");
    assert!(entries.iter().all(|e| !e.acknowledged));
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

    let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), Some("pair"), None);
    assert_eq!(changed, 1, "only the entry that names a slot moves");
    assert_eq!(entries[0].action, "skip");
    assert_eq!(entries[1].action, "skip");
    assert_eq!(entries[2].action, "pair");

    let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), Some("skip"), None);
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
        serde_json::json!({ "parameter": { "source_calculation": "calcPCO2" } }),
        serde_json::json!({
            "parameter": { "source_calculation": { "function": "calcPCO2", "inputs": "lab_co2" } }
        }),
        serde_json::json!({
            "parameter": { "source_calculation": { "function": 7, "inputs": ["a"] } }
        }),
        serde_json::json!(null),
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

/// A project or site renamed in the review pairs onto the catalog entry it now names, matched
/// without regard to case, and is proposed for creation under any other name.
#[test]
fn test_reclassify_entry_resolves_a_renamed_project_and_site_against_the_catalog() {
    let project = Uuid::new_v4();
    let site = Uuid::new_v4();
    let catalog = EntityCatalog {
        projects: vec![(project, "METALP".to_string())],
        sites: vec![(site, "Martigny".to_string())],
        ..EntityCatalog::default()
    };
    let mut entry = plan_entry("FP1", "Depth", "none", 0);

    entry.project.name = "metalp".to_string();
    entry.site.name = "martigny".to_string();
    super::reclassify_entry(&mut entry, &catalog);
    assert_eq!(entry.project.id, Some(project));
    assert!(!entry.project.create);
    assert_eq!(entry.site.id, Some(site));
    assert!(!entry.site.create);

    entry.project.name = "Glacier streams".to_string();
    entry.site.name = "Glacier 1 downstream".to_string();
    super::reclassify_entry(&mut entry, &catalog);
    assert_eq!(entry.project.id, None);
    assert!(entry.project.create);
    assert_eq!(entry.site.id, None);
    assert!(entry.site.create);
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

#[test]
fn test_linked_entity_resolves_a_source_name_onto_the_renamed_row() {
    let site = Uuid::new_v4();
    let links = HashMap::from([("S01".to_string(), site)]);
    let existing = vec![(site, "Val Ferret upstream".to_string())];
    assert_eq!(
        linked_entity(Some("S01"), "s01", &links, &existing),
        Some((site, "Val Ferret upstream".to_string()))
    );
}

#[test]
fn test_linked_entity_ignores_an_entry_renamed_in_the_plan() {
    let site = Uuid::new_v4();
    let links = HashMap::from([("S01".to_string(), site)]);
    let existing = vec![(site, "Val Ferret upstream".to_string())];
    assert_eq!(
        linked_entity(Some("S01"), "Somewhere else", &links, &existing),
        None
    );
    assert_eq!(linked_entity(None, "S01", &links, &existing), None);
}

#[test]
fn test_linked_entity_unlinked_name_resolves_nothing() {
    let links = HashMap::new();
    assert_eq!(linked_entity(Some("S01"), "S01", &links, &[]), None);
}

fn proposal(source_key: &str, name: &str, confirmed: bool) -> PlanInstrumentRef {
    serde_json::from_value(serde_json::json!({
        "curve_column": null, "id": null, "name": name, "source_key": source_key,
        "resolved_by": "placeholder", "create": true, "confirmed": confirmed,
        "stamps_readings": false, "curves": [], "proposed_name": name,
    }))
    .expect("a proposal")
}

fn with_instrument(parameter: &str, instrument: Option<PlanInstrumentRef>) -> PlanEntry {
    let mut entry = plan_entry("FP1", parameter, "none", 0);
    entry.instrument = instrument;
    entry
}

#[test]
fn test_join_named_proposal_merges_into_the_instrument_of_that_name() {
    let mut entries = vec![
        with_instrument("A", Some(proposal("cnet:A", "A", true))),
        with_instrument("A_T", Some(proposal("cnet:A_T", "a", false))),
        with_instrument("A_T", Some(proposal("cnet:A_T", "a", false))),
    ];
    let named = entries[1].stream_id;
    join_named_proposal(&mut entries, named, "cnet");

    assert!(
        entries
            .iter()
            .all(|e| instrument_key(e) == "instrument:cnet:A"),
        "every A_T stream is on A, matched regardless of case"
    );
    assert!(
        entries
            .iter()
            .all(|e| e.instrument.as_ref().is_some_and(|i| i.confirmed)),
        "and the row keeps A's review"
    );
}

#[test]
fn test_join_named_proposal_brings_in_a_parameter_waiting_on_that_suggestion() {
    let mut entries = vec![
        with_instrument("A", None),
        with_instrument("A_T", Some(proposal("cnet:A_T", "A", true))),
    ];
    let named = entries[1].stream_id;
    join_named_proposal(&mut entries, named, "cnet");

    assert_eq!(instrument_key(&entries[0]), "instrument:cnet:A");
    assert_eq!(instrument_key(&entries[1]), "instrument:cnet:A");
    assert!(
        entries
            .iter()
            .all(|e| e.instrument.as_ref().is_some_and(|i| !i.confirmed)),
        "a suggestion nobody accepted is not reviewed by joining it"
    );
}

#[test]
fn test_join_named_proposal_leaves_a_name_of_its_own_and_an_existing_instrument_alone() {
    let mut existing = proposal("cnet:B", "B", true);
    existing.create = false;
    existing.id = Some(Uuid::new_v4());
    let mut entries = vec![
        with_instrument("B", Some(existing)),
        with_instrument("A_T", Some(proposal("cnet:A_T", "B", false))),
        with_instrument("C", Some(proposal("cnet:C", "Fresh", false))),
    ];
    let (to_existing, fresh) = (entries[1].stream_id, entries[2].stream_id);
    join_named_proposal(&mut entries, to_existing, "cnet");
    join_named_proposal(&mut entries, fresh, "cnet");

    assert_eq!(
        instrument_key(&entries[1]),
        "instrument:cnet:A_T",
        "an inventory row is attached by id, not by name"
    );
    assert_eq!(instrument_key(&entries[2]), "instrument:cnet:C");
}

/// Expected behaviour: the slots a plan's apply re-derives are read as a built statement, once per
/// slot, scoped to that plan. Written as SQL it silently returned nothing on a database error and
/// the apply reported success.
#[test]
fn test_a_plan_reads_its_slots_once_each_through_the_pairing() {
    use sea_orm::QueryTrait as _;
    let sql = plan_slots(Uuid::nil())
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    assert!(sql.contains("SELECT DISTINCT"), "one row per slot: {sql}");
    assert!(
        sql.contains(r#""site_parameters"."site_id""#)
            && sql.contains(r#""site_parameters"."parameter_id""#),
        "the slot is the pairing's, not the stream's: {sql}"
    );
    assert!(
        sql.contains(r#"INNER JOIN "site_parameters""#),
        "an unpaired stream names no slot: {sql}"
    );
    assert!(
        sql.contains(r#""data_streams"."pairing_plan_id" ="#),
        "scoped to the plan: {sql}"
    );
}

/// Expected behaviour: a lab row with nothing attached holds the apply, which would otherwise mint
/// a `{source}:{parameter}` instrument nobody named. A device feed drafted without one is its own
/// channel instrument and does not.
#[test]
fn test_a_pairing_row_with_no_instrument_holds_the_apply() {
    let lab = with_instrument("DOC", None);
    let source_key = lab.source_key.clone();
    let mut device = with_instrument("Water Depth", None);
    device.is_device = true;
    let entries = vec![lab, device];
    assert_eq!(super::unconfirmed_instruments(&entries), [source_key]);
    assert!(super::refuse_unconfirmed_instruments(&entries).is_err());
}

fn catalog_param(code: &str, units: &str) -> super::CatalogParam {
    super::CatalogParam {
        id: Uuid::new_v4(),
        code: code.to_string(),
        name: code.to_string(),
        aliases: vec![],
        units: units.to_string(),
        category: "measurement".to_string(),
        site_parameter_count: 0,
        reading_count: 0,
    }
}

fn param_catalog(params: Vec<super::CatalogParam>) -> EntityCatalog {
    EntityCatalog {
        params,
        ..Default::default()
    }
}

fn typed_code(stream_id: Uuid, code: &str) -> PlanEntryUpdate {
    serde_json::from_value(serde_json::json!({
        "stream_id": stream_id,
        "parameter_name": code,
    }))
    .expect("a plan entry update")
}

/// Scenario: the operator types `DOC` over a column the source calls `DOC_ppb`, and the catalog
/// already holds a `DOC`.
///
/// Expected behaviour: the typed code creates a parameter and joins nothing, and the plan says the
/// code is taken rather than quietly attaching years of readings to this column (Q221).
#[test]
fn test_a_typed_code_creates_a_parameter_and_never_joins_an_existing_one() {
    let doc = catalog_param("DOC", "ppb");
    let catalog = param_catalog(vec![doc]);
    let mut entry = plan_entry("FP1", "DOC_ppb", "none", 0);

    entry.parameter.name = "DOC".to_string();
    entry.parameter.attach = Some(super::PlanParamAttach::New);
    super::reclassify_entry(&mut entry, &catalog);

    assert_eq!(entry.parameter.id, None, "{:?}", entry.parameter);
    assert!(entry.parameter.create, "{:?}", entry.parameter);
    let kinds: Vec<&str> = entry.warnings.iter().map(|w| w.kind.as_str()).collect();
    assert!(
        kinds.contains(&"catalog_match"),
        "the taken code is reported: {kinds:?}"
    );
}

/// The catalog parameter chosen from the list, for a column whose own name matches nothing, is what
/// the entry resolves to: the choice decides the identity, not the name beside it.
#[test]
fn test_an_explicit_attachment_resolves_to_the_catalog_parameter() {
    let doc = catalog_param("DOC", "ppb");
    let doc_id = doc.id;
    let catalog = param_catalog(vec![doc]);
    let mut entry = plan_entry("FP1", "DOC_ppb", "none", 0);

    entry.parameter.units = "ppb".to_string();
    entry.parameter.attach = Some(super::PlanParamAttach::Existing { id: doc_id });
    super::reclassify_entry(&mut entry, &catalog);

    assert_eq!(entry.parameter.id, Some(doc_id), "{:?}", entry.parameter);
    assert!(!entry.parameter.create, "{:?}", entry.parameter);
    assert!(entry.warnings.is_empty(), "{:?}", entry.warnings);
}

/// An entry nobody has edited still resolves by the source's own name: that match is the plan's
/// proposal, not something an operator typed.
#[test]
fn test_an_undecided_entry_still_matches_the_source_s_own_name() {
    let doc = catalog_param("DOC_ppb", "ppb");
    let doc_id = doc.id;
    let catalog = param_catalog(vec![doc]);
    let mut entry = plan_entry("FP1", "DOC_ppb", "none", 0);
    entry.parameter.units = "ppb".to_string();

    super::reclassify_entry(&mut entry, &catalog);

    assert_eq!(entry.parameter.attach, None, "{:?}", entry.parameter);
    assert_eq!(entry.parameter.id, Some(doc_id), "{:?}", entry.parameter);
    assert!(!entry.parameter.create, "{:?}", entry.parameter);
}

/// An attachment to a parameter that has left the catalog since the plan was reviewed creates one
/// under the entry's code rather than pairing to an id that no longer resolves.
#[test]
fn test_an_attachment_to_a_vanished_parameter_falls_back_to_creating_one() {
    let mut entry = plan_entry("FP1", "DOC_ppb", "none", 0);
    entry.parameter.attach = Some(super::PlanParamAttach::Existing { id: Uuid::new_v4() });

    super::reclassify_entry(&mut entry, &EntityCatalog::default());

    assert_eq!(entry.parameter.id, None, "{:?}", entry.parameter);
    assert!(entry.parameter.create, "{:?}", entry.parameter);
}

/// Typing a code records the choice, so it survives the plan being reloaded.
#[test]
fn test_typing_a_code_records_the_new_parameter_choice() {
    let entry = plan_entry("FP1", "DOuM", "none", 0);
    let update = typed_code(entry.stream_id, "DO_uM");

    assert_eq!(update.parameter_name.as_deref(), Some("DO_uM"));
    assert_eq!(
        update.parameter_attach, None,
        "the typed code carries no choice of its own; the route records `new`"
    );
}

/// Scenario: DOuM and DOdegC are both proposed as `DO`, and a third column is typed over with a
/// code the catalog already holds.
///
/// Expected behaviour: the apply refuses both, since `LOWER(code)` is unique and the second insert
/// would fail halfway through the run.
#[test]
fn test_two_codes_that_cannot_both_be_created_refuse_the_apply() {
    let mut um = plan_entry("FP1", "DO", "none", 0);
    um.parameter.units = "uM".to_string();
    let mut degc = plan_entry("FP1", "DO", "none", 0);
    degc.parameter.units = "degC".to_string();
    let mut taken = plan_entry("FP2", "DOC", "none", 0);
    taken.parameter.attach = Some(super::PlanParamAttach::New);
    let entries = vec![um, degc, taken];
    let catalog = param_catalog(vec![catalog_param("DOC", "ppb")]);

    let collisions = super::colliding_parameter_codes(&entries, &catalog);
    assert_eq!(collisions.len(), 2, "{collisions:?}");
    assert!(
        collisions.iter().any(|c| c.contains("'do'")),
        "the two unit sets are named: {collisions:?}"
    );
    assert!(
        collisions.iter().any(|c| c.contains("'DOC'")),
        "the taken catalog code is named: {collisions:?}"
    );
    assert!(
        super::refuse_colliding_parameter_codes(&entries, &catalog).is_err(),
        "the apply refuses"
    );
}

/// A skipped row is not applied, so its code collides with nothing.
#[test]
fn test_a_skipped_row_s_code_collides_with_nothing() {
    let mut um = plan_entry("FP1", "DO", "none", 0);
    um.parameter.units = "uM".to_string();
    let mut degc = plan_entry("FP1", "DO", "none", 0);
    degc.parameter.units = "degC".to_string();
    degc.action = "skip".to_string();

    assert!(super::colliding_parameter_codes(&[um, degc], &EntityCatalog::default()).is_empty(),);
}
/// Expected behaviour: the re-derivation after an apply visits only the slots that hold a reading.
/// On the CNET apply of 2026-09-22, 1124 of 2852 slots held none and cost 35 ms each for nothing.
#[test]
fn test_a_plan_attributes_only_the_slots_holding_readings() {
    use sea_orm::QueryTrait as _;
    let sql = plan_slots_holding_readings(Uuid::nil())
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    assert!(
        sql.contains("EXISTS"),
        "a slot with no reading is skipped: {sql}"
    );
    assert!(
        sql.contains(r#"FROM "readings""#),
        "the test is against the readings: {sql}"
    );
    assert!(
        sql.contains(r#""readings"."site_id" = "site_parameters"."site_id""#)
            && sql.contains(r#""readings"."parameter_id" = "site_parameters"."parameter_id""#),
        "correlated to the slot, not to any reading: {sql}"
    );
    assert!(
        sql.contains(r#""data_streams"."pairing_plan_id" ="#),
        "still scoped to the plan: {sql}"
    );
}

/// Scenario: DOuM and DOdegC are both proposed as `DO`, the station is spelled `FP1` on one row and
/// `FP-1` on another, and a third row pairs onto a site nothing else names.
///
/// Expected behaviour: each row of a pair is told what it collides with, in the plan rather than at
/// apply; the row colliding with nothing carries no warning.
#[test]
fn test_the_plan_names_what_two_of_its_own_rows_would_create_twice() {
    let mut um = plan_entry("FP1", "DO", "none", 0);
    um.parameter.units = "uM".to_string();
    um.warnings.clear();
    let mut degc = plan_entry("FP-1", "DO", "none", 0);
    degc.parameter.units = "degC".to_string();
    degc.warnings.clear();
    let mut alone = plan_entry("Saxon", "Depth", "none", 0);
    alone.warnings.clear();
    let mut entries = vec![um, degc, alone];

    super::flag_duplicates_in_plan(&mut entries);

    let kinds = |e: &super::PlanEntry| -> Vec<String> {
        e.warnings.iter().map(|w| w.kind.clone()).collect()
    };
    assert_eq!(
        kinds(&entries[0]),
        ["duplicate_parameter_code", "duplicate_site_name"],
        "{:?}",
        entries[0].warnings
    );
    assert_eq!(
        kinds(&entries[1]),
        ["duplicate_parameter_code", "duplicate_site_name"],
    );
    assert!(entries[0].warnings[0].message.contains("degC"));
    assert!(entries[0].warnings[1].message.contains("FP-1"));
    assert!(kinds(&entries[2]).is_empty(), "{:?}", entries[2].warnings);
}

/// A skipped row creates nothing, so it collides with nothing.
#[test]
fn test_a_skipped_row_is_not_a_duplicate() {
    let mut um = plan_entry("FP1", "DO", "none", 0);
    um.parameter.units = "uM".to_string();
    um.warnings.clear();
    let mut degc = plan_entry("FP1", "DO", "none", 0);
    degc.parameter.units = "degC".to_string();
    degc.action = "skip".to_string();
    degc.warnings.clear();
    let mut entries = vec![um, degc];

    super::flag_duplicates_in_plan(&mut entries);

    assert!(entries[0].warnings.is_empty(), "{:?}", entries[0].warnings);
}

/// An entry attaching to a parameter or a site that already exists creates nothing, so two of them
/// under one code is one parameter used twice, not a collision.
#[test]
fn test_an_attached_row_is_not_a_duplicate() {
    let existing = Uuid::new_v4();
    let mut a = plan_entry("FP1", "DO", "none", 0);
    a.parameter.units = "uM".to_string();
    a.parameter.create = false;
    a.parameter.id = Some(existing);
    a.warnings.clear();
    let mut b = plan_entry("FP1", "DO", "none", 0);
    b.parameter.units = "degC".to_string();
    b.parameter.create = false;
    b.parameter.id = Some(existing);
    b.warnings.clear();
    let mut entries = vec![a, b];

    super::flag_duplicates_in_plan(&mut entries);

    assert!(entries[0].warnings.is_empty(), "{:?}", entries[0].warnings);
}

#[test]
fn test_near_duplicate_names_both_spellings_in_one_sentence() {
    let warning = super::PlanWarning::near_duplicate("site", "FP-1", "FP1");
    assert_eq!(warning.kind, "near_duplicate");
    assert_eq!(
        warning.message,
        "This plan would create the site 'FP-1', and 'FP1' already exists. They differ only in \
         case, spacing or punctuation."
    );
    assert!(warning.parameter.is_none());
    assert!(warning.existing.is_none());
}

#[test]
fn test_plan_group_reads_the_declared_category() {
    let group = super::plan_group(&serde_json::json!({
        "parameter": {
            "category": "  Dissolved Gases ",
            "category_ordinal": 3,
            "description": " CO2 and CH4 ",
        }
    }))
    .expect("a declared category is a group");
    assert_eq!(group.code, "dissolved_gases");
    assert_eq!(group.label, "Dissolved Gases");
    assert_eq!(group.ordinal, 3);
    assert_eq!(group.description.as_deref(), Some("CO2 and CH4"));
    assert!(group.create);
}

#[test]
fn test_plan_group_defaults_what_the_source_leaves_out() {
    let group = super::plan_group(&serde_json::json!({
        "parameter": { "category": "Nutrients", "category_ordinal": 1_i64 << 40, "description": " " }
    }))
    .expect("a declared category is a group");
    assert_eq!(group.ordinal, 0);
    assert!(group.description.is_none());
}

#[test]
fn test_plan_group_is_none_without_a_category() {
    assert!(super::plan_group(&serde_json::json!({})).is_none());
    assert!(super::plan_group(&serde_json::json!({ "parameter": {} })).is_none());
    assert!(super::plan_group(&serde_json::json!({ "parameter": { "category": "  " } })).is_none());
    assert!(super::plan_group(&serde_json::json!({ "parameter": { "category": 4 } })).is_none());
}

#[test]
fn test_plan_replicates_counts_the_declared_columns() {
    let metadata = serde_json::json!({
        "replicates": {
            "source_columns": ["doc_1", "doc_2", "doc_3"],
            "curve_ref_column": "doc_std_curve_id",
            "portal_mean_column": "doc_avg",
            "portal_sd_column": "doc_sd",
        }
    });
    let reps = super::plan_replicates(&metadata).expect("a replicate spec");
    assert_eq!(reps.n, 3);
    assert_eq!(reps.member_columns, vec!["doc_1", "doc_2", "doc_3"]);
    assert_eq!(reps.curve_ref_column.as_deref(), Some("doc_std_curve_id"));
    assert_eq!(reps.portal_mean_column.as_deref(), Some("doc_avg"));
    assert_eq!(reps.portal_sd_column.as_deref(), Some("doc_sd"));
}

#[test]
fn test_plan_replicates_is_none_for_a_single_column_stream() {
    assert!(super::plan_replicates(&serde_json::json!({ "parameter": {} })).is_none());
}

fn column(idx: usize, name: &str, units: &str) -> (usize, String, String) {
    (idx, name.to_string(), units.to_string())
}

fn grouped(entries: &[(usize, String, String)]) -> Vec<(String, String, Vec<usize>)> {
    let mut groups: Vec<_> = super::group_streams_by_parameter(entries)
        .into_iter()
        .map(|g| {
            let mut idx = g.entry_indices;
            idx.sort_unstable();
            (g.proposed_name, g.units, idx)
        })
        .collect();
    groups.sort();
    groups
}

/// Scenario: a station exports lettered and suffixed columns that share a unit.
/// Expected behaviour: only identical names group, case aside; a shared unit or a shared stem is
/// not one parameter.
#[test]
fn test_group_streams_by_parameter_groups_identical_names_only() {
    let entries = vec![
        column(0, "Nitrate", "µg/L"),
        column(1, "Ammonia", "µg/L"),
        column(2, "nitrate", "µg/L"),
        column(3, "Chla_a", "µg/L"),
        column(4, "Chla_b", "µg/L"),
        column(5, "Chla_acid_ugL", "µg/L"),
        column(6, "Chla_acid_ugm2", "µg/m2"),
        column(7, "Nitrate", "mg/L"),
    ];
    let groups = grouped(&entries);
    assert_eq!(groups.len(), 7, "{groups:?}");
    let nitrate: Vec<_> = groups
        .iter()
        .filter(|(name, _, _)| name.eq_ignore_ascii_case("nitrate"))
        .collect();
    assert_eq!(nitrate.len(), 2, "{nitrate:?}");
    assert!(
        nitrate
            .iter()
            .any(|(_, units, idx)| units == "µg/l" && *idx == vec![0, 2])
    );
    assert!(
        nitrate
            .iter()
            .any(|(_, units, idx)| units == "mg/l" && *idx == vec![7])
    );
}

#[test]
fn test_group_streams_by_parameter_keeps_every_spelling_it_folded() {
    let entries = vec![
        column(0, "DOC", "ppb"),
        column(1, "doc", "ppb"),
        column(2, "DOC", "ppb"),
    ];
    let proposals = super::group_streams_by_parameter(&entries);
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].original_names, vec!["DOC", "doc"]);
    assert_eq!(proposals[0].entry_indices.len(), 3);
}

#[test]
fn test_group_streams_by_parameter_of_nothing_is_nothing() {
    assert!(super::group_streams_by_parameter(&[]).is_empty());
}

fn sensor(metadata: Option<serde_json::Value>) -> crate::routes::private::sensors::Model {
    serde_json::from_value(serde_json::json!({
        "id": Uuid::new_v4(),
        "kind": "device",
        "data_frequency": "high",
        "metadata": metadata,
        "deployments": [],
    }))
    .expect("a sensor")
}

#[test]
fn test_is_minted_default_reads_the_registration_marker() {
    let marker = crate::routes::private::sensors::models::MINTED_FROM_STREAM;
    assert!(super::is_minted_default(&sensor(Some(
        serde_json::json!({ marker: "cnet:DOC_avg_ppb" })
    ))));
    assert!(!super::is_minted_default(&sensor(None)));
    assert!(!super::is_minted_default(&sensor(Some(
        serde_json::json!({ "serial": "X1" })
    ))));
}

fn coordinates(
    stream_id: Uuid,
    lat: Option<f64>,
    lon: Option<f64>,
    alt: Option<f64>,
) -> PlanEntryUpdate {
    serde_json::from_value(serde_json::json!({
        "stream_id": stream_id,
        "site_latitude": lat,
        "site_longitude": lon,
        "site_altitude_m": alt,
    }))
    .expect("a plan entry update")
}

/// Scenario: a new station has three feeds and another new station one; the operator edits the
/// coordinates on one feed of the first.
/// Expected behaviour: every feed of that station takes the edit, whatever the case of its name,
/// and the other station is untouched.
#[test]
fn test_apply_site_attribute_updates_moves_every_entry_of_the_site() {
    let mut entries = vec![
        plan_entry("FP1", "Depth", "none", 0),
        plan_entry("fp1", "CDOM", "none", 0),
        plan_entry("FP1", "Turbidity", "none", 0),
        plan_entry("FP2", "Depth", "none", 0),
    ];
    let update = coordinates(entries[1].stream_id, Some(46.1), None, Some(1520.0));
    super::apply_site_attribute_updates(&mut entries, &[update]);
    for entry in &entries[..3] {
        assert_eq!(entry.site.latitude, Some(46.1));
        assert_eq!(entry.site.longitude, None);
        assert_eq!(entry.site.altitude_m, Some(1520.0));
    }
    assert_eq!(entries[3].site.latitude, None);
    assert_eq!(entries[3].site.altitude_m, None);
}

#[test]
fn test_apply_site_attribute_updates_leaves_an_existing_site_alone() {
    let mut entries = vec![
        plan_entry("FP1", "Depth", "none", 0),
        plan_entry("FP1", "CDOM", "none", 0),
    ];
    let existing = Uuid::new_v4();
    for entry in &mut entries {
        entry.site.id = Some(existing);
    }
    let update = coordinates(entries[0].stream_id, Some(46.1), Some(7.2), Some(1520.0));
    super::apply_site_attribute_updates(&mut entries, &[update]);
    assert!(entries.iter().all(|e| e.site.latitude.is_none()));
}

#[test]
fn test_apply_site_attribute_updates_ignores_an_entry_that_does_not_exist() {
    let mut entries = vec![plan_entry("FP1", "Depth", "none", 0)];
    let update = coordinates(Uuid::new_v4(), Some(46.1), Some(7.2), Some(1520.0));
    super::apply_site_attribute_updates(&mut entries, &[update]);
    assert!(entries[0].site.latitude.is_none());
    assert!(entries[0].site.longitude.is_none());
}

#[test]
fn test_apply_site_attribute_updates_skips_an_update_naming_no_coordinate() {
    let mut entries = vec![plan_entry("FP1", "Depth", "none", 0)];
    entries[0].site.latitude = Some(45.0);
    let update = coordinates(entries[0].stream_id, None, None, None);
    super::apply_site_attribute_updates(&mut entries, &[update]);
    assert_eq!(entries[0].site.latitude, Some(45.0));
}

fn decision(key: &str, accepted: bool) -> crate::routes::private::sync::models::PlanObjectUpdate {
    crate::routes::private::sync::models::PlanObjectUpdate {
        key: key.to_string(),
        accepted,
    }
}

#[test]
fn test_apply_object_updates_keeps_the_first_acceptance() {
    let mut accepted = Vec::new();
    super::apply_object_updates(&mut accepted, &[decision("site:FP1", true)], "alice");
    super::apply_object_updates(&mut accepted, &[decision("site:FP1", true)], "bob");
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].key, "site:FP1");
    assert_eq!(accepted[0].accepted_by.as_deref(), Some("alice"));
}

#[test]
fn test_apply_object_updates_takes_a_decision_back() {
    let mut accepted = Vec::new();
    super::apply_object_updates(
        &mut accepted,
        &[decision("site:FP1", true), decision("parameter:DOC", true)],
        "alice",
    );
    super::apply_object_updates(
        &mut accepted,
        &[decision("site:FP1", false), decision("project:CNET", false)],
        "bob",
    );
    let keys: Vec<_> = accepted.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, vec!["parameter:DOC"]);
}

fn holds_sql(scope: &crate::common::authz::AccessScope) -> Option<String> {
    use sea_orm::{EntityTrait, QueryFilter, QueryTrait};
    super::holds_in_scope(scope).map(|condition| {
        super::hold_model::Entity::find()
            .filter(condition)
            .build(sea_orm::DatabaseBackend::Postgres)
            .to_string()
    })
}

#[test]
fn test_holds_in_scope_confines_nothing_for_an_unrestricted_caller() {
    assert!(holds_sql(&crate::common::authz::AccessScope::Unrestricted).is_none());
}

/// Scenario: a caller confined to one project reads the review queue.
/// Expected behaviour: a hold is in scope through its stream's pairing or through the site a
/// stream-less finding names, and both arms confine to that project.
#[test]
fn test_holds_in_scope_reaches_a_hold_through_its_pairing_or_its_site() {
    let project = Uuid::new_v4();
    let sql = holds_sql(&crate::common::authz::AccessScope::one(project)).expect("a condition");
    assert_eq!(sql.matches("EXISTS").count(), 2, "{sql}");
    assert_eq!(sql.matches(&project.to_string()).count(), 2, "{sql}");
    assert!(
        sql.contains(r#""sp_scope"."id" = "ds"."site_parameter_id""#),
        "{sql}"
    );
    assert!(sql.contains(r#""st"."id" = "h"."site_id""#), "{sql}");
    assert!(sql.contains(" OR "), "{sql}");
}

#[test]
fn test_holds_in_scope_with_no_projects_confines_to_nothing() {
    let scope = crate::common::authz::AccessScope::Projects(std::sync::Arc::new(HashSet::new()));
    let sql = holds_sql(&scope).expect("a condition");
    assert!(sql.contains("1 = 2"), "{sql}");
}
