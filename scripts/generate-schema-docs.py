#!/usr/bin/env python3
"""Generate the schema documents under docs/ from a migrated database.

Reads DATABASE_URL, introspects the public schema, and writes schema.md (one Mermaid
entity relationship diagram per subject area plus a column reference for every table),
schema.dot and schema-core.dot, and the SVG each DOT renders to. Tables absent from
AREAS land in the trailing catch-all, so a new migration always shows up somewhere.
"""

import os
import re
import shutil
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

AREAS = [
    ("Projects and sites", [
        "projects", "subprojects", "sites", "site_parameters", "parameters",
        "constants", "notes", "annotations",
    ]),
    ("Instruments and curves", [
        "sensors", "sensor_calibrations", "sensor_deployments", "standard_curves",
    ]),
    ("Streams and readings", [
        "data_streams", "readings", "status_events", "samples", "ingest_receipts",
        "csv_import_staging", "pairing_plans", "replicate_audit_holds",
    ]),
    ("Field visits and curation", [
        "collection_events", "seasonal_checks", "reading_decisions",
        "reading_decision_sets",
    ]),
    ("Derived parameters and tools", [
        "calculation_formulas", "derived_parameter_sources", "tool_scripts",
        "tool_script_versions", "tool_script_activations", "tool_runs",
    ]),
    ("Alarms and notifications", [
        "alarm_thresholds", "alarm_events", "notification_subscribers",
        "notification_subscriptions", "notification_log", "notification_mutes",
        "notification_state", "notification_channel_health", "web_push_subscriptions",
    ]),
    ("Jobs and schedules", [
        "reprocessing_jobs", "reprocessing_job_logs", "schedules", "schedule_audit",
    ]),
    ("Sync control plane", [
        "sync_services", "sync_commands", "sync_events", "sync_service_credentials",
        "sync_service_tokens",
    ]),
    ("Access control", [
        "api_tokens", "api_token_audit_log", "user_project_grants",
    ]),
]

SKIP = {"seaql_migrations"}

# A table this many others point at is drawn on the poster without its incoming edges. Three
# tables absorb a third of every foreign key in the schema, and drawing those lines costs more
# than they carry: they cross every group and make the rest unfollowable. The count is written
# on the table instead, and the per-area diagrams still draw every one of them.
HUB_REFERENCE_THRESHOLD = 6

# What each table is for, crossing the subject areas: a reader wants to know which tables carry
# a measurement and which exist to run the platform. Colours are light fills with a darker
# stroke so the shading survives printing and reads on white. Anything unlisted is operations.
TIERS = [
    ("core", "Core", "the measurement and what identifies it", "#dbe7f3", "#1f4e79", [
        "projects", "subprojects", "sites", "site_parameters", "parameters",
        "sensors", "sensor_deployments", "sensor_calibrations", "standard_curves",
        "data_streams", "readings", "status_events", "samples", "collection_events",
    ]),
    ("catalog", "Catalog", "definitions the core points at", "#dfeee2", "#2f6b45", [
        "constants", "calculation_formulas", "derived_parameter_sources",
        "alarm_thresholds", "tool_scripts", "tool_script_versions", "tool_script_activations",
    ]),
    ("record", "Record", "evidence about readings: provenance and curation", "#f6e8d5", "#c77700", [
        "annotations", "notes", "reading_decisions", "reading_decision_sets",
        "ingest_receipts", "seasonal_checks", "replicate_audit_holds", "tool_runs",
    ]),
    ("operations", "Operations", "machinery that runs the platform", "#ececec", "#777777", []),
]

# The path a measurement takes, for the overview diagram the API README embeds.
CORE = [
    "projects", "subprojects", "sites", "site_parameters", "parameters",
    "data_streams", "readings", "samples", "collection_events",
    "sensors", "sensor_deployments", "sensor_calibrations", "standard_curves",
]

TYPE_SHORT = {
    "timestamp with time zone": "timestamptz",
    "timestamp without time zone": "timestamp",
    "character varying": "varchar",
    "double precision": "float8",
    "boolean": "bool",
    "integer": "int4",
    "bigint": "int8",
    "smallint": "int2",
    "character": "char",
}

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"

FIELD_SEPARATOR = "\x1f"
RECORD_SEPARATOR = "\x1e"


