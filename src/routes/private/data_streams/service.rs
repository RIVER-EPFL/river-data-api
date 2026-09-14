//! Stream queries: the entity's CRUD hooks, the slot's declared decimals, the replicate-family
//! lookups, and the row moves a slot's retirement and reassignment run.

use crudcrate::{ApiError, CRUDOperations};
use sea_orm::prelude::DateTimeWithTimeZone;
use sea_orm::sea_query::{
    Alias, Condition, Expr, JoinType, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, ExprTrait, FromQueryResult,
    Order, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait, UpdateMany,
};
use uuid::Uuid;

use super::models::{
    self, DataStream, METADATA_KEY, MoveScope, Release, ReplicateSpec, SLOT_TABLES, SlotMove,
    SlotRows, SlotScope,
};
use crate::common::bulk_write::{self, TouchedRange};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings;
use crate::routes::private::readings::models as readings_model;
use crate::routes::private::site_parameters;

pub struct DataStreamOperations;

/// The stream's registered replicate-family key, when it has one.
async fn family_key<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<Option<String>, ApiError> {
    let row = super::models::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(ApiError::database)?;
    Ok(row
        .filter(|r| r.metadata.get(METADATA_KEY).is_some())
        .map(|r| r.source_key))
}

impl CRUDOperations for DataStreamOperations {
    type Resource = DataStream;

    /// A replicate family stays classified 'spot' through entity CRUD as well: `/streams/register`
    /// and `/streams/retag` already refuse it, and a plain PATCH must not be the one route that
    /// can move the column. Clearing it (NULL) is refused too, the classification would then fall
    /// through to the owning sensor's data_frequency and can resolve continuous.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<DataStream as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(new_value) = &data.measurement_type
            && new_value.as_deref() != Some("spot")
            && let Some(key) = family_key(db, id).await?
        {
            let target = new_value.as_deref().unwrap_or("NULL");
            return Err(ApiError::bad_request(format!(
                "stream '{key}' declares a replicate family and must stay classified 'spot', \
                 not '{target}'. The continuous aggregates roll up only non-spot rows at \
                 replicate index 0, so a family outside 'spot' loses every replicate but one \
                 from every rollup"
            )));
        }
        Ok(())
    }

    /// The replicate assignments, read out of the metadata the stream already carries.
    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        entity: &mut DataStream,
    ) -> Result<(), ApiError> {
        entity.replicates = super::service::ReplicateSpec::from_metadata(&entity.metadata)
            .map(|spec| spec.column_assignments());
        Ok(())
    }
}

/// The key a stream's declared decimal places are stored under in `data_streams.metadata`.
pub const DECIMAL_PLACES_KEY: &str = "decimal_places";

/// The decimal places the stream's source declared at registration, if any.
#[must_use]
pub fn declared_decimal_places(metadata: &serde_json::Value) -> Option<i16> {
    metadata
        .get(DECIMAL_PLACES_KEY)
        .and_then(serde_json::Value::as_i64)
        .and_then(|n| i16::try_from(n).ok())
}

/// Write a declaration onto a slot that has none. A slot's own declaration is an operator's and
/// is never overwritten. Returns whether the slot was written.
pub async fn declare_slot_decimal_places<C: sea_orm::ConnectionTrait>(
    db: &C,
    site_parameter_id: Uuid,
    decimal_places: Option<i16>,
) -> Result<bool, sea_orm::DbErr> {
    let Some(places) = decimal_places else {
        return Ok(false);
    };
    let written = db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE site_parameters SET decimal_places = $1, updated_at = NOW() \
             WHERE id = $2 AND decimal_places IS NULL",
            [places.into(), site_parameter_id.into()],
        ))
        .await?
        .rows_affected();
    Ok(written > 0)
}

/// Get or create an "api" stream for a given (site_id, parameter_id) pair.
///
/// Used by batch insert endpoints to assign a stream_id to API-submitted readings.
/// Upserts on (source_system="api", source_key="{site_id}:{parameter_id}").
/// The slot a (site, parameter) pair names, or `None` when the parameter is not assigned to the
/// site. It is the one place a reading's attribution comes from.
pub async fn site_parameter_of(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    Ok(site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .filter(site_parameters::Column::ParameterId.eq(parameter_id))
        .one(db)
        .await?
        .map(|row| row.id))
}

