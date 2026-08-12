#!/usr/bin/env python3
"""Build pseudonymised test fixtures from local production data.

The sources are developer-local and stay out of every repo. This emits small, committed slices
under tests/fixtures/ that keep the real measured values, column headers, cadence and curve
coefficients while carrying no site or personal identity.

Rules this enforces:
  - The portal `users` table is never read. It holds real usernames and password hashes;
    credentials are excluded rather than pseudonymised. Same for `data_requests` and free-text
    note/description columns.
  - Station identity is mapped to S01..Sn by order of first appearance, consistently across every
    emitted file so relational integrity survives.
  - The mapping is printed to stdout and never written to disk. A committed forward map would be
    the de-pseudonymisation key.
  - Timestamps are kept: they carry the cadence and gaps the tests exercise, and a bare timestamp
    identifies nothing once the station is mapped.

Usage:
  build-test-fixtures.py --viewlinc-dir DIR --portal-dump FILE.sql [--out tests/fixtures]
"""

import argparse
import csv
import io
import re
import sys
from pathlib import Path

# Columns kept from the portal's ~200-column `data` table. Subsetting, not row count, is what
# keeps this fixture small and readable: it turns a ~1KB row into ~300 bytes.
PORTAL_COLUMNS = [
    "station", "DATE_reading", "TIME_reading", "Convert_to_GMT",
    "WTW_DO_mgL_1", "WTW_DO_sat_1", "WTW_Temp_degC_1", "WTW_pH_1",
    "WTW_Spec_Cond_uScm_1", "WTW_TURB_NTU",
    "Field_BP", "Field_BP_altitude",
    "Vaisala_CO2_min", "Vaisala_CO2_avg", "Vaisala_CO2_max",
    "Reach_depth_avg_cm", "Reach_depth_sd_cm",
    "Reach_depth_rep_1", "Reach_depth_rep_2", "Reach_depth_rep_3", "Reach_depth_rep_4",
    "Reach_depth_rep_5", "Reach_depth_rep_6", "Reach_depth_rep_7", "Reach_depth_rep_8",
    "Reach_depth_rep_9", "Reach_depth_rep_10",
    "DOC_avg_ppb", "DOC_sd_ppb", "DOC_rep_1", "DOC_rep_2", "DOC_rep_3",
    "Alk_init_pH", "Alk_meqL", "Alk_mgL", "Alk_temp_degC",
]

PORTAL_ROWS = 30
WIDE_HEAD_ROWS = 100
WIDE_BODY_ROWS = 100
NARROW_ROWS = 150


class StationMap:
    """Assigns S01..Sn by order of first appearance. Never persisted."""

    def __init__(self):
        self._map = {}

    def get(self, raw):
        key = (raw or "").strip()
        if not key:
            return ""
        if key not in self._map:
            self._map[key] = f"S{len(self._map) + 1:02d}"
        return self._map[key]

    def report(self):
        return dict(self._map)


def parse_create_columns(sql, table):
    """Column names of `table`, in declaration order."""
    m = re.search(r"CREATE TABLE `%s` \((.*?)\n\) ENGINE=" % re.escape(table), sql, re.S)
    if not m:
        raise SystemExit(f"table `{table}` not found in dump")
    cols = []
    for line in m.group(1).splitlines():
        cm = re.match(r"\s*`([^`]+)`\s", line)
        if cm:
            cols.append(cm.group(1))
    return cols


def split_tuples(values_blob):
    """Split a MySQL VALUES blob into rows of raw field strings.

    Hand-rolled because the blob mixes quoted strings containing commas and parentheses with bare
    numbers and NULLs; a naive split on '),(' corrupts any row holding a comma inside a string.
    """
    rows, field, row = [], [], []
    in_str = False
    escaped = False
    depth = 0
    for ch in values_blob:
        if in_str:
            if escaped:
                field.append(ch)
                escaped = False
            elif ch == "\\":
                field.append(ch)
                escaped = True
            elif ch == "'":
                in_str = False
            else:
                field.append(ch)
            continue
        if ch == "'":
            in_str = True
        elif ch == "(":
            depth += 1
            if depth == 1:
                field, row = [], []
        elif ch == ")":
            depth -= 1
            if depth == 0:
                row.append("".join(field))
                rows.append(row)
                field = []
        elif ch == "," and depth == 1:
            row.append("".join(field))
            field = []
        elif depth == 1:
            field.append(ch)
    return rows


def extract_insert(sql, table):
    m = re.search(r"INSERT INTO `%s` VALUES (.*?);\n" % re.escape(table), sql, re.S)
    if not m:
        raise SystemExit(f"no INSERT for `{table}`")
    return split_tuples(m.group(1))


def norm(v):
    """MySQL NULL becomes an empty CSV cell; everything else passes through verbatim."""
    return "" if v.strip().upper() == "NULL" else v.strip()


