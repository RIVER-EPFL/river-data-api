#!/usr/bin/env python3
"""Emit the parameter-group seed migration from the vendored portal registries.

Reads `migration/portal_seed/portal_categories.sql` (CNET) and
`portal_categories_metalp.sql` (METALP) and writes
`migration/src/m20260908_000007_seed_portal_parameter_groups.rs`. The rules it applies are the
ones the tree already holds: a replicate family is a `calcMean`/`calcDOCavg` row over two or more
measured columns (`river-data-rshiny/src/backend/rshiny/families.rs`), and a family's catalog code
is its mean column with the structural `avg` segment removed
(`sync/service.rs::family_parameter_suggestion`).

    python3 scripts/portal_categories_seed.py
"""

import re
import sys
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEED_DIR = ROOT / "migration" / "portal_seed"
# CNET first: it is the current campaign, so its order is the order the groups are listed in and
# its label and units win where both registries describe the same column.
FIXTURES = [SEED_DIR / "portal_categories.sql", SEED_DIR / "portal_categories_metalp.sql"]
TARGET = ROOT / "migration" / "src" / "m20260908_000007_seed_portal_parameter_groups.rs"

NAMESPACE = uuid.UUID("6f1a0c2e-4b7d-5a3e-9c81-2d5f7b0e4a16")
MEAN_FUNCS = {"calcMean", "calcDOCavg"}
SD_FUNCS = {"calcSd", "calcDOCsd"}
CURVE_TOKEN_SUFFIX = "_std_curve_id"


def parse_values(sql, table):
    """The tuples of one MySQL `INSERT ... VALUES` statement, NULL as None."""
    match = re.search(r"INSERT INTO `%s` VALUES (.*?);\s*$" % table, sql, re.M | re.S)
    if not match:
        sys.exit(f"{table}: no INSERT in the registry")
    body, i, rows = match.group(1), 0, []
    while i < len(body):
        if body[i] != "(":
            i += 1
            continue
        i += 1
        values, current, quoted = [], "", False
        while True:
            char = body[i]
            if quoted:
                if char == "\\":
                    current += body[i + 1]
                    i += 2
                elif char == "'":
                    quoted, i = False, i + 1
                else:
                    current += char
                    i += 1
                continue
            if char == "'":
                quoted, i = True, i + 1
            elif char == ",":
                values.append(current)
                current, i = "", i + 1
            elif char == ")":
                values.append(current)
                i += 1
                break
            else:
                current += char
                i += 1
        rows.append([None if v == "NULL" else v for v in values])
    return rows


def data_columns(sql):
    match = re.search(r"CREATE TABLE `data` \((.*?)\n\) ENGINE=", sql, re.S)
    return {m.group(1) for m in re.finditer(r"^\s+`([^`]+)`", match.group(1), re.M)}


def split_tokens(columns_used):
    measured, curve = [], None
    for token in (t.strip() for t in columns_used.split(",")):
        if not token:
            continue
        if token.endswith(CURVE_TOKEN_SUFFIX):
            curve = token
        else:
            measured.append(token)
    return measured, curve


def build_families(calcs, columns):
    """Replicate families keyed by mean column, by the rule `families.rs` applies."""
    families = {}
    for row in calcs:
        _, _, category, output, func, used = row[:6]
        if func not in MEAN_FUNCS:
            continue
        measured, curve = split_tokens(used)
        if len(measured) < 2 or len(set(measured)) != len(measured):
            continue
        referenced = measured + [output] + ([curve] if curve else [])
        if any(c not in columns for c in referenced):
            continue
        families[output] = {
            "category": category,
            "mean_column": output,
            "sd_column": None,
            "members": measured,
            "curve_ref_column": curve,
            "calc": func,
        }
    for row in calcs:
        _, _, _, output, func, used = row[:6]
        if func not in SD_FUNCS or output not in columns:
            continue
        measured, _ = split_tokens(used)
        for family in families.values():
            if set(family["members"]) == set(measured):
                family["sd_column"] = output
                break
    return families


def family_code(mean_column):
    """The catalog code a family's mean column suggests: `DOC_avg_ppb` is `DOC_ppb`."""
    stripped = "_".join(s for s in mean_column.split("_") if s.lower() != "avg")
    return stripped or mean_column


def group_code(label):
    return re.sub(r"[^a-z0-9]+", "_", label.lower()).strip("_")


def deterministic_id(kind, key):
    return str(uuid.uuid5(NAMESPACE, f"{kind}:{key}"))


def quote(value):
    if value is None:
        return "NULL"
    return "'" + str(value).replace("'", "''") + "'"


