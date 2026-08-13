# river-data-api

> **Note:** Not to be mistaken for the river-api project, related to the astrocast project, which will be migrated to this.

Time-series API for RIVER sensor data.

## Quick Start

```bash
cp .env.example .env
docker compose up -d
```

API: `http://localhost:3005` | Docs: `http://localhost:3005/docs`

## Architecture

Background sync tasks poll Vaisala API and store readings in TimescaleDB hypertables. Continuous aggregates provide hourly/daily/weekly/monthly rollups.

## Authentication and roles

Keycloak (realm `river-data`) authenticates human users; API tokens and sync-service session tokens cover machine access. A login gets in only if it holds one of four ordered realm roles:

| Role | Level | Grants |
|------|-------|--------|
| `riverdata-intern` | Intern | Read data and metadata |
| `riverdata-river` | River | Intern, plus write data and field metadata |
| `riverdata-manager` | Manager | River, plus manage sensors and the catalog |
| `riverdata-admin` | Administrator | Everything: users, tokens, onboarding, all projects |

A login holding none of these lands on an unauthorized page. Non-admin users see only the projects granted to them (`user_project_grants`, set per user in the dashboard); administrators see all projects.

The dev realm is in `keycloak-realm-dev.json` (fixture users `admin`, `manager1`, `river1`, `intern1`, `norole`; password = username), imported on first start.

## Triggers

Work that runs on its own, rather than because a handler called it. Three kinds: PostgreSQL
triggers, CrudCrate model hooks, and in-process subscribers and loops.

### Database triggers

| Trigger | Fires on | When | Why here rather than in the caller |
|---------|----------|------|------------------------------------|
| `trg_readings_sample_refresh_ins` / `_del` / `_upd` (`m20260420_000001_samples`, calling the `refresh_sample_aggregate` settled by `m20260813_000005`) | `readings`, per row | AFTER INSERT / DELETE when `sample_id` is set; AFTER UPDATE when `sample_id`, `raw_value`, `calibrated_value` or `is_flagged` changes | Recomputes `samples.mean/stdev/n/min_value/max_value` over the replicates of that `sample_id` whose `is_flagged IS NOT TRUE` (a flag that was never set is not a flag), and removes the sample once no reading references it at all rather than leaving an `n = 0` tombstone. Five write paths plus several raw-SQL statements mutate `readings`, so the statistic holds for all of them only if the database maintains it. An UPDATE that moves the link refreshes both the old and the new sample. |
| `trg_inherit_calibration_parameter_id` (`m20260711_000004`, restated without the mode predicate by `m20260813_000004`) | `sensor_calibrations`, per row | BEFORE INSERT | Fills a curve's `parameter_id` from the sensor's first parameter-bearing curve when the request omits one, so a multi-channel instrument's curves land on a channel without every client knowing the rule. |
| `trg_set_deployment_parameter_id` (`m20260603_000006`, redefined by `m20260711_000003`) | `sensor_deployments`, per row | BEFORE INSERT OR UPDATE OF `sensor_id` | Maintains the read-only `parameter_id` twin on a deployment. It is derived state, so it is computed where the row is written. |
| `projects_default_subproject_trg` (`m20260710_000001`) | `projects`, per row | AFTER INSERT | Gives every project a default subproject, so `sites.subproject_id` can be NOT NULL without each project-creation path remembering to make one. |
| `sites_ensure_subproject_trg` (`m20260710_000001`) | `sites`, per row | BEFORE INSERT OR UPDATE | Keeps `sites.subproject_id` and the denormalised `sites.project_id` consistent whichever one the writer set. |
| `subprojects_move_cascade_trg` (`m20260711_000007`) | `subprojects`, per row | AFTER UPDATE OF `project_id`, when it actually changed | Carries the subproject's sites to the new project. The denormalised `sites.project_id` would otherwise disagree with the subproject's owner. |

