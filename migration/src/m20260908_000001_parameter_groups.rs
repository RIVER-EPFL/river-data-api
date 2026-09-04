use sea_orm_migration::prelude::*;

/// The parameter group: a category's members, their order, their roles and their per-group
/// presentation overrides.
///
/// A parameter belongs to at most one group (`parameter_group_members.parameter_id` UNIQUE), which
/// is the portal's own rule for `grab_param_categories`. A group with members is not deleted; the
/// FK is `ON DELETE RESTRICT`, so a split is a new group plus moves.
///
/// Every change to a group or a membership appends a `parameter_group_history` row from a trigger
/// rather than from the writer, so a seed migration, a CRUD write and a hand edit all leave the
/// same trail. `changed_by` reads the `river.actor` session setting when a writer has set one.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r#"
    CREATE TABLE IF NOT EXISTS public.parameter_groups (
        id          uuid PRIMARY KEY,
        code        text NOT NULL UNIQUE,
        label       text NOT NULL,
        description text,
        ordinal     integer NOT NULL DEFAULT 0,
        created_at  timestamptz NOT NULL DEFAULT now()
    );

    CREATE TABLE IF NOT EXISTS public.parameter_group_members (
        id             uuid PRIMARY KEY,
        group_id       uuid NOT NULL REFERENCES public.parameter_groups(id) ON DELETE RESTRICT,
        parameter_id   uuid NOT NULL UNIQUE REFERENCES public.parameters(id) ON DELETE CASCADE,
        ordinal        integer NOT NULL DEFAULT 0,
        role           text NOT NULL,
        replicates     jsonb,
        label          text,
        units          text,
        decimal_places integer,
        description    text,
        created_at     timestamptz NOT NULL DEFAULT now(),
        CONSTRAINT parameter_group_members_role_check
            CHECK (role IN ('measured', 'entry_only', 'output'))
    );

    CREATE INDEX IF NOT EXISTS idx_parameter_group_members_group
        ON public.parameter_group_members (group_id, ordinal);

    CREATE TABLE IF NOT EXISTS public.parameter_group_history (
        id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
        group_id     uuid,
        parameter_id uuid,
        change       text NOT NULL,
        old          jsonb,
        new          jsonb,
        changed_by   text,
        changed_at   timestamptz NOT NULL DEFAULT now()
    );

    CREATE INDEX IF NOT EXISTS idx_parameter_group_history_group
        ON public.parameter_group_history (group_id, changed_at DESC);

    CREATE OR REPLACE FUNCTION public.record_parameter_group_change() RETURNS trigger AS $fn$
    DECLARE
        actor  text  := NULLIF(current_setting('river.actor', true), '');
        kind   text  := lower(TG_ARGV[0]) || '_' || lower(TG_OP);
        before jsonb := CASE WHEN TG_OP <> 'INSERT' THEN to_jsonb(OLD) END;
        after  jsonb := CASE WHEN TG_OP <> 'DELETE' THEN to_jsonb(NEW) END;
        row    jsonb := COALESCE(after, before);
    BEGIN
        INSERT INTO public.parameter_group_history
               (group_id, parameter_id, change, old, new, changed_by)
        VALUES (CASE WHEN TG_ARGV[0] = 'group'
                     THEN (row->>'id')::uuid ELSE (row->>'group_id')::uuid END,
                CASE WHEN TG_ARGV[0] = 'group'
                     THEN NULL ELSE (row->>'parameter_id')::uuid END,
                kind, before, after, actor);
        RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    END;
    $fn$ LANGUAGE plpgsql;

    DROP TRIGGER IF EXISTS parameter_groups_history ON public.parameter_groups;
    CREATE TRIGGER parameter_groups_history
        AFTER INSERT OR UPDATE OR DELETE ON public.parameter_groups
        FOR EACH ROW EXECUTE FUNCTION public.record_parameter_group_change('group');

    DROP TRIGGER IF EXISTS parameter_group_members_history ON public.parameter_group_members;
    CREATE TRIGGER parameter_group_members_history
        AFTER INSERT OR UPDATE OR DELETE ON public.parameter_group_members
        FOR EACH ROW EXECUTE FUNCTION public.record_parameter_group_change('member');
"#;

const DOWN: &str = "
    DROP TRIGGER IF EXISTS parameter_group_members_history ON public.parameter_group_members;
    DROP TRIGGER IF EXISTS parameter_groups_history ON public.parameter_groups;
    DROP FUNCTION IF EXISTS public.record_parameter_group_change();
    DROP TABLE IF EXISTS public.parameter_group_history;
    DROP TABLE IF EXISTS public.parameter_group_members;
    DROP TABLE IF EXISTS public.parameter_groups;
";

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
