//! Collection event queries: the CRUD guard, the attach helper every spot write path lands
//! through, the visit lists and their grid, and the recompute state a visit is listed with.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait, IntoTableRef, JoinType, OnConflict, Order, PostgresQueryBuilder,
    Query as SeaQuery, SelectStatement, SimpleExpr,
};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Statement, TransactionTrait,
};
use uuid::Uuid;

use super::models::{
    CollectionEvent, ExpectedParameter, SiteVisitCount, StagedEvent, VisitCell, VisitCellCurve,
    VisitListRow, VisitReplicate, VisitRow,
};
use crate::common::bulk_write;
use crate::common::paging::Window;
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::sites::models as sites;
use crate::routes::private::standard_curves::models as standard_curves;
use crate::routes::private::sync::hold_model as holds;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;

pub(super) const MAX_PAGE_SIZE: u64 = 200;

pub struct CollectionEventOperations;

/// How many readings the event holds. The FK is `ON DELETE SET NULL`, so a delete would leave
/// them attached to no visit with no route to re-attach them.
async fn attached_readings<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<i64, ApiError> {
    let n = readings::Entity::find()
        .filter(readings::Column::CollectionEventId.eq(id))
        .count(db)
        .await
        .map_err(ApiError::database)?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

impl CRUDOperations for CollectionEventOperations {
    type Resource = CollectionEvent;

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<CollectionEvent as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        use super::models::{Column, Entity};

        crate::common::actor::refuse_reattribution(data.created_by.is_some())?;

        let current = Entity::find_by_id(id)
            .lock_exclusive()
            .one(db)
            .await
            .map_err(ApiError::database)?
            .ok_or_else(|| ApiError::not_found("collection event", Some(id.to_string())))?;
        let site_id = data.site_id.flatten().unwrap_or(current.site_id);
        let collected_at = data.collected_at.flatten().unwrap_or(current.collected_at);
        if site_id == current.site_id && collected_at == current.collected_at {
            return Ok(());
        }
        if attached_readings(db, id).await? > 0 {
            return Err(ApiError::conflict(
                "A visit holding readings cannot change site or time.",
            ));
        }
        if let Some(existing) = Entity::find()
            .filter(Column::SiteId.eq(site_id))
            .filter(Column::CollectedAt.eq(collected_at))
            .filter(Column::Id.ne(id))
            .one(db)
            .await
            .map_err(ApiError::database)?
        {
            return Err(ApiError::conflict(format!(
                "Visit {} already occupies this site and time.",
                existing.id
            )));
        }
        Ok(())
    }

    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        let n = attached_readings(db, id).await?;
        if n > 0 {
            return Err(ApiError::conflict(format!(
                "Collection event {id} holds {n} reading{} and cannot be deleted: the readings \
                 would be detached from every visit.",
                if n == 1 { "" } else { "s" }
            )));
        }
        Ok(())
    }
}

/// The `source` a visit the sync created carries.
pub const PORTAL_SYNC: &str = "portal_sync";

/// Whether a calculation may run at a visit with this source (Q41): a visit the sync created is
/// the portal's to recompute for as long as the two run side by side, and a correction there
/// belongs in the portal.
///
/// Every door into the chain asks this one function: the enqueue a write goes through, the SELECT
/// a scoped apply walks, and the per-visit route. A door that decides for itself is how the
/// boundary came to hold on two of the three.
#[must_use]
pub fn chain_may_run(source: &str) -> bool {
    source != PORTAL_SYNC
}

/// How the event came to exist, decided by the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Manual,
    PortalSync,
    /// Derive per group from the streams feeding it: a stream this system did not create itself
    /// was registered by a sync service. Used by backfills that span sources.
    ByStreamOrigin,
}

/// `bool_or` SQL deciding whether any stream feeding a group was registered by a sync service:
/// a stream this system did not create itself. Shared with the sample backfill, which uses the
/// same distinction to decide whether a group's readings arrived as declared collections.
pub fn any_sync_origin_sql() -> String {
    "bool_or(ds.source_system NOT IN ('api', 'grab_sample', 'csv', 'csv_import'))".to_string()
}

impl EventSource {
    fn sql(self) -> String {
        match self {
            Self::Manual => "'manual'".to_string(),
            Self::PortalSync => format!("'{PORTAL_SYNC}'"),
            Self::ByStreamOrigin => format!(
                "CASE WHEN {} THEN '{PORTAL_SYNC}' ELSE 'manual' END",
                any_sync_origin_sql()
            ),
        }
    }
}

/// The aliases the attach statements and the predicates they are given share.
fn r() -> Alias {
    Alias::new("r")
}

fn ds() -> Alias {
    Alias::new("ds")
}

fn ce() -> Alias {
    Alias::new("ce")
}