def build_portal_fixtures(dump_path, out_dir, station_map):
    sql = Path(dump_path).read_text(encoding="utf8", errors="replace")

    curve_cols = parse_create_columns(sql, "standard_curves")
    curves = extract_insert(sql, "standard_curves")
    keep = ["date", "parameter", "a", "b"]
    idx = [curve_cols.index(c) for c in keep]
    with (out_dir / "portal_standard_curves.csv").open("w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(keep)
        for r in curves:
            w.writerow([norm(r[i]) for i in idx])

    data_cols = parse_create_columns(sql, "data")
    missing = [c for c in PORTAL_COLUMNS if c not in data_cols]
    if missing:
        raise SystemExit(f"dump is missing expected columns: {missing}")
    col_idx = [data_cols.index(c) for c in PORTAL_COLUMNS]
    rows = extract_insert(sql, "data")

    def density(r):
        return sum(1 for i in col_idx if norm(r[i]))

    doc_i = [data_cols.index(c) for c in ("DOC_rep_1", "DOC_rep_2", "DOC_rep_3")]
    depth_i = [data_cols.index(c) for c in ("Reach_depth_rep_1", "Reach_depth_rep_2")]
    gmt_i = data_cols.index("Convert_to_GMT")

    # Chosen deliberately rather than taking the first N: the tests need a fully populated row, a
    # row whose replicates are NULL, and a row carrying a non-zero UTC offset.
    picked, seen = [], set()

    def take(candidates, n):
        for r in candidates:
            if len(picked) >= PORTAL_ROWS or n <= 0:
                return
            key = id(r)
            if key in seen:
                continue
            seen.add(key)
            picked.append(r)
            n -= 1

    by_density = sorted(rows, key=density, reverse=True)
    take([r for r in by_density if all(norm(r[i]) for i in doc_i)], 8)
    take([r for r in by_density if not any(norm(r[i]) for i in doc_i)], 6)
    take([r for r in by_density if all(norm(r[i]) for i in depth_i)], 6)
    take([r for r in rows if norm(r[gmt_i]) not in ("", "00:00:00")], 5)
    take(by_density, PORTAL_ROWS - len(picked))

    station_i = data_cols.index("station")
    date_i = data_cols.index("DATE_reading")
    time_i = data_cols.index("TIME_reading")
    picked.sort(key=lambda r: (station_map.get(r[station_i]), norm(r[date_i]), norm(r[time_i])))

    with (out_dir / "portal_grab_rows.csv").open("w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(PORTAL_COLUMNS)
        for r in picked:
            out = [norm(r[i]) for i in col_idx]
            out[0] = station_map.get(r[station_i])
            w.writerow(out)


def build_viewlinc_fixtures(viewlinc_dir, out_dir):
    """Emit a wide sparse export and a narrow one whose header casing differs.

    The site lives only in these files' names, never in their contents, so the pseudonymisation
    here is the rename.
    """
    d = Path(viewlinc_dir)
    wide_src = next((p for p in sorted(d.glob("*.csv")) if "martigny" in p.name.lower()), None)
    narrow_src = next((p for p in sorted(d.glob("cleaned_data_*.csv"))), None)
    if wide_src is None or narrow_src is None:
        raise SystemExit(f"expected a martigny export and a cleaned_data_* export in {d}")

    with wide_src.open() as fh:
        reader = csv.reader(fh)
        header = next(reader)
        rows = list(reader)
    head = rows[:WIDE_HEAD_ROWS]
    body = rows[len(rows) // 2:][:WIDE_BODY_ROWS]
    with (out_dir / "viewlinc_wide_sparse.csv").open("w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(header)
        w.writerows(head + body)

    with narrow_src.open() as fh:
        reader = csv.reader(fh)
        header = next(reader)
        rows = [next(reader) for _ in range(NARROW_ROWS)]
    with (out_dir / "viewlinc_narrow.csv").open("w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(header)
        w.writerows(rows)

    return wide_src.name, narrow_src.name


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--viewlinc-dir", required=True)
    ap.add_argument("--portal-dump", required=True)
    ap.add_argument("--out", default="tests/fixtures")
    args = ap.parse_args()

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    station_map = StationMap()
    build_portal_fixtures(args.portal_dump, out_dir, station_map)
    wide, narrow = build_viewlinc_fixtures(args.viewlinc_dir, out_dir)

    print("station mapping (not written to disk):", file=sys.stderr)
    for raw, alias in station_map.report().items():
        print(f"  {raw} -> {alias}", file=sys.stderr)
    print(f"  {wide} -> viewlinc_wide_sparse.csv", file=sys.stderr)
    print(f"  {narrow} -> viewlinc_narrow.csv", file=sys.stderr)

    total = 0
    for p in sorted(out_dir.glob("*.csv")):
        size = p.stat().st_size
        total += size
        print(f"{p.name:34} {size / 1024:7.1f} KB")
    print(f"{'total':34} {total / 1024:7.1f} KB")
    if total > 100 * 1024:
        raise SystemExit("fixtures exceed the 100KB budget")


if __name__ == "__main__":
    main()
