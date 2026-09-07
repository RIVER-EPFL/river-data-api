//! The vendored CNET and METALP registries the parameter-group seed is generated from, and the
//! check that the seed accounts for every row of them.
//!
//! Each `grab_param_categories` row is either a member of a seeded group, or a position in a
//! member's replicate spec, or the `_avg`/`_sd` column that spec names: a row accounted for by
//! none of the three is a column of a portal with no home here, which is the gap this seed
//! exists to close.

const CNET: &str = include_str!("../portal_seed/portal_categories.sql");
const METALP: &str = include_str!("../portal_seed/portal_categories_metalp.sql");

/// The tuples of one MySQL `INSERT ... VALUES` statement, `NULL` as an empty string.
fn insert_rows(fixture: &str, table: &str) -> Vec<Vec<String>> {
    let marker = format!("INSERT INTO `{table}` VALUES ");
    let start = fixture.find(&marker).expect("table is in the fixture") + marker.len();
    let body: Vec<char> = fixture[start..].chars().collect();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            '(' => {}
            ';' => break,
            _ => {
                i += 1;
                continue;
            }
        }
        i += 1;
        let (mut values, mut current, mut quoted) = (Vec::new(), String::new(), false);
        loop {
            let c = body[i];
            if quoted {
                match c {
                    '\\' => {
                        current.push(body[i + 1]);
                        i += 2;
                    }
                    '\'' => {
                        quoted = false;
                        i += 1;
                    }
                    _ => {
                        current.push(c);
                        i += 1;
                    }
                }
                continue;
            }
            match c {
                '\'' => {
                    quoted = true;
                    i += 1;
                }
                ',' => {
                    values.push(std::mem::take(&mut current));
                    i += 1;
                }
                ')' => {
                    values.push(std::mem::take(&mut current));
                    i += 1;
                    break;
                }
                _ => {
                    current.push(c);
                    i += 1;
                }
            }
        }
        rows.push(
            values
                .into_iter()
                .map(|v| if v == "NULL" { String::new() } else { v })
                .collect(),
        );
    }
    rows
}

/// Every `param_name` of one portal's `grab_param_categories`, with the category it belongs to.
pub fn registry_columns_of(fixture: &str) -> Vec<(String, String)> {
    insert_rows(fixture, "grab_param_categories")
        .into_iter()
        .map(|r| (r[2].clone(), r[3].clone()))
        .collect()
}

/// Both registries, deduplicated by (category, column): a column both portals list is one member.
pub fn registry_columns() -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut columns = Vec::new();
    for fixture in [CNET, METALP] {
        for row in registry_columns_of(fixture) {
            if seen.insert(row.clone()) {
                columns.push(row);
            }
        }
    }
    columns
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m20260908_000007_seed_portal_parameter_groups::SEED;

    /// A column the seed names as a member: the parameter lookup the member insert does.
    fn seeded_members() -> Vec<String> {
        SEED.match_indices("LOWER(p.code) = LOWER('")
            .map(|(i, m)| {
                let rest = &SEED[i + m.len()..];
                rest[..rest.find('\'').expect("closing quote")].to_string()
            })
            .collect()
    }

    /// A column carried inside a member's replicate spec: a replicate position, or the portal
    /// mean or sd column the spec names.
    fn spec_columns() -> Vec<String> {
        let mut columns = Vec::new();
        for (i, _) in SEED.match_indices("\"source_columns\"") {
            let block = &SEED[i..i + SEED[i..].find("}'::jsonb").expect("spec ends") + 1];
            let mut rest = block;
            while let Some(open) = rest.find('"') {
                let after = &rest[open + 1..];
                let close = after.find('"').expect("closing quote");
                columns.push(after[..close].to_string());
                rest = &after[close + 1..];
            }
        }
        columns
    }

    #[test]
    fn test_every_registry_column_is_a_member_or_a_replicate_position() {
        let members = seeded_members();
        let spec = spec_columns();
        let unaccounted: Vec<String> = registry_columns()
            .into_iter()
            .filter(|(_, column)| {
                !members.iter().any(|m| m.eq_ignore_ascii_case(column))
                    && !spec.iter().any(|s| s == column)
                    // A family's own code is the mean column without its `avg` segment.
                    && !members.iter().any(|m| {
                        m.eq_ignore_ascii_case(
                            &column
                                .split('_')
                                .filter(|s| !s.eq_ignore_ascii_case("avg"))
                                .collect::<Vec<_>>()
                                .join("_"),
                        )
                    })
            })
            .map(|(category, column)| format!("{category}/{column}"))
            .collect();
        assert!(
            unaccounted.is_empty(),
            "portal columns with no home in the seed: {unaccounted:?}"
        );
    }

    #[test]
    fn test_the_registries_are_the_ones_the_seed_was_generated_from() {
        // CNET 180 rows over 9 categories, METALP 294 over 13, 295 distinct pairs between them; a
        // re-vendored dump that moves any of those numbers needs the generator run again, not the
        // counts here relaxed.
        assert_eq!(registry_columns_of(CNET).len(), 180);
        assert_eq!(registry_columns_of(METALP).len(), 294);
        let columns = registry_columns();
        assert_eq!(columns.len(), 295);
        let mut categories: Vec<&str> = columns.iter().map(|(c, _)| c.as_str()).collect();
        categories.sort_unstable();
        categories.dedup();
        assert_eq!(categories.len(), 13);
    }

    /// The categories METALP alone has entry rows for, and the two columns the registries
    /// disagree on, are each seeded once under the one group for their category.
    #[test]
    fn test_the_metalp_only_categories_and_columns_are_seeded() {
        let members = seeded_members();
        let spec = spec_columns();
        for column in ["WTW_DO_mgL_1", "Li_mgL", "TSS_dry_weight_mgL", "unused_CO2_calc_uM"] {
            assert!(
                members.iter().filter(|m| m.as_str() == column).count() == 1,
                "{column} is not seeded exactly once"
            );
        }
        assert!(spec.iter().any(|s| s == "NH4_rep_A"), "Old Nutrients has no family");
    }

    #[test]
    fn test_a_replicate_family_is_one_member_carrying_its_columns() {
        // DOC is the shape: five portal columns, one member, the other four on its spec.
        let spec = spec_columns();
        for column in [
            "DOC_rep_1",
            "DOC_rep_2",
            "DOC_rep_3",
            "DOC_avg_ppb",
            "DOC_sd_ppb",
        ] {
            assert!(
                spec.contains(&column.to_string()),
                "{column} is not on a spec"
            );
        }
        let members = seeded_members();
        assert!(members.iter().any(|m| m == "DOC_ppb"));
        for column in ["DOC_rep_1", "DOC_avg_ppb", "DOC_sd_ppb"] {
            assert!(
                !members.iter().any(|m| m == column),
                "{column} is seeded as a member of its own"
            );
        }
    }
}