The samples triggers answer what a sample's statistics are and whether anything still references it.
They never create one. Whether a group of readings is a sample has one answer,
`readings::sample_groups::forms_sample`: a group is a sample when the writer declared a collection
event (every `POST /grab_samples` group, and a CSV import that declares `measurement_type: "spot"`)
or when it carries two or more spot readings on a paired slot. The CSV import and the pairing and
plan-apply backfills share one SQL materialiser in the same module, keyed on
`(site_id, parameter_id, time)`, which is the `samples` unique key, so the find-or-create and the
`sample_id` stamping cannot disagree about what a group is. The materialiser applies the caller's
row predicate to the stamping as well as to the grouping, so it cannot pull an unrelated stream's
reading into a sample because it sits on the same slot at the same instant. Every writer of
`samples`, `/grab_samples` included, inserts with `ON CONFLICT DO NOTHING` and then reads the row
back: that unique key is what two concurrent entries for one collection event collide on, and
yielding to whichever won costs a re-read, while testing for the row first costs a whole grab to a
unique violation.

### CrudCrate model hooks

Fired by the generated CRUD routes for the entity named, in the API process.

| Entity | Hooks | What they do | Why in the hook |
|--------|-------|--------------|-----------------|
| `sensor_calibrations` | `before_create`, `before_update`, `perform_update`, `after_create`, `after_update`, `perform_delete` | Before: reject a second curve opening at an instant already taken for that sensor and parameter, and reject an end date at or before the start. Update: the row's own UPDATE carries `valid_until_explicit` in the same statement, recording whether the end date is an operator's or the chain's; nothing is written before every validation has passed, so a rejected request leaves the row untouched. After: `recompute_valid_until` re-chains the windows, shortening an operator-set end date to the next curve's start rather than rewriting it, then enqueue a tracked `calibration_create\|update\|delete` job. Delete: move the curve's readings onto whichever of the sensor's remaining curves covers their time, recomputing each value through `recomposed_value_sql` (re-applying its standard curve) in the same guarded statement, then delete and re-chain; a reading no remaining window covers is left uncorrected, with no calibration and a null value. | The CRUD route has no other place to run before the insert, and the window chain has to be rebuilt for every path that writes a curve. `valid_until_explicit` sits outside both CRUD models, so it needs a write of its own; put it in `before_update` and a later rejection leaves the row permanently on the operator-window branch of `recompute_valid_until`, where `LEAST` ignores a NULL and the window can never reopen. A windowed calibration is deletable and its history reprocesses, which is deliberately unlike a standard curve: the uncorrected value is what ingest stores for a time outside every window and what a reprocess over the same windows recomputes, so the delete leaves the same state either path reaches. |
| `standard_curves` | `before_create`, `before_update`, `before_delete`, `before_delete_many` | Before create: reject a zero slope. Before update: refuse a change to slope, intercept, fit quality, name, instrument or attribution once a reading references the curve; only notes stay editable. Before delete: refuse the delete on the same condition. | The `readings.standard_curve_id` foreign key already refuses the delete, but reports a constraint violation the CRUD layer surfaces as an internal error, so the hook is what makes it a stated 400 while the constraint stays the backstop for raw SQL. Editing is refused rather than reprocessed because a standard curve is picked by hand for one measurement: there is no window to reprocess and no way to tell which readings the operator meant to change. |
| `sensor_deployments` | `before_create`, `before_update`, `after_create`, `after_update`, `perform_delete` | Before create: reject an inverted window, then check slot occupancy, then auto-recall the sensor's open deployments, in that order. Before update: the same, plus `follow_forward_move` so a predecessor's end date follows a start date corrected forward. After: `recompute_deployed_until`, then enqueue a tracked `deployment_create\|update\|delete` job. Delete: clear `readings.deployment_id` through the guarded bulk write. | Hooks do not share the write's transaction, so ordering every rejection ahead of every mutation is what makes a refused request side-effect-free. The `excl_deployment_site_param_slot` constraint stays the backstop. |
| `site_parameters` | `after_create`, `after_update`, `before_delete`, `before_delete_many`, `after_delete`, `after_delete_many` | After create: backfill `name` from the catalog when the client sent an empty one, and enqueue `derived_assignment` for a derived slot. Before delete: `retire_slot` unattributes the slot's readings and status events, deletes its orphaned samples, releases the streams pointing at it and enqueues the rollup rebuild. After update/delete: reconcile alarm events. | The name backfill and the derived enqueue need a database lookup, so they cannot be `on_create` expressions. The teardown must run before CrudCrate's delete or the stream foreign key refuses it. |
| `alarm_thresholds` | `after_create`, `after_update`, `after_delete`, `after_delete_many` | `reconcile_all_from_hook`: reconcile every active slot's alarm events immediately. | Evaluation reads thresholds live, so an edit changes the current breach set at once. Threshold edits are rare and the reconcile is O(active slots), so it does not wait for the backstop sweep. |
| `parameters` | `after_update`, `after_delete`, `after_delete_many` | The same global alarm reconcile. | Alarm evaluation falls back to `parameters.default_*` when no threshold row exists, so a catalog edit moves the breach set too. |
| `derived_parameter_definitions` | `before_create`, `before_update`, `after_create`, `after_update`, `before_delete` | Before: validate the formula. After: resolve the formula's variables, sync `derived_parameter_sources`, and ensure the output `parameters` row exists. Before delete: drop the sources and unlink `site_parameters`. | The sources table and the output parameter are derived from the formula text; deriving them anywhere but at the write would let the two disagree. |

