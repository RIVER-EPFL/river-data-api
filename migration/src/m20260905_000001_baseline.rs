use sea_orm_migration::prelude::*;

/// The whole schema in one migration.
///
/// Everything up to and including `m20260904_000002_instrument_per_slot` is folded in here, so a
/// new database is built once in its final shape rather than assembled and rebuilt: the telegram
/// tables are never created, the continuous aggregates are defined once instead of three times,
/// and `standard_curves` is created in the shape it ends up with.
///
/// The schema and seed sections below were generated from a fully migrated database rather than
/// written by hand, which is what makes the seed rows exact: the tool script text, its normalised
/// manifest and test-case jsonb and its content hash are stored as they were computed.
///
/// This cannot be applied to a database that already holds rows, and there is no schema-only route
/// for one: 26 of the migrations it replaces rewrite existing rows, so skipping them leaves the
/// data loading plausibly and wrong (`sensors` unsplit, api streams unpaired, `collection_events`
/// never backfilled, identity calibrations never retired). Such a database is carried forward by
/// the chain this replaces, which is commit 3b6876a, the last one registering all 99:
///
/// ```text
/// git worktree add /tmp/premigrate 3b6876a
/// cd /tmp/premigrate && cargo build -p migration
/// ./target/debug/migration up -u postgresql://.../<a copy of the dump>
/// ```
///
/// Then dump that copy `--data-only --exclude-table-data='_timescaledb_internal.*'`, build the
/// target from this baseline, `TRUNCATE constants, parameters, tool_scripts, tool_script_versions,
/// tool_script_activations CASCADE` so the seeds below do not collide with the dump's own copies,
/// and restore. `scripts/dbdiff.sh` compares the result against the carried-forward copy; it
/// normalises TimescaleDB's internal aggregate numbering and skips `reprocessing_jobs`, which the
/// split migration queues work into and an empty database has none of.
#[derive(DeriveMigrationName)]
pub struct Migration;

const BASELINE: &str = r#"
-- ===========================================================================
-- Extensions. btree_gist backs the deployment slot-overlap exclusion constraint and
-- timescaledb the hypertables, so both exist before any table is created.
-- ===========================================================================

CREATE EXTENSION IF NOT EXISTS timescaledb;
CREATE EXTENSION IF NOT EXISTS btree_gist;

-- ===========================================================================
-- Tables, indexes, constraints, functions and triggers. Generated with
-- pg_dump --schema-only --schema=public --no-owner --no-privileges --no-comments,
-- minus seaql_migrations, which the migrator manages, and minus the four continuous
-- aggregates, which pg_dump emits as plain views and the next section rebuilds properly.
-- ===========================================================================

CREATE FUNCTION public.inherit_calibration_parameter_id() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
              BEGIN
                  IF NEW.parameter_id IS NULL THEN
                      SELECT parameter_id INTO NEW.parameter_id
                      FROM sensor_calibrations
                      WHERE sensor_id = NEW.sensor_id AND parameter_id IS NOT NULL
                      ORDER BY valid_from
                      LIMIT 1;
                  END IF;
                  RETURN NEW;
              END;
              $$;

CREATE FUNCTION public.projects_default_subproject() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN INSERT INTO subprojects (project_id, name) VALUES (NEW.id, NEW.name); RETURN NEW; END; $$;

CREATE FUNCTION public.reading_decisions_project() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            DECLARE
                c JSONB;
            BEGIN
                IF NEW.kind = 'flag' THEN
                    UPDATE readings SET is_flagged = TRUE, flag_reason = NEW.new ->> 'reason'
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'unflag' THEN
                    UPDATE readings SET is_flagged = FALSE, flag_reason = NULL
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind IN ('withdraw', 'reject') THEN
                    UPDATE readings
                       SET withdrawn_at = COALESCE((NEW.new ->> 'withdrawn_at')::timestamptz, NEW.at),
                           withdrawn_reason = NEW.new ->> 'reason',
                           unverified = CASE WHEN NEW.kind = 'reject' THEN FALSE ELSE unverified END
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'reassert' THEN
                    UPDATE readings SET withdrawn_at = NULL, withdrawn_reason = NULL
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'curve' THEN
                    UPDATE readings SET standard_curve_id = (NEW.new ->> 'standard_curve_id')::uuid
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'calibration_pin' THEN
                    UPDATE readings SET calibration_id = (NEW.new ->> 'calibration_id')::uuid
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'instrument_pin' THEN
                    UPDATE readings SET sensor_id = (NEW.new ->> 'sensor_id')::uuid
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'value_correction' THEN
                    UPDATE readings
                       SET raw_value = (NEW.new ->> 'raw_value')::double precision,
                           calibrated_value = NULL,
                           ingested_at = CASE
                               WHEN raw_value IS DISTINCT FROM (NEW.new ->> 'raw_value')::double precision
                               THEN NOW() ELSE ingested_at END
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND replicate_index = NEW.replicate_index;
                ELSIF NEW.kind = 'unverified_entry' THEN
                    UPDATE readings SET unverified = TRUE
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'verify' THEN
                    UPDATE readings SET unverified = FALSE
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'rollback' THEN
                    c := NEW.new -> 'columns';
                    UPDATE readings SET
                        is_flagged = CASE WHEN c ? 'is_flagged' THEN (c ->> 'is_flagged')::boolean ELSE is_flagged END,
                        flag_reason = CASE WHEN c ? 'flag_reason' THEN c ->> 'flag_reason' ELSE flag_reason END,
                        withdrawn_at = CASE WHEN c ? 'withdrawn_at' THEN (c ->> 'withdrawn_at')::timestamptz ELSE withdrawn_at END,
                        withdrawn_reason = CASE WHEN c ? 'withdrawn_reason' THEN c ->> 'withdrawn_reason' ELSE withdrawn_reason END,
                        standard_curve_id = CASE WHEN c ? 'standard_curve_id' THEN (c ->> 'standard_curve_id')::uuid ELSE standard_curve_id END,
                        sensor_id = CASE WHEN c ? 'sensor_id' THEN (c ->> 'sensor_id')::uuid ELSE sensor_id END,
                        calibration_id = CASE WHEN c ? 'calibration_id' THEN (c ->> 'calibration_id')::uuid ELSE calibration_id END,
                        raw_value = CASE WHEN c ? 'raw_value' THEN (c ->> 'raw_value')::double precision ELSE raw_value END,
                        calibrated_value = CASE WHEN c ? 'raw_value' THEN NULL ELSE calibrated_value END,
                        unverified = CASE WHEN c ? 'unverified' THEN (c ->> 'unverified')::boolean ELSE unverified END
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                END IF;
                RETURN NEW;
            END;
            $$;

CREATE FUNCTION public.refresh_sample_aggregate(target_sample_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    total_refs BIGINT;
    estimator TEXT;
BEGIN
    IF target_sample_id IS NULL THEN
        RETURN;
    END IF;

    -- Serialize concurrent refreshes for the same sample.
    PERFORM pg_advisory_xact_lock(
        hashtextextended(target_sample_id::text, 0)
    );

    SELECT COUNT(*) INTO total_refs
    FROM readings
    WHERE sample_id = target_sample_id;

    IF total_refs = 0 THEN
        DELETE FROM samples WHERE id = target_sample_id;
        RETURN;
    END IF;

    SELECT s.sd_estimator INTO estimator
    FROM samples s
    WHERE s.id = target_sample_id;

    UPDATE samples s
    SET mean       = a.mean,
        stdev      = CASE WHEN estimator = 'population'
                          THEN a.stdev_pop ELSE a.stdev_samp END,
        n          = COALESCE(a.n, 0),
        min_value  = a.min_value,
        max_value  = a.max_value,
        updated_at = NOW()
    FROM (
        SELECT
            AVG(COALESCE(calibrated_value, raw_value))         AS mean,
            STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev_samp,
            STDDEV_POP(COALESCE(calibrated_value, raw_value))  AS stdev_pop,
            COUNT(*)::INTEGER                                   AS n,
            MIN(COALESCE(calibrated_value, raw_value))         AS min_value,
            MAX(COALESCE(calibrated_value, raw_value))         AS max_value
        FROM readings
        WHERE sample_id = target_sample_id
          AND is_flagged IS NOT TRUE
          AND withdrawn_at IS NULL
    ) a
    WHERE s.id = target_sample_id;
END;
$$;

CREATE FUNCTION public.samples_on_reading_delete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            BEGIN
                PERFORM refresh_sample_aggregate(OLD.sample_id);
                RETURN NULL;
            END;
            $$;

CREATE FUNCTION public.samples_on_reading_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            BEGIN
                PERFORM refresh_sample_aggregate(NEW.sample_id);
                RETURN NULL;
            END;
            $$;

CREATE FUNCTION public.samples_on_reading_update() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            BEGIN
                PERFORM refresh_sample_aggregate(OLD.sample_id);
                IF NEW.sample_id IS DISTINCT FROM OLD.sample_id THEN
                    PERFORM refresh_sample_aggregate(NEW.sample_id);
                END IF;
                RETURN NULL;
            END;
            $$;

CREATE FUNCTION public.sites_ensure_subproject() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN IF TG_OP = 'UPDATE' AND NEW.subproject_id IS DISTINCT FROM OLD.subproject_id AND NEW.subproject_id IS NOT NULL THEN SELECT project_id INTO NEW.project_id FROM subprojects WHERE id = NEW.subproject_id; ELSIF TG_OP = 'UPDATE' AND NEW.project_id IS DISTINCT FROM OLD.project_id THEN IF NEW.subproject_id IS NULL OR NOT EXISTS ( SELECT 1 FROM subprojects WHERE id = NEW.subproject_id AND project_id = NEW.project_id) THEN SELECT id INTO NEW.subproject_id FROM subprojects WHERE project_id = NEW.project_id ORDER BY created_at LIMIT 1; END IF; ELSIF TG_OP = 'INSERT' THEN IF NEW.subproject_id IS NOT NULL THEN SELECT project_id INTO NEW.project_id FROM subprojects WHERE id = NEW.subproject_id; ELSIF NEW.project_id IS NOT NULL THEN SELECT id INTO NEW.subproject_id FROM subprojects WHERE project_id = NEW.project_id ORDER BY created_at LIMIT 1; END IF; END IF; RETURN NEW; END; $$;

CREATE FUNCTION public.subprojects_move_cascade() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN UPDATE sites SET project_id = NEW.project_id WHERE subproject_id = NEW.id AND project_id IS DISTINCT FROM NEW.project_id; RETURN NEW; END; $$;

CREATE TABLE public.readings (
    stream_id uuid NOT NULL,
    "time" timestamp with time zone NOT NULL,
    replicate_index smallint DEFAULT 0 NOT NULL,
    site_id uuid,
    parameter_id uuid,
    raw_value double precision NOT NULL,
    calibrated_value double precision,
    sensor_id uuid,
    calibration_id uuid,
    deployment_id uuid,
    logged boolean DEFAULT true,
    measurement_type character varying(32),
    is_flagged boolean DEFAULT false,
    flag_reason text,
    sample_id uuid,
    standard_curve_id uuid,
    collection_event_id uuid,
    withdrawn_at timestamp with time zone,
    withdrawn_reason text,
    ingested_at timestamp with time zone DEFAULT now(),
    provenance jsonb,
    label text,
    notes text,
    created_by text,
    unverified boolean DEFAULT false NOT NULL,
    CONSTRAINT readings_withdrawn_spot_only CHECK (((withdrawn_at IS NULL) OR ((measurement_type)::text = 'spot'::text)))
);

CREATE TABLE public.alarm_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    parameter_id uuid NOT NULL,
    severity smallint NOT NULL,
    max_severity smallint NOT NULL,
    started_at timestamp with time zone NOT NULL,
    value_at_start double precision NOT NULL,
    last_seen_at timestamp with time zone NOT NULL,
    last_value double precision NOT NULL,
    acknowledged_at timestamp with time zone,
    acknowledged_by text,
    resolved_at timestamp with time zone,
    resolved_value double precision,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    notified_at timestamp with time zone,
    resolution_notified_at timestamp with time zone,
    measurement_type character varying(32) DEFAULT 'continuous'::character varying NOT NULL
);

