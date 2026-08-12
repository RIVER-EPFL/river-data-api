#!/usr/bin/env python3
"""Build a pseudonymised suite of test fixtures from local production data.

Sources are developer-local and stay out of every repo (the same dumps back the rshiny connectors
in river-data-ui/docker-compose.yaml under the `portal` profile). This emits small committed slices
covering four source lineages and a range of ingestion scenarios, keeping the real measured values,
column headers, cadence and curve coefficients while carrying no site or personal identity.

Rules this enforces:
  - The portal `users`/`user` tables are never read. They hold usernames and password hashes, so
    they are excluded rather than pseudonymised. Same for `data_requests`, `comment`, and free-text
    note/description columns.
  - No real station, glacier or site name and no coordinate is ever emitted. Station identity maps
    to S01..Sn and glacier identity to G01..Gn by order of first appearance, consistently across
    every file so joins survive. Sites named only in a source filename are pseudonymised by rename.
  - The mapping is printed to stderr and never written to disk. A committed forward map would be
    the de-pseudonymisation key.
  - Timestamps are kept: they carry the cadence and gaps the tests exercise, and a bare timestamp
    identifies nothing once the station is mapped.

Every scenario is derived from real rows. Where a property is absent from the real data it is
produced by removing or reordering real rows, never by inventing values, and is labelled as such
in tests/fixtures/README.md.

Usage:
  build-test-fixtures.py --viewlinc-dir DIR --portal-dump cnet.sql --portal-dump metalp.sql \
                        [--nomis-dump nomis.sql] [--out tests/fixtures]
"""

import argparse
import csv
import re
import sys
from pathlib import Path

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

# NOMIS models a sampling event per glacier rather than a station timeseries. Its `biogeo_1` and
# `microbial_1` tables hold exactly one row per sampling unit (replicate always 'A'); the real
# replicate structure lives in `microbial_2`, which carries A/B pairs keyed by id_patch. That pair
# is the only genuine replicate grouping in this source, so it is what the replicate fixture uses.
NOMIS_LOCATION_COLUMNS = [
    "id_location", "id_glacier", "type", "date", "time",
    "water_temp", "ph", "do", "do_sat", "w_co2", "conductivity", "turb",
]
# The source primary keys (id_biogeo_1, id_microbial_2) embed the glacier code and sampling
# position, eg. "GL100_DN_1_A", so they are dropped rather than mapped. The location, patch and
# replicate triple identifies a row for test purposes without carrying that.
NOMIS_BIOGEO_COLUMNS = [
    "id_location", "replicate",
    "i1_na", "i2_k", "i3_mg", "i4_ca", "i5_cl", "i6_so4",
    "n1_tn", "n2_tp", "n3_srp", "n4_nh4", "n5_no3", "n6_no2",
]
NOMIS_MICROBIAL_COLUMNS = ["id_patch", "id_location", "replicate", "bp", "respiration"]

PORTAL_ROWS = 30
WIDE_HEAD_ROWS = 100
WIDE_BODY_ROWS = 100
NARROW_ROWS = 150
SCENARIO_ROWS = 120
NOMIS_LOCATION_ROWS = 25
NOMIS_REPLICATE_ROWS = 36
BUDGET_BYTES = 100 * 1024


class Aliases:
    """Assigns PREFIX01..n by order of first appearance. Never persisted."""

    def __init__(self, prefix):
        self.prefix = prefix
        self._map = {}

    def get(self, raw):
        key = str(raw or "").strip()
        if not key:
            return ""
        if key not in self._map:
            self._map[key] = f"{self.prefix}{len(self._map) + 1:02d}"
        return self._map[key]

    def items(self):
        return self._map.items()


def parse_create_columns(sql, table):
    m = re.search(r"CREATE TABLE `%s` \((.*?)\n\) ENGINE=" % re.escape(table), sql, re.S)
    if not m:
        raise SystemExit(f"table `{table}` not found in dump")
    return [cm.group(1) for cm in (re.match(r"\s*`([^`]+)`\s", l) for l in m.group(1).splitlines()) if cm]


