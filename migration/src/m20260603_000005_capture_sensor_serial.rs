use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    // Sensors created via stream pairing historically left `serial_number` NULL while stashing the
    // device serial in `metadata->>'source_device_serial'` (and the raw stream carries it at
    // `data_streams.metadata->'device'->>'logger_serial'`). This backfills `serial_number` from that
    // already-present data — no device-specific values are hard-coded here. Because the partial unique
    // index on (serial_number, parameter_id) was dormant while serials were NULL, duplicate sensors
    // for the same (device, parameter) may exist; those are merged into the oldest survivor first.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                -- Repointing readings.sensor_id (step 2) rewrites rows in compressed hypertable
                -- chunks, which TimescaleDB caps at max_tuples_decompressed_per_dml_transaction
                -- (default 100k). A real merge on a populated DB exceeds that and aborts the whole
                -- migration. Lift the cap for this transaction only; SET LOCAL resets on commit.
                SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

                -- 0. Resolve each serial-less sensor's device serial from existing metadata.
                CREATE TEMP TABLE _serial_res ON COMMIT DROP AS
                SELECT
                    s.id,
                    s.parameter_id,
                    s.created_at,
                    COALESCE(
                        NULLIF(s.metadata->>'source_device_serial', ''),
                        (SELECT NULLIF(ds.metadata->'device'->>'logger_serial', '')
                         FROM data_streams ds
                         WHERE ds.sensor_id = s.id
                           AND NULLIF(ds.metadata->'device'->>'logger_serial', '') IS NOT NULL
                         ORDER BY ds.created_at NULLS LAST
                         LIMIT 1)
                    ) AS resolved_serial
                FROM sensors s
                WHERE s.serial_number IS NULL;

                DELETE FROM _serial_res WHERE resolved_serial IS NULL;

                -- 1. Map duplicate sensors (same resolved serial + parameter) to a canonical survivor.
                CREATE TEMP TABLE _dup_map ON COMMIT DROP AS
                WITH ranked AS (
                    SELECT
                        id,
                        first_value(id) OVER (
                            PARTITION BY resolved_serial, parameter_id
                            ORDER BY created_at NULLS LAST, id
                        ) AS canonical_id
                    FROM _serial_res
                )
                SELECT id AS dup_id, canonical_id
                FROM ranked
                WHERE id <> canonical_id;

                -- 2. Repoint child FKs from duplicates onto the canonical sensor.
                UPDATE readings r SET sensor_id = m.canonical_id
                    FROM _dup_map m WHERE r.sensor_id = m.dup_id;
                UPDATE sensor_calibrations c SET sensor_id = m.canonical_id
                    FROM _dup_map m WHERE c.sensor_id = m.dup_id;
                UPDATE sensor_deployments d SET sensor_id = m.canonical_id
                    FROM _dup_map m WHERE d.sensor_id = m.dup_id;
                UPDATE data_streams ds SET sensor_id = m.canonical_id
                    FROM _dup_map m WHERE ds.sensor_id = m.dup_id;

                -- 3. Remove the now-empty duplicate sensors.
                DELETE FROM sensors s USING _dup_map m WHERE s.id = m.dup_id;

                -- 4. Backfill serial_number on the survivors.
                UPDATE sensors s SET serial_number = r.resolved_serial
                    FROM _serial_res r
                    WHERE s.id = r.id AND s.serial_number IS NULL;

                -- 5. Support grouping sensors by serial (the implicit "device" view).
                CREATE INDEX IF NOT EXISTS idx_sensors_serial_number
                    ON sensors (serial_number) WHERE serial_number IS NOT NULL;
                "#,
            )
            .await?;
        Ok(())
    }

    // The merge/backfill is a forward-only data cleanup (deleted duplicate rows cannot be restored).
    // down() only removes the lookup index.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_sensors_serial_number")
            .await?;
        Ok(())
    }
}
