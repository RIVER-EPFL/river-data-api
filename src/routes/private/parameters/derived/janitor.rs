use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use sea_orm::sea_query::{
    Alias, Expr, Func, JoinType, Order, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, ExprTrait, FromQueryResult,
    QueryFilter, QueryOrder, QuerySelect, QueryTrait, Statement,
};
use uuid::Uuid;

use crate::routes::private::readings;
use crate::routes::private::reprocessing_jobs::model as jobs;
use crate::routes::private::sites::parameters::models as site_parameters;

use super::{definition_model, source_model};

const MAX_GAPS_PER_RUN: usize = 50_000;

/// The statuses a prune leaves alone: a job still queued or running is not history yet.
const IN_FLIGHT: [&str; 4] = ["queued", "pending", "running", "retrying"];

/// Whether a site has any active derived `site_parameter`. The spawn-guard for the ingest/batch
/// derived-compute jobs: when false, a derived recompute at that site would do nothing, so the job
/// is skipped entirely (the dominant source of empty `ingest_derived` jobs). Anything genuinely
/// needed is still caught by the periodic janitor gap scan.
pub async fn site_has_active_derived(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> Result<bool, sea_orm::DbErr> {
    let row = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .filter(site_parameters::Column::EntryMode.eq("tool"))
        .filter(
            sea_orm::Condition::any()
                .add(site_parameters::Column::IsActive.eq(true))
                .add(site_parameters::Column::IsActive.is_null()),
        )
        .select_only()
        .column(site_parameters::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await?;
    Ok(row.is_some())
}

/// The gap scan, as the statement it emits.
///
/// `since` bounds the readings side by time. Unbounded, the anti-join hashes the whole hypertable:
/// on the production shape that is a parallel hash whose 16MB doubling step does not fit the DB
/// pod's 64MB `/dev/shm` when another parallel query holds shared memory, so the run aborts and no
/// gap is filled that hour. Bounded, it is an index-range probe over the last few hours; the
/// periodic unbounded run is what still covers drift older than the window.
fn gap_scan(since: Option<chrono::DateTime<chrono::Utc>>) -> SelectStatement {
    let (r, sp, d, dps, r2) = (
        Alias::new("r"),
        Alias::new("sp"),
        Alias::new("d"),
        Alias::new("dps"),
        Alias::new("r2"),
    );

    let derived_written = SeaQuery::select()
        .expr(Expr::val(1))
        .from_as(readings::Entity, r2.clone())
        .and_where(
            Expr::col((r2.clone(), readings::Column::SiteId))
                .equals((r.clone(), readings::Column::SiteId)),
        )
        .and_where(
            Expr::col((r2.clone(), readings::Column::ParameterId))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .and_where(
            Expr::col((r2, readings::Column::Time)).equals((r.clone(), readings::Column::Time)),
        )
        .take();

    let mut query = SeaQuery::select();
    query
        .distinct()
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::Join,
            site_parameters::Entity,
            sp.clone(),
            Expr::col((sp.clone(), site_parameters::Column::SiteId))
                .equals((r.clone(), readings::Column::SiteId))
                .and(Expr::col((sp.clone(), site_parameters::Column::EntryMode)).eq("tool"))
                .and(
                    Expr::expr(Func::coalesce([
                        Expr::col((sp.clone(), site_parameters::Column::IsActive)),
                        Expr::val(true),
                    ]))
                    .eq(true),
                ),
        )
        .join_as(
            JoinType::Join,
            definition_model::Entity,
            d.clone(),
            Expr::col((d.clone(), definition_model::Column::OutputParameterId))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .join_as(
            JoinType::Join,
            source_model::Entity,
            dps.clone(),
            Expr::col((dps.clone(), source_model::Column::DerivedDefinitionId))
                .equals((d, definition_model::Column::Id))
                .and(
                    Expr::col((dps, source_model::Column::ParameterId))
                        .equals((r.clone(), readings::Column::ParameterId)),
                ),
        )
        .and_where(Expr::exists(derived_written).not())
        .order_by((r.clone(), readings::Column::SiteId), Order::Asc)
        .order_by((r.clone(), readings::Column::Time), Order::Asc)
        .limit(MAX_GAPS_PER_RUN as u64);
    if let Some(from) = since {
        query.and_where(
            Expr::col((r, readings::Column::Time))
                .gte(sea_orm::prelude::DateTimeWithTimeZone::from(from)),
        );
    }
    query
}

/// Find (site_id, time) pairs where a source reading exists but no corresponding
/// derived reading was ever written. Recompute each.
///
/// Catches gaps from crashes during ingest, sync cycles missed while the API
/// was down, derived parameters assigned after historical source data already
/// existed, and any other inconsistency between source and derived readings.
///
/// After filling any gaps, refreshes continuous aggregates back to the earliest filled timestamp so
/// the hourly/daily/weekly/monthly rollups reflect the newly written derived values.
///
/// Reports progress into the caller's job rather than opening one of its own: the janitor always
/// runs as a step of the worker-pool `janitor_service` job, and a second row opened from inside that
/// job would carry no lease, so nothing could ever reclaim it.
/// One instant of a derived slot the sweep found stale.
#[derive(FromQueryResult)]
struct StaleSlot {
    site_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
}

pub async fn run_once(
    db: &DatabaseConnection,
    ctx: Option<&JobContext>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<usize, sea_orm::DbErr> {
    let started = std::time::Instant::now();

    let (sql, values) = gap_scan(since).build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
    if let Some(ctx) = ctx {
        ctx.set_progress(0, Some(total)).await;
    }
    if rows.is_empty() {
        tracing::info!("Derived janitor: no gaps found");
        return Ok(0);
    }
    tracing::info!(gaps = total, "Derived janitor: filling gaps");

    let mut filled: i64 = 0;
    let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
    for (i, row) in rows.iter().enumerate() {
        if ctx.is_some_and(JobContext::is_cancelled) {
            break;
        }
        let slot = StaleSlot::from_query_result(row, "")?;
        let site_id = slot.site_id;
        let utc_time = slot.time.with_timezone(&chrono::Utc);
        match crate::routes::private::sensors::calibrations::service::recalculate_derived_at_timestamp(
            db, site_id, utc_time,
        )
        .await
        {
            Ok(()) => {
                filled += 1;
                min_filled = Some(min_filled.map_or(utc_time, |m| Ord::min(m, utc_time)));
            }
            Err(e) => tracing::warn!(error = %e, site_id = %site_id, time = %utc_time, "Janitor failed to fill derived gap"),
        }
        if (i + 1) % 1000 == 0
            && let Some(ctx) = ctx
        {
            ctx.set_progress(i as i32 + 1, Some(total)).await;
        }
    }

    if let Some(since) = min_filled {
        tracing::info!(%since, filled, "Derived janitor: refreshing continuous aggregates after backfill");
        crate::common::sync_state::refresh_continuous_aggregates(db, Some(since))
            .await
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    }
    if let Some(ctx) = ctx {
        ctx.set_progress(total, Some(total)).await;
        ctx.report(
            JobReport::new()
                .scope_opt("earliest_filled", min_filled.map(|t| t.to_rfc3339()))
                .scope("capped_at_limit", total as usize >= MAX_GAPS_PER_RUN)
                .count("gaps_found", total)
                .count("filled", filled),
        )
        .await;
        ctx.info(&format!("Filled {filled} of {total} derived gaps"))
            .await;
    }
    tracing::info!(
        filled,
        total,
        capped_at_limit = total as usize >= MAX_GAPS_PER_RUN,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Derived janitor: gap fill complete"
    );
    Ok(usize::try_from(filled).unwrap_or(0))
}

/// Tiered retention for `reprocessing_jobs` (logs cascade-delete with their job). Three layers:
///   1. `maintenance` rows (high-volume janitor/ingest/refresh/alarm-backfill) age out fast.
///   2. `operator`/`metadata` rows (audit value) age out slowly.
///   3. A hard count cap on `maintenance` rows so an ingestion burst can't blow storage between the
///      daily prunes. Each window/cap of 0 disables that layer.
///
/// Returns the total rows deleted (across all layers).
pub async fn prune_tracked_jobs(
    db: &DatabaseConnection,
    maintenance_days: u32,
    operator_days: u32,
    maintenance_max_rows: u64,
) -> u64 {
    let mut deleted = 0u64;

    if maintenance_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(maintenance_days));
        deleted += run_delete(
            jobs::Entity::delete_many()
                .filter(jobs::Column::Category.eq("maintenance"))
                .filter(jobs::Column::CreatedAt.lt(cutoff))
                .filter(jobs::Column::Status.is_not_in(IN_FLIGHT)),
            db,
            "maintenance age",
        )
        .await;
    }
    if operator_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(operator_days));
        deleted += run_delete(
            jobs::Entity::delete_many()
                .filter(jobs::Column::Category.is_in(["operator", "metadata"]))
                .filter(jobs::Column::CreatedAt.lt(cutoff))
                .filter(jobs::Column::Status.is_not_in(IN_FLIGHT)),
            db,
            "operator/metadata age",
        )
        .await;
    }
    if maintenance_max_rows > 0 {
        // Keep the most-recent N maintenance rows; delete the older overflow.
        let overflow = jobs::Entity::find()
            .select_only()
            .column(jobs::Column::Id)
            .filter(jobs::Column::Category.eq("maintenance"))
            .filter(jobs::Column::Status.is_not_in(IN_FLIGHT))
            .order_by_desc(jobs::Column::CreatedAt)
            .offset(maintenance_max_rows)
            .into_query();
        deleted += run_delete(
            jobs::Entity::delete_many().filter(jobs::Column::Id.in_subquery(overflow)),
            db,
            "maintenance count cap",
        )
        .await;
    }

    if deleted > 0 {
        tracing::info!(deleted, "Tracked-job retention: pruned old job rows");
    }
    deleted
}

async fn run_delete(
    delete: sea_orm::DeleteMany<jobs::Entity>,
    db: &DatabaseConnection,
    label: &str,
) -> u64 {
    match delete.exec(db).await {
        Ok(res) => res.rows_affected,
        Err(e) => {
            tracing::warn!(error = %e, label, "Tracked-job retention: prune layer failed");
            0
        }
    }
}

#[cfg(test)]
#[path = "tests/janitor.rs"]
mod tests;