pub async fn get_or_create_api_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    // Try to find existing
    if let Some(stream) = models::Entity::find()
        .filter(models::Column::SourceSystem.eq("api"))
        .filter(models::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        crate::routes::private::sensors::service::ensure_channel_instrument(
            db,
            &stream,
            site_id,
            parameter_id,
            "API entry",
        )
        .await?;
        return Ok(stream.id);
    }

    // Create new
    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let site_parameter_id = site_parameter_of(db, site_id, parameter_id).await?;
    let active_model = models::ActiveModel {
        id: Set(id),
        source_system: Set("api".to_string()),
        source_key: Set(source_key.clone()),
        source_name: Set(Some("API batch insert".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        // Paired on creation: this channel exists to carry one slot's readings, and attribution is
        // read from the pairing rather than restated per row. A slot that has no `site_parameters`
        // row yet leaves the stream unpaired, like any other undiscovered channel.
        site_parameter_id: Set(site_parameter_id),
        paired_at: Set(site_parameter_id.map(|_| now.into())),
        sensor_id: Set(None),
        measurement_type: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        last_data_time: Set(None),
        last_window_digest: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    models::Entity::insert(active_model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                models::Column::SourceSystem,
                models::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    // Re-fetch in case of race condition (ON CONFLICT DO NOTHING returns no id)
    let stream = models::Entity::find()
        .filter(models::Column::SourceSystem.eq("api"))
        .filter(models::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create API stream".to_string()))?;

    // The channel carries an instrument from the moment it exists, so nothing written through it
    // can land without one.
    crate::routes::private::sensors::service::ensure_channel_instrument(
        db,
        &stream,
        site_id,
        parameter_id,
        "API entry",
    )
    .await?;

    Ok(stream.id)
}

/// Why this source's streams may not be paired yet, or `None` when they may.
///
/// NOMIS reports a plain date and a plain time with no zone column, unlike CNET and METALP, and
/// the connector reads them as UTC (`nomis/mod.rs`, `parse_nomis_datetime`). ADR 0004 records that
/// as an assumption to be confirmed before any NOMIS data is paired: if the columns are Valais
/// wall clock, every NOMIS grab lands one or two hours off, attaches to the wrong collection event
/// and is compared against a sensor window shifted by the same amount, with nothing on the reading
/// saying so. Pairing is where that becomes visible data, so it is refused until the question is
/// answered rather than guarded further downstream.
#[must_use]
pub fn pairing_refusal(source_system: &str) -> Option<String> {
    (source_system.eq_ignore_ascii_case("nomis")).then(|| {
        "NOMIS streams cannot be paired yet: the portal reports a date and a time with no zone, \
         and whether they are UTC or Valais wall clock is unconfirmed (ADR 0004). Pairing one \
         would attribute every grab to a timestamp that may be one or two hours off."
            .to_string()
    })
}

/// Refuse a declaration that cannot describe a replicate family. Two or more members, no
/// duplicates, and the stream must be classified spot: sample formation is spot-only, so a
/// continuous stream declaring replicates would silently never form the samples the spec promises.
pub fn validate_declaration(
    declared: &river_data_core::models::ReplicateSpec,
    stream_measurement_type: Option<&str>,
) -> AppResult<()> {
    if declared.source_columns.len() < 2 {
        return Err(AppError::BadRequest(
            "a replicate spec needs at least two source columns; a single-column stream \
             carries no replicates to declare"
                .to_string(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for col in &declared.source_columns {
        if col.trim().is_empty() {
            return Err(AppError::BadRequest(
                "replicate source columns cannot be empty".to_string(),
            ));
        }
        if !seen.insert(col.as_str()) {
            return Err(AppError::BadRequest(format!(
                "replicate source column '{col}' is listed twice"
            )));
        }
    }
    if stream_measurement_type != Some(crate::routes::private::readings::service::SPOT) {
        return Err(AppError::BadRequest(
            "a stream declaring replicates must be classified 'spot': samples only form from \
             spot readings"
                .to_string(),
        ));
    }
    Ok(())
}

/// Merge an incoming column declaration onto the stored authoritative mapping.
///
/// A known column keeps its stored index regardless of incoming order, so an upstream reorder is
/// a no-op. A genuinely new column appends after the highest index ever assigned. A column absent
/// from the incoming declaration is retired in place, its index never reused; a retired column
/// that reappears reactivates at its stored index (its identity was never lost). A registration
/// that removes an active column and introduces an unknown one in the same step is refused: a
/// rename is indistinguishable from remove-plus-add, and guessing either way silently re-indexes
/// stored readings, so the conflict is surfaced for operator action instead.
pub fn pin_assignments(
    prior: Option<&ReplicateSpec>,
    incoming: &[String],
) -> AppResult<Vec<super::models::ColumnAssignment>> {
    let mut stored = prior
        .map(ReplicateSpec::column_assignments)
        .unwrap_or_default();
    let incoming_set: std::collections::HashSet<&str> =
        incoming.iter().map(String::as_str).collect();
    let known: std::collections::HashSet<&str> = stored.iter().map(|a| a.column.as_str()).collect();

    let added: Vec<&String> = incoming
        .iter()
        .filter(|c| !known.contains(c.as_str()))
        .collect();
    let removed: Vec<&str> = stored
        .iter()
        .filter(|a| !a.retired && !incoming_set.contains(a.column.as_str()))
        .map(|a| a.column.as_str())
        .collect();
    if !added.is_empty() && !removed.is_empty() {
        return Err(AppError::Conflict(format!(
            "ambiguous replicate re-registration: column(s) {} disappeared while {} appeared in \
             the same step. A rename is indistinguishable from remove-plus-add, and either guess \
             would re-index stored readings. Register the removal and the addition separately, \
             or resolve the rename by hand",
            removed.join(", "),
            added
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    for assignment in &mut stored {
        assignment.retired = !incoming_set.contains(assignment.column.as_str());
    }
    let base = stored.iter().map(|a| a.index).max().map_or(0, |m| m + 1);
    for (offset, column) in added.into_iter().enumerate() {
        stored.push(super::models::ColumnAssignment {
            column: column.clone(),
            index: base.saturating_add(i16::try_from(offset).unwrap_or(i16::MAX)),
            retired: false,
        });
    }
    Ok(stored)
}

/// The `source_key` column of a family scan. `data_streams.source_key` is NOT NULL, so a row that
/// does not decode is a family missing from a set that decides whether a retag is refused, not a
/// stream without a key.
fn source_keys(rows: &[sea_orm::QueryResult]) -> AppResult<Vec<String>> {
    rows.iter()
        .map(|r| {
            r.try_get::<String>("", "source_key")
                .map_err(AppError::from)
        })
        .collect()
}

/// The `source_key`s of the replicate families in a stream selection, matching the selection
/// `/streams/retag` updates.
pub async fn family_keys_in_streams<C: ConnectionTrait>(
    db: &C,
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> AppResult<Vec<String>> {
    let mut selected = Condition::any().add(Expr::cust_with_values(
        "s.id = ANY($1)",
        [stream_ids.to_vec()],
    ));
    if let Some(system) = source_system {
        selected = selected
            .add(Expr::col((Alias::new("s"), models::Column::SourceSystem)).eq(system.to_string()));
    }
    let (sql, values) = family_keys_query(selected).build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    source_keys(&rows)
}

/// The declared replicate families among the streams `reached` selects, by source key.
fn family_keys_query(reached: Condition) -> SelectStatement {
    let s = Alias::new("s");
    SeaQuery::select()
        .column((s.clone(), models::Column::SourceKey))
        .from_as(models::Entity, s.clone())
        .and_where(Expr::cust_with_values(
            "s.metadata -> $1 IS NOT NULL",
            [METADATA_KEY],
        ))
        .cond_where(reached)
        .order_by((s, models::Column::SourceKey), Order::Asc)
        .take()
}

/// Streams whose readings carry one of these sensors, which attribution backfill sets without
/// touching `data_streams.sensor_id`.
fn readings_of_sensors(sensor_ids: &[Uuid]) -> Expr {
    Expr::exists(
        SeaQuery::select()
            .expr(Expr::value(1))
            .from_as(readings_model::Entity, Alias::new("r"))
            .and_where(Expr::cust("r.stream_id = s.id"))
            .and_where(Expr::cust_with_values(
                "r.sensor_id = ANY($1)",
                [sensor_ids.to_vec()],
            ))
            .take(),
    )
}

/// The `source_key`s of the replicate families these sensors reach: streams the sensor owns, and
/// streams whose readings carry the sensor_id directly (attribution backfill sets it without
/// touching `data_streams.sensor_id`), matching the retag job's own scope.
pub async fn family_keys_for_sensors<C: ConnectionTrait>(
    db: &C,
    sensor_ids: &[Uuid],
) -> AppResult<Vec<String>> {
    let (sql, values) = family_keys_query(
        Condition::any()
            .add(Expr::cust_with_values(
                "s.sensor_id = ANY($1)",
                [sensor_ids.to_vec()],
            ))
            .add(readings_of_sensors(sensor_ids)),
    )
    .build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    source_keys(&rows)
}

/// The `source_key`s of the replicate families a `measurement_retag` scope reaches, mirroring the
/// job's own readings predicate: streams named directly, streams of the source system, streams
/// the sensors own, and streams whose readings carry a scoped sensor_id.
pub async fn family_keys_in_retag_scope<C: ConnectionTrait>(
    db: &C,
    sensor_ids: &[Uuid],
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> AppResult<Vec<String>> {
    let mut reached = Condition::any()
        .add(Expr::cust_with_values(
            "s.id = ANY($1)",
            [stream_ids.to_vec()],
        ))
        .add(Expr::cust_with_values(
            "s.sensor_id = ANY($1)",
            [sensor_ids.to_vec()],
        ))
        .add(readings_of_sensors(sensor_ids));
    if let Some(system) = source_system {
        reached = reached
            .add(Expr::col((Alias::new("s"), models::Column::SourceSystem)).eq(system.to_string()));
    }
    let (sql, values) = family_keys_query(reached).build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    source_keys(&rows)
}

/// A replicate family stays classified spot for the same reason one may not be registered any
/// other way: the continuous aggregates roll up only non-spot rows at `replicate_index = 0`, so a
/// family outside spot would have every replicate but one dropped from every rollup.
pub fn refuse_family_retag(keys: &[String], target: &str) -> AppResult<()> {
    if keys.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "these streams declare replicate families and must stay classified 'spot', not \
         '{target}': {}. The continuous aggregates roll up only non-spot rows at replicate \
         index 0, so a family outside 'spot' loses every replicate but one from every rollup",
        keys.join(", ")
    )))
}

/// The stream as the API serves it, with the replicate assignments read out of its metadata.
/// The CRUD reads get this through `after_get_one`; the custom stream views call it directly.
#[must_use]
pub fn with_assignments(model: super::models::Model) -> super::DataStream {
    let replicates = ReplicateSpec::from_metadata(&model.metadata).map(|s| s.column_assignments());
    let mut stream = super::DataStream::from(model);
    stream.replicates = replicates;
    stream
}

/// The divisor the pairing will serve for this stream, resolved as the write path resolves it:
/// the stream's registered spec, then the slot's declaration once it is paired, else the
/// undeclared fallback. An unpaired stream has no slot to read, which is the usual case here.
pub(super) async fn preview_estimator(
    db: &DatabaseConnection,
    stream: &models::Model,
) -> AppResult<crate::routes::private::readings::service::Resolved> {
    let spec = super::service::ReplicateSpec::from_metadata(&stream.metadata)
        .and_then(|spec| spec.declared.sd_estimator);
    let spec = crate::routes::private::readings::service::parse_opt(spec.as_deref())?;
    if let Some(estimator) = spec {
        return Ok(crate::routes::private::readings::service::Resolved {
            estimator,
            source: crate::routes::private::readings::service::Source::Stream,
        });
    }
    let Some(site_parameter_id) = stream.site_parameter_id else {
        return Ok(crate::routes::private::readings::service::Resolved::undeclared());
    };
    let slot = site_parameters::Entity::find_by_id(site_parameter_id)
        .one(db)
        .await?;
    let Some(slot) = slot else {
        return Ok(crate::routes::private::readings::service::Resolved::undeclared());
    };
    Ok(
        match crate::routes::private::readings::service::slot_declaration(
            db,
            slot.site_id,
            slot.parameter_id,
        )
        .await?
        {
            Some(estimator) => crate::routes::private::readings::service::Resolved {
                estimator,
                source: crate::routes::private::readings::service::Source::Slot,
            },
            None => crate::routes::private::readings::service::Resolved::undeclared(),
        },
    )
}

/// The stored shapes the hand mappings above read. Derived, so a column added to a query and not
/// to its reader is a compile error rather than a field left at its default.
#[derive(FromQueryResult)]
pub(super) struct PreviewRow {
    pub(super) time: chrono::DateTime<chrono::FixedOffset>,
    pub(super) replicate_index: i16,
    pub(super) value: Option<f64>,
    pub(super) is_flagged: bool,
    pub(super) withdrawn: bool,
}

/// A stream with no readings at all returns no row, which is zero of everything.
#[derive(FromQueryResult, Default)]
pub(super) struct StoredStreamStats {
    pub(super) count: i64,
    pub(super) withdrawn: i64,
    pub(super) min_time: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub(super) max_time: Option<chrono::DateTime<chrono::FixedOffset>>,
}

/// The stream's stored extent: how many readings it holds, how many of those are withdrawn, and
/// the span they cover.
pub(super) fn stream_stats_query(stream_id: Uuid) -> sea_orm::Select<readings::Entity> {
    readings::Entity::find()
        .select_only()
        .column_as(readings::Column::Time.count(), "count")
        .expr_as(
            Expr::cust("COUNT(*) FILTER (WHERE withdrawn_at IS NOT NULL)"),
            "withdrawn",
        )
        .column_as(readings::Column::Time.min(), "min_time")
        .column_as(readings::Column::Time.max(), "max_time")
        .filter(readings::Column::StreamId.eq(stream_id))
}

/// The stream's most recent raw value.
pub(super) fn latest_raw_value_query(stream_id: Uuid) -> sea_orm::Select<readings::Entity> {
    readings::Entity::find()
        .select_only()
        .column(readings::Column::RawValue)
        .filter(readings::Column::StreamId.eq(stream_id))
        .order_by_desc(readings::Column::Time)
        .limit(1)
}

/// Every replicate at the stream's newest `limit` instants, newest first. The instants are chosen
/// first so a limit counts instants rather than replicates.
pub(super) fn preview_query(stream_id: Uuid, limit: u64) -> SelectStatement {
    let instants = SeaQuery::select()
        .distinct()
        .column(readings::Column::Time)
        .from(readings::Entity)
        .and_where(readings::Column::StreamId.eq(stream_id))
        .order_by(readings::Column::Time, Order::Desc)
        .limit(limit)
        .take();
    let t = Alias::new("t");
    SeaQuery::select()
        .column((readings::Entity, readings::Column::Time))
        .column((readings::Entity, readings::Column::ReplicateIndex))
        .expr_as(
            Expr::cust("COALESCE(readings.calibrated_value, readings.raw_value)"),
            "value",
        )
        .expr_as(
            Expr::cust("COALESCE(readings.is_flagged, false)"),
            "is_flagged",
        )
        .expr_as(Expr::cust("readings.withdrawn_at IS NOT NULL"), "withdrawn")
        .from(readings::Entity)
        .join_subquery(
            sea_orm::JoinType::Join,
            instants,
            t.clone(),
            Expr::col((t, readings::Column::Time))
                .equals((readings::Entity, readings::Column::Time)),
        )
        .and_where(readings::Column::StreamId.eq(stream_id))
        .order_by((readings::Entity, readings::Column::Time), Order::Desc)
        .order_by(
            (readings::Entity, readings::Column::ReplicateIndex),
            Order::Asc,
        )
        .take()
}

/// Timestamps at which moving the source's rows onto `target_param` would violate a slot table's
/// unique constraint, at most `LIMIT` of them. Empty when the move is safe.
///
/// Resolving a collision by merging the two rows would rewrite a stored measurement statistic
/// (`samples.mean`/`sd`/`n` are computed over one collection group), so callers refuse instead.
pub async fn slot_move_collisions<C: ConnectionTrait>(
    conn: &C,
    scope: MoveScope,
    source_param: Uuid,
    target_param: Uuid,
) -> AppResult<Vec<String>> {
    let mut collisions = Vec::new();
    for slot in SLOT_TABLES {
        let Some(unique_with) = slot.unique_with else {
            continue;
        };
        let src = Alias::new("src");
        let dst = Alias::new("dst");
        let key = Alias::new(unique_with);
        let mut same_slot = Condition::all()
            .add(Expr::col((src.clone(), Alias::new("parameter_id"))).eq(source_param));
        if let MoveScope::Site(site_id) = scope {
            same_slot = same_slot.add(Expr::col((src.clone(), Alias::new("site_id"))).eq(site_id));
        }
        let (sql, values) = SeaQuery::select()
            .distinct()
            .expr_as(
                Expr::cust(format!("src.{unique_with}::text")),
                Alias::new("value"),
            )
            .from_as(slot.rows.table_ref(), src.clone())
            .join_as(
                JoinType::InnerJoin,
                slot.rows.table_ref(),
                dst.clone(),
                Condition::all()
                    .add(
                        Expr::col((dst.clone(), Alias::new("site_id")))
                            .equals((src.clone(), Alias::new("site_id"))),
                    )
                    .add(Expr::col((dst.clone(), key.clone())).equals((src.clone(), key.clone())))
                    .add(Expr::col((dst.clone(), Alias::new("parameter_id"))).eq(target_param)),
            )
            .cond_where(same_slot)
            .order_by(Alias::new("value"), Order::Asc)
            .limit(20)
            .take()
            .build(PostgresQueryBuilder);
        for row in conn
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                values,
            ))
            .await?
        {
            let value: String = row.try_get("", "value")?;
            collisions.push(format!("{} {value}", slot.rows.name()));
        }
    }
    Ok(collisions)
}

/// Re-point every slot-keyed row from `source_param` onto `target_param`.
///
/// Runs inside the caller's guarded transaction. Call [`slot_move_collisions`] first: a table with
/// a `unique_with` column can refuse the move mid-way otherwise.
pub async fn move_slot_rows<C: ConnectionTrait>(
    conn: &C,
    scope: MoveScope,
    source_param: Uuid,
    target_param: Uuid,
    actor: &str,
    origin: crate::routes::private::readings::models::Origin,
) -> AppResult<SlotMove> {
    let site = match scope {
        MoveScope::EverySite => None,
        MoveScope::Site(site_id) => Some(site_id),
    };
    let mut moved = SlotMove::default();

    // Every reading re-pointed is a slot-move decision (ADR 0008), recorded before the move so
    // the record holds the slot it came from.
    {
        let moved = {
            use crate::routes::private::collection_events::flows::row;
            use crate::routes::private::readings::models::Column;
            sea_orm::Condition::all()
                .add(row(Column::ParameterId).eq(source_param))
                .add_option(site.map(|site_id| row(Column::SiteId).eq(site_id)))
        };
        crate::routes::private::readings::service::record_many(
            conn,
            crate::routes::private::readings::models::Kind::SlotMove,
            moved,
            crate::routes::private::readings::service::NewValue::Literal(
                serde_json::json!({ "parameter_id": target_param }),
            ),
            actor,
            Some("merged into the target parameter"),
            origin,
            Some(Uuid::new_v4()),
        )
        .await?;
    }

    let mut on_source =
        Condition::all().add(Expr::col(Alias::new("parameter_id")).eq(source_param));
    if let Some(site_id) = site {
        on_source = on_source.add(Expr::col(Alias::new("site_id")).eq(site_id));
    }
    for slot in SLOT_TABLES {
        let statement = SeaQuery::update()
            .table(slot.rows.table_ref())
            .value(Alias::new("parameter_id"), target_param)
            .cond_where(on_source.clone())
            .take();

        let rows = if slot.timed {
            let touched = bulk_write::mutation(conn, statement).await?;
            if slot.feeds_rollups {
                moved.touched = moved.touched.merge(touched);
            }
            touched.rows
        } else {
            let (sql, values) = statement.build(PostgresQueryBuilder);
            conn.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                values,
            ))
            .await?
            .rows_affected()
        };

        match slot.rows {
            SlotRows::Readings => moved.readings = rows,
            SlotRows::StatusEvents => moved.status_events = rows,
            _ => {}
        }
    }

    Ok(moved)
}

/// The rows one [`SlotScope`] addresses. The condition names columns every slot table carries,
/// so one selection serves each of them.
pub(super) struct RetireTarget {
    pub(super) rows: Condition,
    /// The slot itself is going away, so the streams pointing at it are unpaired too.
    pub(super) site_parameter_id: Option<Uuid>,
}

pub(super) async fn resolve_retire_target<C: ConnectionTrait>(
    conn: &C,
    scope: SlotScope,
) -> AppResult<Option<RetireTarget>> {
    match scope {
        SlotScope::Stream(stream_id) => Ok(Some(RetireTarget {
            rows: Condition::all().add(Expr::col(Alias::new("stream_id")).eq(stream_id)),
            site_parameter_id: None,
        })),
        SlotScope::SiteParameter(sp_id) => {
            let Some(row) = site_parameters::Entity::find_by_id(sp_id).one(conn).await? else {
                return Ok(None);
            };
            Ok(Some(RetireTarget {
                rows: Condition::all()
                    .add(Expr::col(Alias::new("site_id")).eq(row.site_id))
                    .add(Expr::col(Alias::new("parameter_id")).eq(row.parameter_id)),
                site_parameter_id: Some(sp_id),
            }))
        }
    }
}

pub(super) async fn release_slot_rows<C: ConnectionTrait>(
    conn: &C,
    target: &RetireTarget,
) -> AppResult<TouchedRange> {
    let sample_ids = referenced_ids(conn, target, "sample_id").await?;
    let event_ids = referenced_ids(conn, target, "collection_event_id").await?;
    let mut touched = TouchedRange::default();

    for slot in SLOT_TABLES {
        match slot.release {
            Release::Retain => {}
            Release::Unattribute(columns) => {
                // Narrow to rows that still carry something to release: an UPDATE that rewrites
                // already-null rows maximises what it has to decompress and changes nothing.
                let mut carries = Condition::any();
                let mut statement = SeaQuery::update();
                statement.table(slot.rows.table_ref());
                for c in columns {
                    statement.value(Alias::new(*c), Expr::value(Option::<Uuid>::None));
                    carries = carries.add(Expr::col(Alias::new(*c)).is_not_null());
                }
                let statement = statement
                    .cond_where(target.rows.clone().add(carries))
                    .take();
                if slot.timed {
                    let range = bulk_write::mutation(conn, statement).await?;
                    if slot.feeds_rollups {
                        touched = touched.merge(range);
                    }
                } else {
                    let (sql, values) = statement.build(PostgresQueryBuilder);
                    conn.execute_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        sql,
                        values,
                    ))
                    .await?;
                }
            }
            Release::DeleteWhenOrphaned => {
                if sample_ids.is_empty() {
                    continue;
                }
                let (sql, values) = SeaQuery::delete()
                    .from_table(slot.rows.table_ref())
                    .and_where(Expr::cust_with_values("id = ANY($1)", [sample_ids.clone()]))
                    .and_where(
                        Expr::exists(
                            SeaQuery::select()
                                .expr(Expr::value(1))
                                .from_as(readings_model::Entity, Alias::new("r"))
                                .and_where(Expr::cust(format!(
                                    "r.sample_id = {}.id",
                                    slot.rows.name()
                                )))
                                .take(),
                        )
                        .not(),
                    )
                    .take()
                    .build(PostgresQueryBuilder);
                conn.execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    sql,
                    values,
                ))
                .await?;
            }
        }
    }

    // A visit describes a group of readings the same way a sample does, but it is keyed on
    // (site, collected_at) rather than on the slot, so it is released here rather than through
    // SLOT_TABLES: nothing that walks that list by parameter_id can address it.
    if !event_ids.is_empty() {
        let (sql, values) = SeaQuery::delete()
            .from_table(crate::routes::private::collection_events::models::Entity)
            .and_where(Expr::cust_with_values("id = ANY($1)", [event_ids]))
            .and_where(
                Expr::exists(
                    SeaQuery::select()
                        .expr(Expr::value(1))
                        .from_as(readings_model::Entity, Alias::new("r"))
                        .and_where(Expr::cust("r.collection_event_id = collection_events.id"))
                        .take(),
                )
                .not(),
            )
            .take()
            .build(PostgresQueryBuilder);
        conn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    }

    if let Some(sp_id) = target.site_parameter_id {
        // Load-bearing rather than tidy-up: `data_streams.site_parameter_id` has no ON DELETE
        // clause, so the row cannot be deleted while a stream points at it.
        models::Entity::update_many()
            .col_expr(
                models::Column::SiteParameterId,
                Expr::value(Option::<Uuid>::None),
            )
            .col_expr(
                models::Column::PairedAt,
                Expr::value(Option::<chrono::DateTime<chrono::Utc>>::None),
            )
            .col_expr(models::Column::UpdatedAt, Expr::current_timestamp())
            .filter(models::Column::SiteParameterId.eq(sp_id))
            .exec(conn)
            .await?;
    }

    Ok(touched)
}

/// Rows the scope's readings point at through `column`, read before the readings lose it.
async fn referenced_ids<C: ConnectionTrait>(
    conn: &C,
    target: &RetireTarget,
    column: &str,
) -> AppResult<Vec<Uuid>> {
    let col = Alias::new(column);
    let (sql, values) = SeaQuery::select()
        .distinct()
        .expr_as(Expr::col(col.clone()), Alias::new("id"))
        .from(readings_model::Entity)
        .cond_where(target.rows.clone().add(Expr::col(col).is_not_null()))
        .take()
        .build(PostgresQueryBuilder);
    Ok(conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .iter()
        .map(|row| row.try_get::<Uuid>("", "id"))
        .collect::<Result<Vec<_>, _>>()?)
}

/// The claim a pairing makes on a stream.
///
/// `site_parameter_id IS NULL` is what makes it a claim rather than an overwrite: of two requests
/// pairing one stream, only the first affects a row, and the loser reads `rows_affected() == 0`
/// and is refused. The predicate is a filter rather than a string so that dropping it, or renaming
/// the column, is a compile error.
pub fn claim_stream(
    stream_id: Uuid,
    site_parameter_id: Uuid,
    now: DateTimeWithTimeZone,
) -> UpdateMany<models::Entity> {
    models::Entity::update_many()
        .col_expr(
            models::Column::SiteParameterId,
            Expr::value(Some(site_parameter_id)),
        )
        .col_expr(models::Column::PairedAt, Expr::value(Some(now)))
        .col_expr(models::Column::UpdatedAt, Expr::value(now))
        .filter(models::Column::Id.eq(stream_id))
        .filter(models::Column::SiteParameterId.is_null())
}

/// Move a stream's ingest cursor forward, never back: a source that re-publishes an older instant
/// leaves the cursor where it is.
pub fn advance_cursor(stream_id: Uuid, newest: DateTimeWithTimeZone) -> UpdateMany<models::Entity> {
    models::Entity::update_many()
        .col_expr(
            models::Column::LastDataTime,
            Expr::cust_with_exprs(
                "GREATEST(COALESCE($1, $2), $2)",
                [Expr::col(models::Column::LastDataTime), Expr::value(newest)],
            ),
        )
        .col_expr(models::Column::UpdatedAt, Expr::current_timestamp())
        .filter(models::Column::Id.eq(stream_id))
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
