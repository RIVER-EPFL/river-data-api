//! The site-parameter handlers: applying a parameter group to a site, and declaring or retagging
//! a slot's sd estimator.
//!
//! Under Q98 a calculation applies at a site when the group's output slots are configured there,
//! so the site parameters *are* the declaration and `apply_group` is the flow that writes it. One
//! action per group rather than one per member: pCO2, DIC and Chl a carry roughly 45 stage-1
//! intermediates between them (Q95), and there are 23 CNET stations. Applying twice adds only what
//! is missing, so a group that grows is applied again rather than diffed by hand.
//!
//! `sd_estimator` is excluded from CRUD update because changing the declaration must also
//! recompute the slot's stored samples; `declare_sd_estimator` is the one path, writing the column
//! and enqueueing the tracked `sd_estimator_retag` in the same breath, exactly as the audit
//! resolution's slot scope does.

use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::EntityTrait;
use sea_orm::FromQueryResult;
use sea_orm::Order;
use sea_orm::PaginatorTrait;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::Statement;
use sea_orm::TransactionTrait;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Condition;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::JoinType;
use sea_orm::sea_query::PostgresQueryBuilder;
use sea_orm::sea_query::Query as SeaQuery;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::models::ActiveModel;
use super::models::AppliedSlot;
use super::models::ApplyGroupRequest;
use super::models::ApplyGroupResponse;
use super::models::Column;
use super::models::DeclareSdEstimatorRequest;
use super::models::DeclareSdEstimatorResponse;
use super::models::Entity;
use super::models::GroupMember;
use super::models::RetagCounts;
use super::models::RetagSdEstimatorRequest;
use super::models::RetagSdEstimatorResponse;
use super::models::UndeclaredRow;
use super::service::MergeSiteParametersRequest;
use super::service::MergeSiteParametersResponse;
use super::service::partition_members;
use super::service::slot_scope;
use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::scope::Unowned;
use crate::common::scope::project_filter;
use crate::common::scope::project_of_site_parameter;
use crate::common::scope::require_target_in_scope;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::data_streams;
use crate::routes::private::parameter_groups::member_model;
use crate::routes::private::parameter_groups::service::rules::Role;
use crate::routes::private::parameters;
use crate::routes::private::readings::samples;
use crate::routes::private::site_parameters;
use crate::routes::private::sites;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;

