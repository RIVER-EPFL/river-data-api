use sea_orm_migration::prelude::*;

/// Gives a reading a second curve reference, so it can record both the time-windowed base
/// calibration it was corrected with and the hand-picked standard curve applied on top. With one
/// column the two were indistinguishable: choosing a lab curve meant the base was neither applied
/// nor recorded.
///
/// This reverses part of `m20260711_000005_drop_standard_curves`, which folded standard curves into
/// `sensor_calibrations` on the grounds that one table meant one foreign key on readings. That
/// argument ends here: readings now carry both references, so the two kinds no longer have to share
/// a table, and the `mode` discriminator, its CHECK and the trigger scoping that existed only to
/// keep lab curves out of window queries all go with it.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Three statements, not one: `ADD COLUMN ... REFERENCES` is rejected on a hypertable with
        // columnstore enabled ("cannot add column with constraints"), while the bare column and a
        // separate ADD CONSTRAINT both succeed.
        db.execute_unprepared(
            "ALTER TABLE readings ADD COLUMN IF NOT EXISTS standard_curve_id UUID;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings ADD CONSTRAINT readings_standard_curve_id_fkey \
             FOREIGN KEY (standard_curve_id) REFERENCES standard_curves(id);",
        )
        .await?;
        // No ON DELETE clause, so Postgres refuses to drop a curve a reading references. Without an
        // index on the referencing side that check scans the hypertable on every curve delete.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_standard_curve_id \
             ON readings (standard_curve_id) WHERE standard_curve_id IS NOT NULL;",
        )
        .await?;

        // The readings UPDATE below reaches compressed chunks.
        db.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;",
        )
        .await?;

        // `main.rs` sets a 30s `statement_timeout` on the pool the migrator may draw from, and
        // sea-orm runs every pending migration in one transaction, so a statement that overruns
        // rolls back the whole batch rather than just this migration. The updates below scan
        // `readings`, which is unbounded, so they need longer than a request does.
        db.execute_unprepared("SET LOCAL statement_timeout = '30min';")
            .await?;

        // Ids are preserved, so repointing readings is a move between tables rather than a remap.
        // Order matters: copy, repoint, then delete, or the delete hits the readings foreign key.
        db.execute_unprepared(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept, r_squared, notes, created_at) \
             SELECT id, sensor_id, name, slope, intercept, r_squared, notes, COALESCE(created_at, now()) \
             FROM sensor_calibrations WHERE mode = 'instant';",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE readings SET standard_curve_id = calibration_id, calibration_id = NULL \
             WHERE calibration_id IN (SELECT id FROM standard_curves) \
               AND measurement_type = 'spot';",
        )
        .await?;
        // A standard curve is a grab's correction, so only a grab may come out of the move carrying
        // one. A logger row that named an instant curve loses the reference rather than gaining a
        // second one the write paths refuse to create and reprocess would half-rewrite. Its stored
        // value is left alone; window resolution owns that row and re-derives both the reference and
        // the value from the timeline it covers.
        db.execute_unprepared(
            "UPDATE readings SET calibration_id = NULL \
             WHERE calibration_id IN (SELECT id FROM standard_curves) \
               AND measurement_type IS DISTINCT FROM 'spot';",
        )
        .await?;
        db.execute_unprepared("DELETE FROM sensor_calibrations WHERE mode = 'instant';")
            .await?;

        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations DROP CONSTRAINT IF EXISTS sensor_calibrations_mode_check;",
        )
        .await?;

        // Restate the parameter-inherit trigger without the mode predicate that
        // m20260711_000006_inherit_windowed_only added. Every remaining row is windowed by
        // construction, so the behaviour is unchanged.
        db.execute_unprepared(
            r"CREATE OR REPLACE FUNCTION inherit_calibration_parameter_id() RETURNS trigger AS $fn$
              BEGIN
                  IF NEW.parameter_id IS NULL THEN
                      SELECT parameter_id INTO NEW.parameter_id
                      FROM sensor_calibrations
                      WHERE sensor_id = NEW.sensor_id AND parameter_id IS NOT NULL
                      ORDER BY valid_from
                      LIMIT 1;
                  END IF;
                  RETURN NEW;
              END;
              $fn$ LANGUAGE plpgsql;",
        )
        .await?;

        db.execute_unprepared("ALTER TABLE sensor_calibrations DROP COLUMN IF EXISTS mode;")
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations ADD COLUMN IF NOT EXISTS mode TEXT NOT NULL DEFAULT 'windowed';",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations ADD CONSTRAINT sensor_calibrations_mode_check \
             CHECK (mode = ANY (ARRAY['windowed'::text, 'instant'::text]));",
        )
        .await?;
        db.execute_unprepared(
            r"CREATE OR REPLACE FUNCTION inherit_calibration_parameter_id() RETURNS trigger AS $fn$
              BEGIN
                  IF NEW.parameter_id IS NULL AND NEW.mode = 'windowed' THEN
                      SELECT parameter_id INTO NEW.parameter_id
                      FROM sensor_calibrations
                      WHERE sensor_id = NEW.sensor_id AND parameter_id IS NOT NULL
                      ORDER BY valid_from
                      LIMIT 1;
                  END IF;
                  RETURN NEW;
              END;
              $fn$ LANGUAGE plpgsql;",
        )
        .await?;

        db.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, notes, name, mode, r_squared, created_at) \
             SELECT id, sensor_id, slope, intercept, created_at, notes, name, 'instant', r_squared, created_at \
             FROM standard_curves;",
        )
        .await?;
        // Grabs go back to naming their curve through `calibration_id`. A logger row whose instant
        // reference `up` dropped is not restored: which curve it named is no longer recorded
        // anywhere, and window resolution re-derives it.
        db.execute_unprepared(
            "UPDATE readings SET calibration_id = standard_curve_id, standard_curve_id = NULL \
             WHERE standard_curve_id IS NOT NULL;",
        )
        .await?;

        // The curves live in `sensor_calibrations` again and nothing references them here, so the
        // copies go. Leaving them would make the ids exist in both tables, and re-applying `up`
        // would then conflict on the primary key.
        db.execute_unprepared("DELETE FROM standard_curves;")
            .await?;

        db.execute_unprepared("DROP INDEX IF EXISTS idx_readings_standard_curve_id;")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE readings DROP CONSTRAINT IF EXISTS readings_standard_curve_id_fkey;",
        )
        .await?;
        db.execute_unprepared("ALTER TABLE readings DROP COLUMN IF EXISTS standard_curve_id;")
            .await?;
        Ok(())
    }
}