def read_registries():
    """The four registry pieces, merged across portals.

    The two portals overlap almost entirely: a column both list is one catalog parameter and one
    member, so the merge is by column, not by portal. The CNET file comes first, so its category
    order, its label and its units are the ones that stand where both describe the same thing;
    METALP contributes the categories CNET's registry has entry rows for nowhere (Chl a, TSS, Old
    Nutrients, Unused) and the columns it alone lists.
    """
    categories, calcs, plotting, columns = [], [], [], set()
    seen_category, seen_calc, seen_plot = set(), set(), set()
    for portal, fixture in enumerate(FIXTURES):
        sql = fixture.read_text(encoding="utf8")
        columns |= data_columns(sql)
        for row in parse_values(sql, "grab_param_categories"):
            key = (row[2], row[3])
            if key not in seen_category:
                seen_category.add(key)
                # The portal index leads the sort key, so a column the later registry alone lists
                # is appended after the shared ones rather than interleaved into their order.
                categories.append(row + [portal])
        for row in parse_values(sql, "parameter_calculations"):
            key = (row[2], row[3], row[4], row[5])
            if key not in seen_calc:
                seen_calc.add(key)
                calcs.append(row)
        for row in parse_values(sql, "grab_params_plotting"):
            key = (row[3], row[6])
            if key not in seen_plot:
                seen_plot.add(key)
                plotting.append(row)
    return categories, calcs, plotting, columns


def main():
    categories, calcs, plotting, columns = read_registries()

    families = build_families(calcs, columns)
    consumed = set()
    for family in families.values():
        consumed.update(family["members"])
        consumed.add(family["mean_column"])
        if family["sd_column"]:
            consumed.add(family["sd_column"])

    # A column a calculation writes is an output, whatever category it was entered under.
    calculated = {row[3] for row in calcs if row[3] in columns}

    # Label and units for the plotted subset, keyed by every column the plot reads.
    presentation = {}
    for row in plotting:
        _, _, _, option_name, _, units, data, sd = row[:8]
        for column in (c.strip() for c in data.split(",")):
            presentation.setdefault(column, (option_name, units or ""))
        if sd:
            presentation.setdefault(sd, (option_name, units or ""))

    # Groups in the order the registry lists them, then the calculation-only labels.
    labels, order = [], {}
    for row in categories:
        label = row[2]
        if label not in order:
            order[label] = len(labels)
            labels.append(label)
    for row in calcs:
        label = row[2]
        if label not in order and any(
            c in columns for c in [row[3]] + split_tokens(row[5])[0]
        ):
            order[label] = len(labels)
            labels.append(label)

    members_by_label = {label: [] for label in labels}
    described = {}
    for row in categories:
        described[row[3]] = row[4]

    # Entry columns of a category, in the portal's own display order.
    for label in labels:
        rows = sorted(
            (r for r in categories if r[2] == label),
            key=lambda r: (r[-1], int(r[1]), int(r[0])),
        )
        for row in rows:
            column = row[3]
            if column in consumed and column not in families:
                continue
            if column in families:
                family = families[column]
                members_by_label[label].append(
                    {
                        "code": family_code(column),
                        "role": "measured",
                        "family": family,
                        "description": row[4],
                        "source_rows": [column]
                        + family["members"]
                        + ([family["sd_column"]] if family["sd_column"] else []),
                    }
                )
                continue
            members_by_label[label].append(
                {
                    "code": column,
                    "role": "output" if column in calculated else "entry_only",
                    "family": None,
                    "description": row[4],
                    "source_rows": [column],
                }
            )

    # A calculation-only label has no entry rows; its members are the columns it names.
    placed = {m["code"] for members in members_by_label.values() for m in members}
    for label in labels:
        if members_by_label[label]:
            continue
        seen = []
        for row in (r for r in calcs if r[2] == label):
            for column in [row[3]] + split_tokens(row[5])[0]:
                if column not in columns or column in consumed or column in seen:
                    continue
                if column in placed or any(
                    column in [m["code"] for m in members_by_label[other]]
                    for other in labels
                ):
                    continue
                seen.append(column)
                members_by_label[label].append(
                    {
                        "code": column,
                        "role": "output" if column in calculated else "entry_only",
                        "family": None,
                        "description": described.get(column),
                        "source_rows": [],
                    }
                )
        for column in families:
            if families[column]["category"] != label:
                continue
            family = families[column]
            members_by_label[label].append(
                {
                    "code": family_code(column),
                    "role": "measured",
                    "family": family,
                    "description": described.get(column),
                    "source_rows": [],
                }
            )

    statements = []
    seen_codes = set()
    for label in labels:
        for member in members_by_label[label]:
            code = member["code"]
            if code in seen_codes:
                continue
            seen_codes.add(code)
            source = member["family"]["mean_column"] if member["family"] else code
            # `default_units` is NOT NULL and the registry declares units only for the plotted
            # subset, so an undeclared column is seeded with an empty one; `needs_review` is
            # what says the row is waiting to be finished.
            name, units = presentation.get(source, (code, ""))
            statements.append(
                "INSERT INTO parameters (id, code, name, default_units, category, description, "
                "needs_review)\n    SELECT {}, {}, {}, {}, 'measurement', {}, true\n     WHERE NOT "
                "EXISTS (SELECT 1 FROM parameters WHERE LOWER(code) = LOWER({}));".format(
                    quote(deterministic_id("parameter", code)),
                    quote(code),
                    quote(name),
                    quote(units),
                    quote((member["description"] or "").strip() or None),
                    quote(code),
                )
            )

    for label in labels:
        statements.append(
            "INSERT INTO parameter_groups (id, code, label, ordinal)\n    VALUES ({}, {}, {}, {})\n"
            "    ON CONFLICT (code) DO NOTHING;".format(
                quote(deterministic_id("group", label)),
                quote(group_code(label)),
                quote(label),
                order[label],
            )
        )
        for ordinal, member in enumerate(members_by_label[label]):
            family = member["family"]
            replicates = "NULL"
            if family:
                replicates = quote(
                    '{{"source_columns": [{}], "portal_mean_column": {}, "portal_sd_column": {}, '
                    '"curve_ref_column": {}, "calc": "{}"}}'.format(
                        ", ".join('"%s"' % m for m in family["members"]),
                        '"%s"' % family["mean_column"],
                        '"%s"' % family["sd_column"] if family["sd_column"] else "null",
                        '"%s"' % family["curve_ref_column"]
                        if family["curve_ref_column"]
                        else "null",
                        family["calc"],
                    )
                ) + "::jsonb"
            source = family["mean_column"] if family else member["code"]
            plot_label, units = presentation.get(source, (None, None))
            units = units or None
            statements.append(
                "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal, role, "
                "replicates, label, units, description)\n    SELECT {}, {}, p.id, {}, {}, {}, {}, "
                "{}, {}\n      FROM parameters p WHERE LOWER(p.code) = LOWER({})\n    ON CONFLICT "
                "DO NOTHING;".format(
                    quote(deterministic_id("member", f"{label}:{member['code']}")),
                    quote(deterministic_id("group", label)),
                    ordinal,
                    quote(member["role"]),
                    replicates,
                    quote(plot_label),
                    quote(units),
                    quote((member["description"] or "").strip() or None),
                    quote(member["code"]),
                )
            )

    statements.append(
        "UPDATE tool_scripts SET parameter_group_id = (SELECT id FROM parameter_groups "
        "WHERE code = 'doc')\n     WHERE name = 'doc' AND parameter_group_id IS NULL;"
    )

    body = "\n\n".join("    " + s.replace("\n", "\n") for s in statements)
    counts = {label: len(members_by_label[label]) for label in labels}
    TARGET.write_text(TEMPLATE.format(body=body, counts=counts, groups=len(labels),
                                      members=sum(counts.values())), encoding="utf8")
    print(f"{len(labels)} groups, {sum(counts.values())} members -> {TARGET.name}")
    for label in labels:
        print(f"  {label}: {counts[label]}")