CREATE TABLE public.alarm_thresholds (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    parameter_id uuid NOT NULL,
    site_id uuid,
    warning_min double precision,
    warning_max double precision,
    alarm_min double precision,
    alarm_max double precision,
    description text,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);

CREATE TABLE public.annotations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    parameter_id uuid NOT NULL,
    start_time timestamp with time zone NOT NULL,
    end_time timestamp with time zone NOT NULL,
    text text NOT NULL,
    category character varying(64) DEFAULT 'general'::character varying NOT NULL,
    created_by character varying(128),
    created_at timestamp with time zone DEFAULT now(),
    audit_hold_id uuid,
    source_system text,
    source_key text,
    standard_curve_id uuid
);

CREATE TABLE public.api_token_audit_log (
    id uuid NOT NULL,
    token_id uuid NOT NULL,
    method text NOT NULL,
    path text NOT NULL,
    status_code integer NOT NULL,
    project_scope uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.api_tokens (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name character varying(128) NOT NULL,
    token_hash character varying(256) NOT NULL,
    project_scope uuid,
    permissions jsonb DEFAULT '{"read_data": true, "write_data": false, "read_metadata": true, "write_metadata": false}'::jsonb NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    expires_at timestamp with time zone,
    last_used_at timestamp with time zone,
    created_by character varying(128),
    token_prefix text NOT NULL,
    description text,
    rate_limit_per_second integer
);

CREATE TABLE public.collection_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    collected_at timestamp with time zone NOT NULL,
    source text DEFAULT 'manual'::text NOT NULL,
    created_by text,
    notes text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone,
    CONSTRAINT collection_events_source_check CHECK ((source = ANY (ARRAY['manual'::text, 'portal_sync'::text])))
);

CREATE TABLE public.constants (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name character varying(128) NOT NULL,
    value double precision NOT NULL,
    units character varying(32),
    description text,
    created_at timestamp with time zone DEFAULT now()
);

CREATE TABLE public.csv_import_staging (
    import_token uuid NOT NULL,
    stream_id uuid NOT NULL,
    site_id uuid,
    parameter_id uuid,
    "time" timestamp with time zone NOT NULL,
    raw_value double precision NOT NULL,
    sensor_id uuid,
    calibration_id uuid,
    deployment_id uuid,
    seq bigint DEFAULT 0 NOT NULL
);

CREATE TABLE public.data_streams (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    source_system text NOT NULL,
    source_key text NOT NULL,
    source_name text,
    source_path text,
    metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    site_parameter_id uuid,
    sensor_id uuid,
    pairing_plan_id uuid,
    is_active boolean DEFAULT true NOT NULL,
    discovered_at timestamp with time zone DEFAULT now() NOT NULL,
    paired_at timestamp with time zone,
    last_data_time timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    measurement_type character varying(32),
    last_window_digest text,
    CONSTRAINT data_streams_measurement_type_check CHECK (measurement_type IN ('continuous', 'spot', 'derived'))
);

CREATE TABLE public.derived_parameter_definitions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    code character varying(128) CONSTRAINT derived_parameter_definitions_name_not_null NOT NULL,
    name character varying(256),
    units character varying(32),
    formula text NOT NULL,
    description text,
    required_parameter_types jsonb,
    created_at timestamp with time zone DEFAULT now(),
    output_parameter_id uuid
);

CREATE TABLE public.derived_parameter_sources (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    derived_definition_id uuid NOT NULL,
    parameter_id uuid NOT NULL,
    variable_name character varying(64) NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);

CREATE TABLE public.ingest_receipts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    stream_id uuid NOT NULL,
    at timestamp with time zone DEFAULT now() NOT NULL,
    window_from timestamp with time zone,
    window_to timestamp with time zone,
    submitted integer NOT NULL,
    new_rows integer NOT NULL,
    changed integer NOT NULL,
    unchanged integer NOT NULL,
    retained integer NOT NULL,
    rejected_total integer NOT NULL,
    rejected jsonb NOT NULL,
    dropped integer NOT NULL,
    withdrawn integer NOT NULL,
    changed_keys jsonb,
    braked boolean DEFAULT false NOT NULL,
    brake_threshold real,
    CONSTRAINT receipt_arithmetic_closes CHECK ((submitted = (((new_rows + changed) + unchanged) + rejected_total)))
);

CREATE TABLE public.notes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    text text NOT NULL,
    verified boolean DEFAULT false NOT NULL,
    created_by character varying(128),
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);