/// Find-or-create the `collection_events` rows for the attributed spot readings a predicate
/// selects, then stamp `collection_event_id` onto them.
///
/// `row_predicate` is SQL over the aliases `r` (`readings`) and `ds` (`data_streams`) with the
/// given binds, exactly as `sample_groups::materialise_samples` takes it. The stamping UPDATE can
/// reach compressed chunks, so callers run inside a `bulk_write::guarded` transaction when the
/// window can be historical.
pub async fn attach_collection_events<C: ConnectionTrait>(
    conn: &C,
    selected: Condition,
    source: EventSource,
) -> AppResult<()> {
    let attributed_spot = |selected: Condition| {
        Condition::all()
            .add(selected)
            .add(Expr::col((r(), readings::Column::CollectionEventId)).is_null())
            .add(Expr::col((r(), readings::Column::SiteId)).is_not_null())
            .add(ExprTrait::eq(
                Expr::col((r(), readings::Column::MeasurementType)),
                "spot",
            ))
    };

    let mut rows = SeaQuery::select();
    rows.column((r(), readings::Column::SiteId))
        .column((r(), readings::Column::Time))
        .expr(Expr::cust(source.sql()))
        .from_as(readings::Entity, r())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds(),
            Expr::col((r(), readings::Column::StreamId)).equals((ds(), data_streams::Column::Id)),
        )
        .cond_where(attributed_spot(selected.clone()))
        .add_group_by([
            Expr::col((r(), readings::Column::SiteId)),
            Expr::col((r(), readings::Column::Time)),
        ]);
    let mut insert = SeaQuery::insert();
    insert
        .into_table(super::Entity)
        .columns([
            super::Column::SiteId,
            super::Column::CollectedAt,
            super::Column::Source,
        ])
        .on_conflict(
            OnConflict::columns([super::Column::SiteId, super::Column::CollectedAt])
                .do_nothing()
                .to_owned(),
        );
    insert
        .select_from(rows)
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let (sql, values) = insert.build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    let mut stamp = SeaQuery::update();
    stamp
        .table(IntoTableRef::into_table_ref(readings::Entity).alias(r()))
        .value(
            readings::Column::CollectionEventId,
            Expr::col((ce(), super::Column::Id)),
        )
        .from(IntoTableRef::into_table_ref(data_streams::Entity).alias(ds()))
        .from(IntoTableRef::into_table_ref(super::Entity).alias(ce()))
        .cond_where(
            attributed_spot(selected)
                .add(
                    Expr::col((r(), readings::Column::StreamId))
                        .equals((ds(), data_streams::Column::Id)),
                )
                .add(
                    Expr::col((ce(), super::Column::SiteId))
                        .equals((r(), readings::Column::SiteId)),
                )
                .add(
                    Expr::col((ce(), super::Column::CollectedAt))
                        .equals((r(), readings::Column::Time)),
                ),
        );
    let (sql, values) = stamp.build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    Ok(())
}

/// The per-visit counts, computed the same way on the site grid and the cross-site list. A
/// hold is keyed on the slot (an event-audit finding) or on the stream that raised it, so the
/// stream's pairing resolves the site.
/// Find or create the visit at `(site_id, collected_at)`: the portal's New Entry, made
/// idempotent. A visit already standing is returned as it is, `created` false, so two tools
/// entering the same visit land on one row instead of racing the unique key.
///
/// `unverified` is the state a new visit lands in, from the stager's level (Q177). A visit already
/// standing keeps the state it has: staging into a verified visit does not reopen it, and staging
/// into a pending one does not rule on it.
pub async fn stage_visit<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    collected_at: DateTime<Utc>,
    actor: &str,
    notes: Option<&str>,
    unverified: bool,
) -> AppResult<StagedEvent> {
    let row = conn
        .query_one(&stage_visit_statement(
            site_id,
            collected_at,
            actor,
            notes,
            unverified,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("Staging returned no visit".to_string()))?;
    Ok(StagedEvent {
        id: row.try_get("", "id")?,
        site_id: row.try_get("", "site_id")?,
        collected_at: row.try_get("", "collected_at")?,
        source: row.try_get("", "source")?,
        created_by: row.try_get("", "created_by")?,
        notes: row.try_get("", "notes")?,
        unverified: row.try_get("", "unverified")?,
        created: row.try_get_by_index(STAGED_CREATED_INDEX)?,
    })
}

/// Where `stage_visit_statement` returns whether it inserted; the column carries no name.
const STAGED_CREATED_INDEX: usize = 7;

/// The insert behind `stage_visit`. `DO UPDATE` rather than `DO NOTHING`: the insert then waits on
/// the transaction it conflicts with and returns the row that won, where a second statement would
/// still read the snapshot taken before that transaction committed. `xmax = 0` holds only of a
/// tuple this statement inserted.
fn stage_visit_statement(
    site_id: Uuid,
    collected_at: DateTime<Utc>,
    actor: &str,
    notes: Option<&str>,
    unverified: bool,
) -> sea_orm::sea_query::InsertStatement {
    use super::Column;
    SeaQuery::insert()
        .into_table(super::Entity)
        .columns([
            Column::SiteId,
            Column::CollectedAt,
            Column::Source,
            Column::CreatedBy,
            Column::Notes,
            Column::Unverified,
        ])
        .values_panic([
            site_id.into(),
            collected_at.into(),
            "manual".into(),
            actor.into(),
            notes.map(str::to_string).into(),
            unverified.into(),
        ])
        .on_conflict(
            OnConflict::columns([Column::SiteId, Column::CollectedAt])
                .update_column(Column::SiteId)
                .to_owned(),
        )
        .returning(SeaQuery::returning().exprs([
            Expr::col(Column::Id),
            Expr::col(Column::SiteId),
            Expr::col(Column::CollectedAt),
            Expr::col(Column::Source),
            Expr::col(Column::CreatedBy),
            Expr::col(Column::Notes),
            Expr::col(Column::Unverified),
            Expr::col(Alias::new("xmax")).eq(0),
        ]))
        .to_owned()
}

/// One review-queue row per field day an intern opened, so a manager rules on the visit beside
/// every other finding. Keyed on the visit: a second staging at the same site and instant
/// refreshes the open hold rather than filing a second one.
pub async fn open_unverified_visit_hold<C: ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    collected_at: DateTime<Utc>,
    actor: &str,
) -> AppResult<()> {
    use crate::routes::private::sync::service as audit;
    audit::upsert_hold(
        conn,
        &audit::Hold {
            key: audit::HoldKey::Visit {
                site_id,
                group_time: collected_at,
            },
            kind: HoldKind::UnverifiedVisit,
            expected: serde_json::json!({ "state": "verified" }),
            computed: serde_json::json!({ "state": "unverified", "opened_by": actor }),
            delta: serde_json::json!({}),
            status: HoldStatus::Pending,
            tool: None,
        },
    )
    .await
}

/// The ids among `site_ids` that name no site, in the order given.
pub async fn missing_sites<C: ConnectionTrait>(
    conn: &C,
    site_ids: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    use crate::routes::private::sites::{Column, Entity};
    let found: Vec<Uuid> = Entity::find()
        .filter(Column::Id.is_in(site_ids.iter().copied()))
        .all(conn)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();
    Ok(site_ids
        .iter()
        .filter(|id| !found.contains(id))
        .copied()
        .collect())
}