TEMPLATE = '''use sea_orm_migration::prelude::*;

/// The CNET and METALP portals' categories as parameter groups.
///
/// Generated by `scripts/portal_categories_seed.py` from `migration/portal_seed/`, the two
/// portals' own registries vendored verbatim. {groups} groups, {members} members. Edit the
/// script, not this file.
///
/// The registries are merged by column, not kept apart by portal: a column both list is one
/// catalog parameter and one member. They disagree on exactly two columns, `WTW_DO_mgL_1` under
/// Field data and `Li_mgL` under Ions, each listed by one portal; both are members of the one
/// group for their category. Chl a, TSS, Old Nutrients and Unused have entry rows in METALP's
/// registry alone.
///
/// A replicate family is one member, not the portal's columns: its replicates are one parameter
/// at several `replicate_index` values and the `samples` trigger computes the mean and the
/// standard deviation, so the family's `_avg` and `_sd` columns are carried on the member's
/// `replicates` spec rather than seeded as parameters of their own. Everything else is a member
/// at the position the portal lists it, `output` where a calculation writes it and `entry_only`
/// where the value is typed in.
///
/// Nothing is overwritten: a parameter whose code already exists keeps its row, and a group or a
/// membership that is already there is left alone, so an operator's edits survive a re-run.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const SEED: &str = r#"
{body}
"#;

const DOWN: &str = "
    UPDATE tool_scripts SET parameter_group_id = NULL;
    DELETE FROM parameter_group_members;
    DELETE FROM parameter_groups;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {{
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {{
        manager.get_connection().execute_unprepared(SEED).await?;
        Ok(())
    }}

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {{
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }}
}}
'''

if __name__ == "__main__":
    main()
