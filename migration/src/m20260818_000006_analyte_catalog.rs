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

const ANALYTES: &[(&str, &str, &str)] = &[
    ("DOC", "Dissolved organic carbon", "ppb"),
    ("NUT_P", "Phosphorus", "ug/L"),
    ("NUT_NH4", "Ammonium", "ug/L"),
    ("NUT_NOx", "Nitrate + nitrite", "ug/L"),
    ("NUT_NO2", "Nitrite", "ug/L"),
    ("NUT_NO3", "Nitrate", "ug/L"),
    ("NUT_TDP", "Total dissolved phosphorus", "ug/L"),
    ("NUT_TDN", "Total dissolved nitrogen", "ug/L"),
    ("NH4", "Ammonium (legacy channel)", "ug/L"),
    (
        "SRP",
        "Soluble reactive phosphorus (legacy channel)",
        "ug/L",
    ),
    ("DIC", "Dissolved inorganic carbon", "uM"),
    ("d13C_DIC", "d13C of DIC", "permil"),
    ("CH4_umol_L", "Dissolved methane", "umol/L"),
    ("CO2_HS_Um", "Headspace CO2", "uM"),
    ("pCO2_HS_uatm", "pCO2 (headspace)", "uatm"),
    ("pCO2_HS_P1_uatm", "pCO2 (headspace, P1)", "uatm"),
    ("pCO2_HS_P2_uatm", "pCO2 (headspace, P2)", "uatm"),
    ("d13C_CO2", "d13C of CO2", "permil"),
    ("chla_acid_ugL", "Chlorophyll-a (acidified)", "ug/L"),
    ("chla_noacid_ugL", "Chlorophyll-a (non-acidified)", "ug/L"),
    (
        "chla_acid_ugm2",
        "Chlorophyll-a per area (acidified)",
        "ug/m2",
    ),
    (
        "chla_noacid_ugm2",
        "Chlorophyll-a per area (non-acidified)",
        "ug/m2",
    ),
    ("afdm_gm2", "Benthic AFDM per area", "g/m2"),
    ("TSS", "Total suspended solids", "mg/L"),
    ("AFDM", "Ash-free dry mass", "mg/L"),
    ("Reach_depth", "Reach depth", "cm"),
    ("Q_Ls", "Discharge", "L/s"),
];

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
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM parameters WHERE code = ANY($1) AND needs_review",
            [codes.into()],
        ))
        .await?;
        db.execute_unprepared("ALTER TABLE parameters DROP COLUMN IF EXISTS needs_review")
            .await?;
        Ok(())
    }
}