pub(super) fn visit_count_columns() -> String {
    let filled = SeaQuery::select()
        .expr(Expr::col((r(), readings::Column::ParameterId)).count_distinct())
        .from_as(readings::Entity, r())
        .and_where(
            Expr::col((r(), readings::Column::CollectionEventId)).equals((ce(), super::Column::Id)),
        )
        .and_where(Expr::col((r(), readings::Column::WithdrawnAt)).is_null())
        .and_where(Expr::cust("r.is_flagged IS NOT TRUE"))
        .and_where(Expr::col((r(), readings::Column::ParameterId)).is_not_null())
        .to_owned()
        .to_string(PostgresQueryBuilder);
    let findings_open = holds::with_stream_slot()
        .expr(Expr::val(1).count())
        .and_where(
            Expr::col((holds::h(), holds::Column::GroupTime))
                .equals((ce(), super::Column::CollectedAt)),
        )
        .and_where(Expr::col((holds::h(), holds::Column::Status)).eq(HoldStatus::Pending.as_str()))
        .and_where(holds::slot_site().equals((ce(), super::Column::SiteId)))
        .to_owned()
        .to_string(PostgresQueryBuilder);
    format!("({filled}) AS filled, ({findings_open}) AS findings_open")
}

/// Paging is opt-in: a caller naming neither `page` nor `page_size` gets every row.
pub(super) fn paging(page: Option<u64>, page_size: Option<u64>) -> Option<Window> {
    if page.is_none() && page_size.is_none() {
        return None;
    }
    Some(Window::from_page(
        page,
        page_size,
        MAX_PAGE_SIZE,
        MAX_PAGE_SIZE,
    ))
}

fn s() -> Alias {
    Alias::new("s")
}

/// The visits holding a live value, unflagged and not withdrawn, of every parameter named: the
/// visits a calculation reading those parameters can run at.
pub(super) fn visits_holding(parameter_ids: &[Uuid]) -> SelectStatement {
    let wanted: HashSet<Uuid> = parameter_ids.iter().copied().collect();
    let n = i64::try_from(wanted.len()).unwrap_or(i64::MAX);
    readings::Entity::find()
        .select_only()
        .column(readings::Column::CollectionEventId)
        .filter(readings::Column::CollectionEventId.is_not_null())
        .filter(readings::Column::ParameterId.is_in(wanted))
        .filter(readings::Column::WithdrawnAt.is_null())
        .filter(
            Condition::any()
                .add(readings::Column::IsFlagged.is_null())
                .add(readings::Column::IsFlagged.eq(false)),
        )
        .group_by(readings::Column::CollectionEventId)
        .having(
            Expr::col((readings::Entity, readings::Column::ParameterId))
                .count_distinct()
                .eq(n),
        )
        .into_query()
}

/// Confines a listing to the visits holding every parameter named, or to nothing more when none is.
#[must_use]
pub(super) fn holding(parameter_ids: &[Uuid]) -> Option<SimpleExpr> {
    (!parameter_ids.is_empty())
        .then(|| Expr::col((ce(), super::Column::Id)).in_subquery(visits_holding(parameter_ids)))
}

/// Each site with a visit holding every parameter named, and how many it has, confined to
/// `projects` when the caller is scoped.
pub(super) async fn sites_holding<C: ConnectionTrait>(
    db: &C,
    parameter_ids: &[Uuid],
    projects: Option<Vec<Uuid>>,
) -> AppResult<Vec<SiteVisitCount>> {
    use super::models::{Column, Entity};
    let mut query = Entity::find()
        .select_only()
        .column(Column::SiteId)
        .column_as(Column::Id.count(), "visits")
        .filter(Column::Id.in_subquery(visits_holding(parameter_ids)))
        .group_by(Column::SiteId);
    if let Some(projects) = projects {
        query = query.filter(
            Column::SiteId.in_subquery(
                sites::Entity::find()
                    .select_only()
                    .column(sites::Column::Id)
                    .filter(sites::Column::ProjectId.is_in(projects))
                    .into_query(),
            ),
        );
    }
    Ok(query.into_model::<SiteVisitCount>().all(db).await?)
}

/// The visits a listing covers: at the site when one is named, in the caller's projects when the
/// scope is restricted, and collected within the bounds given.
#[must_use]
pub(super) fn visit_filter(
    site_id: Option<Uuid>,
    projects: Option<Vec<Uuid>>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> Condition {
    Condition::all()
        .add_option(site_id.map(|id| Expr::col((ce(), super::Column::SiteId)).eq(id)))
        .add_option(projects.map(|ids| Expr::col((s(), sites::Column::ProjectId)).is_in(ids)))
        .add_option(start.map(|at| Expr::col((ce(), super::Column::CollectedAt)).gte(at)))
        .add_option(end.map(|at| Expr::col((ce(), super::Column::CollectedAt)).lte(at)))
}

/// `collection_events` as `ce` beside its site as `s`, narrowed to `filter`.
fn visits_where(filter: Condition) -> SelectStatement {
    SeaQuery::select()
        .from_as(super::Entity, ce())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s(),
            Expr::col((s(), sites::Column::Id)).equals((ce(), super::Column::SiteId)),
        )
        .cond_where(filter)
        .to_owned()
}

/// The columns a listed visit carries: its own, and the fill and finding counts over it.
fn visit_headers(filter: Condition, paging: Option<Window>) -> SelectStatement {
    let mut query = visits_where(filter);
    query
        .columns(
            [
                super::Column::Id,
                super::Column::SiteId,
                super::Column::CollectedAt,
                super::Column::Source,
                super::Column::CreatedBy,
                super::Column::Notes,
                super::Column::Unverified,
                super::Column::WithdrawnAt,
            ]
            .map(|c| (ce(), c)),
        )
        .expr_as(
            Expr::col((s(), sites::Column::Name)),
            Alias::new("site_name"),
        )
        .expr(Expr::cust(visit_count_columns()));
    if let Some(window) = paging {
        query.limit(window.limit).offset(window.offset);
    }
    query
}