CREATE TABLE public.notification_channel_health (
    channel text NOT NULL,
    healthy boolean NOT NULL,
    detail text,
    checked_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.notification_log (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    alarm_event_id uuid,
    kind text NOT NULL,
    channel text NOT NULL,
    recipient text NOT NULL,
    status text NOT NULL,
    error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.notification_mutes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    parameter_id uuid NOT NULL,
    expires_at timestamp with time zone,
    created_by text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.notification_state (
    kind text NOT NULL,
    subject_key text NOT NULL,
    state text DEFAULT 'firing'::text NOT NULL,
    last_notified_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.notification_subscribers (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    keycloak_sub text NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    last_verified_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    web_push_enabled boolean DEFAULT true NOT NULL
);

CREATE TABLE public.notification_subscriptions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    keycloak_sub text NOT NULL,
    project_id uuid,
    site_id uuid,
    parameter_id uuid,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.pairing_plans (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    source_system text NOT NULL,
    status text DEFAULT 'draft'::text NOT NULL,
    created_by text,
    summary jsonb DEFAULT '{}'::jsonb NOT NULL,
    entries jsonb DEFAULT '[]'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    applied_at timestamp with time zone,
    apply_result jsonb,
    curve_assignments jsonb DEFAULT '[]'::jsonb NOT NULL
);

CREATE TABLE public.parameters (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    code character varying(128) CONSTRAINT parameters_name_not_null NOT NULL,
    name character varying(128) CONSTRAINT parameters_display_name_not_null NOT NULL,
    default_units character varying(32) DEFAULT ''::character varying NOT NULL,
    category character varying(32) DEFAULT 'measurement'::character varying NOT NULL,
    description text,
    aliases text[] DEFAULT '{}'::text[] NOT NULL,
    default_warning_min double precision,
    default_warning_max double precision,
    default_alarm_min double precision,
    default_alarm_max double precision,
    created_at timestamp with time zone DEFAULT now(),
    needs_review boolean DEFAULT false NOT NULL,
    CONSTRAINT parameters_category_check CHECK (category IN ('measurement', 'device_health'))
);

CREATE TABLE public.projects (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name character varying(64) NOT NULL,
    description text,
    data_source character varying(64),
    is_public boolean DEFAULT false,
    public_code character varying(64),
    public_api_title character varying(128),
    public_api_description text,
    public_api_version character varying(32),
    public_contact_email character varying(128),
    created_at timestamp with time zone DEFAULT now(),
    discovered_at timestamp with time zone
);

CREATE TABLE public.reading_decision_sets (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    kind text NOT NULL,
    selection jsonb NOT NULL,
    new jsonb DEFAULT '{}'::jsonb NOT NULL,
    actor text NOT NULL,
    at timestamp with time zone DEFAULT now() NOT NULL,
    reason text,
    rows_decided bigint DEFAULT 0 NOT NULL,
    rolled_back_at timestamp with time zone,
    rolled_back_by text
);

CREATE TABLE public.reading_decisions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    stream_id uuid NOT NULL,
    "time" timestamp with time zone NOT NULL,
    replicate_index smallint,
    kind text NOT NULL,
    old jsonb DEFAULT '{}'::jsonb NOT NULL,
    new jsonb DEFAULT '{}'::jsonb NOT NULL,
    actor text NOT NULL,
    at timestamp with time zone DEFAULT now() NOT NULL,
    reason text,
    origin text NOT NULL,
    supersedes uuid,
    rolled_back_by uuid,
    set_id uuid,
    CONSTRAINT reading_decisions_kind_check CHECK ((kind = ANY (ARRAY['flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text, 'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text, 'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text, 'chain'::text, 'detach'::text, 'return'::text, 'rollback'::text]))),
    CONSTRAINT reading_decisions_origin_check CHECK ((origin = ANY (ARRAY['manual'::text, 'sync'::text, 'csv'::text, 'audit'::text, 'chain'::text, 'rollback'::text, 'migration'::text, 'system'::text])))
);

CREATE TABLE public.replicate_audit_holds (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    stream_id uuid,
    group_time timestamp with time zone NOT NULL,
    expected jsonb NOT NULL,
    computed jsonb NOT NULL,
    delta jsonb NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    acknowledged_by text,
    acknowledged_at timestamp with time zone,
    manual_value double precision,
    resolution jsonb,
    kind text DEFAULT 'replicate_stats'::text NOT NULL,
    site_id uuid,
    parameter_id uuid,
    tool text,
    CONSTRAINT audit_hold_subject CHECK (((stream_id IS NOT NULL) OR ((kind <> 'replicate_stats'::text) AND (site_id IS NOT NULL) AND (parameter_id IS NOT NULL)))),
    CONSTRAINT replicate_audit_holds_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'deferred'::text, 'acknowledged'::text, 'remediated'::text, 'superseded'::text, 'use_portal'::text, 'use_manual'::text, 'consumed'::text])))
);

CREATE TABLE public.reprocessing_job_logs (
    job_id uuid NOT NULL,
    seq bigint NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    level text NOT NULL,
    message text NOT NULL,
    context jsonb DEFAULT '{}'::jsonb NOT NULL
);

CREATE TABLE public.reprocessing_jobs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    sensor_id uuid,
    trigger_type text NOT NULL,
    trigger_id uuid,
    status text DEFAULT 'pending'::text NOT NULL,
    readings_updated integer,
    error_message text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    completed_at timestamp with time zone,
    total integer,
    progress integer,
    retry_count integer DEFAULT 0 NOT NULL,
    detail jsonb DEFAULT '{}'::jsonb NOT NULL,
    category text DEFAULT 'maintenance'::text NOT NULL,
    site_id uuid,
    parent_job_id uuid,
    owner text,
    lease_expires_at timestamp with time zone,
    lease_epoch bigint DEFAULT 0 NOT NULL,
    cancel_requested boolean DEFAULT false NOT NULL,
    params jsonb DEFAULT '{}'::jsonb NOT NULL,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    dedupe_key text
)
WITH (autovacuum_vacuum_scale_factor='0.02', autovacuum_vacuum_threshold='200', autovacuum_vacuum_cost_limit='2000', autovacuum_analyze_scale_factor='0.05');

CREATE TABLE public.samples (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    parameter_id uuid NOT NULL,
    collected_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    mean double precision,
    stdev double precision,
    n integer DEFAULT 0 NOT NULL,
    min_value double precision,
    max_value double precision,
    updated_at timestamp with time zone,
    sd_estimator text DEFAULT 'sample'::text NOT NULL,
    sd_estimator_source text DEFAULT 'default'::text NOT NULL,
    CONSTRAINT samples_sd_estimator_check CHECK ((sd_estimator = ANY (ARRAY['sample'::text, 'population'::text]))),
    CONSTRAINT samples_sd_estimator_source_check CHECK ((sd_estimator_source = ANY (ARRAY['default'::text, 'slot'::text, 'sample'::text, 'stream'::text, 'tool'::text])))
);

CREATE TABLE public.schedule_audit (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    job_name text NOT NULL,
    changed_by text,
    old_value jsonb,
    new_value jsonb,
    changed_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.schedules (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    job_name text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    next_run_at timestamp with time zone,
    interval_seconds bigint,
    overlap_policy text,
    catchup_policy text,
    tunables jsonb DEFAULT '{}'::jsonb NOT NULL,
    last_enqueued_at timestamp with time zone,
    updated_by text,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.seasonal_checks (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    checked_time timestamp with time zone NOT NULL,
    entries jsonb NOT NULL,
    created_by text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.sensor_calibrations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    sensor_id uuid NOT NULL,
    slope double precision NOT NULL,
    intercept double precision NOT NULL,
    valid_from timestamp with time zone NOT NULL,
    performed_by character varying(128),
    notes text,
    created_at timestamp with time zone DEFAULT now(),
    valid_until timestamp with time zone,
    name text,
    parameter_id uuid,
    r_squared double precision,
    valid_until_explicit boolean DEFAULT false NOT NULL
);

CREATE TABLE public.sensor_deployments (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    sensor_id uuid NOT NULL,
    site_id uuid NOT NULL,
    deployed_from timestamp with time zone NOT NULL,
    deployed_until timestamp with time zone,
    deployment_type character varying(64) DEFAULT 'permanent'::character varying NOT NULL,
    notes text,
    created_at timestamp with time zone DEFAULT now(),
    parameter_id uuid NOT NULL
);

CREATE TABLE public.sensors (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    serial_number character varying(64),
    name character varying(128),
    manufacturer character varying(128),
    model character varying(128),
    is_active boolean DEFAULT true,
    is_lab_instrument boolean DEFAULT false,
    notes text,
    metadata jsonb,
    created_at timestamp with time zone DEFAULT now(),
    data_frequency character varying(16) DEFAULT 'high'::character varying NOT NULL,
    source_system text,
    source_key text,
    CONSTRAINT sensors_data_frequency_check CHECK (data_frequency IN ('high', 'low'))
);

CREATE TABLE public.site_parameters (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    site_id uuid NOT NULL,
    parameter_id uuid NOT NULL,
    name character varying(128) NOT NULL,
    sensor_type character varying(64) DEFAULT ''::character varying NOT NULL,
    display_units character varying(32),
    units_name character varying(64),
    units_min double precision,
    units_max double precision,
    decimal_places smallint,
    channel_id integer,
    sample_interval_sec integer DEFAULT 600,
    is_active boolean DEFAULT true,
    is_derived boolean DEFAULT false,
    derived_definition_id uuid,
    variable_mappings jsonb,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    discovered_at timestamp with time zone,
    is_public boolean DEFAULT false NOT NULL,
    needs_review boolean DEFAULT false NOT NULL,
    sd_estimator text,
    CONSTRAINT site_parameters_sd_estimator_check CHECK (((sd_estimator IS NULL) OR (sd_estimator = ANY (ARRAY['sample'::text, 'population'::text]))))
);

CREATE TABLE public.sites (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    project_id uuid,
    name character varying(64) NOT NULL,
    latitude double precision,
    longitude double precision,
    altitude_m double precision,
    public_code character varying(64),
    created_at timestamp with time zone DEFAULT now(),
    discovered_at timestamp with time zone,
    subproject_id uuid
);

CREATE TABLE public.standard_curves (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    sensor_id uuid NOT NULL,
    name text,
    slope double precision NOT NULL,
    intercept double precision NOT NULL,
    r_squared double precision,
    notes text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by text,
    source_system text,
    source_key text
);

CREATE TABLE public.status_events (
    stream_id uuid NOT NULL,
    "time" timestamp with time zone NOT NULL,
    site_id uuid,
    parameter_id uuid,
    value text NOT NULL,
    sensor_id uuid
);

CREATE TABLE public.subprojects (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    project_id uuid NOT NULL,
    name character varying(64) NOT NULL,
    description text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.sync_commands (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    service_id uuid NOT NULL,
    command character varying(64) NOT NULL,
    payload jsonb,
    status character varying(32) DEFAULT 'pending'::character varying NOT NULL,
    result jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    acknowledged_at timestamp with time zone,
    completed_at timestamp with time zone
);

CREATE TABLE public.sync_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    service_id uuid NOT NULL,
    command_id uuid,
    event_type character varying(64) NOT NULL,
    status character varying(32) DEFAULT 'started'::character varying NOT NULL,
    readings_synced bigint DEFAULT 0 NOT NULL,
    status_events_synced bigint DEFAULT 0 NOT NULL,
    errors jsonb,
    log jsonb,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    completed_at timestamp with time zone,
    duration_ms bigint DEFAULT 0,
    readings_skipped bigint DEFAULT 0 NOT NULL
);

CREATE TABLE public.sync_service_credentials (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    client_id character varying(128) NOT NULL,
    client_secret_hash character varying(256) NOT NULL,
    service_type character varying(64) NOT NULL,
    service_id uuid,
    revoked boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.sync_service_tokens (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    service_id uuid NOT NULL,
    token_hash character varying(256) NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.sync_services (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    service_type character varying(64) NOT NULL,
    instance_id character varying(128) NOT NULL,
    status character varying(32) DEFAULT 'registered'::character varying NOT NULL,
    current_operation character varying(128),
    last_heartbeat timestamp with time zone,
    last_sync_completed_at timestamp with time zone,
    last_error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    paused boolean DEFAULT false NOT NULL,
    sync_interval_secs integer
);

CREATE TABLE public.tool_runs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    tool_name text NOT NULL,
    tool_version jsonb NOT NULL,
    inputs jsonb NOT NULL,
    constants jsonb NOT NULL,
    curves jsonb NOT NULL,
    outputs jsonb NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    context jsonb,
    source text DEFAULT 'interactive'::text NOT NULL
);

CREATE TABLE public.tool_script_activations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    tool_script_id uuid NOT NULL,
    from_version_id uuid,
    to_version_id uuid NOT NULL,
    activated_by text,
    activated_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.tool_script_versions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    tool_script_id uuid NOT NULL,
    version_no integer NOT NULL,
    script text NOT NULL,
    entry_function text DEFAULT 'tool'::text NOT NULL,
    manifest jsonb NOT NULL,
    test_cases jsonb DEFAULT '{}'::jsonb NOT NULL,
    content_hash text NOT NULL,
    created_by text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    validated_at timestamp with time zone,
    note text
);

CREATE TABLE public.tool_scripts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    label text NOT NULL,
    description text,
    active_version_id uuid,
    created_by text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    enabled boolean DEFAULT true NOT NULL
);

CREATE TABLE public.user_project_grants (
    user_sub text NOT NULL,
    project_id uuid NOT NULL,
    granted_by text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.web_push_subscriptions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    keycloak_sub text NOT NULL,
    endpoint text NOT NULL,
    p256dh text NOT NULL,
    auth text NOT NULL,
    user_agent text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_success_at timestamp with time zone
);

ALTER TABLE ONLY public.alarm_events
    ADD CONSTRAINT alarm_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.alarm_thresholds
    ADD CONSTRAINT alarm_thresholds_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.annotations
    ADD CONSTRAINT annotations_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.api_token_audit_log
    ADD CONSTRAINT api_token_audit_log_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.api_tokens
    ADD CONSTRAINT api_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.api_tokens
    ADD CONSTRAINT api_tokens_token_hash_key UNIQUE (token_hash);

ALTER TABLE ONLY public.collection_events
    ADD CONSTRAINT collection_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.collection_events
    ADD CONSTRAINT collection_events_site_id_collected_at_key UNIQUE (site_id, collected_at);

ALTER TABLE ONLY public.constants
    ADD CONSTRAINT constants_name_key UNIQUE (name);

ALTER TABLE ONLY public.constants
    ADD CONSTRAINT constants_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.data_streams
    ADD CONSTRAINT data_streams_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.derived_parameter_definitions
    ADD CONSTRAINT derived_parameter_definitions_name_key UNIQUE (code);

ALTER TABLE ONLY public.derived_parameter_definitions
    ADD CONSTRAINT derived_parameter_definitions_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.derived_parameter_sources
    ADD CONSTRAINT derived_parameter_sources_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sensor_deployments
    ADD CONSTRAINT excl_deployment_site_param_slot EXCLUDE USING gist (site_id WITH =, parameter_id WITH =, tstzrange(deployed_from, COALESCE(deployed_until, 'infinity'::timestamp with time zone), '[)'::text) WITH &&);

ALTER TABLE ONLY public.ingest_receipts
    ADD CONSTRAINT ingest_receipts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.notification_channel_health
    ADD CONSTRAINT notification_channel_health_pkey PRIMARY KEY (channel);

ALTER TABLE ONLY public.notification_log
    ADD CONSTRAINT notification_log_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.notification_mutes
    ADD CONSTRAINT notification_mutes_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.notification_state
    ADD CONSTRAINT notification_state_pkey PRIMARY KEY (kind, subject_key);

ALTER TABLE ONLY public.notification_subscribers
    ADD CONSTRAINT notification_subscribers_keycloak_sub_key UNIQUE (keycloak_sub);

ALTER TABLE ONLY public.notification_subscribers
    ADD CONSTRAINT notification_subscribers_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.notification_subscriptions
    ADD CONSTRAINT notification_subscriptions_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.pairing_plans
    ADD CONSTRAINT pairing_plans_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.parameters
    ADD CONSTRAINT parameters_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.projects
    ADD CONSTRAINT projects_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.reading_decision_sets
    ADD CONSTRAINT reading_decision_sets_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.reading_decisions
    ADD CONSTRAINT reading_decisions_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_pkey PRIMARY KEY (stream_id, "time", replicate_index);

ALTER TABLE public.readings
    ADD CONSTRAINT readings_replicate_index_spot_only CHECK (((replicate_index = 0) OR ((measurement_type)::text = 'spot'::text))) NOT VALID;

ALTER TABLE ONLY public.replicate_audit_holds
    ADD CONSTRAINT replicate_audit_holds_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.reprocessing_job_logs
    ADD CONSTRAINT reprocessing_job_logs_pkey PRIMARY KEY (job_id, seq);

ALTER TABLE ONLY public.reprocessing_jobs
    ADD CONSTRAINT reprocessing_jobs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.samples
    ADD CONSTRAINT samples_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.schedule_audit
    ADD CONSTRAINT schedule_audit_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.schedules
    ADD CONSTRAINT schedules_job_name_key UNIQUE (job_name);

ALTER TABLE ONLY public.schedules
    ADD CONSTRAINT schedules_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.seasonal_checks
    ADD CONSTRAINT seasonal_checks_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sensor_calibrations
    ADD CONSTRAINT sensor_calibrations_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sensor_deployments
    ADD CONSTRAINT sensor_deployments_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sensors
    ADD CONSTRAINT sensors_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.site_parameters
    ADD CONSTRAINT site_parameters_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sites
    ADD CONSTRAINT sites_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.standard_curves
    ADD CONSTRAINT standard_curves_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_pkey PRIMARY KEY (stream_id, "time");

ALTER TABLE ONLY public.subprojects
    ADD CONSTRAINT subprojects_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sync_commands
    ADD CONSTRAINT sync_commands_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sync_events
    ADD CONSTRAINT sync_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sync_service_credentials
    ADD CONSTRAINT sync_service_credentials_client_id_key UNIQUE (client_id);

ALTER TABLE ONLY public.sync_service_credentials
    ADD CONSTRAINT sync_service_credentials_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sync_service_tokens
    ADD CONSTRAINT sync_service_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sync_services
    ADD CONSTRAINT sync_services_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.tool_runs
    ADD CONSTRAINT tool_runs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.tool_script_activations
    ADD CONSTRAINT tool_script_activations_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.tool_script_versions
    ADD CONSTRAINT tool_script_versions_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.tool_script_versions
    ADD CONSTRAINT tool_script_versions_tool_script_id_content_hash_key UNIQUE (tool_script_id, content_hash);

ALTER TABLE ONLY public.tool_script_versions
    ADD CONSTRAINT tool_script_versions_tool_script_id_version_no_key UNIQUE (tool_script_id, version_no);

ALTER TABLE ONLY public.tool_scripts
    ADD CONSTRAINT tool_scripts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.derived_parameter_sources
    ADD CONSTRAINT uq_derived_param UNIQUE (derived_definition_id, parameter_id);

ALTER TABLE ONLY public.derived_parameter_sources
    ADD CONSTRAINT uq_derived_var UNIQUE (derived_definition_id, variable_name);

ALTER TABLE ONLY public.site_parameters
    ADD CONSTRAINT uq_site_param UNIQUE (site_id, parameter_id);

ALTER TABLE ONLY public.site_parameters
    ADD CONSTRAINT uq_site_param_name UNIQUE (site_id, name);

ALTER TABLE ONLY public.data_streams
    ADD CONSTRAINT uq_stream_source UNIQUE (source_system, source_key);

ALTER TABLE ONLY public.user_project_grants
    ADD CONSTRAINT user_project_grants_pkey PRIMARY KEY (user_sub, project_id);

ALTER TABLE ONLY public.web_push_subscriptions
    ADD CONSTRAINT web_push_subscriptions_endpoint_key UNIQUE (endpoint);

ALTER TABLE ONLY public.web_push_subscriptions
    ADD CONSTRAINT web_push_subscriptions_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX annotations_provenance_uniq ON public.annotations USING btree (source_system, source_key) WHERE ((source_system IS NOT NULL) AND (source_key IS NOT NULL));

CREATE INDEX idx_alarm_events_last_seen ON public.alarm_events USING btree (last_seen_at DESC);

CREATE INDEX idx_alarm_events_open ON public.alarm_events USING btree (resolved_at) WHERE (resolved_at IS NULL);

CREATE INDEX idx_alarm_events_pending_open ON public.alarm_events USING btree (started_at) WHERE ((notified_at IS NULL) AND (resolved_at IS NULL));

CREATE INDEX idx_alarm_events_pending_resolve ON public.alarm_events USING btree (resolved_at) WHERE ((resolved_at IS NOT NULL) AND (resolution_notified_at IS NULL));

CREATE INDEX idx_alarm_thresholds_site_id ON public.alarm_thresholds USING btree (site_id) WHERE (site_id IS NOT NULL);

CREATE INDEX idx_annotations_audit_hold ON public.annotations USING btree (audit_hold_id) WHERE (audit_hold_id IS NOT NULL);

CREATE INDEX idx_annotations_site_param ON public.annotations USING btree (site_id, parameter_id);

CREATE INDEX idx_annotations_standard_curve ON public.annotations USING btree (standard_curve_id) WHERE (standard_curve_id IS NOT NULL);

CREATE INDEX idx_api_token_audit_token_ts ON public.api_token_audit_log USING btree (token_id, created_at DESC);

CREATE UNIQUE INDEX idx_api_tokens_token_prefix ON public.api_tokens USING btree (token_prefix);

CREATE INDEX idx_csv_import_staging_token ON public.csv_import_staging USING btree (import_token);

CREATE INDEX idx_data_streams_pairing_plan_id ON public.data_streams USING btree (pairing_plan_id) WHERE (pairing_plan_id IS NOT NULL);

CREATE INDEX idx_data_streams_sensor_id ON public.data_streams USING btree (sensor_id) WHERE (sensor_id IS NOT NULL);

CREATE INDEX idx_data_streams_site_param ON public.data_streams USING btree (site_parameter_id) WHERE (site_parameter_id IS NOT NULL);

CREATE INDEX idx_data_streams_source ON public.data_streams USING btree (source_system);

CREATE INDEX idx_ingest_receipts_stream ON public.ingest_receipts USING btree (stream_id, at DESC);

CREATE INDEX idx_notes_site_id ON public.notes USING btree (site_id);

CREATE INDEX idx_notification_log_created ON public.notification_log USING btree (created_at DESC);

CREATE INDEX idx_notification_subscriptions_sub ON public.notification_subscriptions USING btree (keycloak_sub);

CREATE INDEX idx_pairing_plans_source_system ON public.pairing_plans USING btree (source_system);

CREATE INDEX idx_pairing_plans_status ON public.pairing_plans USING btree (status);

CREATE INDEX idx_reading_decisions_key ON public.reading_decisions USING btree (stream_id, "time", replicate_index, at DESC);

CREATE INDEX idx_reading_decisions_set ON public.reading_decisions USING btree (set_id) WHERE (set_id IS NOT NULL);

CREATE INDEX idx_readings_calibration_id ON public.readings USING btree (calibration_id) WHERE (calibration_id IS NOT NULL);

CREATE INDEX idx_readings_collection_event ON public.readings USING btree (collection_event_id) WHERE (collection_event_id IS NOT NULL);

CREATE INDEX idx_readings_flagged_site_param_time ON public.readings USING btree (site_id, parameter_id, "time") WHERE is_flagged;

CREATE INDEX idx_readings_provenance_site_time ON public.readings USING btree (site_id, "time") WHERE (provenance IS NOT NULL);

CREATE INDEX idx_readings_sample_id ON public.readings USING btree (sample_id) WHERE (sample_id IS NOT NULL);

CREATE INDEX idx_readings_sensor_time ON public.readings USING btree (sensor_id, "time" DESC) WHERE (sensor_id IS NOT NULL);

CREATE INDEX idx_readings_site_param_time ON public.readings USING btree (site_id, parameter_id, "time" DESC) WHERE (site_id IS NOT NULL);

CREATE INDEX idx_readings_spot_sensor_time ON public.readings USING btree (sensor_id, "time") WHERE ((measurement_type)::text = 'spot'::text);

CREATE INDEX idx_readings_spot_site_param_time ON public.readings USING btree (site_id, parameter_id, "time") WHERE ((measurement_type)::text = 'spot'::text);

CREATE INDEX idx_readings_standard_curve_id ON public.readings USING btree (standard_curve_id) WHERE (standard_curve_id IS NOT NULL);

CREATE INDEX idx_readings_stream_time ON public.readings USING btree (stream_id, "time" DESC);

CREATE INDEX idx_replicate_audit_holds_status ON public.replicate_audit_holds USING btree (status, created_at);

CREATE INDEX idx_reprocessing_jobs_category ON public.reprocessing_jobs USING btree (category);

CREATE INDEX idx_reprocessing_jobs_claimable ON public.reprocessing_jobs USING btree (next_attempt_at) WHERE (status = 'queued'::text);

CREATE INDEX idx_reprocessing_jobs_event_recompute ON public.reprocessing_jobs USING btree (((params ->> 'collection_event_id'::text)), created_at DESC) WHERE (trigger_type = 'event_recompute'::text);

CREATE INDEX idx_reprocessing_jobs_lease ON public.reprocessing_jobs USING btree (lease_expires_at) WHERE (status = 'running'::text);

CREATE INDEX idx_reprocessing_jobs_sensor_status ON public.reprocessing_jobs USING btree (sensor_id, status);

CREATE INDEX idx_reprocessing_jobs_status ON public.reprocessing_jobs USING btree (status);

CREATE INDEX idx_samples_sd_estimator_default ON public.samples USING btree (site_id, parameter_id) WHERE (sd_estimator_source = 'default'::text);

CREATE INDEX idx_schedule_audit_job_changed ON public.schedule_audit USING btree (job_name, changed_at DESC);

CREATE INDEX idx_schedules_due ON public.schedules USING btree (next_run_at) WHERE enabled;

CREATE INDEX idx_seasonal_checks_site ON public.seasonal_checks USING btree (site_id, created_at);

CREATE INDEX idx_sensor_calibrations_parameter_id ON public.sensor_calibrations USING btree (parameter_id);

CREATE INDEX idx_sensor_calibrations_sensor_valid ON public.sensor_calibrations USING btree (sensor_id, valid_from DESC);

CREATE INDEX idx_sensor_deployments_sensor_id ON public.sensor_deployments USING btree (sensor_id);

CREATE INDEX idx_sensor_deployments_sensor_time ON public.sensor_deployments USING btree (sensor_id, deployed_from DESC);

CREATE INDEX idx_sensor_deployments_site_id ON public.sensor_deployments USING btree (site_id);

CREATE INDEX idx_sensor_deployments_site_param_time ON public.sensor_deployments USING btree (site_id, parameter_id, deployed_from);

CREATE UNIQUE INDEX idx_sensors_serial_unique ON public.sensors USING btree (serial_number) WHERE (serial_number IS NOT NULL);

CREATE INDEX idx_site_parameters_parameter_id ON public.site_parameters USING btree (parameter_id);

CREATE INDEX idx_sites_project_id ON public.sites USING btree (project_id);

CREATE INDEX idx_standard_curves_sensor ON public.standard_curves USING btree (sensor_id);

CREATE INDEX idx_status_events_site_param_time ON public.status_events USING btree (site_id, parameter_id, "time" DESC) WHERE (site_id IS NOT NULL);

CREATE INDEX idx_status_events_stream_time ON public.status_events USING btree (stream_id, "time" DESC);

CREATE INDEX idx_sync_commands_pending ON public.sync_commands USING btree (service_id, status) WHERE ((status)::text = 'pending'::text);

CREATE INDEX idx_sync_events_service ON public.sync_events USING btree (service_id, started_at DESC);

CREATE INDEX idx_sync_svc_creds_service_id ON public.sync_service_credentials USING btree (service_id) WHERE (service_id IS NOT NULL);

CREATE INDEX idx_sync_svc_tokens_service_id ON public.sync_service_tokens USING btree (service_id);

CREATE INDEX idx_sync_tokens_hash ON public.sync_service_tokens USING btree (token_hash);

CREATE INDEX idx_tool_runs_created_at ON public.tool_runs USING btree (created_at);

CREATE INDEX idx_tool_script_activations_script ON public.tool_script_activations USING btree (tool_script_id, activated_at DESC);

CREATE INDEX idx_tool_script_versions_script ON public.tool_script_versions USING btree (tool_script_id, version_no DESC);

CREATE UNIQUE INDEX idx_tool_scripts_name ON public.tool_scripts USING btree (lower(name));

CREATE INDEX idx_user_project_grants_project ON public.user_project_grants USING btree (project_id);

CREATE INDEX idx_wps_keycloak_sub ON public.web_push_subscriptions USING btree (keycloak_sub);

CREATE UNIQUE INDEX parameters_code_lower_idx ON public.parameters USING btree (lower((code)::text));

CREATE UNIQUE INDEX projects_name_lower_idx ON public.projects USING btree (lower((name)::text));

CREATE UNIQUE INDEX projects_public_code_idx ON public.projects USING btree (public_code) WHERE (public_code IS NOT NULL);

CREATE INDEX readings_time_idx ON public.readings USING btree ("time" DESC);

CREATE UNIQUE INDEX replicate_audit_holds_event_live_uniq ON public.replicate_audit_holds USING btree (kind, site_id, parameter_id, group_time) WHERE ((stream_id IS NULL) AND (status = 'pending'::text));

CREATE UNIQUE INDEX replicate_audit_holds_live_uniq ON public.replicate_audit_holds USING btree (stream_id, group_time, kind) WHERE (status = ANY (ARRAY['pending'::text, 'deferred'::text]));

CREATE INDEX samples_parameter_idx ON public.samples USING btree (parameter_id, collected_at DESC);

CREATE INDEX samples_site_idx ON public.samples USING btree (site_id, collected_at DESC);

CREATE UNIQUE INDEX samples_site_param_time_uniq ON public.samples USING btree (site_id, parameter_id, collected_at);

CREATE UNIQUE INDEX sensors_provenance_uniq ON public.sensors USING btree (source_system, source_key) WHERE ((source_system IS NOT NULL) AND (source_key IS NOT NULL));

CREATE UNIQUE INDEX sites_name_lower_idx ON public.sites USING btree (lower((name)::text));

CREATE INDEX sites_subproject_idx ON public.sites USING btree (subproject_id);

CREATE UNIQUE INDEX standard_curves_provenance_uniq ON public.standard_curves USING btree (source_system, source_key) WHERE ((source_system IS NOT NULL) AND (source_key IS NOT NULL));

CREATE INDEX status_events_time_idx ON public.status_events USING btree ("time" DESC);

CREATE INDEX subprojects_project_idx ON public.subprojects USING btree (project_id);

CREATE UNIQUE INDEX subprojects_project_name_idx ON public.subprojects USING btree (project_id, lower((name)::text));

CREATE UNIQUE INDEX uq_alarm_events_open ON public.alarm_events USING btree (site_id, parameter_id, measurement_type) WHERE (resolved_at IS NULL);

CREATE UNIQUE INDEX uq_alarm_thresh_param_global ON public.alarm_thresholds USING btree (parameter_id) WHERE (site_id IS NULL);

CREATE UNIQUE INDEX uq_alarm_thresh_param_site ON public.alarm_thresholds USING btree (parameter_id, site_id) WHERE (site_id IS NOT NULL);

CREATE UNIQUE INDEX uq_notification_mutes_slot ON public.notification_mutes USING btree (site_id, parameter_id);

CREATE UNIQUE INDEX uq_notification_subscriptions_scope ON public.notification_subscriptions USING btree (keycloak_sub, COALESCE(project_id, '00000000-0000-0000-0000-000000000000'::uuid), COALESCE(site_id, '00000000-0000-0000-0000-000000000000'::uuid), COALESCE(parameter_id, '00000000-0000-0000-0000-000000000000'::uuid));

CREATE UNIQUE INDEX uq_reprocessing_jobs_dedupe_key ON public.reprocessing_jobs USING btree (dedupe_key) WHERE (dedupe_key IS NOT NULL);

CREATE TRIGGER projects_default_subproject_trg AFTER INSERT ON public.projects FOR EACH ROW EXECUTE FUNCTION public.projects_default_subproject();

CREATE TRIGGER sites_ensure_subproject_trg BEFORE INSERT OR UPDATE ON public.sites FOR EACH ROW EXECUTE FUNCTION public.sites_ensure_subproject();

CREATE TRIGGER subprojects_move_cascade_trg AFTER UPDATE OF project_id ON public.subprojects FOR EACH ROW WHEN ((old.project_id IS DISTINCT FROM new.project_id)) EXECUTE FUNCTION public.subprojects_move_cascade();

CREATE TRIGGER trg_inherit_calibration_parameter_id BEFORE INSERT ON public.sensor_calibrations FOR EACH ROW EXECUTE FUNCTION public.inherit_calibration_parameter_id();

CREATE TRIGGER trg_reading_decisions_project AFTER INSERT ON public.reading_decisions FOR EACH ROW EXECUTE FUNCTION public.reading_decisions_project();

CREATE TRIGGER trg_readings_sample_refresh_del AFTER DELETE ON public.readings FOR EACH ROW WHEN ((old.sample_id IS NOT NULL)) EXECUTE FUNCTION public.samples_on_reading_delete();

CREATE TRIGGER trg_readings_sample_refresh_ins AFTER INSERT ON public.readings FOR EACH ROW WHEN ((new.sample_id IS NOT NULL)) EXECUTE FUNCTION public.samples_on_reading_insert();

CREATE TRIGGER trg_readings_sample_refresh_upd AFTER UPDATE ON public.readings FOR EACH ROW WHEN (((old.sample_id IS DISTINCT FROM new.sample_id) OR (old.raw_value IS DISTINCT FROM new.raw_value) OR (old.calibrated_value IS DISTINCT FROM new.calibrated_value) OR (old.is_flagged IS DISTINCT FROM new.is_flagged) OR (old.withdrawn_at IS DISTINCT FROM new.withdrawn_at))) EXECUTE FUNCTION public.samples_on_reading_update();

ALTER TABLE ONLY public.alarm_events
    ADD CONSTRAINT alarm_events_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.alarm_events
    ADD CONSTRAINT alarm_events_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.alarm_thresholds
    ADD CONSTRAINT alarm_thresholds_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.alarm_thresholds
    ADD CONSTRAINT alarm_thresholds_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.annotations
    ADD CONSTRAINT annotations_audit_hold_id_fkey FOREIGN KEY (audit_hold_id) REFERENCES public.replicate_audit_holds(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.annotations
    ADD CONSTRAINT annotations_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.annotations
    ADD CONSTRAINT annotations_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.annotations
    ADD CONSTRAINT annotations_standard_curve_id_fkey FOREIGN KEY (standard_curve_id) REFERENCES public.standard_curves(id);

ALTER TABLE ONLY public.api_tokens
    ADD CONSTRAINT api_tokens_project_scope_fkey FOREIGN KEY (project_scope) REFERENCES public.projects(id);

ALTER TABLE ONLY public.collection_events
    ADD CONSTRAINT collection_events_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.data_streams
    ADD CONSTRAINT data_streams_pairing_plan_id_fkey FOREIGN KEY (pairing_plan_id) REFERENCES public.pairing_plans(id);

ALTER TABLE ONLY public.data_streams
    ADD CONSTRAINT data_streams_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.data_streams
    ADD CONSTRAINT data_streams_site_parameter_id_fkey FOREIGN KEY (site_parameter_id) REFERENCES public.site_parameters(id);

ALTER TABLE ONLY public.derived_parameter_definitions
    ADD CONSTRAINT derived_parameter_definitions_output_parameter_id_fkey FOREIGN KEY (output_parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.derived_parameter_sources
    ADD CONSTRAINT derived_parameter_sources_derived_definition_id_fkey FOREIGN KEY (derived_definition_id) REFERENCES public.derived_parameter_definitions(id);

ALTER TABLE ONLY public.derived_parameter_sources
    ADD CONSTRAINT derived_parameter_sources_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT fk_readings_collection_event FOREIGN KEY (collection_event_id) REFERENCES public.collection_events(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.tool_scripts
    ADD CONSTRAINT fk_tool_scripts_active_version FOREIGN KEY (active_version_id) REFERENCES public.tool_script_versions(id);

ALTER TABLE ONLY public.ingest_receipts
    ADD CONSTRAINT ingest_receipts_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES public.data_streams(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.notification_log
    ADD CONSTRAINT notification_log_alarm_event_id_fkey FOREIGN KEY (alarm_event_id) REFERENCES public.alarm_events(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.notification_mutes
    ADD CONSTRAINT notification_mutes_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.notification_mutes
    ADD CONSTRAINT notification_mutes_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.notification_subscriptions
    ADD CONSTRAINT notification_subscriptions_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.notification_subscriptions
    ADD CONSTRAINT notification_subscriptions_project_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.notification_subscriptions
    ADD CONSTRAINT notification_subscriptions_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.reading_decisions
    ADD CONSTRAINT reading_decisions_rolled_back_by_fkey FOREIGN KEY (rolled_back_by) REFERENCES public.reading_decisions(id);

ALTER TABLE ONLY public.reading_decisions
    ADD CONSTRAINT reading_decisions_supersedes_fkey FOREIGN KEY (supersedes) REFERENCES public.reading_decisions(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_calibration_id_fkey FOREIGN KEY (calibration_id) REFERENCES public.sensor_calibrations(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_deployment_id_fkey FOREIGN KEY (deployment_id) REFERENCES public.sensor_deployments(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_sample_id_fkey FOREIGN KEY (sample_id) REFERENCES public.samples(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_standard_curve_id_fkey FOREIGN KEY (standard_curve_id) REFERENCES public.standard_curves(id);

ALTER TABLE ONLY public.readings
    ADD CONSTRAINT readings_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES public.data_streams(id);

ALTER TABLE ONLY public.replicate_audit_holds
    ADD CONSTRAINT replicate_audit_holds_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.replicate_audit_holds
    ADD CONSTRAINT replicate_audit_holds_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES public.data_streams(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.reprocessing_job_logs
    ADD CONSTRAINT reprocessing_job_logs_job_id_fkey FOREIGN KEY (job_id) REFERENCES public.reprocessing_jobs(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.reprocessing_jobs
    ADD CONSTRAINT reprocessing_jobs_parent_job_id_fkey FOREIGN KEY (parent_job_id) REFERENCES public.reprocessing_jobs(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.reprocessing_jobs
    ADD CONSTRAINT reprocessing_jobs_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id);

ALTER TABLE ONLY public.samples
    ADD CONSTRAINT samples_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.samples
    ADD CONSTRAINT samples_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.seasonal_checks
    ADD CONSTRAINT seasonal_checks_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.sensor_calibrations
    ADD CONSTRAINT sensor_calibrations_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.sensor_calibrations
    ADD CONSTRAINT sensor_calibrations_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id);

ALTER TABLE ONLY public.sensor_deployments
    ADD CONSTRAINT sensor_deployments_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.sensor_deployments
    ADD CONSTRAINT sensor_deployments_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id);

ALTER TABLE ONLY public.sensor_deployments
    ADD CONSTRAINT sensor_deployments_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.site_parameters
    ADD CONSTRAINT site_parameters_derived_definition_id_fkey FOREIGN KEY (derived_definition_id) REFERENCES public.derived_parameter_definitions(id);

ALTER TABLE ONLY public.site_parameters
    ADD CONSTRAINT site_parameters_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.site_parameters
    ADD CONSTRAINT site_parameters_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.sites
    ADD CONSTRAINT sites_project_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id);

ALTER TABLE ONLY public.sites
    ADD CONSTRAINT sites_subproject_id_fkey FOREIGN KEY (subproject_id) REFERENCES public.subprojects(id);

ALTER TABLE ONLY public.standard_curves
    ADD CONSTRAINT standard_curves_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id);

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_parameter_id_fkey FOREIGN KEY (parameter_id) REFERENCES public.parameters(id);

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_sensor_id_fkey FOREIGN KEY (sensor_id) REFERENCES public.sensors(id);

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_site_id_fkey FOREIGN KEY (site_id) REFERENCES public.sites(id);

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES public.data_streams(id);

ALTER TABLE ONLY public.subprojects
    ADD CONSTRAINT subprojects_project_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.sync_commands
    ADD CONSTRAINT sync_commands_service_id_fkey FOREIGN KEY (service_id) REFERENCES public.sync_services(id);

ALTER TABLE ONLY public.sync_events
    ADD CONSTRAINT sync_events_command_id_fkey FOREIGN KEY (command_id) REFERENCES public.sync_commands(id);

ALTER TABLE ONLY public.sync_events
    ADD CONSTRAINT sync_events_service_id_fkey FOREIGN KEY (service_id) REFERENCES public.sync_services(id);

ALTER TABLE ONLY public.sync_service_credentials
    ADD CONSTRAINT sync_service_credentials_service_id_fkey FOREIGN KEY (service_id) REFERENCES public.sync_services(id);

ALTER TABLE ONLY public.sync_service_tokens
    ADD CONSTRAINT sync_service_tokens_service_id_fkey FOREIGN KEY (service_id) REFERENCES public.sync_services(id);

ALTER TABLE ONLY public.tool_script_activations
    ADD CONSTRAINT tool_script_activations_from_version_id_fkey FOREIGN KEY (from_version_id) REFERENCES public.tool_script_versions(id);

ALTER TABLE ONLY public.tool_script_activations
    ADD CONSTRAINT tool_script_activations_to_version_id_fkey FOREIGN KEY (to_version_id) REFERENCES public.tool_script_versions(id);

ALTER TABLE ONLY public.tool_script_activations
    ADD CONSTRAINT tool_script_activations_tool_script_id_fkey FOREIGN KEY (tool_script_id) REFERENCES public.tool_scripts(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.tool_script_versions
    ADD CONSTRAINT tool_script_versions_tool_script_id_fkey FOREIGN KEY (tool_script_id) REFERENCES public.tool_scripts(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.user_project_grants
    ADD CONSTRAINT user_project_grants_project_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id) ON DELETE CASCADE;

-- ===========================================================================
-- TimescaleDB objects. A schema dump carries none of these: hypertables, chunk
-- intervals, columnstore settings, compression policies and continuous aggregates live
-- in the extension's own catalog.
-- ===========================================================================

SELECT create_hypertable('readings', 'time',
                         chunk_time_interval => INTERVAL '90 days',
                         if_not_exists => TRUE, migrate_data => TRUE);
SELECT create_hypertable('status_events', 'time',
                         chunk_time_interval => INTERVAL '30 days',
                         if_not_exists => TRUE, migrate_data => TRUE);

ALTER TABLE readings SET (timescaledb.compress, timescaledb.compress_segmentby = 'stream_id');
ALTER TABLE status_events SET (timescaledb.compress, timescaledb.compress_segmentby = 'stream_id');

SELECT add_compression_policy('readings', INTERVAL '30 days', if_not_exists => TRUE);
SELECT add_compression_policy('status_events', INTERVAL '90 days', if_not_exists => TRUE);

CREATE MATERIALIZED VIEW readings_hourly
WITH (timescaledb.continuous) AS
SELECT
    time_bucket('1 hour', time) AS bucket,
    site_id,
    parameter_id, sensor_id,
    AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
    MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
    MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
    COUNT(*) AS count,
    STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value,
    SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,
    SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value
FROM readings
WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE) AND measurement_type IS DISTINCT FROM 'spot'
GROUP BY time_bucket('1 hour', time), site_id, parameter_id, sensor_id
WITH NO DATA;

CREATE MATERIALIZED VIEW readings_daily
WITH (timescaledb.continuous) AS
SELECT
    time_bucket('1 day', time) AS bucket,
    site_id,
    parameter_id, sensor_id,
    AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
    MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
    MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
    COUNT(*) AS count,
    STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value,
    SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,
    SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value
FROM readings
WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE) AND measurement_type IS DISTINCT FROM 'spot'
GROUP BY time_bucket('1 day', time), site_id, parameter_id, sensor_id
WITH NO DATA;

CREATE MATERIALIZED VIEW readings_weekly
WITH (timescaledb.continuous) AS
SELECT
    time_bucket('1 week', time) AS bucket,
    site_id,
    parameter_id, sensor_id,
    AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
    MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
    MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
    COUNT(*) AS count,
    STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value,
    SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,
    SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value
FROM readings
WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE) AND measurement_type IS DISTINCT FROM 'spot'
GROUP BY time_bucket('1 week', time), site_id, parameter_id, sensor_id
WITH NO DATA;

CREATE MATERIALIZED VIEW readings_monthly
WITH (timescaledb.continuous) AS
SELECT
    time_bucket('1 month', time) AS bucket,
    site_id,
    parameter_id, sensor_id,
    AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
    MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
    MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
    COUNT(*) AS count,
    STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value,
    SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,
    SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value
FROM readings
WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE) AND measurement_type IS DISTINCT FROM 'spot'
GROUP BY time_bucket('1 month', time), site_id, parameter_id, sensor_id
WITH NO DATA;

SELECT add_continuous_aggregate_policy('readings_hourly',
    start_offset => INTERVAL '3 hours',  end_offset => INTERVAL '1 hour',  schedule_interval => INTERVAL '1 hour',  if_not_exists => TRUE);
SELECT add_continuous_aggregate_policy('readings_daily',
    start_offset => INTERVAL '3 days',   end_offset => INTERVAL '1 day',   schedule_interval => INTERVAL '1 day',   if_not_exists => TRUE);
SELECT add_continuous_aggregate_policy('readings_weekly',
    start_offset => INTERVAL '3 weeks',  end_offset => INTERVAL '1 week',  schedule_interval => INTERVAL '1 week',  if_not_exists => TRUE);
SELECT add_continuous_aggregate_policy('readings_monthly',
    start_offset => INTERVAL '3 months', end_offset => INTERVAL '1 month', schedule_interval => INTERVAL '1 month', if_not_exists => TRUE);

-- ===========================================================================
-- Rows the migration chain seeded, dumped rather than recomputed so the tool script
-- text, its normalised manifest and test-case jsonb and its content hash are exact.
-- ===========================================================================

INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('714f9826-4216-42f2-ad6d-b8a95899bd85', 'gas_const_r_atm', 0.0820574, 'L*atm/(mol*K)', 'Ideal gas constant (R) in L*atm/(mol*K)', '2026-09-04 12:10:27.649872+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('70b877bb-dc39-4627-a01f-1828148633c0', 'h_co2_29815k', 0.034733, 'M/atm', 'Henry volatility constant for CO2 at 298.15K', '2026-09-04 12:10:27.649872+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('7343056a-4178-4e76-9b90-60e90cd687c6', 'c_const', 2400, 'K', 'Constant C of van''t Hoff equation (K)', '2026-09-04 12:10:27.649872+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('697c695c-fab5-47e9-ad15-6ea4a35c1a72', 'vol_sa', 0.03, 'L', 'Volume of SA in syringe', '2026-09-04 12:10:27.898738+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('f8c8b614-46c2-425d-87f2-5d99159311a4', 'vol_water', 0.03, 'L', 'Volume of water in syringe', '2026-09-04 12:10:27.898738+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('377d50b9-75d5-4710-b2a8-1ea2c7b1a60a', 'lab_press_avg_atm', 0.957237, 'atm', 'Lab pressure (average of past years)', '2026-09-04 12:10:27.898738+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('7f40c736-07dc-49a5-960e-260df897f793', 'lab_temp_avg_degC', 22.5, 'degC', 'Lab temp (average of past years)', '2026-09-04 12:10:27.898738+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('22fede87-6784-4478-8cbf-bcc8e8ed4495', 'h_ch4_29815k', 0.00213, 'M/atm', 'Henry constant for CH4 at 298.15K', '2026-09-04 12:10:27.898738+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('89eb1e90-2790-457f-bd91-43098934b44e', 'gas_const_r_mol', 8.31446, 'J/(K*mol)', 'Ideal gas constant (R) in J/(K*mol)', '2026-09-04 12:10:27.649872+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('57db1913-d35d-41ec-ad26-d20ce46b4564', 'vial_volume', 12.168, 'mL', 'Max DIC vial volume', '2026-09-04 12:10:27.649872+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('05a58e95-b3e5-46c5-aa8b-629ef217912a', 'h3po4_added', 0.3, 'mL', 'Volume of added H3PO4', '2026-09-04 12:10:27.649872+00');
INSERT INTO public.constants (id, name, value, units, description, created_at) VALUES ('f3844df4-b718-4f45-a011-f588ed900c2c', 'ch4_in_sa', 2e-06, NULL, 'Fraction of CH4 in standard air (dimensionless)', '2026-09-04 12:10:27.649872+00');

INSERT INTO public.parameters (id, code, name, default_units, category, description, aliases, default_warning_min, default_warning_max, default_alarm_min, default_alarm_max, created_at, needs_review) VALUES ('91b027c4-396b-4c9b-b855-0ed3dfcbd207', 'DOC', 'Dissolved organic carbon', 'ppb', 'measurement', NULL, '{}', NULL, NULL, NULL, NULL, '2026-09-04 12:10:27.928014+00', true);

INSERT INTO public.tool_scripts (id, name, label, description, active_version_id, created_by, created_at, updated_at, enabled) VALUES ('8183564e-05b4-4956-89d4-c8013cca066f', 'doc', 'DOC', 'Dissolved organic carbon: the analyser replicates are stored as readings, corrected through the chosen standard curve, and their mean and standard deviation are the served DOC.', NULL, 'seed', '2026-09-04 12:10:27.924083+00', '2026-09-04 12:10:27.924083+00', true);

INSERT INTO public.tool_script_versions (id, tool_script_id, version_no, script, entry_function, manifest, test_cases, content_hash, created_by, created_at, validated_at, note) VALUES ('abc3ee60-2bb7-474b-91e9-d76754dea651', '8183564e-05b4-4956-89d4-c8013cca066f', 1, '# DOC is data entry: the replicates are stored as readings, the chosen standard curve corrects
# them, and the manifest''s aggregate outputs (mean, sd) are computed by the engine over the
# curve-applied values, so the preview equals what the database will serve. Nothing is left for
# the script to calculate.

tool <- function(inputs, constants, curves) {
  list()
}
', 'tool', '{"label": "DOC", "curves": [{"name": "std_curve", "label": "Standard curve (DOC corr)", "required": false, "description": "Applied to every replicate before averaging: corrected = slope * measured + intercept."}], "params": [{"kind": "replicates", "name": "DOC", "when": null, "curve": "std_curve", "label": "DOC", "units": "ppb", "default": null, "section": "lab", "required": false, "suggested": 3, "description": "One value per analyser vial, in run order. Add or remove vials as measured; a blank vial is a gap, never a shift.", "parameter_code": "DOC"}], "outputs": [{"key": "DOC_avg_ppb", "label": "DOC average", "units": "ppb", "aggregate": "mean", "aggregate_of": "DOC", "per_replicate": false, "suggested_parameter_code": null}, {"key": "DOC_sd_ppb", "label": "DOC standard deviation", "units": "ppb", "aggregate": "sd", "aggregate_of": "DOC", "per_replicate": false, "suggested_parameter_code": null}], "sections": [{"key": "lab", "label": "Lab measurements"}], "constants": [], "description": "Dissolved organic carbon: the analyser replicates are stored as readings, corrected through the chosen standard curve, and their mean and standard deviation are the served DOC.", "match_keywords": ["doc", "organic carbon", "dissolved organic"]}', '{"cases": [{"name": "rust_pin_with_curve", "absent": [], "curves": {"std_curve": {"slope": 1.05, "intercept": -2.0}}, "inputs": {"DOC": [120.0, 125.0, 118.0]}, "expected": {"DOC_sd_ppb": 3.78582883923719, "DOC_avg_ppb": 125.05}, "constants": {}}, {"name": "golden_no_curve", "absent": [], "curves": {}, "inputs": {"DOC": [120.0, 125.0, 118.0]}, "expected": {"DOC_sd_ppb": 3.60555127546399, "DOC_avg_ppb": 121.0}, "constants": {}}, {"name": "single_replicate_omits_sd", "absent": ["DOC_sd_ppb"], "curves": {}, "inputs": {"DOC": [120.0]}, "expected": {"DOC_avg_ppb": 120.0}, "constants": {}}, {"name": "lone_second_replicate_stays_on_its_own_number", "absent": ["DOC_sd_ppb"], "curves": {}, "inputs": {"DOC": [null, 120.0]}, "expected": {"DOC_avg_ppb": 120.0}, "constants": {}}, {"name": "all_null_omits_both", "absent": ["DOC_avg_ppb", "DOC_sd_ppb"], "curves": {}, "inputs": {"DOC": [null, null, null]}, "expected": {}, "constants": {}}, {"name": "one_null_no_curve", "absent": [], "curves": {}, "inputs": {"DOC": [120.0, null, 118.0]}, "expected": {"DOC_sd_ppb": 1.4142135623731, "DOC_avg_ppb": 119.0}, "constants": {}}, {"name": "golden_avg_rand_1", "absent": [], "curves": {"std_curve": {"slope": 0.961278225202113, "intercept": -0.97404258325696}}, "inputs": {"DOC": [190.671224833932, 245.170278369915, 230.796007509343]}, "expected": {"DOC_sd_ppb": 27.1515432188154, "DOC_avg_ppb": 212.633998467253}, "constants": {}}, {"name": "golden_avg_rand_9_null_with_curve", "absent": [], "curves": {"std_curve": {"slope": 1.10074498825707, "intercept": 2.28358845226467}}, "inputs": {"DOC": [null, 467.760288110003, 472.654093289748]}, "expected": {"DOC_sd_ppb": 3.8090651005153, "DOC_avg_ppb": 519.861797057587}, "constants": {}}, {"name": "golden_sd_rand_1_with_curve", "absent": [], "curves": {"std_curve": {"slope": 0.932979776454158, "intercept": -3.29249400179833}}, "inputs": {"DOC": [108.206150960177, 429.328132607043, 410.717265773565]}, "expected": {"DOC_sd_ppb": 168.186120201977, "DOC_avg_ppb": 291.60734550696}, "constants": {}}, {"name": "golden_sd_rand_2_no_curve", "absent": [], "curves": {}, "inputs": {"DOC": [74.2234499659389, 429.851801227778, 476.666156097781]}, "expected": {"DOC_sd_ppb": 220.084544270995, "DOC_avg_ppb": 326.913802430499}, "constants": {}}, {"name": "no_replicates_omits_both", "absent": ["DOC_avg_ppb", "DOC_sd_ppb"], "curves": {}, "inputs": {}, "expected": {}, "constants": {}}, {"name": "two_replicates_third_absent", "absent": [], "curves": {"std_curve": {"slope": 1.01862098516431, "intercept": -4.9273814773187}}, "inputs": {"DOC": [271.935691975523, 399.443094816525]}, "expected": {"DOC_sd_ppb": 91.8402423462114, "DOC_avg_ppb": 337.012879132948}, "constants": {}}, {"name": "five_replicates", "absent": [], "curves": {}, "inputs": {"DOC": [120.0, 125.0, 118.0, 122.5, 119.75]}, "expected": {"DOC_sd_ppb": 2.7294688127912363, "DOC_avg_ppb": 121.05}, "constants": {}}, {"name": "five_replicates_with_curve", "absent": [], "curves": {"std_curve": {"slope": 1.05, "intercept": -2.0}}, "inputs": {"DOC": [120.0, 125.0, 118.0, 122.5, 119.75]}, "expected": {"DOC_sd_ppb": 2.865942253430795, "DOC_avg_ppb": 125.1025}, "constants": {}}, {"name": "ten_replicates", "absent": [], "curves": {}, "inputs": {"DOC": [101.0, 102.5, 99.0, 100.25, 103.0, 98.5, 101.75, 100.0, 102.0, 99.5]}, "expected": {"DOC_sd_ppb": 1.536590742882148, "DOC_avg_ppb": 100.75}, "constants": {}}, {"name": "no_replicates_at_all", "absent": ["DOC_avg_ppb", "DOC_sd_ppb"], "curves": {}, "inputs": {}, "expected": {}, "constants": {}}, {"name": "empty_list_is_no_replicates", "absent": ["DOC_avg_ppb", "DOC_sd_ppb"], "curves": {}, "inputs": {"DOC": []}, "expected": {}, "constants": {}}], "notes": "No portal-vs-Rust divergence for this tool: both average the curve-corrected replicates and take their sd (na.rm). The three replicates are declared as the portal''s own columns DOC_rep_1/2/3 and read by name, so a replicate entered alone keeps its number (lone_second_replicate_stays_on_its_own_number); an absent replicate is NA in the row the portal builds. A lone replicate yields an average and no sd: calcSd returns the ''KEEP OLD'' sentinel, which carries the stored value forward in the portal, and this tool is stateless, so the key is omitted. The sentinel is the only omission; no plausibility filter is applied, so a NaN or Inf would be emitted as the portal displays it. calcMean and calcSd over finite replicates reach neither, so no case pins one. Inputs reshaped to the replicates vector; expected values unchanged. The five-, ten- and empty-replicate cases pin the width generalisation.", "tolerance": 0.000000001}', 'sha256:90284796affc9f931c0e91c5ebe1cd3a3188cca537e34bc873460d599e3b20fb', 'seed', '2026-09-04 12:10:27.924083+00', '2026-09-04 12:10:27.924083+00', NULL);

INSERT INTO public.tool_script_activations (id, tool_script_id, from_version_id, to_version_id, activated_by, activated_at) VALUES ('7fe1a009-07e0-4ffc-8e76-df411d5b6ffb', '8183564e-05b4-4956-89d4-c8013cca066f', NULL, 'abc3ee60-2bb7-474b-91e9-d76754dea651', 'seed', '2026-09-04 12:10:27.924083+00');

UPDATE public.tool_scripts SET active_version_id = 'abc3ee60-2bb7-474b-91e9-d76754dea651' WHERE id = '8183564e-05b4-4956-89d4-c8013cca066f';
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(BASELINE).await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "the baseline is forward-only: drop the database instead".to_string(),
        ))
    }
}
