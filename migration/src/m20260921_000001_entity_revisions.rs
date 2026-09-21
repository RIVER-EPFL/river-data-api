use sea_orm_migration::prelude::*;

/// Every dependency a calculation reads has a revision (Q215).
///
/// An entity row's revision is the newest `change_audit` row for its subject, so the change-audit
/// trigger extends to the six tables a formula can read from or is defined by, and one
/// `<subject>_insert` row is backfilled per existing row so no row is without one. A reading's
/// revision is the newest `reading_decisions` row at its key. Both tables gain `seq`, assigned by
/// a trigger that first takes an advisory lock on the subject or the key, so two writers that
/// share `now()` are ordered by their effect rather than by allocation.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// The tables a formula reads from or is defined by, with the subject prefix each audit row takes.
pub const AUDITED: &[(&str, &str)] = &[
    ("sites", "site"),
    ("constants", "constant"),
    ("standard_curves", "standard_curve"),
    ("sensor_calibrations", "sensor_calibration"),
    ("derived_parameter_sources", "derived_parameter_source"),
    ("calculation_formulas", "calculation_formula"),
];

const SEQUENCES: &str = "
CREATE SEQUENCE IF NOT EXISTS public.change_audit_seq;
CREATE SEQUENCE IF NOT EXISTS public.reading_decisions_seq;
ALTER TABLE public.change_audit ADD COLUMN IF NOT EXISTS seq bigint;
ALTER TABLE public.reading_decisions ADD COLUMN IF NOT EXISTS seq bigint;

CREATE OR REPLACE FUNCTION public.assign_change_audit_seq() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
    BEGIN
        PERFORM pg_advisory_xact_lock(hashtextextended('change_audit:' || NEW.subject, 0));
        NEW.seq := nextval('public.change_audit_seq');
        RETURN NEW;
    END;
    $$;
DROP TRIGGER IF EXISTS change_audit_seq ON public.change_audit;
CREATE TRIGGER change_audit_seq BEFORE INSERT ON public.change_audit
    FOR EACH ROW EXECUTE FUNCTION public.assign_change_audit_seq();

CREATE OR REPLACE FUNCTION public.assign_reading_decision_seq() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
    BEGIN
        PERFORM pg_advisory_xact_lock(
            hashtextextended('reading:' || NEW.stream_id::text || '@' || NEW.time::text, 0));
        NEW.seq := nextval('public.reading_decisions_seq');
        RETURN NEW;
    END;
    $$;
DROP TRIGGER IF EXISTS reading_decisions_seq ON public.reading_decisions;
CREATE TRIGGER reading_decisions_seq BEFORE INSERT ON public.reading_decisions
    FOR EACH ROW EXECUTE FUNCTION public.assign_reading_decision_seq();

UPDATE public.change_audit c SET seq = o.n
FROM (SELECT id, row_number() OVER (ORDER BY changed_at, id) AS n
      FROM public.change_audit WHERE seq IS NULL) o
WHERE c.id = o.id;
SELECT setval('public.change_audit_seq', COALESCE((SELECT max(seq) FROM public.change_audit), 0) + 1, false);
UPDATE public.reading_decisions d SET seq = o.n
FROM (SELECT id, row_number() OVER (ORDER BY at, id) AS n
      FROM public.reading_decisions WHERE seq IS NULL) o
WHERE d.id = o.id;
SELECT setval('public.reading_decisions_seq', COALESCE((SELECT max(seq) FROM public.reading_decisions), 0) + 1, false);

ALTER TABLE public.change_audit ALTER COLUMN seq SET NOT NULL;
ALTER TABLE public.reading_decisions ALTER COLUMN seq SET NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS change_audit_seq_key ON public.change_audit (seq);
CREATE UNIQUE INDEX IF NOT EXISTS reading_decisions_seq_key ON public.reading_decisions (seq);
CREATE INDEX IF NOT EXISTS change_audit_subject_seq ON public.change_audit (subject, seq DESC);
CREATE INDEX IF NOT EXISTS reading_decisions_key_seq
    ON public.reading_decisions (stream_id, time, seq DESC);
";

/// The audit trigger on one table.
fn trigger(table: &str, subject: &str) -> String {
    format!(
        "DROP TRIGGER IF EXISTS {table}_change_audit ON public.{table};
         CREATE TRIGGER {table}_change_audit AFTER INSERT OR DELETE OR UPDATE ON public.{table}
             FOR EACH ROW EXECUTE FUNCTION public.record_entity_change('{subject}')"
    )
}

/// One `<subject>_insert` row for every existing row of `table` that has no audit row yet, so a
/// row that predates the trigger has a revision like one written after it.
pub fn backfill(table: &str, subject: &str) -> String {
    format!(
        "INSERT INTO public.change_audit (subject, change, old_value, new_value, changed_by)
         SELECT '{subject}:' || t.id, '{subject}_insert', NULL, to_jsonb(t), NULL
         FROM public.{table} t
         WHERE NOT EXISTS (SELECT 1 FROM public.change_audit c WHERE c.subject = '{subject}:' || t.id)
         ORDER BY t.id"
    )
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(SEQUENCES).await?;
        for (table, subject) in AUDITED {
            db.execute_unprepared(&trigger(table, subject)).await?;
            db.execute_unprepared(&backfill(table, subject)).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for (table, _) in AUDITED {
            db.execute_unprepared(&format!(
                "DROP TRIGGER IF EXISTS {table}_change_audit ON public.{table}"
            ))
            .await?;
        }
        db.execute_unprepared(
            "DROP TRIGGER IF EXISTS change_audit_seq ON public.change_audit;
             DROP TRIGGER IF EXISTS reading_decisions_seq ON public.reading_decisions;
             DROP FUNCTION IF EXISTS public.assign_change_audit_seq();
             DROP FUNCTION IF EXISTS public.assign_reading_decision_seq();
             ALTER TABLE public.change_audit DROP COLUMN IF EXISTS seq;
             ALTER TABLE public.reading_decisions DROP COLUMN IF EXISTS seq;
             DROP SEQUENCE IF EXISTS public.change_audit_seq;
             DROP SEQUENCE IF EXISTS public.reading_decisions_seq",
        )
        .await?;
        Ok(())
    }
}
