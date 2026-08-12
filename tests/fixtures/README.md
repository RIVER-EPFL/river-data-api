# Test fixtures

A pseudonymised suite of real production data covering the four source lineages this system
ingests from, sliced into per-scenario files. Used as ingestion input by the e2e onboarding tracks
and by the CSV, tools and grab-sample tests.

Regenerate with `scripts/build-test-fixtures.py`. The sources it reads are developer-local and are
deliberately not in any repo; the same dumps back the rshiny connectors in
`river-data-ui/docker-compose.yaml` under the `portal` profile.

```
python3 scripts/build-test-fixtures.py \
  --viewlinc-dir "<local viewLinc export dir>" \
  --portal-dump <cnet>.sql --portal-dump <metalp>.sql --nomis-dump <nomis>.sql
```

## Continuous sensor series (viewLinc lineage)

| File | Scenario |
|------|----------|
| `viewlinc_wide_sparse.csv` | 200 rows, 9 columns, 10-minute cadence. The first 100 rows are the sparse head where only `Depthmm` and `CDOMppb` are populated; the rest are fully populated. Column resolution and null handling. |
| `viewlinc_narrow.csv` | 150 rows, 4 columns, 30-minute cadence. Headers differ in case from the wide file (`DateTime` vs `datetime`, `WaterTempdegC` vs `WaterTempDegC`), which is what exercises case-insensitive column resolution. |
| `viewlinc_alarm_excursion.csv` | 120 rows spanning a real battery excursion below the 11.5 V alarm bound and back into range, including a 0.00 V flatline. Drives an alarm event from open to resolved. |
| `viewlinc_sensor_fault.csv` | 120 rows around a real negative dissolved-oxygen reading (to -156 uM). Out-of-domain values that are not nulls. |
| `viewlinc_duplicate_timestamps.csv` | 60 rows including a real timestamp repeated three times. CSV import conflict modes. |
| `viewlinc_gap.csv` | 120 rows with a 10.2-hour hole. **Derived**: the exports contain no real outage, so a contiguous stretch of rows was removed. The remaining values are untouched. |

## Grab and lab records (CNET and METALP portals)

| File | Scenario |
|------|----------|
| `portal_grab_rows_{cnet,metalp}.csv` | 30 rows each, column-subset from the ~200-column `data` table to what the tools read. Two independent station namespaces. Rows are chosen to include fully populated ones, ones with NULL replicates, and ones carrying a non-zero `Convert_to_GMT` offset. Replicate columns (`DOC_rep_1..3`, `Reach_depth_rep_1..10`) drive the replicate fan-out and sample statistics. |
| `portal_standard_curves_{cnet,metalp}.csv` | Real slope/intercept pairs for the chla acid and no-acid and Vaisala corrections, including two generations of the same curve at different dates. |

## Glacier expedition sampling (NOMIS portal)

A different shape entirely: sampling events per glacier rather than a station timeseries.

| File | Scenario |
|------|----------|
| `nomis_location_rows.csv` | 25 sampling events with field measurements (`water_temp`, `ph`, `do`, `do_sat`, `w_co2`, `conductivity`, `turb`). Note `date` is `DD.MM.YYYY`, a third date format. |
| `nomis_biogeo_chemistry.csv` | Ion and nutrient chemistry per sampling event. Contains the `-9999` missing-value sentinel used by this source, which is not a null. |
| `nomis_microbial_replicates.csv` | 36 rows in 18 complete A/B replicate pairs keyed by patch. This is the only genuine replicate grouping in the source: `biogeo_1` and `microbial_1` hold one row per unit with the replicate always `A`. |

## What was removed

- The portal `users` and `user` tables are never read. They hold usernames and password hashes, so
  they are excluded rather than pseudonymised. `data_requests`, `comment` and free-text note and
  description columns are also excluded.
- No real station, glacier or site name and no coordinate is emitted. Station identity maps to
  `S01..Sn`, glacier to `G01..Gn`, location to `L01..Ln` and patch to `P01..Pn`, by order of first
  appearance and consistently across every file so joins survive. The mapping is printed to stderr
  by the build script and is stored nowhere.
- NOMIS source primary keys (`id_biogeo_1`, `id_microbial_2`) are dropped rather than mapped: they
  embed the glacier code and sampling position, eg. `GL100_DN_1_A`.
- The viewLinc site name lived only in the export filenames, never in their contents, so those
  files are pseudonymised by the rename.

Measured values, column headers, cadence, gaps, timestamps and curve coefficients are kept as they
are: they are what the tests assert against.