/// How many visits `filter` covers.
pub async fn count_visits(db: &DatabaseConnection, filter: Condition) -> AppResult<u64> {
    let mut query = visits_where(filter);
    query.expr_as(
        Expr::col((ce(), super::Column::Id)).count(),
        Alias::new("n"),
    );
    let n = CountRow::find_by_statement(sea_orm::DatabaseBackend::Postgres.build(&query))
        .one(db)
        .await?
        .map_or(0, |r| r.n);
    Ok(u64::try_from(n).unwrap_or(0))
}

/// A page of the visits `filter` covers in `order`, with their fill and finding counts.
pub async fn visit_headers_page(
    db: &DatabaseConnection,
    filter: Condition,
    order: &[(Expr, Order)],
    paging: Option<Window>,
) -> AppResult<Vec<VisitHeader>> {
    let mut query = visit_headers(filter, paging);
    for (expr, direction) in order {
        query.order_by_expr(expr.clone(), direction.clone());
    }
    Ok(
        VisitHeader::find_by_statement(sea_orm::DatabaseBackend::Postgres.build(&query))
            .all(db)
            .await?,
    )
}

/// A site's visits newest first, the order its grid shows them in.
#[must_use]
pub(super) fn newest_first() -> Vec<(Expr, Order)> {
    vec![(Expr::col((ce(), super::Column::CollectedAt)), Order::Desc)]
}

/// The order a sort name resolves to; the trailing keys keep ties stable.
pub(super) fn visit_list_order(
    sort: Option<&str>,
    order: Option<&str>,
) -> AppResult<Vec<(Expr, Order)>> {
    let direction = match order.unwrap_or("desc") {
        "asc" => Order::Asc,
        "desc" => Order::Desc,
        other => {
            return Err(AppError::BadRequest(format!(
                "order must be asc or desc, not {other}"
            )));
        }
    };
    let column = match sort.unwrap_or("collected_at") {
        "collected_at" => Expr::col((ce(), super::Column::CollectedAt)),
        "parameters_filled" => Expr::col(Alias::new("filled")),
        "findings_open" => Expr::col(Alias::new("findings_open")),
        "site_name" => Expr::col((s(), sites::Column::Name)),
        other => {
            return Err(AppError::BadRequest(format!(
                "sort must be collected_at, parameters_filled, findings_open or site_name, \
                 not {other}"
            )));
        }
    };
    Ok(vec![
        (column, direction),
        (Expr::col((ce(), super::Column::CollectedAt)), Order::Desc),
        (Expr::col((ce(), super::Column::Id)), Order::Asc),
    ])
}

/// A listed visit's header row, as [`visit_headers`] selects it.
#[derive(Debug, FromQueryResult)]
pub struct VisitHeader {
    pub id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub collected_at: DateTime<Utc>,
    pub source: String,
    pub created_by: Option<String>,
    pub notes: Option<String>,
    pub unverified: bool,
    pub withdrawn_at: Option<DateTime<Utc>>,
    pub filled: i64,
    pub findings_open: i64,
    #[sea_orm(skip)]
    pub recompute: String,
}

impl From<VisitHeader> for VisitRow {
    fn from(h: VisitHeader) -> Self {
        Self {
            id: h.id,
            collected_at: h.collected_at,
            source: h.source,
            created_by: h.created_by,
            notes: h.notes,
            parameters_filled: h.filled,
            findings_open: h.findings_open,
            unverified: h.unverified,
            withdrawn_at: h.withdrawn_at,
            recompute: h.recompute,
            cells: Vec::new(),
        }
    }
}

impl From<VisitHeader> for VisitListRow {
    fn from(h: VisitHeader) -> Self {
        Self {
            id: h.id,
            site_id: h.site_id,
            site_name: h.site_name,
            collected_at: h.collected_at,
            source: h.source,
            created_by: h.created_by,
            notes: h.notes,
            parameters_filled: h.filled,
            findings_open: h.findings_open,
            unverified: h.unverified,
            withdrawn_at: h.withdrawn_at,
            recompute: h.recompute,
        }
    }
}

#[derive(FromQueryResult)]
struct CountRow {
    n: i64,
}

#[derive(FromQueryResult)]
struct RecomputeJobRow {
    event_id: Option<String>,
    status: String,
    created_at: sea_orm::prelude::DateTimeWithTimeZone,
}

#[derive(FromQueryResult)]
struct EventIdRow {
    id: Uuid,
}

#[must_use]
pub fn dedupe_key(event_id: Uuid) -> String {
    format!("event_recompute:{event_id}")
}

/// What a parameter is to the calculations that touch it: the ones reading it, by name, and the
/// one writing it. A grid column and a detail cell carry both, so a typed value says what it feeds.
#[must_use]
pub fn parameter_roles(
    impacts: &[crate::routes::private::tools::models::CalculationImpact],
    parameter_id: Uuid,
) -> (Vec<String>, Option<String>) {
    let read_by = impacts
        .iter()
        .filter(|i| i.reads.iter().any(|r| r.parameter_id == parameter_id))
        .map(|i| i.tool.clone())
        .collect();
    let written_by = impacts
        .iter()
        .find(|i| i.outputs.iter().any(|o| o.parameter_id == parameter_id))
        .map(|i| i.tool.clone());
    (read_by, written_by)
}

/// The visit's recompute state from what its latest job and its open findings say: an active
/// job is the state whatever the findings (it is being repaired), a failed job outranks a stale
/// finding (the repair itself needs attention), a stale finding outranks nothing else.
#[must_use]
pub fn visit_status(latest_job_status: Option<&str>, has_stale_finding: bool) -> &'static str {
    match latest_job_status {
        Some("queued" | "pending" | "retrying") => "queued",
        Some("running") => "running",
        Some("failed") => "failed",
        _ if has_stale_finding => "stale",
        _ => "current",
    }
}