#[utoipa::path(
    post,
    path = "/api/site_parameters/{id}/declare_sd_estimator",
    request_body = DeclareSdEstimatorRequest,
    responses(
        (status = 200, body = DeclareSdEstimatorResponse),
        (status = 400, description = "Estimator is not 'sample', 'population' or null"),
        (status = 404, description = "No site parameter with this id"),
    ),
    tag = "site_parameters"
)]
pub async fn declare_sd_estimator(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<DeclareSdEstimatorRequest>,
) -> AppResult<Json<DeclareSdEstimatorResponse>> {
    let estimator = match payload.estimator.as_deref() {
        None | Some("sample" | "population") => payload.estimator.clone(),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "estimator must be 'sample', 'population' or null, not '{other}'"
            )));
        }
    };

    let (previous, affected) = state
        .db
        .transaction::<_, (Option<String>, i64), sea_orm::DbErr>(|txn| {
            let estimator = estimator.clone();
            Box::pin(async move {
                crate::common::actor::declare(txn).await?;
                let slot = Entity::find_by_id(id)
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| sea_orm::DbErr::RecordNotFound(id.to_string()))?;
                let (site_id, parameter_id, previous) =
                    (slot.site_id, slot.parameter_id, slot.sd_estimator);

                Entity::update_many()
                    .col_expr(Column::SdEstimator, Expr::value(estimator.clone()))
                    .filter(Column::Id.eq(id))
                    .exec(txn)
                    .await?;

                // Counted inside the transaction the declaration lands in, so the number
                // reported is the one the retag will act on. A cleared declaration recomputes
                // nothing: stored samples keep the estimator they were computed with.
                // `sd_estimator` is NOT NULL, so `ne` is the `IS DISTINCT FROM` this had.
                let affected = if let Some(est) = &estimator {
                    samples::Entity::find()
                        .filter(samples::Column::SiteId.eq(site_id))
                        .filter(samples::Column::ParameterId.eq(parameter_id))
                        .filter(samples::Column::SdEstimator.ne(est.clone()))
                        .filter(samples::Column::SdEstimatorSource.ne("sample"))
                        .count(txn)
                        .await?
                        .try_into()
                        .unwrap_or(i64::MAX)
                } else {
                    0
                };
                Ok((previous, affected))
            })
        })
        .await
        .map_err(|e| match e {
            sea_orm::TransactionError::Transaction(sea_orm::DbErr::RecordNotFound(_)) => {
                AppError::NotFound(format!("site parameter {id} not found"))
            }
            sea_orm::TransactionError::Connection(db) => AppError::from(db),
            sea_orm::TransactionError::Transaction(db) => AppError::from(db),
        })?;

    let job_id = if let Some(est) = &estimator
        && affected > 0
    {
        crate::routes::private::reprocessing_jobs::service::enqueue(
            &state.db,
            "sd_estimator_retag",
            None,
            None,
            &serde_json::json!({
                "estimator": est,
                "site_parameter_ids": [id],
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(DeclareSdEstimatorResponse {
        site_parameter_id: id,
        estimator,
        previous,
        samples_affected: affected,
        job_id,
    }))
}

#[utoipa::path(
    post,
    path = "/api/actions/retag_sd_estimator",
    request_body = RetagSdEstimatorRequest,
    responses(
        (status = 200, body = RetagSdEstimatorResponse),
        (status = 400, description = "Unknown estimator, no slot or stream named, a window \
                                      that ends before it starts, or a slot in scope that does \
                                      not declare the estimator"),
    ),
    tag = "actions"
)]
pub async fn retag_sd_estimator(
    State(state): State<AppState>,
    Json(payload): Json<RetagSdEstimatorRequest>,
) -> AppResult<Json<RetagSdEstimatorResponse>> {
    let estimator = crate::routes::private::readings::service::parse(&payload.estimator)?;
    if payload.site_parameter_ids.is_empty() && payload.stream_ids.is_empty() {
        return Err(AppError::BadRequest(
            "name at least one site_parameter_id or stream_id".to_string(),
        ));
    }
    if let (Some(start), Some(end)) = (payload.start, payload.end)
        && end < start
    {
        return Err(AppError::BadRequest(
            "the window ends before it starts".to_string(),
        ));
    }

    let db = &state.db;
    let undeclared = if payload.dry_run {
        Vec::new()
    } else {
        db.query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT st.name AS site_name, p.name AS parameter_name, sp.sd_estimator
             FROM site_parameters sp
             JOIN sites st ON st.id = sp.site_id
             JOIN parameters p ON p.id = sp.parameter_id
             WHERE (sp.id = ANY($1)
                    OR EXISTS (SELECT 1 FROM data_streams ds
                               WHERE ds.id = ANY($2) AND ds.site_parameter_id = sp.id))
               AND sp.sd_estimator IS DISTINCT FROM $3
             ORDER BY st.name, p.name",
            [
                payload.site_parameter_ids.clone().into(),
                payload.stream_ids.clone().into(),
                estimator.into(),
            ],
        ))
        .await?
    };
    if !undeclared.is_empty() {
        let named: Vec<String> = undeclared
            .iter()
            .map(|row| {
                let row = UndeclaredRow::from_query_result(row, "")?;
                Ok(format!(
                    "{} / {} ({})",
                    row.site_name,
                    row.parameter_name,
                    row.sd_estimator.as_deref().unwrap_or("not declared")
                ))
            })
            .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
        return Err(AppError::BadRequest(format!(
            "declare '{estimator}' on the slot first; it is not what {} declares",
            named.join(", ")
        )));
    }

    // One pass, split by whether the instant declared for itself: the two FILTER aggregates are
    // the only text left, and the scope and window are composed rather than appended with
    // hand-counted placeholders.
    let mut counts_query = SeaQuery::select();
    counts_query
        .expr_as(
            Expr::cust("COUNT(*) FILTER (WHERE sd_estimator_source <> 'sample')::bigint"),
            Alias::new("slot_rows"),
        )
        .expr_as(
            Expr::cust("COUNT(*) FILTER (WHERE sd_estimator_source = 'sample')::bigint"),
            Alias::new("instant_rows"),
        )
        .from(samples::Entity)
        .and_where(slot_scope(&payload.site_parameter_ids, &payload.stream_ids))
        .and_where(Expr::col(samples::Column::SdEstimator).ne(estimator));
    if let Some(start) = payload.start {
        counts_query.and_where(Expr::col(samples::Column::CollectedAt).gte(start));
    }
    if let Some(end) = payload.end {
        counts_query.and_where(Expr::col(samples::Column::CollectedAt).lte(end));
    }
    let (sql, values) = counts_query.build(PostgresQueryBuilder);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values.0,
        ))
        .await?;
    let counts = row
        .map(|row| RetagCounts::from_query_result(&row, ""))
        .transpose()?;
    let (slot_rows, instant_rows) = counts.map_or((0, 0), |c| (c.slot_rows, c.instant_rows));
    let affected = if payload.override_instants {
        slot_rows + instant_rows
    } else {
        slot_rows
    };

    let job_id = if affected > 0 && !payload.dry_run {
        let mut params = serde_json::json!({
            "estimator": estimator,
            "site_parameter_ids": payload.site_parameter_ids,
            "stream_ids": payload.stream_ids,
            "override_instants": payload.override_instants,
        });
        if let Some(start) = payload.start {
            params["start"] = start.to_rfc3339().into();
        }
        if let Some(end) = payload.end {
            params["end"] = end.to_rfc3339().into();
        }
        crate::routes::private::reprocessing_jobs::service::enqueue(
            db,
            "sd_estimator_retag",
            None,
            None,
            &params,
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(RetagSdEstimatorResponse {
        estimator: estimator.to_string(),
        samples_affected: affected,
        instant_decisions: instant_rows,
        job_id,
    }))
}

