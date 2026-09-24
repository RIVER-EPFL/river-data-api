use crate::routes::private::tools::flows::{
    applies_at_site, failed_step_awaited, reads_a_measurement,
};
use crate::routes::private::tools::models::{Manifest, parse_manifest};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// A calculation applies at a site only where someone added it there, which declares its outputs
/// (Q325, Q274). Declaring every input is not an assignment.
#[test]
fn a_tool_does_not_apply_where_the_site_declares_only_what_it_reads() {
    let doc = Uuid::new_v4();
    let a254 = Uuid::new_v4();
    let suva = Uuid::new_v4();
    let outputs = vec![("suva".to_string(), suva)];

    let inputs: HashSet<Uuid> = [doc, a254].into_iter().collect();
    assert!(
        !applies_at_site(&outputs, &inputs),
        "holding every input is not the calculation being added at the site"
    );

    let applied: HashSet<Uuid> = [doc, a254, suva].into_iter().collect();
    assert!(applies_at_site(&outputs, &applied));
}

/// A slot the chain minted with `needs_review` is left out of the applied set before it is asked
/// (Q317), so a site holding only that slot holds nothing the calculation writes.
#[test]
fn a_tool_does_not_apply_where_its_only_output_slot_waits_on_review() {
    let doc = Uuid::new_v4();
    let suva = Uuid::new_v4();
    let outputs = vec![("suva".to_string(), suva)];
    let applied: HashSet<Uuid> = [doc].into_iter().collect();
    assert!(!applies_at_site(&outputs, &applied));
}

#[test]
fn a_tool_applies_where_the_site_holds_one_of_its_outputs() {
    let doc = Uuid::new_v4();
    let dom = Uuid::new_v4();
    let outputs = vec![("doc_avg".to_string(), doc), ("doc_sd".to_string(), dom)];
    let applied: HashSet<Uuid> = [doc].into_iter().collect();
    assert!(applies_at_site(&outputs, &applied));
}

#[test]
fn a_tool_the_site_declared_nothing_of_does_not_apply() {
    let outputs = vec![("doc_avg".to_string(), Uuid::new_v4())];
    assert!(!applies_at_site(&outputs, &HashSet::new()));
    assert!(!applies_at_site(
        &outputs,
        &[Uuid::new_v4()].into_iter().collect()
    ));
}

#[test]
fn a_tool_that_saves_nothing_reaches_no_site() {
    assert!(!applies_at_site(&[], &[Uuid::new_v4()].into_iter().collect()));
}

mod reported_skips {
    use crate::routes::private::tools::flows::skipped_entry;

    /// Scenario: a set computes three outputs and one formula reads a parameter the visit does
    /// not hold, so `evaluate_set` records it and the other two save.
    ///
    /// Expected behaviour: the entry names the output and the engine's own reason, which is what
    /// the finding filed against that slot carries.
    #[test]
    fn a_skip_entry_names_the_output_and_the_reason_the_engine_gave() {
        let entry = serde_json::json!({ "output": "doc_avg", "reason": "no value for doc (DOC)" });
        assert_eq!(
            skipped_entry(&entry),
            Some(("doc_avg", "no value for doc (DOC)"))
        );
    }

    /// Anything else in the vector files nothing: a finding with no slot and no reason is worse
    /// than the silence it replaces.
    #[test]
    fn an_entry_missing_either_half_files_nothing() {
        assert_eq!(
            skipped_entry(&serde_json::json!({ "output": "doc_avg" })),
            None
        );
        assert_eq!(skipped_entry(&serde_json::json!({ "reason": "why" })), None);
        assert_eq!(skipped_entry(&serde_json::json!("doc_avg")), None);
    }
}

/// Scenario: a site declares pCO2 as a tool slot the stream engine fills, and the lab enters a
/// visit whose values a calculation producing pCO2 reads.
///
/// Expected behaviour: the chain leaves that output alone and says why, because `readings` holds
/// one row per slot instant and writing it would replace the stream pass's value and provenance.
mod the_arm_a_slot_is_filled_on {
    use crate::routes::private::readings::models::Owner;
    use crate::routes::private::tools::flows::output_skip_reason;

