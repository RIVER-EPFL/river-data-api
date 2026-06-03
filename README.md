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