Create-time column defaults (`site_parameters.is_active`, `is_public`, `sync_services.paused`)
are model `on_create` expressions, not hooks: they resolve at the CreateModel to ActiveModel
boundary, so an omitted field is already correct at insert and an explicit value is kept.

### In-process subscribers and loops

| Trigger | Fires on | When | Why here rather than in the caller |
|---------|----------|------|------------------------------------|
| Job scheduler (`reprocessing_jobs::scheduler::tick`) | A due `schedules` row, claimed `FOR UPDATE SKIP LOCKED` | Background loop | Enqueues each due Service as a tracked job. The row's grid is advanced off its scheduled time inside the claiming transaction, so cadence never drifts by a run's latency and a peer cannot re-pick the slot. Advancing is unconditional; the two policies then decide whether the slot actually enqueues. `catchup_policy = skip` declines a slot a full interval or more behind now, so a scheduler gap waits for the next scheduled slot instead of replaying its backlog, while `run_once` (the default) fires once to resync. `overlap_policy = skip_if_running` declines while a previous run is non-terminal. The decision lives here rather than in each Service because every Service would otherwise repeat it. |
| `cache::spawn_write_invalidator` | `AppEvent::DataIngested` carrying a site, on the broadcast bus the writers already use for SSE | Started from `AppState::new` when caching is on; a no-op otherwise, so cacheless test binaries spawn nothing | Drops that site's cached responses, private and public together. `/ingest`, the largest writer, called no cache function at all; subscribing to the announcement writers already make means a new writer is invalidated without its own cache call. A lagged receiver means writes were missed, so it drops the whole cache rather than guessing. `JobCompleted` is deliberately not a trigger: its row count carries whatever the job returned, which says nothing about the served bytes. |
| Alarm sweeper | Interval loop (`ALARM_SWEEP_INTERVAL_SECONDS`, default 60) plus the CRUD hooks above | Background | Reconciles persisted `alarm_events` against the current breach set: opens new breaches, auto-resolves ones that returned to range. It is the backstop the hooks short-circuit. |
| Derived janitor | Enqueued by the DB-backed scheduler on the `schedules` row's cadence | Background | Fills derived-value gaps and runs the periodic aggregate refresh. The full refresh fires on the tick opening each `JANITOR_FULL_REFRESH_SECONDS` period, decided from the slot and cadence the scheduler stamped into the job params rather than from the process's start-up interval, so an operator cadence change cannot silence it. A `run_now` carries no slot and falls back to the wall clock, so `POST /api/actions/refresh_aggregates {full:true}` remains the way to force one. A refresh failure here is logged and retried on the next tick, because the tick has no caller to answer to. |
| SSE job-project resolution (`/api/events`) | Each job frame, for a restricted principal only | Per request, once per job id per open connection | Resolves a job's project through `reprocessing_jobs` joined to `sites` and `sensor_deployments` so a frame is forwarded only in scope. The memo is capped at 1024 ids and cleared when full; a resolution error withholds the frame. An unrestricted principal does no database work on this path. |
| Notification mute gate | Every notification, inside `dispatcher::deliver` | Per delivered message | One indexed lookup suppresses a message for a muted slot. It sits in `deliver`, the single point every channel fan-out passes through, so stale-data and battery alerts are muted by construction rather than by each trigger remembering to ask. A message with no slot has nothing to mute against. |

