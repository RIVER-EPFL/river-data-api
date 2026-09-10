use super::{GrabFacts, StoredFacts, declared_instrument};

fn stored(label: &str, notes: &str, author: &str) -> StoredFacts {
    StoredFacts {
        created_by: Some(author.to_string()),
        label: Some(label.to_string()),
        notes: Some(notes.to_string()),
        provenance: Some(serde_json::json!({ "tool": "doc" })),
        kind: Some("tool_run".to_string()),
    }
}

#[test]
fn a_row_the_curve_did_not_correct_takes_the_slot_s_declaration() {
    let curve_instrument = uuid::Uuid::new_v4();
    let declared = uuid::Uuid::new_v4();
    assert_eq!(
        declared_instrument(Some(curve_instrument), Some(declared)),
        Some(curve_instrument),
        "the row carrying the curve names the instrument that curve was fitted on"
    );
    assert_eq!(
        declared_instrument(None, Some(declared)),
        Some(declared),
        "every other row names what the slot says measures it"
    );
    assert_eq!(
        declared_instrument(None, None),
        None,
        "an undeclared slot resolves nothing here; the entry channel's marker is added at the write"
    );
}

#[test]
fn a_silent_field_keeps_what_the_group_carried() {
    let prior = stored("batch 7", "filtered on site", "lab");
    let request = GrabFacts {
        created_by: None,
        label: None,
        notes: Some("corrected note"),
        provenance: None,
        kind: "manual",
    };
    let merged = request.over(Some(&prior));
    assert_eq!(merged.label.as_deref(), Some("batch 7"));
    assert_eq!(merged.notes.as_deref(), Some("corrected note"));
    assert_eq!(merged.created_by.as_deref(), Some("lab"));
    assert_eq!(
        merged.provenance,
        Some(serde_json::json!({ "tool": "doc" })),
        "a rewrite that names no run keeps the blob behind the value"
    );
}

#[test]
fn a_first_write_carries_only_what_the_request_says() {
    let request = GrabFacts {
        created_by: Some("evan"),
        label: None,
        notes: None,
        provenance: None,
        kind: "manual",
    };
    let merged = request.over(None);
    assert_eq!(merged.created_by.as_deref(), Some("evan"));
    assert_eq!(merged.label, None);
    assert!(!merged.is_empty(), "an author alone is worth storing");
    assert!(
        GrabFacts {
            created_by: None,
            label: None,
            notes: None,
            provenance: None,
            kind: "manual",
        }
        .over(None)
        .is_empty(),
        "a request that records nothing writes nothing"
    );
}