def query(sql):
    """Rows as lists of fields, delimited by bytes no SQL value can contain.

    A generated column's expression prints over several lines, so a newline cannot be the
    record separator.
    """
    url = os.environ.get("DATABASE_URL")
    if not url:
        sys.exit("DATABASE_URL is not set")
    out = subprocess.run(
        ["psql", url, "-tA", "-F", FIELD_SEPARATOR, "-R", RECORD_SEPARATOR,
         "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-c", sql],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit(out.stderr.strip())
    # psql separates records rather than terminating them, and ends the output with a newline.
    return [record.split(FIELD_SEPARATOR)
            for record in out.stdout.rstrip("\n").split(RECORD_SEPARATOR) if record]


def short_type(name):
    base = TYPE_SHORT.get(name, name)
    return re.sub(r"[^A-Za-z0-9_]", "_", base)


def tier_of(table):
    for key, _, _, _, _, members in TIERS:
        if table in members:
            return key
    return "operations"


def class_lines(tables):
    """classDef and class statements colouring each entity by its tier."""
    lines = []
    grouped = {}
    for table in tables:
        grouped.setdefault(tier_of(table), []).append(table)
    for key, _, _, fill, stroke, _ in TIERS:
        if key in grouped:
            lines.append(f"    classDef {key} fill:{fill},stroke:{stroke}")
    for key, _, _, _, _, _ in TIERS:
        if key in grouped:
            lines.append(f"    class {','.join(sorted(grouped[key]))} {key}")
    return lines


def collect():
    tables = [r[0] for r in query(
        "SELECT table_name FROM information_schema.tables "
        "WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY 1"
    ) if r[0] not in SKIP]

    columns = defaultdict(list)
    for table, column, typ, nullable, default in query("""
        SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod),
               NOT a.attnotnull, COALESCE(pg_get_expr(d.adbin, d.adrelid), '')
        FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
        WHERE n.nspname = 'public' AND c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY c.relname, a.attnum
    """):
        columns[table].append({
            "name": column, "type": typ, "nullable": nullable == "t", "default": default,
        })

    primary = defaultdict(list)
    for table, column in query("""
        SELECT c.relname, a.attname
        FROM pg_constraint k
        JOIN pg_class c ON c.oid = k.conrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        JOIN unnest(k.conkey) WITH ORDINALITY AS u(attnum, ord) ON true
        JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = u.attnum
        WHERE n.nspname = 'public' AND k.contype = 'p'
        ORDER BY c.relname, u.ord
    """):
        primary[table].append(column)

    foreign = []
    for table, column, target, target_column in query("""
        SELECT c.relname, a.attname, t.relname, ta.attname
        FROM pg_constraint k
        JOIN pg_class c ON c.oid = k.conrelid
        JOIN pg_class t ON t.oid = k.confrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        JOIN unnest(k.conkey) WITH ORDINALITY AS u(attnum, ord) ON true
        JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = u.attnum
        JOIN unnest(k.confkey) WITH ORDINALITY AS fu(attnum, ord) ON fu.ord = u.ord
        JOIN pg_attribute ta ON ta.attrelid = k.confrelid AND ta.attnum = fu.attnum
        WHERE n.nspname = 'public' AND k.contype = 'f'
        ORDER BY c.relname, a.attname
    """):
        foreign.append((table, column, target, target_column))

    hypertables = {}
    for name, interval in query("""
        SELECT h.hypertable_name, d.time_interval::text
        FROM timescaledb_information.hypertables h
        JOIN timescaledb_information.dimensions d
          ON d.hypertable_name = h.hypertable_name AND d.dimension_number = 1
    """):
        hypertables[name] = interval

    aggregates = [(r[0], r[1]) for r in query("""
        SELECT view_name, materialization_hypertable_name
        FROM timescaledb_information.continuous_aggregates ORDER BY 1
    """)]

    head = query("SELECT max(version) FROM seaql_migrations")[0][0]

    verify(tables, columns)
    return tables, columns, primary, foreign, hypertables, aggregates, head


def verify(tables, columns):
    """Refuse to write documents that disagree with `information_schema`.

    The introspection above reads `pg_catalog` for the defaults and the generated expressions
    `information_schema` does not carry, so the two are compared once per run.
    """
    declared = defaultdict(set)
    for table, column in query(
        "SELECT c.table_name, c.column_name FROM information_schema.columns c "
        "JOIN information_schema.tables t ON t.table_schema = c.table_schema "
        "AND t.table_name = c.table_name "
        "WHERE c.table_schema = 'public' AND t.table_type = 'BASE TABLE'"
    ):
        if table not in SKIP:
            declared[table].add(column)
    drawn = {t: {c["name"] for c in columns[t]} for t in tables}
    if declared.keys() - drawn.keys():
        sys.exit(f"tables missing from the diagram: {sorted(declared.keys() - drawn.keys())}")
    for table in sorted(drawn):
        if drawn[table] != declared[table]:
            sys.exit(
                f"{table} columns disagree with information_schema: "
                f"{sorted(drawn[table] ^ declared[table])}"
            )


def diagram(members, columns, primary, foreign, scoped, isolate=False):
    """Mermaid erDiagram for `members`.

    `scoped` limits entities to key columns; `isolate` drops references to tables
    outside `members` rather than drawing them.
    """
    lines = ["erDiagram"]
    fk_columns = defaultdict(set)
    for table, column, target, _ in foreign:
        fk_columns[table].add(column)

    outside = []
    for table, column, target, target_column in sorted(foreign):
        if table not in members:
            continue
        if target not in members:
            if isolate:
                continue
            outside.append(target)
        nullable = any(c["name"] == column and c["nullable"] for c in columns[table])
        link = "}o--o|" if nullable else "}o--||"
        lines.append(f"    {table} {link} {target} : {column}")

    for table in members + sorted(set(outside) - set(members)):
        lines.append(f"    {table} {{")
        for column in columns[table]:
            keys = []
            if column["name"] in primary[table]:
                keys.append("PK")
            if column["name"] in fk_columns[table]:
                keys.append("FK")
            if scoped and not keys:
                continue
            marker = f' {",".join(keys)}' if keys else ""
            lines.append(f'        {short_type(column["type"])} {column["name"]}{marker}')
        lines.append("    }")
    lines += class_lines(members + sorted(set(outside) - set(members)))
    return "\n".join(lines)


def dot_diagram(tables, columns, primary, foreign, clustered=True):
    """The whole graph as Graphviz DOT.

    mermaid cannot do this one: an ER diagram has no way to group entities, and the flowchart
    that can group them draws relationships as anonymous arrows between boxes. Graphviz keeps
    a cluster's tables together and attaches each edge to the column it comes from, so a
    relationship can be followed by eye at this size.
    """
    fk_columns = defaultdict(set)
    for table, column, _, _ in foreign:
        fk_columns[table].add(column)

    referencing = defaultdict(set)
    for table, _, target, _ in foreign:
        referencing[target].add(table)
    hubs = (
        {t for t, sources in referencing.items() if len(sources) >= HUB_REFERENCE_THRESHOLD}
        if clustered else set()
    )

    lines = [
        "digraph schema {",
        '  graph [rankdir=LR, splines=spline, nodesep=0.4, ranksep=1.4, bgcolor="#ffffff",'
        ' fontname="Helvetica", labelloc=t];',
        '  node [shape=plaintext, fontname="Helvetica", fontsize=11];',
        '  edge [color="#8a8a8a", penwidth=0.9, arrowsize=0.7];',
    ]
    groups = [(key, label, fill, stroke, [t for t in tables if tier_of(t) == key])
              for key, label, _, fill, stroke, _ in TIERS] if clustered else [
        (key, None, fill, stroke, [t for t in tables if tier_of(t) == key])
        for key, _, _, fill, stroke, _ in TIERS]
    for key, label, fill, stroke, members in groups:
        if not members:
            continue
        if label:
            lines.append(f"  subgraph cluster_{key} {{")
            lines.append(
                f'    label="{label}"; fontsize=20; fontcolor="{stroke}"; color="{stroke}";'
                " penwidth=2; style=rounded; margin=18;"
            )
        for table in members:
            rows = [
                f'<TR><TD BGCOLOR="{fill}" COLSPAN="2"><B>{table}</B></TD></TR>'
            ]
            if table in hubs:
                rows.append(
                    f'<TR><TD BGCOLOR="{fill}" COLSPAN="2"><FONT POINT-SIZE="9" '
                    f'COLOR="#666666">referenced by {len(referencing[table])} tables</FONT>'
                    "</TD></TR>"
                )
            for column in columns[table]:
                keys = []
                if column["name"] in primary[table]:
                    keys.append("PK")
                if column["name"] in fk_columns[table]:
                    keys.append("FK")
                if not keys:
                    continue
                # Two ports per row, one at each end, so an edge can be pinned to the side
                # it actually travels towards instead of leaving from whichever edge Graphviz
                # picks and doubling back across the table.
                rows.append(
                    f'<TR><TD PORT="{column["name"]}__l" ALIGN="LEFT">{column["name"]}</TD>'
                    f'<TD PORT="{column["name"]}__r" ALIGN="RIGHT">'
                    f'<FONT COLOR="#777777">{",".join(keys)}</FONT></TD></TR>'
                )
            table_html = (
                f'<TABLE BORDER="0" CELLBORDER="1" CELLSPACING="0" CELLPADDING="5" '
                f'COLOR="{stroke}">' + "".join(rows) + "</TABLE>"
            )
            lines.append(f'    "{table}" [label=<{table_html}>];')
        if label:
            lines.append("  }")
    for table, column, target, target_column in sorted(foreign):
        if table not in tables or target not in tables or target in hubs:
            continue
        nullable = any(c["name"] == column and c["nullable"] for c in columns[table])
        head = "teeodot" if nullable else "tee"
        # A self-reference has no side to travel towards, so it keeps the free attachment
        # and Graphviz draws it as a short loop rather than around the whole table.
        if table == target:
            tail_port, head_port = f'"{column}__l"', f'"{target_column}__l"'
        else:
            tail_port, head_port = f'"{column}__r":e', f'"{target_column}__l":w'
        lines.append(
            f'  "{table}":{tail_port} -> "{target}":{head_port}'
            f" [dir=both, arrowtail=crow, arrowhead={head}];"
        )
    lines.append("}")
    return "\n".join(lines)


def render(name):
    """The SVG a reader opens, from the DOT just written. Skipped where Graphviz is absent."""
    if not shutil.which("dot"):
        print(f"graphviz is not installed, {name}.svg not rendered", file=sys.stderr)
        return None
    out = subprocess.run(
        ["dot", "-Tsvg", "-o", str(DOCS / f"{name}.svg"), str(DOCS / f"{name}.dot")],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit(out.stderr.strip())
    return f"{name}.svg"


def main():
    tables, columns, primary, foreign, hypertables, aggregates, head = collect()

    assigned = {t for _, group in AREAS for t in group}
    areas = [(title, [t for t in group if t in tables]) for title, group in AREAS]
    leftover = sorted(t for t in tables if t not in assigned)
    if leftover:
        areas.append(("Other tables", leftover))

    fk_index = defaultdict(list)
    for table, column, target, target_column in foreign:
        fk_index[(table, column)].append(f"{target}.{target_column}")

    tier_counts = {key: 0 for key, *_ in TIERS}
    for table in tables:
        tier_counts[tier_of(table)] += 1
    out = [
        "# Database schema",
        "",
        "Generated from a migrated database by `scripts/generate-schema-docs.py`; edits here are",
        f"overwritten. Migration head `{head}`, {len(tables)} tables.",
        "",
        "Every table in one picture is [schema.svg](./schema.svg), grouped by tier. The tables",
        "most of the schema points at carry a reference count instead of their incoming edges,",
        "which would otherwise cross every group; the diagrams below draw every one of them,",
        "split by subject area, with each table's columns listed underneath.",
        "",
    ]

    out.append("Every diagram shades its tables by what they are for:")
    out.append("")
    out.append("| Tier | What it holds | Tables |")
    out.append("|------|---------------|--------|")
    for key, label, meaning, _, _, _ in TIERS:
        out.append(f"| {label} | {meaning} | {tier_counts[key]} |")
    out.append("")

    for name, interval in sorted(hypertables.items()):
        out.append(f"`{name}` is a TimescaleDB hypertable with a chunk interval of {interval}.")
    if aggregates:
        out.append("")
        out.append("Continuous aggregates: " + ", ".join(f"`{v}`" for v, _ in aggregates) + ".")
    out.append("")

    for title, members in areas:
        if not members:
            continue
        out.append(f"## {title}")
        out.append("")
        out.append("```mermaid")
        out.append(diagram(members, columns, primary, foreign, scoped=True))
        out.append("```")
        out.append("")
        for table in members:
            out.append(f"### {table}")
            out.append("")
            out.append("| Column | Type | Null | Default | References |")
            out.append("|--------|------|------|---------|------------|")
            for column in columns[table]:
                key = "PK " if column["name"] in primary[table] else ""
                default = " ".join(column["default"].split()).replace("|", "\\|")
                default = f"`{default}`" if default else ""
                refs = ", ".join(f"`{r}`" for r in fk_index[(table, column["name"])])
                out.append(
                    f'| {key}`{column["name"]}` | `{column["type"]}` | '
                    f'{"yes" if column["nullable"] else "no"} | {default} | {refs} |'
                )
            out.append("")

    DOCS.mkdir(exist_ok=True)
    (DOCS / "schema.md").write_text("\n".join(out).rstrip() + "\n")
    (DOCS / "schema.dot").write_text(
        dot_diagram(sorted(tables), columns, primary, foreign) + "\n"
    )
    core = [t for t in CORE if t in tables]
    (DOCS / "schema-core.dot").write_text(
        dot_diagram(core, columns, primary, foreign, clustered=False) + "\n"
    )
    rendered = [render(name) for name in ("schema", "schema-core")]
    written = ["schema.md", "schema.dot", "schema-core.dot"] + [r for r in rendered if r]
    print(f"wrote {', '.join(written)} ({len(tables)} tables)")


if __name__ == "__main__":
    main()