/// The state a calculation's latest recompute puts on its health row: `queued`, `running` or
/// `failed` while that run needs watching, `None` once it finished or when none ran, which leaves
/// the open findings to say how the calculation stands.
#[must_use]
pub fn calculation_repair(latest_job_status: Option<&str>) -> Option<&'static str> {
    match visit_status(latest_job_status, false) {
        "current" => None,
        state => Some(state),
    }
}

/// Every `event_recompute` run naming one of `event_ids`, newest first.
async fn recompute_jobs_for(
    db: &DatabaseConnection,
    event_ids: &[Uuid],
) -> AppResult<Vec<RecomputeJobRow>> {
    use crate::routes::private::reprocessing_jobs::{Column, Entity};
    use sea_orm::sea_query::extension::postgres::PgExpr as _;
    let event_id = Expr::col(Column::Params).cast_json_field("collection_event_id");
    let ids: Vec<String> = event_ids.iter().map(ToString::to_string).collect();
    Ok(Entity::find()
        .select_only()
        .column_as(event_id.clone(), "event_id")
        .column(Column::Status)
        .column(Column::CreatedAt)
        .filter(Column::TriggerType.eq("event_recompute"))
        .filter(event_id.is_in(ids))
        .order_by_desc(Column::CreatedAt)
        .into_model::<RecomputeJobRow>()
        .all(db)
        .await?)
}

/// The status of each visit's newest recompute run. The id is `params ->> 'collection_event_id'`,
/// text, so a run whose params name no uuid belongs to no visit.
fn newest_job_per_visit(rows: impl IntoIterator<Item = RecomputeJobRow>) -> HashMap<Uuid, String> {
    let mut newest: HashMap<Uuid, RecomputeJobRow> = HashMap::new();
    for row in rows {
        let Some(id) = row.event_id.as_deref().and_then(|s| s.parse::<Uuid>().ok()) else {
            continue;
        };
        match newest.get(&id) {
            Some(kept) if kept.created_at >= row.created_at => {}
            _ => {
                newest.insert(id, row);
            }
        }
    }
    newest
        .into_iter()
        .map(|(id, row)| (id, row.status))
        .collect()
}