def split_tuples(values_blob):
    """Split a MySQL VALUES blob into rows of raw field strings.

    Hand-rolled because the blob mixes quoted strings containing commas and parentheses with bare
    numbers and NULLs; a naive split on '),(' corrupts any row holding a comma inside a string.
    """
    rows, field, row = [], [], []
    in_str = escaped = False
    depth = 0
    for ch in values_blob:
        if in_str:
            if escaped:
                field.append(ch); escaped = False
            elif ch == "\\":
                field.append(ch); escaped = True
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
                row.append("".join(field)); rows.append(row); field = []
        elif ch == "," and depth == 1:
            row.append("".join(field)); field = []
        elif depth == 1:
            field.append(ch)
    return rows


def extract_insert(sql, table):
    m = re.search(r"INSERT INTO `%s` VALUES (.*?);\n" % re.escape(table), sql, re.S)
    return split_tuples(m.group(1)) if m else []


def norm(v):
    """MySQL NULL becomes an empty CSV cell; everything else passes through verbatim."""
    return "" if str(v).strip().upper() == "NULL" else str(v).strip()


def write_csv(path, header, rows):
    with path.open("w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(header)
        w.writerows(rows)


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


# ---------------------------------------------------------------- portal (CNET / METALP)

def build_portal(dump_path, label, out_dir, stations):
    sql = Path(dump_path).read_text(encoding="utf8", errors="replace")

    curves = extract_insert(sql, "standard_curves")
    if curves:
        cols = parse_create_columns(sql, "standard_curves")
        keep = ["date", "parameter", "a", "b"]
        idx = [cols.index(c) for c in keep]
        write_csv(out_dir / f"portal_standard_curves_{label}.csv", keep,
                  [[norm(r[i]) for i in idx] for r in curves])

    data_cols = parse_create_columns(sql, "data")
    rows = extract_insert(sql, "data")
    present = [c for c in PORTAL_COLUMNS if c in data_cols]
    col_idx = [data_cols.index(c) for c in present]
    density = lambda r: sum(1 for i in col_idx if norm(r[i]))

    doc_i = [data_cols.index(c) for c in ("DOC_rep_1", "DOC_rep_2", "DOC_rep_3") if c in data_cols]
    depth_i = [data_cols.index(c) for c in ("Reach_depth_rep_1", "Reach_depth_rep_2") if c in data_cols]
    gmt_i = data_cols.index("Convert_to_GMT")

    # Chosen deliberately rather than taking the first N: the tests need fully populated rows, rows
    # whose replicates are NULL, and rows carrying a non-zero UTC offset.
    picked, seen = [], set()

    def take(cands, n):
        for r in cands:
            if len(picked) >= PORTAL_ROWS or n <= 0:
                return
            if id(r) in seen:
                continue
            seen.add(id(r)); picked.append(r); n -= 1

    by_density = sorted(rows, key=density, reverse=True)
    if doc_i:
        take([r for r in by_density if all(norm(r[i]) for i in doc_i)], 8)
        take([r for r in by_density if not any(norm(r[i]) for i in doc_i)], 6)
    if depth_i:
        take([r for r in by_density if all(norm(r[i]) for i in depth_i)], 6)
    take([r for r in rows if norm(r[gmt_i]) not in ("", "00:00:00")], 5)
    take(by_density, PORTAL_ROWS - len(picked))

    st_i, d_i, t_i = data_cols.index("station"), data_cols.index("DATE_reading"), data_cols.index("TIME_reading")
    picked.sort(key=lambda r: (stations.get(r[st_i]), norm(r[d_i]), norm(r[t_i])))

    out = []
    for r in picked:
        vals = [norm(r[i]) for i in col_idx]
        vals[present.index("station")] = stations.get(r[st_i])
        out.append(vals)
    write_csv(out_dir / f"portal_grab_rows_{label}.csv", present, out)


# ---------------------------------------------------------------- NOMIS

def build_nomis(dump_path, out_dir, glaciers, locations):
    sql = Path(dump_path).read_text(encoding="utf8", errors="replace")

    loc_cols = parse_create_columns(sql, "location")
    loc_rows = extract_insert(sql, "location")
    keep = [c for c in NOMIS_LOCATION_COLUMNS if c in loc_cols]
    idx = [loc_cols.index(c) for c in keep]
    measured = [c for c in keep if c not in ("id_location", "id_glacier", "type", "date", "time")]
    m_idx = [loc_cols.index(c) for c in measured]

    ranked = sorted(loc_rows, key=lambda r: sum(1 for i in m_idx if norm(r[i])), reverse=True)
    chosen = ranked[:NOMIS_LOCATION_ROWS]
    li, gi = loc_cols.index("id_location"), loc_cols.index("id_glacier")
    for r in chosen:
        locations.get(norm(r[li])); glaciers.get(norm(r[gi]))

    out = []
    for r in chosen:
        vals = [norm(r[i]) for i in idx]
        vals[keep.index("id_location")] = locations.get(norm(r[li]))
        vals[keep.index("id_glacier")] = glaciers.get(norm(r[gi]))
        out.append(vals)
    out.sort(key=lambda v: v[keep.index("id_location")])
    write_csv(out_dir / "nomis_location_rows.csv", keep, out)

    bio_cols = parse_create_columns(sql, "biogeo_1")
    bio_rows = extract_insert(sql, "biogeo_1")
    bkeep = [c for c in NOMIS_BIOGEO_COLUMNS if c in bio_cols]
    bidx = [bio_cols.index(c) for c in bkeep]
    bl = bio_cols.index("id_location")

    chosen_locs = {norm(x[li]) for x in chosen}
    bout = []
    for r in bio_rows:
        if norm(r[bl]) not in chosen_locs:
            continue
        vals = [norm(r[i]) for i in bidx]
        vals[bkeep.index("id_location")] = locations.get(norm(r[bl]))
        bout.append(vals)
    bout.sort(key=lambda v: v[bkeep.index("id_location")])
    write_csv(out_dir / "nomis_biogeo_chemistry.csv", bkeep, bout)

    build_nomis_replicates(sql, out_dir, locations)


def build_nomis_replicates(sql, out_dir, locations):
    """Whole A/B replicate groups from `microbial_2`, keyed by patch.

    A partial group would not exercise the sample statistics trigger, so groups are kept intact.
    """
    patch_cols = parse_create_columns(sql, "patch")
    patch_rows = extract_insert(sql, "patch")
    pi, pli = patch_cols.index("id_patch"), patch_cols.index("id_location")
    patch_to_loc = {norm(r[pi]): norm(r[pli]) for r in patch_rows}

    mic_cols = parse_create_columns(sql, "microbial_2")
    mic_rows = extract_insert(sql, "microbial_2")
    mpi = mic_cols.index("id_patch")

    groups = {}
    for r in mic_rows:
        groups.setdefault(norm(r[mpi]), []).append(r)

    patches = Aliases("P")
    keep = [c for c in NOMIS_MICROBIAL_COLUMNS if c in mic_cols or c == "id_location"]
    src_idx = {c: mic_cols.index(c) for c in keep if c in mic_cols}

    out = []
    for patch_id, rows in sorted(groups.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        if len(out) >= NOMIS_REPLICATE_ROWS or len(rows) < 2:
            break
        for r in sorted(rows, key=lambda x: norm(x[mic_cols.index("replicate")])):
            vals = []
            for c in keep:
                if c == "id_patch":
                    vals.append(patches.get(patch_id))
                elif c == "id_location":
                    vals.append(locations.get(patch_to_loc.get(patch_id, "")))
                else:
                    vals.append(norm(r[src_idx[c]]))
            out.append(vals)
    write_csv(out_dir / "nomis_microbial_replicates.csv", keep, out)


# ---------------------------------------------------------------- viewLinc scenarios

def load_csv(path):
    with path.open() as fh:
        rd = csv.reader(fh)
        return next(rd), list(rd)


def window_around(rows, col_idx, predicate, size):
    """The first `size`-row window containing at least one row matching `predicate`."""
    for i, r in enumerate(rows):
        v = num(r[col_idx]) if col_idx < len(r) else None
        if v is not None and predicate(v):
            start = max(0, i - size // 2)
            return rows[start:start + size]
    return []


def build_viewlinc(viewlinc_dir, out_dir):
    d = Path(viewlinc_dir)
    wide_src = next((p for p in sorted(d.glob("*.csv")) if "martigny" in p.name.lower()), None)
    alt_src = next((p for p in sorted(d.glob("*.csv")) if "dailles" in p.name.lower()), None)
    narrow_src = next(iter(sorted(d.glob("cleaned_data_*.csv"))), None)
    dup_src = next((p for p in sorted(d.glob("cleaned_data_*.csv")) if "verbier" in p.name.lower()), narrow_src)
    if not (wide_src and narrow_src):
        raise SystemExit(f"expected a martigny export and a cleaned_data_* export in {d}")

    header, rows = load_csv(wide_src)
    write_csv(out_dir / "viewlinc_wide_sparse.csv", header,
              rows[:WIDE_HEAD_ROWS] + rows[len(rows) // 2:][:WIDE_BODY_ROWS])

    nheader, nrows = load_csv(narrow_src)
    write_csv(out_dir / "viewlinc_narrow.csv", nheader, nrows[:NARROW_ROWS])

    col = {c: i for i, c in enumerate(header)}

    # Battery crossing the documented alarm bound and recovering: drives an alarm event open->resolve.
    if "BatteryVolt" in col:
        w = window_around(rows, col["BatteryVolt"], lambda v: v < 11.5, SCENARIO_ROWS)
        if w:
            write_csv(out_dir / "viewlinc_alarm_excursion.csv", header, w)

    # Real sensor faults: negative dissolved oxygen and conductivity.
    if "DOuM" in col:
        w = window_around(rows, col["DOuM"], lambda v: v < 0, SCENARIO_ROWS)
        if w:
            write_csv(out_dir / "viewlinc_sensor_fault.csv", header, w)

    # Real duplicate timestamps, the CSV importer's conflict-mode case.
    dheader, drows = load_csv(dup_src)
    seen, dups = {}, set()
    for r in drows:
        if r[0] in seen:
            dups.add(r[0])
        seen[r[0]] = True
    if dups:
        keep, added = [], 0
        for r in drows:
            if r[0] in dups:
                keep.append(r); added += 1
            elif added and len(keep) < 60:
                keep.append(r)
            if len(keep) >= 60:
                break
        write_csv(out_dir / "viewlinc_duplicate_timestamps.csv", dheader, keep)

    # No real outage exists in these exports, so the gap is produced by removing a contiguous
    # stretch of real rows. Values are untouched.
    src_rows = rows if not alt_src else load_csv(alt_src)[1]
    src_header = header if not alt_src else load_csv(alt_src)[0]
    if len(src_rows) > 400:
        gapped = src_rows[100:160] + src_rows[220:280]
        write_csv(out_dir / "viewlinc_gap.csv", src_header, gapped)

    return {wide_src.name: "viewlinc_wide_sparse.csv", narrow_src.name: "viewlinc_narrow.csv",
            dup_src.name: "viewlinc_duplicate_timestamps.csv",
            (alt_src.name if alt_src else wide_src.name): "viewlinc_gap.csv"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--viewlinc-dir", required=True)
    ap.add_argument("--portal-dump", action="append", default=[])
    ap.add_argument("--nomis-dump")
    ap.add_argument("--out", default="tests/fixtures")
    args = ap.parse_args()

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    stations = Aliases("S")
    glaciers = Aliases("G")
    locations = Aliases("L")

    for dump in args.portal_dump:
        label = Path(dump).stem.split("_")[0]
        build_portal(dump, label, out_dir, stations)
    if args.nomis_dump:
        build_nomis(args.nomis_dump, out_dir, glaciers, locations)
    renames = build_viewlinc(args.viewlinc_dir, out_dir)

    print("pseudonym mapping (not written to disk):", file=sys.stderr)
    for label, alias in list(stations.items()) + list(glaciers.items()) + list(locations.items()):
        print(f"  {label} -> {alias}", file=sys.stderr)
    for src, dst in renames.items():
        print(f"  {src} -> {dst}", file=sys.stderr)

    total = 0
    for p in sorted(out_dir.glob("*.csv")):
        total += p.stat().st_size
        print(f"{p.name:38} {p.stat().st_size / 1024:7.1f} KB")
    print(f"{'total':38} {total / 1024:7.1f} KB")
    if total > BUDGET_BYTES:
        raise SystemExit(f"fixtures exceed the {BUDGET_BYTES // 1024}KB budget")


if __name__ == "__main__":
    main()
