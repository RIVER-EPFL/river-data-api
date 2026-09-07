use sea_orm_migration::prelude::*;

/// One catalog parameter for dissolved organic carbon.
///
/// The baseline seeds `DOC` and the DOC calculation writes to it, while a CNET replicate family
/// resolves its own units-bearing code, `DOC_ppb`: the portal's column with the structural `avg`
/// segment removed, which is the identity a units-bearing column keeps. Two rows for one analyte
/// put a station's synced history and its tool entries on different slots, so the seeded row takes
/// the portal's code and the calculation follows it.
///
/// The manifest is rewritten in place rather than reseeded, and `content_hash` with it: the hash
/// is taken over the stored jsonb, so leaving it would identify content the row no longer holds.
/// `tests/tools/seeded_version_hashes.rs` recomputes it from the row.
///
/// A database that already holds both rows is left with both: merging them moves readings,
/// samples and site parameters, which is `POST /api/actions/merge_parameters`, an operator action
/// with a job behind it rather than something a migration should do unattended.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r#"
    UPDATE public.parameters SET code = 'DOC_ppb'
     WHERE code = 'DOC'
       AND NOT EXISTS (SELECT 1 FROM public.parameters WHERE LOWER(code) = LOWER('DOC_ppb'));

    UPDATE public.tool_script_versions v
       SET manifest = jsonb_set(
               v.manifest,
               '{params}',
               (SELECT jsonb_agg(
                           CASE WHEN p->>'parameter_code' = 'DOC'
                                THEN jsonb_set(p, '{parameter_code}', '"DOC_ppb"')
                                ELSE p END
                           ORDER BY ord)
                  FROM jsonb_array_elements(v.manifest->'params') WITH ORDINALITY AS t(p, ord))),
           content_hash = 'sha256:1d683aadb6114d3ea36c336e78d9291fa6929a66d3249398d5bf0f43eee7ba74'
      FROM public.tool_scripts s
     WHERE s.id = v.tool_script_id
       AND s.name = 'doc'
       AND s.created_by = 'seed'
       AND v.manifest @> '{"params": [{"parameter_code": "DOC"}]}'::jsonb;
"#;

const DOWN: &str = r#"
    UPDATE public.tool_script_versions v
       SET manifest = jsonb_set(
               v.manifest,
               '{params}',
               (SELECT jsonb_agg(
                           CASE WHEN p->>'parameter_code' = 'DOC_ppb'
                                THEN jsonb_set(p, '{parameter_code}', '"DOC"')
                                ELSE p END
                           ORDER BY ord)
                  FROM jsonb_array_elements(v.manifest->'params') WITH ORDINALITY AS t(p, ord))),
           content_hash = 'sha256:90284796affc9f931c0e91c5ebe1cd3a3188cca537e34bc873460d599e3b20fb'
      FROM public.tool_scripts s
     WHERE s.id = v.tool_script_id
       AND s.name = 'doc'
       AND s.created_by = 'seed'
       AND v.manifest @> '{"params": [{"parameter_code": "DOC_ppb"}]}'::jsonb;

    UPDATE public.parameters SET code = 'DOC' WHERE code = 'DOC_ppb';
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