#[utoipa::path(
    post,
    path = "/api/sites/{site_id}/parameter_groups",
    request_body = ApplyGroupRequest,
    responses(
        (status = 200, body = ApplyGroupResponse),
        (status = 404, description = "No site or no parameter group with this id"),
    ),
    tag = "site_parameters"
)]
pub async fn apply_group(
    State(state): State<AppState>,
    Path(site_id): Path<Uuid>,
    Json(payload): Json<ApplyGroupRequest>,
) -> AppResult<Json<ApplyGroupResponse>> {
    let site = crate::routes::private::sites::models::Entity::find_by_id(site_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {site_id} not found")))?;

    let members = member_model::Entity::find()
        .filter(member_model::Column::GroupId.eq(payload.group_id))
        .order_by_asc(member_model::Column::Ordinal)
        .all(&state.db)
        .await?;
    if members.is_empty() {
        return Err(AppError::NotFound(format!(
            "Parameter group {} holds no members",
            payload.group_id
        )));
    }

    let codes: std::collections::HashMap<Uuid, String> =
        crate::routes::private::parameters::Entity::find()
            .filter(
                crate::routes::private::parameters::Column::Id
                    .is_in(members.iter().map(|m| m.parameter_id)),
            )
            .all(&state.db)
            .await?
            .into_iter()
            .map(|p| (p.id, p.code))
            .collect();

    // The role follows from the calculations, never from the membership row (Q135).
    let roles =
        crate::routes::private::parameter_groups::service::calculation_roles(&state.db).await?;
    let rows: Vec<GroupMember> = members
        .iter()
        .map(|m| {
            (
                m.parameter_id,
                codes.get(&m.parameter_id).cloned().unwrap_or_default(),
                roles
                    .get(&m.parameter_id)
                    .copied()
                    .unwrap_or(Role::EntryOnly)
                    .as_str()
                    .to_string(),
            )
        })
        .collect();

    let held: std::collections::HashSet<Uuid> = Entity::find()
        .filter(Column::SiteId.eq(site_id))
        .select_only()
        .column(Column::ParameterId)
        .into_tuple::<Uuid>()
        .all(&state.db)
        .await?
        .into_iter()
        .collect();

    let (to_create, already) = partition_members(&rows, &held);
    let slot = |(parameter_id, parameter_code, role): &GroupMember,
                site_parameter_id: Option<Uuid>| AppliedSlot {
        parameter_id: *parameter_id,
        parameter_code: parameter_code.clone(),
        role: role.clone(),
        site_parameter_id,
    };

    if payload.dry_run {
        return Ok(Json(ApplyGroupResponse {
            site_id,
            group_id: payload.group_id,
            dry_run: true,
            created: to_create.iter().map(|m| slot(m, None)).collect(),
            existing: already.iter().map(|m| slot(m, None)).collect(),
        }));
    }

    // One transaction: a half-applied group is a site whose calculations partly apply, which is
    // the state this whole flow exists to prevent.
    let txn = state.db.begin().await?;
    // The change-audit trigger reads the writer from the transaction it fires in.
    crate::common::actor::declare(&txn).await?;
    crate::common::actor::declare(&txn).await?;
    let mut created = Vec::with_capacity(to_create.len());
    for member in &to_create {
        let id = Uuid::new_v4();
        ActiveModel {
            id: Set(id),
            site_id: Set(site_id),
            parameter_id: Set(member.0),
            name: Set(format!("{} {}", site.name, member.1).trim().to_string()),
            sensor_type: Set(String::new()),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
            needs_review: Set(false),
            instrument_sensor_id: Set(payload.instrument_sensor_id),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        created.push(slot(member, Some(id)));
    }
    txn.commit().await?;

    if !created.is_empty() {
        crate::common::cache::invalidate_site(&state.response_cache, site_id);
    }

    Ok(Json(ApplyGroupResponse {
        site_id,
        group_id: payload.group_id,
        dry_run: false,
        created,
        existing: already.iter().map(|m| slot(m, None)).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Undeclared sd estimators

#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct UndeclaredEstimatorSlot {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub site_name: String,
    pub parameter_name: String,
    pub parameter_code: String,
    pub site_parameter_id: Uuid,
    /// Samples at this slot computed under no declaration, ie. `sd_estimator_source = 'default'`.
    pub undeclared_samples: i64,
    /// Whether any stream feeding the slot ships a precomputed sd column. A slot whose source
    /// states an sd is one whose convention is answerable from the evidence; one that does not is
    /// a choice about what this lab publishes.
    pub source_reports_sd: bool,
    /// Every stream feeding the slot, as `source_system/source_key`.
    pub streams: serde_json::Value,
    /// Open holds at this slot, and how many carry the population-divisor signature. That second
    /// number is the evidence for the decision; this report states it and rules on nothing.
    pub open_holds: i64,
    pub population_signature_holds: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UndeclaredEstimatorsResponse {
    pub total_slots: usize,
    pub total_undeclared_samples: i64,
    /// Open holds across these slots that the population divisor would explain. Every one of them
    /// is blocked from plain acknowledgement until its slot declares an estimator.
    pub total_population_signature_holds: i64,
    pub slots: Vec<UndeclaredEstimatorSlot>,
}

/// Slots serving replicate statistics under no declared sd estimator.
///
/// The sources stored both divisors over the years, row by row within one stream, so the
/// convention cannot be inferred and is declared per slot instead. Until a slot declares one, its
/// samples are computed with the sample divisor and stamped `default`, which is what this lists.
/// No write path can notice this shape on its own: every ingest is individually valid, and the gap
/// is in what nobody stated.
///
/// Read-only. Which divisor a slot publishes is a question about this lab's practice and the
/// source's, so nothing here decides one.
#[utoipa::path(
    get,
    path = "/api/actions/undeclared_sd_estimators",
    responses((status = 200, description = "Slots with no declared sd estimator", body = UndeclaredEstimatorsResponse)),
    tag = "actions"
)]
pub async fn undeclared_sd_estimators(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<UndeclaredEstimatorsResponse>> {
    use sea_orm::{FromQueryResult, Statement};

    let population_sd = &*crate::routes::private::sync::service::POPULATION_SD_SQL;
    let sp = Alias::new("sp");
    let st = Alias::new("st");
    let p = Alias::new("p");
    let u = Alias::new("u");
    let s_ = Alias::new("s");
    let h = Alias::new("h");
    let sm = Alias::new("sm");
    let ds = Alias::new("ds");
    let ds2 = Alias::new("ds2");
    let hold = Alias::new("h");

    let undeclared = SeaQuery::select()
        .expr_as(
            Expr::cust("COUNT(*)::bigint"),
            Alias::new("undeclared_samples"),
        )
        .from_as(samples::Entity, sm.clone())
        .and_where(Expr::cust("sm.site_id = sp.site_id"))
        .and_where(Expr::cust("sm.parameter_id = sp.parameter_id"))
        .and_where(Expr::col((sm.clone(), samples::Column::SdEstimatorSource)).eq("default"))
        .take();

    let sources = SeaQuery::select()
        .expr_as(
            Expr::cust("bool_or(ds.metadata #>> '{replicates,portal_sd_column}' IS NOT NULL)"),
            Alias::new("source_reports_sd"),
        )
        .expr_as(
            Expr::cust(
                "jsonb_agg(jsonb_build_object('stream_id', ds.id, 'source_system', \
                 ds.source_system, 'source_key', ds.source_key))",
            ),
            Alias::new("streams"),
        )
        .from_as(data_streams::Entity, ds.clone())
        .and_where(Expr::cust("ds.site_parameter_id = sp.id"))
        .take();

    let holds = SeaQuery::select()
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("open_holds"))
        .expr_as(
            Expr::cust(format!("COUNT(*) FILTER (WHERE {population_sd})::bigint")),
            Alias::new("population_signature_holds"),
        )
        .from_as(Alias::new("replicate_audit_holds"), hold.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds2.clone(),
            Expr::cust("ds2.id = h.stream_id"),
        )
        .and_where(Expr::cust("ds2.site_parameter_id = sp.id"))
        .and_where(Expr::cust(format!(
            "h.kind = '{}'",
            HoldKind::ReplicateStats.as_str()
        )))
        .and_where(Expr::cust(format!(
            "h.status IN {}",
            HoldStatus::sql_list(&HoldStatus::OPEN)
        )))
        .take();

    let mut undeclared_slots = Condition::all()
        .add(Expr::col((sp.clone(), site_parameters::Column::SdEstimator)).is_null());
    if let Some(confine) = project_filter(&scope, (st.clone(), sites::Column::ProjectId)) {
        undeclared_slots = undeclared_slots.add(confine);
    }

    let on_true = || Condition::all().add(Expr::cust("true"));
    let (sql, values) = SeaQuery::select()
        .columns([
            (sp.clone(), site_parameters::Column::SiteId),
            (sp.clone(), site_parameters::Column::ParameterId),
        ])
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::Id)),
            Alias::new("site_parameter_id"),
        )
        .expr_as(
            Expr::col((st.clone(), sites::Column::Name)),
            Alias::new("site_name"),
        )
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Name)),
            Alias::new("parameter_name"),
        )
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Code)),
            Alias::new("parameter_code"),
        )
        .column((u.clone(), Alias::new("undeclared_samples")))
        .expr_as(
            Expr::cust("COALESCE(s.source_reports_sd, false)"),
            Alias::new("source_reports_sd"),
        )
        .expr_as(
            Expr::cust("COALESCE(s.streams, '[]'::jsonb)"),
            Alias::new("streams"),
        )
        .expr_as(
            Expr::cust("COALESCE(h.open_holds, 0)"),
            Alias::new("open_holds"),
        )
        .expr_as(
            Expr::cust("COALESCE(h.population_signature_holds, 0)"),
            Alias::new("population_signature_holds"),
        )
        .from_as(site_parameters::Entity, sp.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            st.clone(),
            Expr::col((st.clone(), sites::Column::Id))
                .equals((sp.clone(), site_parameters::Column::SiteId)),
        )
        .join_as(
            JoinType::InnerJoin,
            parameters::Entity,
            p.clone(),
            Expr::col((p.clone(), parameters::Column::Id))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .join_lateral(
            JoinType::InnerJoin,
            undeclared,
            u.clone(),
            Condition::all().add(Expr::cust("u.undeclared_samples > 0")),
        )
        .join_lateral(JoinType::LeftJoin, sources, s_.clone(), on_true())
        .join_lateral(JoinType::LeftJoin, holds, h.clone(), on_true())
        .cond_where(undeclared_slots)
        .order_by_expr(
            Expr::cust("COALESCE(h.population_signature_holds, 0)"),
            Order::Desc,
        )
        .order_by_expr(Expr::cust("u.undeclared_samples"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let slots = UndeclaredEstimatorSlot::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(&app_state.db)
    .await?;

    Ok(Json(UndeclaredEstimatorsResponse {
        total_slots: slots.len(),
        total_undeclared_samples: slots.iter().map(|s| s.undeclared_samples).sum(),
        total_population_signature_holds: slots.iter().map(|s| s.population_signature_holds).sum(),
        slots,
    }))
}

// --- The slot merge action ---

/// Merge two `site_parameters`, absorb `source` into `target`. Moves every slot-keyed table's rows
/// (readings, status events, samples, annotations) and the streams feeding the slot, then deletes
/// the source row. All or nothing: the whole move is one transaction. Requires `write_metadata`.
///
/// Refused with 409 when source and target both hold a grab sample at the same instant: merging two
/// separately collected groups would rewrite the survivor's stored mean, sd and n.
#[utoipa::path(
    post,
    path = "/api/actions/merge_site_parameters",
    request_body = MergeSiteParametersRequest,
    responses(
        (status = 200, description = "Counts of moved rows and source deletion status", body = MergeSiteParametersResponse),
        (status = 403, description = "Either slot is outside the caller's projects"),
        (status = 404, description = "Source or target not found"),
        (status = 409, description = "Source and target hold a sample at the same instant"),
    ),
    tag = "actions"
)]
pub async fn merge_site_parameters_handler(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<MergeSiteParametersRequest>,
) -> AppResult<Json<serde_json::Value>> {
    // Both slots must be in scope: absorbing one slot into another is a write to both sides, so a
    // merge spanning two projects is a cross-project write even when one side is granted. Refuse
    // before enqueueing, otherwise a refused request leaves a job that performs the merge anyway.
    for site_parameter_id in [
        payload.source_site_parameter_id,
        payload.target_site_parameter_id,
    ] {
        let row = project_of_site_parameter(&state.db, site_parameter_id).await?;
        require_target_in_scope(&scope, &row, Unowned::Deny, "site parameter")?;
    }

    // Background the multi-table move on the worker pool; the job's `detail` carries the counts the
    // UI used to read synchronously. Alarm reconcile runs on job completion (central lifecycle).
    let trigger_id = payload.source_site_parameter_id;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "merge_site_parameters",
        None,
        Some(trigger_id),
        &serde_json::json!({
            "source_site_parameter_id": payload.source_site_parameter_id,
            "target_site_parameter_id": payload.target_site_parameter_id,
            "actor": crate::common::actor::label(&auth),
            "origin": auth.origin().as_str(),
        }),
        None,
    )
    .await?;
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}
