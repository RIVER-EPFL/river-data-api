//! The tracked jobs an instrument's attribution and correction enqueue: re-derive one sensor's
//! readings, one `(site, parameter)` slot's, every slot's, or the sensors a calibration window
//! covers but never stamped.

use async_trait::async_trait;
use sea_orm::sea_query;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, QuerySelect};
use uuid::Uuid;

use crate::routes::private::readings::models as readings;
use crate::routes::private::reprocessing_jobs::flows::{
    SlotOutcome, build, optional_uuid, required_uuid, uuid_array,
};
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};
use crate::routes::private::sensor_calibrations::service::{
    reprocess_sensor_readings, reprocess_site_parameter_readings,
};
use crate::routes::private::sensor_deployments as deployments;

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
        use sea_orm::sea_query::ExprTrait;

        let sensor_id = required_uuid(ctx.params(), "sensor_id")?;
        ctx.info(&format!("Reprocessing readings for sensor {sensor_id}"))
            .await;
        let count = reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id())).await?;
        if let Ok(Some(row)) = ctx
            .db()
            .query_one_raw(build(
                &sea_query::Query::select()
                    .distinct()
                    .column(readings::Column::SiteId)
                    .from(readings::Entity)
                    .and_where(sea_query::Expr::col(readings::Column::SensorId).eq(sensor_id))
                    .and_where(sea_query::Expr::col(readings::Column::SiteId).is_not_null())
                    .limit(1)
                    .to_owned(),
            ))
            .await
        {
            if let Ok(site_id) = row.try_get::<Uuid>("", "site_id") {
                ctx.set_site(site_id).await;
            }
        }
        ctx.report(
            JobReport::new()
                .scope("sensor_id", sensor_id.to_string())
                .count("readings_updated", count),
        )
        .await;
        Ok(count as i64)
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
        let count =
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id, Some(ctx.job_id()))
                .await? as i64;
        if let Some(sensor_id) = optional_uuid(ctx.params(), "sensor_id") {
            reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id())).await?;
        }
        ctx.set_site(site_id).await;
        ctx.report(
            JobReport::new()
                .scope("site_id", site_id.to_string())
                .scope("parameter_id", parameter_id.to_string())
                .count("readings_updated", count),
        )
        .await;
        Ok(count)
    }
}

/// `reprocess_all` operator action. A failed slot logs and continues, a partial backdate is more
/// useful than aborting the whole batch on one bad slot.
pub struct ReprocessAll;

#[async_trait]
impl Job for ReprocessAll {
    fn name(&self) -> &'static str {
        "reprocess_all"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let slots: Vec<(Uuid, Uuid)> = deployments::models::Entity::find()
            .select_only()
            .column(deployments::models::Column::SiteId)
            .column(deployments::models::Column::ParameterId)
            .distinct()
            .into_tuple()
            .all(ctx.db())
            .await?;
        let slot_count = slots.len();
        ctx.info(&format!("Backdating {slot_count} slot(s)")).await;

        let mut results = Vec::with_capacity(slot_count);
        for (site_id, parameter_id) in slots {
            let moved = reprocess_site_parameter_readings(
                ctx.db(),
                site_id,
                parameter_id,
                Some(ctx.job_id()),
            )
            .await
            .map(|n| n as i64);
            results.push((
                serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
                moved,
            ));
        }
        let outcome = SlotOutcome::from(results);
        let total = outcome.readings;
        let report = outcome
            .record(
                &ctx,
                JobReport::new()
                    .count("slots", slot_count)
                    .count("readings_updated", total),
            )
            .await;
        ctx.report(report).await;
        if outcome.all_failed() {
            return Err(outcome.error());
        }
        tracing::info!(readings_updated = total, "reprocess_all complete");
        Ok(total)
    }
}

/// Re-derive `calibrated_value`/`calibration_id` for the sensors carrying readings a calibration
/// window covers but never stamped. The sensor set is carried in `params.sensors`. Backs the
/// `backfill_calibrations` operator action. A failed sensor logs and continues.
pub struct BackfillCalibrations;

#[async_trait]
impl Job for BackfillCalibrations {
    fn name(&self) -> &'static str {
        "backfill_calibrations"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let sensors = uuid_array(ctx.params(), "sensors");
        let mut results = Vec::with_capacity(sensors.len());
        for sensor_id in sensors {
            let moved = reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id()))
                .await
                .map(|n| n as i64);
            results.push((serde_json::json!({ "sensor_id": sensor_id }), moved));
        }
        let outcome = SlotOutcome::from(results);
        let total = outcome.readings;
        let report = outcome
            .record(&ctx, JobReport::new().count("readings_updated", total))
            .await;
        ctx.report(report).await;
        if outcome.all_failed() {
            return Err(outcome.error());
        }
        tracing::info!(readings_updated = total, "backfill_calibrations complete");
        Ok(total)
    }
}
