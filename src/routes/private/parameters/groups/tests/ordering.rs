use super::*;

fn column(code: &str, ordinal: i32, section: Option<&str>) -> Column {
    Column {
        parameter_id: Uuid::new_v4(),
        code: code.to_string(),
        ordinal,
        role: Role::Measured,
        section: section.map(str::to_string),
    }
}

fn codes(members: &[Column]) -> Vec<&str> {
    column_order(members)
        .into_iter()
        .map(|c| c.code.as_str())
        .collect()
}

#[test]
fn test_the_member_ordinal_is_the_order() {
    let members = [
        column("hix", 3, None),
        column("bix", 1, None),
        column("fi", 2, None),
    ];
    assert_eq!(codes(&members), vec!["bix", "fi", "hix"]);
}

#[test]
fn test_a_section_never_reorders_a_column() {
    // The manifest declares fluorescence before absorbance; the ordinals say otherwise, and
    // the ordinals are the order.
    let members = [
        column("a254", 1, Some("absorbance")),
        column("bix", 2, Some("fluorescence")),
        column("a300", 3, Some("absorbance")),
    ];
    assert_eq!(codes(&members), vec!["a254", "bix", "a300"]);
    assert_eq!(
        section_order(&members),
        vec!["absorbance".to_string(), "fluorescence".to_string()]
    );
}

#[test]
fn test_a_shared_ordinal_breaks_on_the_code() {
    let members = [column("suva", 1, None), column("a254", 1, None)];
    assert_eq!(codes(&members), vec!["a254", "suva"]);
}

#[test]
fn test_a_group_with_no_sections_declares_none() {
    let members = [column("bix", 1, None), column("fi", 2, None)];
    assert!(section_order(&members).is_empty());
}

#[test]
fn test_an_empty_group_orders_to_nothing() {
    assert!(column_order(&[]).is_empty());
    assert!(section_order(&[]).is_empty());
}
