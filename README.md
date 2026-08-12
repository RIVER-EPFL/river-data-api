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
| `trg_readings_sample_refresh_ins` / `_del` / `_upd` (`m20260420_000001_samples`) | `readings`, per row | AFTER INSERT / DELETE when `sample_id` is set; AFTER UPDATE when `sample_id`, `raw_value`, `calibrated_value` or `is_flagged` changes | Recomputes `samples.mean/stdev/n/min_value/max_value` over the non-flagged replicates of that `sample_id`. Five write paths plus several raw-SQL statements mutate `readings`, so the statistic holds for all of them only if the database maintains it. An UPDATE that moves the link refreshes both the old and the new sample. |
| `trg_inherit_calibration_parameter_id` (`m20260711_000004`, narrowed to windowed curves by `m20260711_000006`) | `sensor_calibrations`, per row | BEFORE INSERT | Fills a windowed curve's `parameter_id` from the sensor's first parameter-bearing curve when the request omits one, so a multi-channel instrument's curves land on a channel without every client knowing the rule. |
| `trg_set_deployment_parameter_id` (`m20260603_000006`, redefined by `m20260711_000003`) | `sensor_deployments`, per row | BEFORE INSERT OR UPDATE OF `sensor_id` | Maintains the read-only `parameter_id` twin on a deployment. It is derived state, so it is computed where the row is written. |
| `projects_default_subproject_trg` (`m20260710_000001`) | `projects`, per row | AFTER INSERT | Gives every project a default subproject, so `sites.subproject_id` can be NOT NULL without each project-creation path remembering to make one. |
| `sites_ensure_subproject_trg` (`m20260710_000001`) | `sites`, per row | BEFORE INSERT OR UPDATE | Keeps `sites.subproject_id` and the denormalised `sites.project_id` consistent whichever one the writer set. |
| `subprojects_move_cascade_trg` (`m20260711_000007`) | `subprojects`, per row | AFTER UPDATE OF `project_id`, when it actually changed | Carries the subproject's sites to the new project. The denormalised `sites.project_id` would otherwise disagree with the subproject's owner. |

### CrudCrate model hooks

Fired by the generated CRUD routes for the entity named, in the API process.

| Entity | Hooks | What they do | Why in the hook |
|--------|-------|--------------|-----------------|
| `sensor_calibrations` | `before_create`, `before_update`, `after_create`, `after_update`, `perform_delete` | Before: reject a second windowed curve opening at an instant already taken for that sensor and parameter (instant/lab curves exempt). After: `recompute_valid_until` re-chains the windows, then enqueue a tracked `calibration_create\|update\|delete` job. Delete: clear `readings.calibration_id` through the guarded bulk write. | The CRUD route has no other place to run before the insert, and the window chain has to be rebuilt for every path that writes a curve. |
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
| `cache::spawn_write_invalidator` | `AppEvent::DataIngested` carrying a site, on the broadcast bus the writers already use for SSE | Started from `AppState::new` when caching is on; a no-op otherwise, so cacheless test binaries spawn nothing | Drops that site's cached responses, private and public together. `/ingest`, the largest writer, called no cache function at all; subscribing to the announcement writers already make means a new writer is invalidated without its own cache call. A lagged receiver means writes were missed, so it drops the whole cache rather than guessing. `JobCompleted` is deliberately not a trigger: its row count carries whatever the job returned, which says nothing about the served bytes. |
| Alarm sweeper | Interval loop (`ALARM_SWEEP_INTERVAL_SECONDS`, default 60) plus the CRUD hooks above | Background | Reconciles persisted `alarm_events` against the current breach set: opens new breaches, auto-resolves ones that returned to range. It is the backstop the hooks short-circuit. |
| Derived janitor | Interval loop | Background | Fills derived-value gaps and runs the periodic aggregate refresh. A refresh failure here is logged and retried on the next tick, because the tick has no caller to answer to. |
| SSE job-project resolution (`/api/events`) | Each job frame, for a restricted principal only | Per request, once per job id per open connection | Resolves a job's project through `reprocessing_jobs` joined to `sites` and `sensor_deployments` so a frame is forwarded only in scope. The memo is capped at 1024 ids and cleared when full; a resolution error withholds the frame. An unrestricted principal does no database work on this path. |
| Notification mute gate | Every notification, inside `dispatcher::deliver` | Per delivered message | One indexed lookup suppresses a message for a muted slot. It sits in `deliver`, the single point every channel fan-out passes through, so stale-data and battery alerts are muted by construction rather than by each trigger remembering to ask. A message with no slot has nothing to mute against. |

The continuous-aggregate refresh is deliberately **not** any of these: `refresh_continuous_aggregate`
is a procedure with its own transaction control, so it cannot run inside a trigger or inside the
write's transaction. Every caller runs it post-commit through `common::aggregates::refresh`, and a
failure now reaches the caller instead of being logged and dropped.

One dependency worth stating: the readings SELECT serves grab points as
`COALESCE(smp.mean, calibrated_value, raw_value)`, so a grab's served value depends on the samples
refresh trigger having populated `samples`.
