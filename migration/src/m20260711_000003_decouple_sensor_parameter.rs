use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    // Decouple parameter from the sensor. A sensor (field sonde or lab instrument) is a physical
    // device that may measure many parameters; the parameter belongs on the deployment (field) or
    // the reading (lab grab), never on the sensor. This:
    //   1. Collapses duplicate-serial sensor rows (one row per (serial, parameter) today) into one
    //      canonical row per serial, re-pointing every child FK (deployments, calibrations, streams,
    //      readings) to the survivor. Decompression-safe: the readings UPDATE lifts TimescaleDB's
    //      per-statement decompression cap so deep-historical (compressed) chunks re-point too.
    //   2. Frees deployment.parameter_id from the sensor: drops the set_deployment_parameter_id
    //      trigger so the column is authored by the write paths (adopt/swap/pairing/CRUD).
    //   3. Drops sensors.parameter_id, its FK, and the (serial, parameter) unique index; replaces the
    //      dedup key with a partial UNIQUE(serial_number) so a serial identifies one instrument.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        conn.execute_unprepared(
            r#"
            -- Lift the decompression cap so the readings re-point below rewrites rows inside
            -- compressed (>30-day) chunks instead of aborting. No-op on uncompressed data.
            SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

            -- 1. Canonical survivor per serial = the earliest-created row. Map every other
            --    same-serial row to it. (Serial-less rows have no dedup key; they are left alone.)
            CREATE TEMP TABLE _sensor_canon ON COMMIT DROP AS
            SELECT s.id AS dup_id, c.canon_id
            FROM sensors s
            JOIN (
                SELECT serial_number,
                       (array_agg(id ORDER BY created_at NULLS LAST, id))[1] AS canon_id
                FROM sensors
                WHERE serial_number IS NOT NULL
                GROUP BY serial_number
            ) c ON c.serial_number = s.serial_number
            WHERE s.serial_number IS NOT NULL AND s.id <> c.canon_id;

            -- 2. Re-point child references from every duplicate to its canonical survivor.
            UPDATE sensor_deployments d SET sensor_id = m.canon_id
                FROM _sensor_canon m WHERE d.sensor_id = m.dup_id;
            UPDATE sensor_calibrations sc SET sensor_id = m.canon_id
                FROM _sensor_canon m WHERE sc.sensor_id = m.dup_id;
            UPDATE data_streams ds SET sensor_id = m.canon_id
                FROM _sensor_canon m WHERE ds.sensor_id = m.dup_id;
            UPDATE readings r SET sensor_id = m.canon_id
                FROM _sensor_canon m WHERE r.sensor_id = m.dup_id;

            -- 3. Delete the now-orphaned duplicate sensor rows.
            DELETE FROM sensors s USING _sensor_canon m WHERE s.id = m.dup_id;

            -- 4. Deployment parameter becomes authored: drop the derive-from-sensor trigger + function.
            DROP TRIGGER IF EXISTS trg_set_deployment_parameter_id ON sensor_deployments;
            DROP FUNCTION IF EXISTS set_deployment_parameter_id();

            -- 5. Drop the parameter coupling on sensors: the (serial, parameter) unique index, the
            --    serial grouping index, the column (+ its FK), and add the serial-only dedup key.
            DROP INDEX IF EXISTS idx_sensors_serial_parameter;
            DROP INDEX IF EXISTS idx_sensors_serial_number;
            ALTER TABLE sensors DROP COLUMN IF EXISTS parameter_id;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sensors_serial_unique
                ON sensors (serial_number) WHERE serial_number IS NOT NULL;
            "#,
        )
        .await?;
        Ok(())
    }

    // Forward-only: collapsed duplicate rows and their re-pointed children cannot be un-merged, and
    // the sensor's parameter is gone. down() restores the column shape (nullable, no backfill) and
    // the derive trigger so the schema is structurally reversible for local rollback.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                DROP INDEX IF EXISTS idx_sensors_serial_unique;
                ALTER TABLE sensors ADD COLUMN IF NOT EXISTS parameter_id UUID REFERENCES parameters(id);
                CREATE INDEX IF NOT EXISTS idx_sensors_serial_number
                    ON sensors (serial_number) WHERE serial_number IS NOT NULL;

                CREATE OR REPLACE FUNCTION set_deployment_parameter_id() RETURNS trigger AS $fn$
                BEGIN
                    IF NEW.parameter_id IS NULL THEN
                        SELECT parameter_id INTO NEW.parameter_id FROM sensors WHERE id = NEW.sensor_id;
                    END IF;
                    RETURN NEW;
                END;
                $fn$ LANGUAGE plpgsql;
                DROP TRIGGER IF EXISTS trg_set_deployment_parameter_id ON sensor_deployments;
                CREATE TRIGGER trg_set_deployment_parameter_id
                    BEFORE INSERT OR UPDATE OF sensor_id ON sensor_deployments
                    FOR EACH ROW EXECUTE FUNCTION set_deployment_parameter_id();
                "#,
            )
            .await?;
        Ok(())
    }
}
