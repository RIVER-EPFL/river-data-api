use sea_orm_migration::prelude::*;

/// A parameter and a site parameter leave the same trail a parameter group does.
///
/// `change_audit` already holds `(subject, change, old_value, new_value, changed_by, changed_at)`
/// for schedules and parameter groups. The edits that move what is served are on these two tables:
/// a slot's units, decimal places, sd estimator, declared instrument and public flag, and a
/// parameter's code. The trigger is the group one generalised, so the subject prefix is its
/// argument rather than a second copy of the function.
///
/// An update that changes nothing writes nothing: a reprocess or a backfill that rewrites a row to
/// what it already held is not a change anyone made.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r#"
    CREATE OR REPLACE FUNCTION public.record_entity_change() RETURNS trigger AS $fn$
    DECLARE
        actor  text  := NULLIF(current_setting('river.actor', true), '');
        kind   text  := lower(TG_ARGV[0]) || '_' || lower(TG_OP);
        before jsonb := CASE WHEN TG_OP <> 'INSERT' THEN to_jsonb(OLD) END;
        after  jsonb := CASE WHEN TG_OP <> 'DELETE' THEN to_jsonb(NEW) END;
        row    jsonb := COALESCE(after, before);
    BEGIN
        IF TG_OP = 'UPDATE' AND before IS NOT DISTINCT FROM after THEN
            RETURN NEW;
        END IF;
        INSERT INTO public.change_audit (subject, change, old_value, new_value, changed_by)
        VALUES (TG_ARGV[0] || ':' || (row->>'id'), kind, before, after, actor);
        RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    END;
    $fn$ LANGUAGE plpgsql;

    DROP TRIGGER IF EXISTS parameters_change_audit ON public.parameters;
    CREATE TRIGGER parameters_change_audit
        AFTER INSERT OR UPDATE OR DELETE ON public.parameters
        FOR EACH ROW EXECUTE FUNCTION public.record_entity_change('parameter');

    DROP TRIGGER IF EXISTS site_parameters_change_audit ON public.site_parameters;
    CREATE TRIGGER site_parameters_change_audit
        AFTER INSERT OR UPDATE OR DELETE ON public.site_parameters
        FOR EACH ROW EXECUTE FUNCTION public.record_entity_change('site_parameter');
"#;

const DOWN: &str = r#"
    DROP TRIGGER IF EXISTS parameters_change_audit ON public.parameters;
    DROP TRIGGER IF EXISTS site_parameters_change_audit ON public.site_parameters;
    DROP FUNCTION IF EXISTS public.record_entity_change();
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
