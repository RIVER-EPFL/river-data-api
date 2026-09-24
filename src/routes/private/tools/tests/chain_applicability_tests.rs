use crate::routes::private::tools::flows::applies_at_site;
use std::collections::HashSet;
use uuid::Uuid;

/// A site declares which calculations apply to it by holding slots for what they read (Q193,
/// narrowing Q98): the output slot follows from the inputs and is minted by the run that
/// publishes it. Q98's test stands beside it, for a calculation that reads no parameter.
#[test]
fn a_tool_applies_where_the_site_declares_everything_it_reads() {
    let doc = Uuid::new_v4();
    let a254 = Uuid::new_v4();
    let suva = Uuid::new_v4();
    let outputs = vec![("suva".to_string(), suva)];
    let inputs = vec![doc, a254];

    let both: HashSet<Uuid> = [doc, a254].into_iter().collect();
    assert!(
        applies_at_site(&inputs, &outputs, &both),
        "the site holds the inputs, so it gets the column the run publishes"
    );

    let half: HashSet<Uuid> = [doc].into_iter().collect();
    assert!(
        !applies_at_site(&inputs, &outputs, &half),
        "a read the site does not declare is a calculation it has not asked for"
    );
}

#[test]
fn a_tool_applies_where_the_site_already_holds_one_of_its_outputs() {
    let doc = Uuid::new_v4();
    let dom = Uuid::new_v4();
    let outputs = vec![("doc_avg".to_string(), doc), ("doc_sd".to_string(), dom)];
    let declared: HashSet<Uuid> = [doc].into_iter().collect();

    assert!(
        applies_at_site(&[Uuid::new_v4()], &outputs, &declared),
        "a declared output keeps the calculation the site already asked for"
    );
}

/// A calculation reading only site properties and constants declares no parameter, so the inputs
/// say nothing about where it belongs and its outputs still do.
#[test]
fn a_tool_reading_no_parameter_is_placed_by_its_outputs_alone() {
    let out = Uuid::new_v4();
    let outputs = vec![("pressure".to_string(), out)];
    assert!(applies_at_site(&[], &outputs, &[out].into_iter().collect()));
    assert!(!applies_at_site(&[], &outputs, &HashSet::new()));
}

#[test]
fn a_tool_the_site_declared_nothing_of_does_not_apply() {
    let outputs = vec![("doc_avg".to_string(), Uuid::new_v4())];
    assert!(!applies_at_site(
        &[Uuid::new_v4()],
        &outputs,
        &HashSet::new()
    ));
    assert!(!applies_at_site(
        &[Uuid::new_v4()],
        &outputs,
        &[Uuid::new_v4()].into_iter().collect()
    ));
}

#[test]
fn a_tool_that_saves_nothing_reaches_no_site() {
    assert!(!applies_at_site(
        &[],
        &[],
        &[Uuid::new_v4()].into_iter().collect()
    ));
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

/// Scenario: three sites, one declaring every parameter a calculation reads, one missing an
/// input, and one missing an input but already holding an output.
///
/// Expected behaviour: the calculation is active at the first and the third, the same verdict
/// `applies_at_site` gives the chain at each.
mod the_sites_a_calculation_is_active_at {
    use crate::routes::private::tools::flows::{applies_at_site, sites_applied};
    use std::collections::{HashMap, HashSet};
    use uuid::Uuid;

    #[test]
    fn a_site_is_counted_where_the_chain_would_fire() {
        let (doc, a254, suva) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let (every_input, missing_one, holding_output) = (
            Uuid::from_u128(10),
            Uuid::from_u128(11),
            Uuid::from_u128(12),
        );
        let inputs = vec![doc, a254];
        let outputs = vec![("suva".to_string(), suva)];
        let declared: HashMap<Uuid, HashSet<Uuid>> = [
            (every_input, [doc, a254].into_iter().collect()),
            (missing_one, [doc].into_iter().collect()),
            (holding_output, [doc, suva].into_iter().collect()),
        ]
        .into_iter()
        .collect();

        let mut active = sites_applied(&inputs, &outputs, &declared);
        active.sort();
        assert_eq!(active, vec![every_input, holding_output]);
        for (site, parameters) in &declared {
            assert_eq!(
                active.contains(site),
                applies_at_site(&inputs, &outputs, parameters)
            );
        }
    }

    /// The chain skips a calculation that publishes nothing before it asks where it applies.
    #[test]
    fn a_calculation_that_saves_nothing_is_active_nowhere() {
        let doc = Uuid::from_u128(1);
        let declared: HashMap<Uuid, HashSet<Uuid>> =
            [(Uuid::from_u128(10), [doc].into_iter().collect())]
                .into_iter()
                .collect();
        assert!(sites_applied(&[doc], &[], &declared).is_empty());
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
