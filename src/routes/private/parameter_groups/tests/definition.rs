use super::*;
use crate::routes::private::parameter_groups::models::member_statistics;
use crate::routes::private::parameter_groups::service::ordering::{self, Column};
use crate::routes::private::parameter_groups::service::rules::Role;

fn spec() -> serde_json::Value {
    serde_json::json!({ "positions": 3 })
}

#[test]
fn test_a_replicated_member_shows_a_mean_and_an_sd() {
    let stats = member_statistics("doc", Some(&spec()), Some(2))
        .expect("a replicated member carries statistics");
    assert_eq!(stats.mean_label, "doc mean");
    assert_eq!(stats.sd_label, "doc sd");
    assert_eq!(stats.decimal_places, Some(2));
}

/// A slot that declares no places is one no source and no lab has spoken for, so the
/// definition says so rather than inventing a precision (Q128).
#[test]
fn test_an_undeclared_slot_carries_no_places() {
    assert_eq!(
        member_statistics("doc", Some(&spec()), None)
            .expect("a replicated member carries statistics")
            .decimal_places,
        None
    );
}

#[test]
fn test_a_member_entered_once_shows_none() {
    assert_eq!(member_statistics("ph", None, Some(2)), None);
}

// The statistics are the member's own columns, not members of the group, so the order the grid
// and the Toolbox share stays the members' own.
#[test]
fn test_statistics_are_not_columns_of_their_own() {
    let columns = [
        Column {
            parameter_id: Uuid::new_v4(),
            code: "doc".into(),
            ordinal: 1,
            role: Role::Measured,
            section: None,
        },
        Column {
            parameter_id: Uuid::new_v4(),
            code: "ph".into(),
            ordinal: 2,
            role: Role::Measured,
            section: None,
        },
    ];
    let order: Vec<&str> = ordering::column_order(&columns)
        .into_iter()
        .map(|c| c.code.as_str())
        .collect();
    assert_eq!(order, vec!["doc", "ph"]);
}