    #[test]
    fn a_high_cadence_slot_is_the_stream_engine_s_and_a_visit_skips_it() {
        let reason = output_skip_reason("pco2", Owner::Tool, Some("high"), false)
            .expect("a high-cadence output is not the chain's to write");
        assert!(reason.contains("pco2"), "{reason}");
        assert!(reason.contains("stream"), "{reason}");
    }

    #[test]
    fn a_low_cadence_slot_is_written_at_the_visit() {
        assert_eq!(
            output_skip_reason("pco2", Owner::Tool, Some("low"), false),
            None
        );
    }

    /// A site holding no slot for the output is the mint case: the run publishes it, so there is
    /// no declaration to read and nothing to skip.
    #[test]
    fn an_undeclared_output_is_written_and_its_slot_minted() {
        assert_eq!(output_skip_reason("pco2", Owner::Tool, None, false), None);
    }

    /// The detached ruling is the older gate and outranks cadence: a manual value stands whatever
    /// arm the slot is on.
    #[test]
    fn a_detached_slot_is_skipped_on_either_arm() {
        for cadence in [Some("low"), Some("high"), None] {
            let reason = output_skip_reason("pco2", Owner::Manual, cadence, false)
                .expect("a detached output is never written by the chain");
            assert!(reason.contains("detached"), "{reason}");
        }
    }

    /// A synced visit's slot the portal already fills stays the portal's until Q310 says how the
    /// chain takes it; an output the portal does not compute is written.
    #[test]
    fn a_slot_the_portal_holds_is_skipped() {
        let reason = output_skip_reason("pco2", Owner::Tool, Some("low"), true)
            .expect("a portal-held output is not replaced by the chain");
        assert!(reason.contains("portal"), "{reason}");
    }
}

/// Scenario: the audit reports on a visit whose calculation publishes to a detached slot, a
/// high-cadence slot, a low-cadence one and one the portal's value holds.
///
/// Expected behaviour: it compares only the slot the chain would write, so every finding it
/// raises is one a recompute can act on.
mod the_outputs_an_audit_compares {
    use crate::routes::private::readings::models::Owner;
    use crate::routes::private::tools::flows::outputs_audited;
    use uuid::Uuid;

    #[test]
    fn the_audit_compares_what_the_chain_writes_and_nothing_else() {
        let (detached, streamed, visit, minted) = (
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
        );
        let audited = outputs_audited(vec![
            (
                "a".to_string(),
                detached,
                Owner::Manual,
                Some("low".to_string()),
                false,
            ),
            (
                "b".to_string(),
                streamed,
                Owner::Tool,
                Some("high".to_string()),
                false,
            ),
            (
                "c".to_string(),
                visit,
                Owner::Tool,
                Some("low".to_string()),
                false,
            ),
            ("d".to_string(), minted, Owner::Tool, None, false),
            (
                "e".to_string(),
                Uuid::from_u128(5),
                Owner::Tool,
                Some("low".to_string()),
                true,
            ),
        ]);
        assert_eq!(
            audited,
            vec![("c".to_string(), visit), ("d".to_string(), minted)]
        );
    }

    #[test]
    fn no_outputs_audit_nothing() {
        assert!(outputs_audited(Vec::new()).is_empty());
    }
}

/// Scenario: three sites, one holding every input a calculation reads, one holding one of its
/// outputs, and one holding the inputs and an output.
///
/// Expected behaviour: the calculation is active at the second and the third, the same verdict
/// `applies_at_site` gives the chain at each.
mod the_sites_a_calculation_is_active_at {
    use crate::routes::private::tools::flows::{applies_at_site, sites_applied};
    use std::collections::{HashMap, HashSet};
    use uuid::Uuid;

    #[test]
    fn a_site_is_counted_where_the_chain_would_fire() {
        let (doc, a254, suva) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let (every_input, holding_output, both) = (
            Uuid::from_u128(10),
            Uuid::from_u128(11),
            Uuid::from_u128(12),
        );
        let outputs = vec![("suva".to_string(), suva)];
        let applied: HashMap<Uuid, HashSet<Uuid>> = [
            (every_input, [doc, a254].into_iter().collect()),
            (holding_output, [suva].into_iter().collect()),
            (both, [doc, a254, suva].into_iter().collect()),
        ]
        .into_iter()
        .collect();

        let mut active = sites_applied(&outputs, &applied);
        active.sort();
        assert_eq!(active, vec![holding_output, both]);
        for (site, parameters) in &applied {
            assert_eq!(active.contains(site), applies_at_site(&outputs, parameters));
        }
    }