/// The recompute state of each visit: `queued` | `running` | `failed` from its latest
/// `event_recompute` job, else `stale` when an open stale-output or skipped-step finding names
/// it, else `current`.
pub async fn status_for(
    db: &DatabaseConnection,
    event_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, String>> {
    let mut out: HashMap<Uuid, String> = HashMap::new();
    if event_ids.is_empty() {
        return Ok(out);
    }
    let latest_job = newest_job_per_visit(recompute_jobs_for(db, event_ids).await?);
    let stale = SeaQuery::select()
        .distinct()
        .column((ce(), super::Column::Id))
        .from_as(holds::Entity, holds::h())
        .join_as(
            JoinType::InnerJoin,
            super::Entity,
            ce(),
            Condition::all()
                .add(
                    Expr::col((ce(), super::Column::SiteId))
                        .equals((holds::h(), holds::Column::SiteId)),
                )
                .add(
                    Expr::col((ce(), super::Column::CollectedAt))
                        .equals((holds::h(), holds::Column::GroupTime)),
                ),
        )
        .and_where(Expr::col((holds::h(), holds::Column::Kind)).is_in([
            HoldKind::StaleOutput.as_str(),
            HoldKind::SkippedOutput.as_str(),
        ]))
        .and_where(Expr::col((holds::h(), holds::Column::Status)).eq(HoldStatus::Pending.as_str()))
        .and_where(Expr::col((holds::h(), holds::Column::StreamId)).is_null())
        .and_where(Expr::col((ce(), super::Column::Id)).is_in(event_ids.iter().copied()))
        .to_owned();
    let stale = EventIdRow::find_by_statement(sea_orm::DatabaseBackend::Postgres.build(&stale))
        .all(db)
        .await?;
    let stale_events: std::collections::HashSet<Uuid> = stale.into_iter().map(|r| r.id).collect();
    for id in event_ids {
        out.insert(
            *id,
            visit_status(
                latest_job.get(id).map(String::as_str),
                stale_events.contains(id),
            )
            .to_string(),
        );
    }
    Ok(out)
}

// --- Visit grid ---

/// Fill each listed visit's grid cells from the page's readings, samples and open findings.
pub async fn fill_visit_cells(db: &DatabaseConnection, visits: &mut [VisitRow]) -> AppResult<()> {
    let event_ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    if event_ids.is_empty() {
        return Ok(());
    }
    let cell_rows = visit_cell_rows(db, &event_ids).await?;
    let measurements = replicate_measurements(db, &event_ids).await?;
    let findings = open_findings_by_slot(db, &event_ids).await?;
    let curve_names = curve_names(db, &cell_rows).await?;
    place_cells(visits, cell_rows, &measurements, &findings, &curve_names);
    Ok(())
}

/// The oldest open finding's kind and the count open, by (instant, parameter).
type Findings = HashMap<(DateTime<Utc>, Uuid), (String, i64)>;

/// A site's grid columns, each with the calculations reading it and the one writing it.
pub async fn expected_parameters(
    db: &DatabaseConnection,
    site_id: Uuid,
) -> AppResult<Vec<ExpectedParameter>> {
    // The column set is every parameter the site's visits carry, plus every active slot configured
    // on it, at any cadence. The first arm reads the readings through
    // `collection_events` rather than by an unbounded DISTINCT over the hypertable, which pays a
    // planning cost proportional to the chunk count on every page load; `samples` cannot speak for
    // it, since a measurement taken once forms no row there. Taking the union means a parameter
    // with no reading yet still gets a column, so its value has somewhere to render and the fill
    // ratio cannot exceed its own denominator.
    let p = Alias::new("p");
    let sp = Alias::new("sp");
    let measured_here = SeaQuery::select()
        .column((Alias::new("r"), readings::Column::ParameterId))
        .from_as(readings::Entity, Alias::new("r"))
        .inner_join(
            super::Entity,
            Expr::col((super::Entity, super::Column::Id))
                .equals((Alias::new("r"), readings::Column::CollectionEventId)),
        )
        .and_where(Expr::col((super::Entity, super::Column::SiteId)).eq(site_id))
        .take();
    let declared_here = SeaQuery::select()
        .column(site_parameters::Column::ParameterId)
        .from(site_parameters::Entity)
        .and_where(Expr::col(site_parameters::Column::SiteId).eq(site_id))
        .and_where(Expr::cust("COALESCE(is_active, true) = true"))
        .take();
    let expected_query = SeaQuery::select()
        .distinct_on([(p.clone(), parameters::Column::Code)])
        .column((p.clone(), parameters::Column::Id))
        .column((p.clone(), parameters::Column::Code))
        .column((p.clone(), parameters::Column::Name))
        .expr_as(
            Expr::col((p.clone(), parameters::Column::DefaultUnits)),
            Alias::new("units"),
        )
        .column((sp.clone(), site_parameters::Column::DecimalPlaces))
        .from_as(parameters::Entity, p.clone())
        .join_as(
            JoinType::LeftJoin,
            site_parameters::Entity,
            sp.clone(),
            Condition::all()
                .add(
                    Expr::col((sp.clone(), site_parameters::Column::ParameterId))
                        .equals((p.clone(), parameters::Column::Id)),
                )
                .add(Expr::col((sp.clone(), site_parameters::Column::SiteId)).eq(site_id)),
        )
        .cond_where(
            Condition::any()
                .add(Expr::col((p.clone(), parameters::Column::Id)).in_subquery(measured_here))
                .add(Expr::col((p.clone(), parameters::Column::Id)).in_subquery(declared_here)),
        )
        .order_by((p.clone(), parameters::Column::Code), Order::Asc)
        .order_by((sp.clone(), site_parameters::Column::Id), Order::Asc)
        .take();
    let expected_rows =
        ExpectedRow::find_by_statement(sea_orm::DatabaseBackend::Postgres.build(&expected_query))
            .all(db)
            .await?;
    let mut expected_parameters = Vec::with_capacity(expected_rows.len());
    for r in expected_rows {
        expected_parameters.push(ExpectedParameter {
            parameter_id: r.id,
            code: r.code,
            name: r.name,
            units: r.units,
            decimal_places: r.decimal_places,
            written_by: None,
            read_by: Vec::new(),
        });
    }
    let columns: Vec<Uuid> = expected_parameters.iter().map(|p| p.parameter_id).collect();
    let impacts = crate::routes::private::tools::service::calculations_fed_by(db, &columns).await?;
    for column in &mut expected_parameters {
        (column.read_by, column.written_by) = parameter_roles(&impacts, column.parameter_id);
    }
    Ok(expected_parameters)
}

/// Each listed visit's cells, one per (visit, parameter): the served value, the curation counts,
/// the sample statistics and the replicates as parallel arrays.
async fn visit_cell_rows(db: &DatabaseConnection, event_ids: &[Uuid]) -> AppResult<Vec<CellRow>> {
    // The served value is the sample mean where a replicate group formed one, else the lowest
    // unflagged replicate's own value. The aggregates stay `Expr::cust`: FILTER, BOOL_AND and
    // the array subscript have no builder form.
    let r = Alias::new("r");
    let s_ = Alias::new("s");
    let agg = |sql: &str, name: &str| (Expr::cust(sql.to_string()), Alias::new(name.to_string()));
    let mut cell_query = SeaQuery::select();
    cell_query
        .expr_as(
            Expr::col((r.clone(), readings::Column::CollectionEventId)),
            Alias::new("event_id"),
        )
        .column((r.clone(), readings::Column::ParameterId));
    for (expr, name) in [
        agg(
            "COALESCE(MAX(s.mean), \
             (ARRAY_AGG(COALESCE(r.calibrated_value, r.raw_value) ORDER BY r.replicate_index) \
              FILTER (WHERE r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL))[1])",
            "value",
        ),
        agg("BOOL_AND(r.is_flagged IS TRUE)", "all_flagged"),
        agg("BOOL_AND(r.withdrawn_at IS NOT NULL)", "all_withdrawn"),
        agg("COUNT(*)::bigint", "n_total"),
        agg(
            "COUNT(*) FILTER (WHERE r.is_flagged IS TRUE)::bigint",
            "n_flagged",
        ),
        agg(
            "COUNT(*) FILTER (WHERE r.withdrawn_at IS NOT NULL)::bigint",
            "n_withdrawn",
        ),
        agg(
            "COUNT(*) FILTER (WHERE r.unverified IS TRUE)::bigint",
            "n_unverified",
        ),
        // A single measurement forms no `samples` row, and the serving arm reports it as
        // n = 1 (`routes/public/views.rs`). Count what the mean would stand on, so the two
        // surfaces agree and a lone excluded replicate still says zero.
        agg(
            "COALESCE(MAX(s.n), COUNT(*) FILTER (WHERE r.unverified IS NOT TRUE \
               AND r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL)::int)",
            "sample_n",
        ),
        agg("MAX(s.stdev)", "stdev"),
        agg("MAX(s.median)", "median"),
        agg("MAX(s.min_value)", "min_value"),
        agg("MAX(s.max_value)", "max_value"),
        // The replicates themselves, as parallel arrays in one index order: a composite array
        // would decode by hand, and the four are read back together or not at all.
        agg(
            "ARRAY_AGG(r.replicate_index ORDER BY r.replicate_index)",
            "replicate_indexes",
        ),
        agg(
            "ARRAY_AGG(COALESCE(r.calibrated_value, r.raw_value) ORDER BY r.replicate_index)",
            "replicate_values",
        ),
        agg(
            "ARRAY_AGG(r.is_flagged IS TRUE ORDER BY r.replicate_index)",
            "replicate_flagged",
        ),
        agg(
            "ARRAY_AGG(r.withdrawn_at IS NOT NULL ORDER BY r.replicate_index)",
            "replicate_withdrawn",
        ),
        agg(
            "ARRAY_AGG(r.unverified IS TRUE ORDER BY r.replicate_index)",
            "replicate_unverified",
        ),
        agg(
            "ARRAY_AGG(r.stream_id ORDER BY r.replicate_index)",
            "replicate_streams",
        ),
        agg("BOOL_OR(r.provenance IS NOT NULL)", "has_provenance"),
        agg("MAX(r.provenance ->> 'tool')", "tool"),
        agg("(MAX(r.provenance ->> 'run_id'))::uuid", "tool_run_id"),
        agg(
            "ARRAY_AGG(r.standard_curve_id ORDER BY r.replicate_index) \
             FILTER (WHERE r.standard_curve_id IS NOT NULL)",
            "replicate_curves",
        ),
    ] {
        cell_query.expr_as(expr, name);
    }
    let cell_query = cell_query
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            s_.clone(),
            Expr::col((s_.clone(), samples::Column::Id))
                .equals((r.clone(), readings::Column::SampleId)),
        )
        .and_where(Expr::cust_with_values(
            "r.collection_event_id = ANY($1)",
            [event_ids.to_vec()],
        ))
        .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).is_not_null())
        .add_group_by([
            Expr::col((r.clone(), readings::Column::CollectionEventId)),
            Expr::col((r.clone(), readings::Column::ParameterId)),
        ])
        .take();
    Ok(
        CellRow::find_by_statement(sea_orm::DatabaseBackend::Postgres.build(&cell_query))
            .all(db)
            .await?,
    )
}

