#!/usr/bin/env bash
# Compare two databases three ways: public DDL, TimescaleDB objects, and per-table content hashes.
# Usage: dbdiff.sh <db_a> <db_b> [port]
#
# Verify that a baseline builds the same database as the migration chain.
# TimescaleDB numbers each materialised hypertable from a global sequence, so a database that
# created and dropped continuous aggregates on the way to its final shape carries higher internal
# ids than one that created them once. The numbering is internal and carries no meaning, so it is
# normalised out; everything else is compared verbatim.
#
# IGNORE_TABLES skips content comparison for tables whose rows are an artifact of how the database
# was built rather than part of its definition.
# NORMALIZE_SEEDS=1 excludes creation times and generated audit ids for seeded constants.
set -euo pipefail
export PGPASSWORD=psql
A=$1; B=$2; PORT=${3:-5460}
IGNORE_TABLES=${IGNORE_TABLES-reprocessing_jobs}
NORMALIZE_SEEDS=${NORMALIZE_SEEDS:-0}

norm() { sed -E 's/_materialized_hypertable_[0-9]+/_materialized_hypertable_N/g;
                 s/"mat_hypertable_id": [0-9]+/"mat_hypertable_id": N/g'; }
OUT=$(mktemp -d)
q() { local o; o=$(psql -h localhost -p "$PORT" -U postgres -d "$1" -tAF'|' -v ON_ERROR_STOP=1 -c "$2" 2>&1) || { echo "QUERY FAILED on $1: $o" >&2; exit 9; }; printf '%s\n' "$o"; }

dump_ddl() {
  pg_dump -h localhost -p "$PORT" -U postgres -d "$1" --schema-only --schema=public \
    --no-owner --no-privileges --no-comments \
  | grep -v '^--' | grep -v '^$' | grep -v '^\\restrict' | grep -v '^\\unrestrict' \
  | grep -v '^SET ' | grep -v '^SELECT pg_catalog.set_config'
}

dump_ts() {
  q "$1" "select 'HYPERTABLE '||hypertable_name||' compress='||compression_enabled
        from timescaledb_information.hypertables order by 1"
  q "$1" "select 'DIMENSION '||hypertable_name||' '||column_name||' '||coalesce(time_interval::text,'')
        from timescaledb_information.dimensions where hypertable_name not like '\_materialized%' order by 1"
  q "$1" "select 'CAGG '||view_name||' matonly='||materialized_only||' finalized='||finalized
        from timescaledb_information.continuous_aggregates order by 1"
  q "$1" "select 'CAGGDEF '||view_name||' '||md5(view_definition)
        from timescaledb_information.continuous_aggregates order by 1"
  q "$1" "select 'POLICY '||proc_name||' '||coalesce(hypertable_name,'')||' '||coalesce(config::text,'')
        ||' schedule='||schedule_interval||' scheduled='||scheduled
        from timescaledb_information.jobs where proc_name not in ('policy_telemetry','policy_job_stat_history_retention') order by 1"
  q "$1" "select 'COMPRESS '||hypertable_name||' '||coalesce(attname,'')||' seg='||coalesce(segmentby_column_index::text,'')||' ord='||coalesce(orderby_column_index::text,'')
        from timescaledb_information.compression_settings order by 1"
}

dump_data() {
  local db=$1
  local tables; tables=$(q "$db" "select tablename from pg_tables where schemaname='public' and tablename<>'seaql_migrations' order by 1")
  for t in $tables; do
    case " $IGNORE_TABLES " in *" $t "*) continue;; esac
    local row='to_jsonb(x)'
    if [ "$NORMALIZE_SEEDS" = 1 ]; then
      case "$t" in
        constants) row="to_jsonb(x) - 'created_at'";;
        change_audit) row="CASE WHEN x.change = 'constant_insert' THEN
          (to_jsonb(x) - 'id' - 'changed_at') ||
          jsonb_build_object('new_value', x.new_value - 'created_at') ELSE to_jsonb(x) END";;
      esac
    fi
    local r; r=$(q "$db" "select count(*)||' '||coalesce(md5(string_agg(h,'' order by h)),'-') from (select md5(($row)::text) h from public.\"$t\" x) s")
    echo "$t $r"
  done
}

status=0
for kind in ddl ts data; do
  dump_$kind "$A" | norm > "$OUT/a.$kind"
  dump_$kind "$B" | norm > "$OUT/b.$kind"
  if diff -u "$OUT/a.$kind" "$OUT/b.$kind" > "$OUT/$kind.diff"; then
    echo "OK    $kind  identical ($(wc -l < "$OUT/a.$kind") lines)"
  else
    echo "DIFF  $kind"
    head -60 "$OUT/$kind.diff"
    status=1
  fi
done
echo "artifacts: $OUT"
exit "$status"
