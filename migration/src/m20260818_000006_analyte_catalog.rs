use sea_orm::Statement;
use sea_orm_migration::prelude::*;

/// `parameters.needs_review` marks a catalog entry created mechanically rather than by a person,
/// so seeded rows are visibly uncleaned until a manager confirms or merges them
/// (`POST /actions/merge_parameters`). Seeds the per-replicate analyte codes the tool save flow
/// maps outputs onto; codes are the portal column bases, matching the manifests'
/// `suggested_parameter_code`. Scalar and echo-only outputs are deliberately not seeded: those
/// parameters are defined by a manager when wanted.
#[derive(DeriveMigrationName)]
pub struct Migration;

// Analytes enter alongside their tool as each is reworked onto the replicates model.
const ANALYTES: &[(&str, &str, &str)] = &[("DOC", "Dissolved organic carbon", "ppb")];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE parameters ADD COLUMN needs_review BOOLEAN NOT NULL DEFAULT false",
        )
        .await?;

        for (code, name, units) in ANALYTES {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO parameters (id, code, name, default_units, category, needs_review)
                  SELECT gen_random_uuid(), $1, $2, $3, 'measurement', true
                  WHERE NOT EXISTS
                      (SELECT 1 FROM parameters WHERE LOWER(code) = LOWER($1))",
                [(*code).into(), (*name).into(), (*units).into()],
            ))
            .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let codes: Vec<String> = ANALYTES.iter().map(|(c, _, _)| String::from(*c)).collect();
        // A row anything still references, or one a manager adopted by clearing `needs_review`,
        // has outlived this migration. Deleting row by row inside a block that traps
        // `foreign_key_violation` skips exactly the rows something still points at; the block is
        // a subtransaction, so the failed DELETE rolls back alone and the rest proceeds. The
        // codes travel through a temp table because a DO block takes no bind parameters.
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "CREATE TEMP TABLE rollback_analyte_codes (code text)",
        ))
        .await?;
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO rollback_analyte_codes (code) SELECT unnest($1::text[])",
            [codes.into()],
        ))
        .await?;
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"DO $$
              DECLARE target uuid;
              BEGIN
                FOR target IN
                  SELECT p.id FROM parameters p
                  JOIN rollback_analyte_codes c ON c.code = p.code
                  WHERE p.needs_review
                LOOP
                  BEGIN
                    DELETE FROM parameters WHERE id = target;
                  EXCEPTION WHEN foreign_key_violation THEN
                    RAISE NOTICE 'parameter % is still referenced; leaving it in place', target;
                  END;
                END LOOP;
              END $$;",
        ))
        .await?;
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "DROP TABLE rollback_analyte_codes",
        ))
        .await?;
        db.execute_unprepared("ALTER TABLE parameters DROP COLUMN IF EXISTS needs_review")
            .await?;
        Ok(())
    }
}