/// The open findings at each listed visit, by (instant, parameter): the oldest one's kind and how
/// many are open there.
async fn open_findings_by_slot(db: &DatabaseConnection, event_ids: &[Uuid]) -> AppResult<Findings> {
    // A hold is keyed on the slot (an event-audit finding) or on the stream that raised it (a
    // statistics disagreement, a source modification, a brake). Both land at a visit's instant and
    // both belong in its grid, so the stream's own pairing resolves the slot rather than the hold
    // being skipped for lacking one. Oldest first, matching the detail endpoint, so the two grids
    // cannot disagree about which finding a cell carries.
    let query = holds::with_stream_slot()
        .expr_as(holds::slot_parameter(), Alias::new("parameter_id"))
        .column((holds::h(), holds::Column::GroupTime))
        .column((holds::h(), holds::Column::Kind))
        .column((holds::h(), holds::Column::CreatedAt))
        .join_as(
            JoinType::InnerJoin,
            super::Entity,
            ce(),
            Condition::all()
                .add(holds::slot_site().equals((ce(), super::Column::SiteId)))
                .add(
                    Expr::col((ce(), super::Column::CollectedAt))
                        .equals((holds::h(), holds::Column::GroupTime)),
                ),
        )
        .and_where(Expr::col((holds::h(), holds::Column::Status)).eq(HoldStatus::Pending.as_str()))
        .and_where(Expr::col((ce(), super::Column::Id)).is_in(event_ids.iter().copied()))
        .and_where(holds::slot_parameter().is_not_null())
        .order_by((holds::h(), holds::Column::CreatedAt), Order::Asc)
        .to_owned();
    let finding_rows = db.query_all(&query).await?;
    // Oldest wins, and the rest are counted: a cell carrying two open findings says so
    // rather than picking one silently.
    let mut findings: HashMap<(DateTime<Utc>, Uuid), (String, i64)> = HashMap::new();
    for f in &finding_rows {
        let f = VisitFindingRow::from_query_result(f, "")?;
        findings
            .entry((f.group_time.with_timezone(&Utc), f.parameter_id))
            .and_modify(|(_, n)| *n += 1)
            .or_insert((f.kind, 1));
    }
    Ok(findings)
}

/// Put each cell on its visit and each open finding on its cell, giving a finding with no cell (a
/// missing output names a parameter with no readings) one of its own.
fn place_cells(
    visits: &mut [VisitRow],
    cell_rows: Vec<CellRow>,
    measurements: &HashMap<ReplicateAt, Measurement>,
    findings: &Findings,
    curve_names: &HashMap<Uuid, Option<String>>,
) {
    let mut by_event: HashMap<Uuid, Vec<VisitCell>> = HashMap::new();
    for c in cell_rows {
        let replicates = replicates_of(&c, measurements);
        let curves = cell_curves(
            c.replicate_curves.as_deref().unwrap_or_default(),
            curve_names,
        );
        by_event.entry(c.event_id).or_default().push(VisitCell {
            parameter_id: c.parameter_id,
            value: c.value,
            flagged: c.all_flagged.unwrap_or(false),
            withdrawn: c.all_withdrawn.unwrap_or(false),
            n_total: c.n_total,
            n_flagged: c.n_flagged,
            n_withdrawn: c.n_withdrawn,
            n_unverified: c.n_unverified,
            n: c.sample_n,
            stdev: c.stdev,
            median: c.median,
            min: c.min_value,
            max: c.max_value,
            finding: None,
            finding_count: None,
            replicates,
            has_provenance: c.has_provenance.unwrap_or(false),
            tool: c.tool,
            tool_run_id: c.tool_run_id,
            curves,
        });
    }
    for visit in visits.iter_mut() {
        let mut cells = by_event.remove(&visit.id).unwrap_or_default();
        for cell in &mut cells {
            if let Some((kind, n)) = findings.get(&(visit.collected_at, cell.parameter_id)) {
                cell.finding = Some(kind.clone());
                cell.finding_count = (*n > 1).then_some(*n);
            }
        }
        // A missing-output finding names a parameter with no readings; it still gets a cell.
        for ((at, parameter_id), (kind, n)) in findings {
            if *at == visit.collected_at && !cells.iter().any(|c| c.parameter_id == *parameter_id) {
                cells.push(VisitCell {
                    parameter_id: *parameter_id,
                    value: None,
                    flagged: false,
                    withdrawn: false,
                    n_total: 0,
                    n_flagged: 0,
                    n_withdrawn: 0,
                    n_unverified: 0,
                    n: None,
                    stdev: None,
                    median: None,
                    min: None,
                    max: None,
                    finding: Some(kind.clone()),
                    finding_count: (*n > 1).then_some(*n),
                    replicates: Vec::new(),
                    has_provenance: false,
                    tool: None,
                    tool_run_id: None,
                    curves: Vec::new(),
                });
            }
        }
        visit.cells = cells;
    }
}