    #[test]
    fn a_calculation_that_saves_nothing_is_active_nowhere() {
        let doc = Uuid::from_u128(1);
        let applied: HashMap<Uuid, HashSet<Uuid>> =
            [(Uuid::from_u128(10), [doc].into_iter().collect())]
                .into_iter()
                .collect();
        assert!(sites_applied(&[], &applied).is_empty());
    }
}

/// Scenario: a visit run saves nothing, because its only output is a stream slot it skipped.
///
/// Expected behaviour: nothing is raised for that output; an output the run owned and left without
/// a value is raised, unless a refusal already explains it.
mod a_run_that_saved_nothing {
    use uuid::Uuid;

    use crate::routes::private::tools::flows::unexplained_outputs;

    #[test]
    fn raises_only_the_owned_outputs_no_refusal_explains() {
        let (kept, refused) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let owned = vec![("kept".to_string(), kept), ("refused".to_string(), refused)];
        assert_eq!(
            unexplained_outputs(&owned, &["refused".to_string()]),
            vec![("kept".to_string(), kept)]
        );
    }

    #[test]
    fn raises_nothing_when_every_output_was_skipped_as_another_arm_s() {
        assert!(unexplained_outputs(&[], &[]).is_empty());
    }
}

/// pCO2's shape: two measured scalars and a replicate family, none of them required.
fn pco2() -> Manifest {
    parse_manifest(&serde_json::json!({
        "label": "pco2",
        "params": [
            { "name": "co2", "label": "CO2", "kind": "replicates", "parameter_code": "lab_co2_co2ppm" },
            { "name": "temp", "label": "Temp", "kind": "number", "required": false },
            { "name": "bp", "label": "BP", "kind": "number", "required": false },
            { "name": "kh", "label": "kH", "kind": "number", "required": false, "default": 0.03 }
        ],
        "outputs": [],
        "event_inputs": [
            { "param": "temp", "parameter_code": "lab_co2_lab_temp" },
            { "param": "bp", "parameter_code": "Field_BP" }
        ]
    }))
    .expect("manifest")
}

fn inputs(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[test]
fn test_reads_a_measurement_none_present() {
    // A default is no measurement, and an all-gap family is none either.
    let at_visit = inputs(serde_json::json!({ "kh": 0.03, "co2": [null, null] }));
    assert!(!reads_a_measurement(&pco2(), &at_visit));
}

#[test]
fn test_reads_a_measurement_one_scalar_present() {
    let at_visit = inputs(serde_json::json!({ "kh": 0.03, "bp": 950.0 }));
    assert!(reads_a_measurement(&pco2(), &at_visit));
}

#[test]
fn test_reads_a_measurement_one_replicate_present() {
    let at_visit = inputs(serde_json::json!({ "co2": [null, 412.0] }));
    assert!(reads_a_measurement(&pco2(), &at_visit));
}

#[test]
fn test_reads_a_measurement_no_measured_input_declared() {
    // A calculation over site properties and constants has nothing to be missing at a visit.
    let manifest = parse_manifest(&serde_json::json!({
        "label": "pressure",
        "params": [{ "name": "altitude", "label": "Altitude", "kind": "number" }],
        "outputs": []
    }))
    .expect("manifest");
    assert!(reads_a_measurement(&manifest, &serde_json::Map::new()));
}

#[test]
fn test_failed_step_awaited_names_the_step_that_owed_the_input() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let failed = HashMap::from([(b, "chain_b".to_string())]);
    assert_eq!(failed_step_awaited(&[a, b], &failed), Some("chain_b"));
}

#[test]
fn test_failed_step_awaited_nothing_failed() {
    assert_eq!(
        failed_step_awaited(&[Uuid::new_v4()], &HashMap::new()),
        None
    );
}

#[test]
fn test_failed_step_awaited_failure_elsewhere() {
    let failed = HashMap::from([(Uuid::new_v4(), "chain_b".to_string())]);
    assert_eq!(failed_step_awaited(&[Uuid::new_v4()], &failed), None);
}
