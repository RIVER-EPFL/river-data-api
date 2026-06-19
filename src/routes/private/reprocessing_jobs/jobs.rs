//! Concrete `Job` implementations — the worker-run handler for each `trigger_type`. Each reads its
//! inputs from `ctx.params()` and calls the same service function the inline trigger used.

use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, Statement};
use uuid::Uuid;

use super::job::Job;
use super::lifecycle::JobContext;
use crate::common::sync_state;
use crate::routes::private::sensor_calibrations::services::{
    reprocess_sensor_readings, reprocess_site_parameter_readings,
};

fn required_uuid(params: &serde_json::Value, key: &str) -> Result<Uuid, DbErr> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| DbErr::Custom(format!("job params missing uuid {key}")))
}

fn optional_uuid(params: &serde_json::Value, key: &str) -> Option<Uuid> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Re-derive FK columns and `calibrated_value` for one sensor's readings. Backs the sensor-scoped
/// reprocess triggers (manual reprocess, calibration changes).
pub struct ReprocessSensor {
    name: &'static str,
}

impl ReprocessSensor {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Job for ReprocessSensor {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let sensor_id = required_uuid(ctx.params(), "sensor_id")?;
        ctx.info(&format!("Reprocessing readings for sensor {sensor_id}")).await;
        let count = reprocess_sensor_readings(ctx.db(), sensor_id).await?;
        if let Ok(Some(row)) = ctx
            .db()
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT DISTINCT site_id FROM readings WHERE sensor_id = $1 AND site_id IS NOT NULL LIMIT 1",
                [sensor_id.into()],
            ))
            .await
        {
            if let Ok(site_id) = row.try_get::<Uuid>("", "site_id") {
                ctx.set_site(site_id).await;
            }
        }
        ctx.set_detail(serde_json::json!({
            "scope": { "sensor_id": sensor_id },
            "counts": { "readings_updated": count },
        }))
        .await;
        Ok(count as i64)
    }
}

/// Refresh continuous aggregates — incremental (recent window) or full. Single bounded statement.
pub struct RefreshAggregates {
    name: &'static str,
    full: bool,
}

impl RefreshAggregates {
    #[must_use]
    pub fn incremental() -> Self {
        Self { name: "refresh_aggregates", full: false }
    }

    #[must_use]
    pub fn full() -> Self {
        Self { name: "refresh_aggregates_full", full: true }
    }
}

#[async_trait]
impl Job for RefreshAggregates {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let outcome = tokio::time::timeout(Duration::from_secs(600), async {
            if self.full {
                sync_state::refresh_continuous_aggregates_full(ctx.db()).await;
            } else {
                sync_state::refresh_continuous_aggregates(ctx.db(), None).await;
            }
        })
        .await;
        outcome
            .map(|()| 0)
            .map_err(|_| DbErr::Custom("Aggregate refresh timed out after 10 minutes".into()))
    }
}

/// Re-derive readings for one (site, parameter) slot, and the sensor too when `sensor_id` is given.
/// Backs slot-scoped triggers (stream pairing, sensor swap, adopt).
pub struct ReprocessSlot {
    name: &'static str,
}

impl ReprocessSlot {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Job for ReprocessSlot {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let site_id = required_uuid(ctx.params(), "site_id")?;
        let parameter_id = required_uuid(ctx.params(), "parameter_id")?;
        let count = reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64;
        if let Some(sensor_id) = optional_uuid(ctx.params(), "sensor_id") {
            reprocess_sensor_readings(ctx.db(), sensor_id).await?;
        }
        ctx.set_site(site_id).await;
        ctx.set_detail(serde_json::json!({
            "scope": { "site_id": site_id, "parameter_id": parameter_id },
            "counts": { "readings_updated": count },
        }))
        .await;
        Ok(count)
    }
}

/// Re-derive readings for a sensor's deployment slot. Derives the slot parameter from the sensor,
/// then re-derives the (site, parameter) slot and the sensor. Backs the deployment-change triggers.
pub struct ReprocessDeployment {
    name: &'static str,
}

impl ReprocessDeployment {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Job for ReprocessDeployment {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let sensor_id = required_uuid(ctx.params(), "sensor_id")?;
        let site_id = required_uuid(ctx.params(), "site_id")?;
        let parameter_id: Option<Uuid> = ctx
            .db()
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT parameter_id FROM sensors WHERE id = $1",
                [sensor_id.into()],
            ))
            .await?
            .and_then(|r| r.try_get::<Uuid>("", "parameter_id").ok());
        let count = if let Some(parameter_id) = parameter_id {
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64
        } else {
            0
        };
        reprocess_sensor_readings(ctx.db(), sensor_id).await?;
        ctx.set_site(site_id).await;
        ctx.set_detail(serde_json::json!({
            "scope": { "sensor_id": sensor_id, "site_id": site_id },
            "counts": { "readings_updated": count },
        }))
        .await;
        Ok(count)
    }
}