/// A grid column as [`expected_parameters`] selects it.
#[derive(FromQueryResult)]
struct ExpectedRow {
    id: Uuid,
    code: String,
    name: String,
    units: Option<String>,
    decimal_places: Option<i16>,
}

#[derive(FromQueryResult)]
struct VisitFindingRow {
    group_time: sea_orm::prelude::DateTimeWithTimeZone,
    parameter_id: Uuid,
    kind: String,
}

#[derive(FromQueryResult)]
struct CellRow {
    event_id: Uuid,
    parameter_id: Uuid,
    value: Option<f64>,
    all_flagged: Option<bool>,
    all_withdrawn: Option<bool>,
    n_total: i64,
    n_flagged: i64,
    n_withdrawn: i64,
    n_unverified: i64,
    sample_n: Option<i32>,
    stdev: Option<f64>,
    median: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    replicate_indexes: Vec<i16>,
    replicate_values: Vec<f64>,
    replicate_flagged: Vec<bool>,
    replicate_withdrawn: Vec<bool>,
    replicate_unverified: Vec<bool>,
    replicate_streams: Vec<Uuid>,
    has_provenance: Option<bool>,
    tool: Option<String>,
    tool_run_id: Option<Uuid>,
    replicate_curves: Option<Vec<Uuid>>,
}

/// The name of every curve the page's cells were corrected through, in one lookup.
async fn curve_names(
    db: &DatabaseConnection,
    rows: &[CellRow],
) -> AppResult<HashMap<Uuid, Option<String>>> {
    let ids: std::collections::BTreeSet<Uuid> = rows
        .iter()
        .flat_map(|r| r.replicate_curves.iter().flatten().copied())
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect())
}

/// A cell's curves: each distinct one its replicates name, in replicate order.
fn cell_curves(
    replicate_curves: &[Uuid],
    names: &HashMap<Uuid, Option<String>>,
) -> Vec<VisitCellCurve> {
    let mut curves: Vec<VisitCellCurve> = Vec::new();
    for id in replicate_curves {
        if curves.iter().any(|c| c.id == *id) {
            continue;
        }
        curves.push(VisitCellCurve {
            id: *id,
            name: names.get(id).cloned().flatten(),
        });
    }
    curves
}

/// A replicate's measurement before any curve, and the curves that correct it, as the page's one
/// lookup selects it.
#[derive(FromQueryResult)]
struct Measurement {
    collection_event_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    stream_id: Uuid,
    replicate_index: i16,
    raw_value: f64,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
}

/// A listed replicate: its visit, parameter, stream and index.
type ReplicateAt = (Uuid, Uuid, Uuid, i16);

/// Every listed replicate's [`Measurement`].
async fn replicate_measurements(
    db: &DatabaseConnection,
    event_ids: &[Uuid],
) -> AppResult<HashMap<ReplicateAt, Measurement>> {
    let rows = readings::Entity::find()
        .select_only()
        .columns([
            readings::Column::CollectionEventId,
            readings::Column::ParameterId,
            readings::Column::StreamId,
            readings::Column::ReplicateIndex,
            readings::Column::RawValue,
            readings::Column::CalibrationId,
            readings::Column::StandardCurveId,
        ])
        .filter(readings::Column::CollectionEventId.is_in(event_ids.iter().copied()))
        .into_model::<Measurement>()
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|m| {
            let at = (
                m.collection_event_id?,
                m.parameter_id?,
                m.stream_id,
                m.replicate_index,
            );
            Some((at, m))
        })
        .collect())
}

/// The parallel arrays one `ARRAY_AGG` group returns, read back as replicates. They come out of one
/// group over one ordering, so position `i` is the same replicate in each.
fn replicates_of(
    row: &CellRow,
    measurements: &HashMap<ReplicateAt, Measurement>,
) -> Vec<VisitReplicate> {
    row.replicate_indexes
        .iter()
        .zip(&row.replicate_values)
        .enumerate()
        .map(|(i, (replicate_index, value))| {
            let stream_id = row.replicate_streams.get(i).copied().unwrap_or_default();
            let measured =
                measurements.get(&(row.event_id, row.parameter_id, stream_id, *replicate_index));
            VisitReplicate {
                replicate_index: *replicate_index,
                value: *value,
                raw_value: measured.map_or(*value, |m| m.raw_value),
                calibration_id: measured.and_then(|m| m.calibration_id),
                standard_curve_id: measured.and_then(|m| m.standard_curve_id),
                stream_id,
                flagged: row.replicate_flagged.get(i).copied().unwrap_or(false),
                withdrawn: row.replicate_withdrawn.get(i).copied().unwrap_or(false),
                unverified: row.replicate_unverified.get(i).copied().unwrap_or(false),
            }
        })
        .collect()
}

/// Put each listed visit's recompute state on it, per [`status_for`].
pub async fn attach_recompute_status(
    db: &DatabaseConnection,
    visits: &mut [VisitHeader],
) -> AppResult<()> {
    let ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    let states = status_for(db, &ids).await?;
    for visit in visits {
        visit.recompute = states
            .get(&visit.id)
            .cloned()
            .unwrap_or_else(|| "current".to_string());
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
