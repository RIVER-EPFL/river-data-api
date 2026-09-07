use sea_orm_migration::prelude::*;

/// One (subject, old, new, who, when) table for the two audit trails that were that shape.
///
/// `schedule_audit` keyed its rows by `job_name` and `parameter_group_history` by
/// `(group_id, parameter_id)`; both then carried a before and an after snapshot, an actor and a
/// timestamp, and neither key means anything to the other. `subject` is the key both can express,
/// `schedule:{job_name}` and `parameter_group:{group_id}`, and `change` says what happened, so one
/// table answers "what was done to this thing, by whom" for either.
///
/// `tool_script_activations` is deliberately not folded in: its three foreign keys to
/// `tool_scripts` and `tool_script_versions` are what stop an activation naming a version that
/// does not exist, and a text subject cannot hold that.
///
/// The group history's `parameter_id` column is not carried: the trigger already writes the whole
/// row into `old`/`new`, so `new->>'parameter_id'` is the same fact and the column was only ever a
/// second copy of it.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r#"
    ALTER TABLE public.schedule_audit RENAME TO change_audit;

    ALTER TABLE public.change_audit ADD COLUMN IF NOT EXISTS subject text;
    ALTER TABLE public.change_audit ADD COLUMN IF NOT EXISTS change text;
    UPDATE public.change_audit
       SET subject = COALESCE(subject, 'schedule:' || job_name),
           change  = COALESCE(change, 'schedule_update');
    ALTER TABLE public.change_audit ALTER COLUMN subject SET NOT NULL;
    ALTER TABLE public.change_audit ALTER COLUMN change SET NOT NULL;
    -- Drops `idx_schedule_audit_job_changed` with it, which indexed the column.
    ALTER TABLE public.change_audit DROP COLUMN IF EXISTS job_name;

    CREATE INDEX IF NOT EXISTS idx_change_audit_subject_changed
        ON public.change_audit (subject, changed_at DESC);

    INSERT INTO public.change_audit (id, subject, change, old_value, new_value, changed_by, changed_at)
    SELECT id, 'parameter_group:' || group_id, change, old, new, changed_by, changed_at
      FROM public.parameter_group_history
     WHERE group_id IS NOT NULL;

    DROP TRIGGER IF EXISTS parameter_groups_history ON public.parameter_groups;
    DROP TRIGGER IF EXISTS parameter_group_members_history ON public.parameter_group_members;
    DROP TABLE IF EXISTS public.parameter_group_history;

    CREATE OR REPLACE FUNCTION public.record_parameter_group_change() RETURNS trigger AS $fn$
    DECLARE
        actor  text  := NULLIF(current_setting('river.actor', true), '');
        kind   text  := lower(TG_ARGV[0]) || '_' || lower(TG_OP);
        before jsonb := CASE WHEN TG_OP <> 'INSERT' THEN to_jsonb(OLD) END;
        after  jsonb := CASE WHEN TG_OP <> 'DELETE' THEN to_jsonb(NEW) END;
        row    jsonb := COALESCE(after, before);
    BEGIN
        INSERT INTO public.change_audit (subject, change, old_value, new_value, changed_by)
        VALUES ('parameter_group:' || CASE WHEN TG_ARGV[0] = 'group'
                                           THEN row->>'id' ELSE row->>'group_id' END,
                kind, before, after, actor);
        RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    END;
    $fn$ LANGUAGE plpgsql;

    CREATE TRIGGER parameter_groups_history
        AFTER INSERT OR UPDATE OR DELETE ON public.parameter_groups
        FOR EACH ROW EXECUTE FUNCTION public.record_parameter_group_change('group');

    CREATE TRIGGER parameter_group_members_history
        AFTER INSERT OR UPDATE OR DELETE ON public.parameter_group_members
        FOR EACH ROW EXECUTE FUNCTION public.record_parameter_group_change('member');
"#;

const DOWN: &str = r#"
    DROP TRIGGER IF EXISTS parameter_groups_history ON public.parameter_groups;
    DROP TRIGGER IF EXISTS parameter_group_members_history ON public.parameter_group_members;

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

    INSERT INTO public.parameter_group_history
           (id, group_id, parameter_id, change, old, new, changed_by, changed_at)
    SELECT id,
           replace(subject, 'parameter_group:', '')::uuid,
           COALESCE(new_value->>'parameter_id', old_value->>'parameter_id')::uuid,
           change, old_value, new_value, changed_by, changed_at
      FROM public.change_audit WHERE subject LIKE 'parameter_group:%';
    DELETE FROM public.change_audit WHERE subject LIKE 'parameter_group:%';

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

    CREATE TRIGGER parameter_groups_history
        AFTER INSERT OR UPDATE OR DELETE ON public.parameter_groups
        FOR EACH ROW EXECUTE FUNCTION public.record_parameter_group_change('group');
    CREATE TRIGGER parameter_group_members_history
        AFTER INSERT OR UPDATE OR DELETE ON public.parameter_group_members
        FOR EACH ROW EXECUTE FUNCTION public.record_parameter_group_change('member');

    ALTER TABLE public.change_audit ADD COLUMN IF NOT EXISTS job_name text;
    UPDATE public.change_audit SET job_name = replace(subject, 'schedule:', '');
    ALTER TABLE public.change_audit ALTER COLUMN job_name SET NOT NULL;
    -- Drops `idx_change_audit_subject_changed` with it.
    ALTER TABLE public.change_audit DROP COLUMN IF EXISTS subject;
    ALTER TABLE public.change_audit DROP COLUMN IF EXISTS change;
    ALTER TABLE public.change_audit RENAME TO schedule_audit;
    CREATE INDEX IF NOT EXISTS idx_schedule_audit_job_changed
        ON public.schedule_audit (job_name, changed_at DESC);
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
