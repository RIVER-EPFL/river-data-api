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
#[path = "tests/ordering.rs"]
mod tests;
