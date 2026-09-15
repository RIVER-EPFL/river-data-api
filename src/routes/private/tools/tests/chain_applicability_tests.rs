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
