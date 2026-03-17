# river-data-api

> **Note:** Not to be mistaken for the river-api project, related to the astrocast project, which will be migrated to this.

Time-series API for RIVER sensor data, storing readings from multiple data collection systems in PostgreSQL/TimescaleDB.

## Quick Start

```bash
cp .env.example .env
docker compose up -d
```

API: `http://localhost:3005` | Docs: `http://localhost:3005/docs`

## Architecture

### Data Flow

```
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│   Vaisala    │  │  Campbell    │  │  CSV Import  │
│  Sync Svc   │  │  Sync Svc    │  │  (future)    │
└──────┬───────┘  └──────┬───────┘  └──────┬───────┘
       │                 │                 │
       └─────────┬───────┘─────────────────┘
                 ▼
       ┌─────────────────┐
       │  /api/service/   │  ◄── API token or Keycloak auth
       │  readings/batch  │
       │  source_mappings │
       │  sync/enroll     │
       └────────┬────────┘
                ▼
       ┌─────────────────┐
       │  TimescaleDB     │  ◄── Hypertables, continuous aggregates
       │  readings        │      compression after 30 days
       │  status_events   │
       └────────┬────────┘
                ▼
       ┌─────────────────┐     ┌─────────────────┐
       │  /api/admin/     │     │  /api/public/    │
       │  Keycloak auth   │     │  No auth         │
       │  CrudCrate CRUD  │     │  Read-only       │
       └─────────────────┘     └─────────────────┘
```

### Multi-Source Sync

Multiple sync services can feed data into the same logical measurements. Each service:

1. **Enrolls** with the control plane (`POST /api/service/sync/enroll`) using provisioned credentials
2. **Discovers** sensors and registers them as site_parameters with source mappings
3. **Pushes readings** in batches via `/api/service/readings/batch`
4. **Reports health** via periodic heartbeats

External identifiers are resolved to internal UUIDs at ingestion time through **source mappings** (`source_system`, `entity_type`, `source_key` → `entity_id`). This keeps the readings table clean of source-specific columns.

When a sensor moves between systems, an admin merges the two site_parameters via `POST /api/admin/actions/merge_site_parameters`, unifying the timeline and updating source mappings so future syncs route to the correct entity.

See [docs/sync-service-onboarding.md](docs/sync-service-onboarding.md) for the full onboarding guide.

### API Tiers

| Tier | Auth | Purpose |
|------|------|---------|
| `/api/admin/` | Keycloak JWT (Administrator role) | Entity CRUD, sync control, merge operations |
| `/api/service/` | API token or Keycloak JWT | Programmatic read/write (sync services, scripts) |
| `/api/public/` | None | Read-only data access for external partners |

### Data Model

```
Projects
  └── Sites (physical locations with coordinates)
       └── SiteParameters (site-specific measurement config)
            ├── Readings (hypertable, 10-min intervals, 7-day chunks)
            └── StatusEvents (hypertable, non-numeric, 30-day chunks)

Parameters (global catalog: measurement + device_health types)
  └── Sensors (physical instruments by serial number)
       ├── SensorCalibrations (slope/intercept, time-versioned)
       └── SensorDeployments (sensor-to-site history)

SourceMappings (source_system + entity_type + source_key → UUID)
SyncServices / SyncEvents (control plane + audit trail)
```

### Key Design Decisions

- **UUID-only fact tables**: Readings reference UUIDs, never external IDs. Source resolution happens once at ingestion.
- **Provenance without per-row cost**: Source system tracked via source_mappings and sensor_deployments, not on each reading row. Avoids bloating compressed hypertable chunks.
- **First-write-wins deduplication**: Readings PK is `(site_id, parameter_id, time, replicate_index)`. Overlap during source migration handled by ON CONFLICT DO NOTHING.
- **Forward-only migrations**: Database can be re-created from source systems. Migrations are additive, never destructive.

## Stack

- **Framework**: Axum 0.8
- **ORM**: SeaORM 1.1
- **Database**: TimescaleDB 2.23 / PostgreSQL 18
- **Rust**: 1.93, Edition 2024
- **Docs**: utoipa + Scalar UI
- **Auth**: Keycloak (admin), API tokens (service/public)
- **CRUD Generation**: CrudCrate

## Development

```bash
# Full environment with hot-reload
docker compose up -d

# With pgAdmin
docker compose --profile tools up -d

# Local development (requires running TimescaleDB)
cargo run

# Tests (requires DATABASE_URL)
cargo test

# Lint
cargo clippy --workspace
```

### Access Points

| Service | URL |
|---------|-----|
| API | http://localhost:3005 |
| API Docs | http://localhost:3005/docs |
| Traefik | http://localhost:8088 |
| PostgreSQL | localhost:5443 |
| pgAdmin | http://localhost:5050 |

## Documentation

- [Sync Service Onboarding Guide](docs/sync-service-onboarding.md) — How to add a new data source and manage sensor migrations
