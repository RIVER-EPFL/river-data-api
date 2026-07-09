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
