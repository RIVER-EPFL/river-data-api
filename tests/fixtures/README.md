# Test fixtures

Pseudonymised slices of real production data, used as ingestion input by the e2e tracks and the
CSV, tools and grab-sample tests. Regenerate with `scripts/build-test-fixtures.py`; the sources it
reads are developer-local and are deliberately not in any repo.

| File | Source | Content |
|------|--------|---------|
| `viewlinc_wide_sparse.csv` | viewLinc site export | 200 rows, 9 columns, 10-minute cadence. The first 100 rows are the sparse head where only `Depthmm` and `CDOMppb` are populated; the rest are fully populated. |
| `viewlinc_narrow.csv` | cleaned site export | 150 rows, 4 columns, 30-minute cadence. Headers differ in case from the wide file (`DateTime` vs `datetime`, `WaterTempdegC` vs `WaterTempDegC`), which is what exercises case-insensitive column resolution. |
| `portal_grab_rows.csv` | portal `data` table | 30 grab rows, column-subset from ~200 columns to what the tools read. Rows are chosen to include fully populated ones, ones with NULL replicates, and ones carrying a non-zero `Convert_to_GMT` offset. |
| `portal_standard_curves.csv` | portal `standard_curves` | All 6 rows, real slope/intercept pairs for the chla acid/no-acid and Vaisala corrections. |

## What was removed

- The portal `users` table is never read. It holds usernames and password hashes, so it is excluded
  rather than pseudonymised. `data_requests` and free-text note and description columns are also
  excluded.
- Station identity is mapped to `S01..Sn` by order of first appearance, consistently across every
  file. The mapping is printed to stderr by the build script and is not stored anywhere.
- Coordinates are not exported.
- The site name lived only in the viewLinc filenames, never in their contents, so those files are
  pseudonymised by the rename.

Measured values, column headers, cadence, gaps, timestamps and curve coefficients are kept as they
are: they are what the tests assert against.