The continuous-aggregate refresh is deliberately **not** any of these: `refresh_continuous_aggregate`
is a procedure with its own transaction control, so it cannot run inside a trigger or inside the
write's transaction. Every caller runs it post-commit through `common::aggregates::refresh`, and a
failure now reaches the caller instead of being logged and dropped.

One dependency worth stating: the readings SELECT serves grab points as
`COALESCE(smp.mean, calibrated_value, raw_value)`, so a grab's served value depends on the samples
refresh trigger having populated `samples`.

## Standard curves

A standard curve belongs to one instrument and is chosen by hand for a measurement. It has no time
columns, so no window query can resolve one, which is why it is its own table rather than a flag on
`sensor_calibrations`.

A reading records both references: `calibration_id` is the time-windowed base calibration,
`standard_curve_id` the curve the operator chose. With one column an identity base and an unrecorded
base were indistinguishable. `GET /api/sites/{id}/readings?include_curves=true` serves both per
point, in JSON, CSV and NDJSON.

`POST /grab_samples` takes `standard_curve_id` per reading, and `POST /readings/batch` accepts the
same field. One rule governs both, stated in `readings::batch::admit_standard_curves`: a reading may
name a curve only when it is that instrument's own spot measurement. An unknown id, another
instrument's curve, a reading naming no instrument, and a continuous or derived reading are all 400.
A submitted `calibrated_value` is not stored alongside a curve, because it can only be recomputed
from one, never checked against it. The server resolves the base calibration from the instrument's
windows at the grab time, applies base then standard curve, and stores the measured value, both
references and the result. That order lives in `calibrations::service::apply_curves`, the one
function every path corrects a value through.

Reprocessing never resolves a grab's curves again: a window resolution cannot recover a hand-picked
curve. `calibrations::service::window_resolved_rows` is the one predicate holding spot rows back, and
every reprocess statement is written through it.

It does recompute a grab's value from the curves the row already names, in `recompose_spot_readings`,
so editing a base calibration's coefficients moves the values it produced. The delete path recomposes
the same way after repointing a reading onto its new covering curve, through the shared
`recomposed_value_sql`, which also leaves a reading with neither curve at a null `calibrated_value`
rather than a copy of its raw value.

The database enforces existence: `readings.standard_curve_id` is a foreign key with no `ON DELETE`,
so a referenced curve cannot be deleted, by the API or by hand. The API enforces provenance: an
applied curve is immutable, and correcting one means creating a new curve and re-entering the
affected measurements against it. This is deliberately unlike a windowed calibration, where editing
coefficients is expected to reprocess the readings the window covers.

## Stream registration and the instrument behind a feed

`POST /api/streams/register` upserts a stream on `(source_system, source_key)` and accepts the
instrument that produces it as `sensor_id`. It is optional: a caller that does not know the
instrument omits it, and the instrument is resolved later from the metadata device serial when the
stream is imported or paired. Declaring it is what keeps pairing from minting a second, serial-less
instrument beside the real one: with no `sensor_id` and no device serial in the metadata, pairing
has nothing to match on and creates a fresh instrument for the feed.

A declared instrument is confined three ways. It has to exist. A caller confined to a project set
may only name an instrument already deployed into one of those projects: inventory deployed nowhere
belongs to no project, so nothing distinguishes another team's spare instrument from this caller's,
and attaching one makes every reading the feed writes resolve that instrument's calibration
windows. Wiring undeployed inventory to its first feed is therefore an unrestricted caller's
operation, ie. an administrator or an unscoped sync service; the route already refuses
project-scoped tokens outright. And the instrument must not contradict the device serial the
stream's own metadata reports. That serial check is a cross-check on feeds that describe their
device rather than the confinement itself: metadata arrives in the same request, so a caller can
always omit it, and the project rule is what a restricted caller is held to.

An instrument attaches to a feed that has none. Re-registering with the same instrument is a no-op,
and moving an established feed to a different instrument is refused with a 409: that reattributes
everything the feed has ever written, which is what `POST /api/actions/swap` and the `data_streams`
CRUD surface are for.
