//! The one order the grid, the tool form and the Toolbox render a group in.
//!
//! Four things looked like grouping and only two of them are: `parameters.category` is the
//! device-health split that gates alarms and the public arm, and the group is the scientific
//! category. A manifest `section` is neither: it labels a run of columns inside a group, so it is
//! a display hint that never reorders anything. The member's `ordinal` is the order, everywhere.

use uuid::Uuid;

use super::rules::Role;

/// One column of a group as the three surfaces see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub parameter_id: Uuid,
    /// The catalog code, which breaks a tie between two members sharing an ordinal so the order is
    /// the same in every database rather than the store's.
    pub code: String,
    pub ordinal: i32,
    pub role: Role,
    /// The manifest section this member's field renders under, when a calculation names one.
    pub section: Option<String>,
}

/// The group's columns in the order they are rendered: by member ordinal, then by code.
pub fn column_order(members: &[Column]) -> Vec<&Column> {
    let mut ordered: Vec<&Column> = members.iter().collect();
    ordered.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then_with(|| a.code.cmp(&b.code)));
    ordered
}

/// The section labels a group renders, in the order their first column appears under
/// [`column_order`]. A run of columns naming no section carries no label.
pub fn section_order(members: &[Column]) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    for column in column_order(members) {
        if let Some(section) = &column.section
            && !sections.contains(section)
        {
            sections.push(section.clone());
        }
    }
    sections
}

#[cfg(test)]
mod tests {
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
}
