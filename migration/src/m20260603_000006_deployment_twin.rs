use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    // Make deployment the structural twin of calibration: a sensor's deployments become an editable,
    // overlap-free timeline over (site, parameter). This denormalizes the sensor's (immutable)
    // parameter onto the deployment so one-sensor-per-(site,parameter) can be hard-enforced with a
    // btree_gist exclusion constraint, and adds the indexes the window-resolution / reprocess paths need.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                -- Overlap exclusion needs btree_gist (equality on uuid columns inside a GiST index).
                CREATE EXTENSION IF NOT EXISTS btree_gist;

                -- Denormalize the sensor's parameter onto the deployment. A sensor measures exactly one
                -- (immutable) parameter, so this is a safe functional dependency, maintained by trigger.
                ALTER TABLE sensor_deployments ADD COLUMN IF NOT EXISTS parameter_id UUID REFERENCES parameters(id);

                UPDATE sensor_deployments d
                SET parameter_id = s.parameter_id
                FROM sensors s
                WHERE d.sensor_id = s.id AND d.parameter_id IS NULL;

                ALTER TABLE sensor_deployments ALTER COLUMN parameter_id SET NOT NULL;

                -- Keep parameter_id correct on every insert path (CRUD, pairing auto-deploy, adopt) and
                -- whenever sensor_id changes. Pure function of the immutable sensors.parameter_id — no
                -- race: the exclusion constraint below is the atomic enforcement, this only fills a column.
                CREATE OR REPLACE FUNCTION set_deployment_parameter_id() RETURNS trigger AS $fn$
                BEGIN
                    SELECT parameter_id INTO NEW.parameter_id FROM sensors WHERE id = NEW.sensor_id;
                    RETURN NEW;
                END;
                $fn$ LANGUAGE plpgsql;

                DROP TRIGGER IF EXISTS trg_set_deployment_parameter_id ON sensor_deployments;
                CREATE TRIGGER trg_set_deployment_parameter_id
                    BEFORE INSERT OR UPDATE OF sensor_id ON sensor_deployments
                    FOR EACH ROW EXECUTE FUNCTION set_deployment_parameter_id();

                -- Resolve any pre-existing overlaps before adding the constraint: chain each deployment's
                -- deployed_until down to the next deployment's deployed_from within the same
                -- (site, parameter). Half-open [) semantics mean end-A-at-T / start-B-at-T don't overlap.
                -- Only shortens genuinely-overlapping windows (LEAST keeps the existing bound otherwise).
                WITH ordered AS (
                    SELECT id,
                           LEAST(
                               COALESCE(deployed_until, 'infinity'::timestamptz),
                               COALESCE(
                                   LEAD(deployed_from) OVER (
                                       PARTITION BY site_id, parameter_id
                                       ORDER BY deployed_from, id
                                   ),
                                   'infinity'::timestamptz
                               )
                           ) AS new_until
                    FROM sensor_deployments
                )
                UPDATE sensor_deployments d
                SET deployed_until = NULLIF(ordered.new_until, 'infinity'::timestamptz)
                FROM ordered
                WHERE d.id = ordered.id
                  AND COALESCE(d.deployed_until, 'infinity'::timestamptz) <> ordered.new_until;

                -- Hard-enforce one sensor per (site, parameter) at a time.
                DO $do$ BEGIN
                    IF NOT EXISTS (
                        SELECT 1 FROM pg_constraint WHERE conname = 'excl_deployment_site_param_slot'
                    ) THEN
                        ALTER TABLE sensor_deployments
                            ADD CONSTRAINT excl_deployment_site_param_slot
                            EXCLUDE USING gist (
                                site_id WITH =,
                                parameter_id WITH =,
                                tstzrange(deployed_from, COALESCE(deployed_until, 'infinity'::timestamptz), '[)') WITH &&
                            );
                    END IF;
                END $do$;

                -- Deployment-timeline lookups (latest-by-sensor; slot-by-(site,parameter,time)).
                CREATE INDEX IF NOT EXISTS idx_sensor_deployments_sensor_time
                    ON sensor_deployments (sensor_id, deployed_from DESC);
                CREATE INDEX IF NOT EXISTS idx_sensor_deployments_site_param_time
                    ON sensor_deployments (site_id, parameter_id, deployed_from);

                -- Per-sensor reading reprocess (window UPDATEs filter on sensor_id + time).
                CREATE INDEX IF NOT EXISTS idx_readings_sensor_time
                    ON readings (sensor_id, time DESC) WHERE sensor_id IS NOT NULL;
                "#,
            )
            .await?;
        Ok(())
    }

    // Schema is reversible; the one-time overlap resolution (deployed_until shortening) is forward-only.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                DROP INDEX IF EXISTS idx_readings_sensor_time;
                DROP INDEX IF EXISTS idx_sensor_deployments_site_param_time;
                DROP INDEX IF EXISTS idx_sensor_deployments_sensor_time;
                ALTER TABLE sensor_deployments DROP CONSTRAINT IF EXISTS excl_deployment_site_param_slot;
                DROP TRIGGER IF EXISTS trg_set_deployment_parameter_id ON sensor_deployments;
                DROP FUNCTION IF EXISTS set_deployment_parameter_id();
                ALTER TABLE sensor_deployments DROP COLUMN IF EXISTS parameter_id;
                "#,
            )
            .await?;
        Ok(())
    }
}
