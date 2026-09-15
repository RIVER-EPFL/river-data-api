use crate::routes::private::tools::flows::applies_at_site;
use std::collections::HashSet;
use uuid::Uuid;

/// A site declares which calculations apply to it by holding slots for their outputs (Q98).
#[test]
fn a_tool_applies_where_the_site_holds_one_of_its_outputs() {
    let doc = Uuid::new_v4();
    let dom = Uuid::new_v4();
    let outputs = vec![("doc_avg".to_string(), doc), ("doc_sd".to_string(), dom)];

    let declared: HashSet<Uuid> = [doc].into_iter().collect();
    assert!(
        applies_at_site(&outputs, &declared),
        "one declared output is enough: the rest are slots the site is missing, not a tool it \
         never wanted"
    );

    let both: HashSet<Uuid> = [doc, dom].into_iter().collect();
    assert!(applies_at_site(&outputs, &both));
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
    assert!(!applies_at_site(
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
