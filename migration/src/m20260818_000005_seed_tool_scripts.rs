use sea_orm::Statement;
use sea_orm_migration::prelude::*;

/// Seeds the 12 portal-lineage tools as version 1 of each script: the vendored CNET/METALP
/// calculation functions this tool reaches (verbatim, see [`crate::tool_prelude`]) plus a thin
/// wrapper per tool, with the manifest and the golden-derived test cases authored under
/// `migration/tool_seed/`. Versions land
/// pre-validated (every case passed against the runner before shipping) and active.
/// Idempotent: a script whose name already exists is left alone entirely.
#[derive(DeriveMigrationName)]
pub struct Migration;

const PRELUDE: &str = include_str!("../tool_seed/prelude.R");

struct Seed {
    name: &'static str,
    wrapper: &'static str,
    manifest: &'static str,
    cases: &'static str,
}

macro_rules! seed {
    ($name:literal) => {
        Seed {
            name: $name,
            wrapper: include_str!(concat!("../tool_seed/", $name, "/wrapper.R")),
            manifest: include_str!(concat!("../tool_seed/", $name, "/manifest.json")),
            cases: include_str!(concat!("../tool_seed/", $name, "/cases.json")),
        }
    };
}

const SEEDS: &[Seed] = &[
    seed!("alkalinity"),
    seed!("benthic"),
    seed!("chlorophyll"),
    seed!("co2_air"),
    seed!("dic"),
    seed!("discharge"),
    seed!("doc"),
    seed!("dom"),
    seed!("field_data"),
    seed!("nutrients"),
    seed!("pco2"),
    seed!("tss_afdm"),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for seed in SEEDS {
            let script = crate::tool_prelude::script_for(PRELUDE, seed.wrapper);
            // The same bundle hash the authoring path computes, so one rule identifies a version
            // however it was created and a manifest edit over one script is a distinct identity.
            // Taken over the jsonb form and stored as that form, so the hash on the seeded row is
            // recomputable from the row rather than from the files under `tool_seed/`.
            let manifest: serde_json::Value = serde_json::from_str(seed.manifest).map_err(|e| {
                DbErr::Custom(format!(
                    "tool seed {}: manifest is not JSON: {e}",
                    seed.name
                ))
            })?;
            let cases: serde_json::Value = serde_json::from_str(seed.cases).map_err(|e| {
                DbErr::Custom(format!("tool seed {}: cases are not JSON: {e}", seed.name))
            })?;
            let stored =
                crate::tool_hash::stored_version_content(db, &script, "tool", &manifest, &cases)
                    .await?;

            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO tool_scripts (name, label, description, created_by)
                  SELECT $1, COALESCE($2::jsonb->>'label', $1), $2::jsonb->>'description', 'seed'
                  WHERE NOT EXISTS
                      (SELECT 1 FROM tool_scripts WHERE LOWER(name) = LOWER($1))",
                [seed.name.into(), seed.manifest.into()],
            ))
            .await?;

            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO tool_script_versions
                      (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                       content_hash, created_by, validated_at)
                  SELECT s.id, 1, $2, 'tool', $3::jsonb, $4::jsonb, $5, 'seed', now()
                  FROM tool_scripts s
                  WHERE LOWER(s.name) = LOWER($1)
                    AND NOT EXISTS (SELECT 1 FROM tool_script_versions v
                                    WHERE v.tool_script_id = s.id)",
                [
                    seed.name.into(),
                    script.into(),
                    stored.manifest.into(),
                    stored.test_cases.into(),
                    stored.content_hash.into(),
                ],
            ))
            .await?;

            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE tool_scripts s SET active_version_id = v.id, updated_at = now()
                  FROM tool_script_versions v
                  WHERE LOWER(s.name) = LOWER($1) AND v.tool_script_id = s.id
                    AND v.version_no = 1 AND s.active_version_id IS NULL",
                [seed.name.into()],
            ))
            .await?;

            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO tool_script_activations (tool_script_id, to_version_id, activated_by)
                  SELECT s.id, s.active_version_id, 'seed'
                  FROM tool_scripts s
                  WHERE LOWER(s.name) = LOWER($1) AND s.active_version_id IS NOT NULL
                    AND NOT EXISTS (SELECT 1 FROM tool_script_activations a
                                    WHERE a.tool_script_id = s.id)",
                [seed.name.into()],
            ))
            .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let names: Vec<String> = SEEDS.iter().map(|s| s.name.to_string()).collect();
        let db = manager.get_connection();
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE tool_scripts SET active_version_id = NULL WHERE name = ANY($1)",
            [names.clone().into()],
        ))
        .await?;
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM tool_scripts WHERE name = ANY($1)",
            [names.into()],
        ))
        .await?;
        Ok(())
    }
}
