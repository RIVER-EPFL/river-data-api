use super::missing_role_names;

fn owned(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn full_realm_is_satisfied() {
    let present = owned(&[
        "riverdata-admin",
        "riverdata-manager",
        "riverdata-river",
        "riverdata-intern",
    ]);
    assert!(missing_role_names(&present).is_empty());
}

/// Scenario: the RIVER realm predating the four access levels, carrying the retired
/// `riverdata-user` role.
/// Expected behaviour: the three levels it never had are reported, and `riverdata-user`
/// does not stand in for any of them.
#[test]
fn legacy_two_role_realm_reports_the_three_absent_levels() {
    let present = owned(&["riverdata-admin", "riverdata-user", "admin"]);
    assert_eq!(
        missing_role_names(&present),
        vec!["riverdata-manager", "riverdata-river", "riverdata-intern"]
    );
}

#[test]
fn empty_realm_reports_every_level() {
    assert_eq!(missing_role_names(&[]).len(), 4);
}
