use sea_orm::Statement;
use sea_orm_migration::prelude::*;

/// The remaining analytes a seeded tool output names in `suggested_parameter_code`.
///
/// `m20260818_000006_analyte_catalog` seeded the per-replicate families and left the scalar and
/// raw-entry outputs for a manager to define. That left those outputs resolving to nothing, so the
/// save panel had nowhere to write them and the calculated value could not reach a site parameter,
/// which is the point of running the tool. They are seeded here on the same terms: `needs_review`
/// stays true, so a manager can rename one or merge it into a legacy row
/// (`POST /actions/merge_parameters`) without the entry pretending to be curated.
///
/// Each insert is guarded on `LOWER(code)`, so a deployment where one of these codes was already
/// created by hand keeps its own row and this migration adds nothing for it. The guard is on the
/// code alone: a catalog may already hold the same analyte under a different code, which this
/// cannot detect and does not try to, since deciding that two rows are one analyte is the
/// manager's call and `POST /actions/merge_parameters` is where it is made.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// `(code, name, default_units)`. Units are the portal's `tool_*_info.html` entry for that column,
/// and an empty string is a genuinely dimensionless quantity: `default_units` is NOT NULL, so
/// "no unit" is the empty string rather than a placeholder token.
const ANALYTES: &[(&str, &str, &str)] = &[
    ("Alk_meqL", "Alkalinity (meq/L)", "meq/L"),
    ("Alk_mgL", "Alkalinity (mg/L)", "mg/L"),
    ("WTW_pH_1", "Field pH (WTW)", ""),
    ("lab_co2air_ch4_dry", "Air methane (dry)", "ppm"),
    ("SUVA", "Specific ultraviolet absorbance", "L/(mg*m)"),
    ("A_T", "Peak A/Peak T", ""),
    ("C_A", "Peak C/Peak A", ""),
    ("C_M", "Peak C/Peak M", ""),
    ("C_T", "Peak C/Peak T", ""),
    (
        "Field_BP_altitude",
        "Barometric pressure from elevation",
        "hPa",
    ),
    (
        "Vaisala_CO2_min_corr",
        "Vaisala CO2 minimum (corrected)",
        "ppm",
    ),
    (
        "Vaisala_CO2_avg_corr",
        "Vaisala CO2 average (corrected)",
        "ppm",
    ),
    (
        "Vaisala_CO2_max_corr",
        "Vaisala CO2 maximum (corrected)",
        "ppm",
    ),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
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
        // `needs_review` is the mark of an entry nobody has confirmed, so clearing it is how a
        // manager adopts the row. Rolling back must not take an adopted entry with it, nor one
        // anything else in the database has come to depend on: either way the row has outlived
        // this migration and a person owns it now.
        //
        // Whether anything depends on it is asked of the foreign keys rather than of a list of
        // tables. A dozen tables reference `parameters` with NO ACTION (readings, samples,
        // annotations, alarm_thresholds, alarm_events, sensor_deployments, status_events,
        // derived_parameter_definitions, derived_parameter_sources, notification_mutes,
        // site_parameters), and a hand-written NOT EXISTS over them is one migration behind the
        // next table that joins the list, which turns a rollback into an aborted transaction.
        // Deleting row by row inside a block that traps `foreign_key_violation` skips exactly the
        // rows something still points at: the block is a subtransaction, so the failed DELETE is
        // rolled back on its own and the rest of the rollback proceeds.
        //
        // The codes travel through a temporary table because a DO block takes no bind parameters:
        // `$1` inside its body would be read as a PL/pgSQL argument that does not exist.
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
        Ok(())
    }
}
