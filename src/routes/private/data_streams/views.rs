use axum::{
    Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::{get, post},
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};
use uuid::Uuid;

use super::flows::retire_slot;
use super::models::receipts;
use super::models::{
    PairStreamRequest, PairStreamResponse, PreviewInstant, PreviewReplicate, ReceiptRow,
    ReceiptsQuery, ReceiptsResponse, RegisterStreamRequest, RetagStreamsRequest,
    RetagStreamsResponse, SlotScope, StreamPreviewResponse, StreamStatsResponse,
    UnpairStreamResponse,
};
use super::service::{
    PreviewRow, StoredStreamStats, latest_raw_value_query, preview_query, stream_stats_query,
};
use crate::common::AppState;
use crate::common::bulk_write;
use crate::common::middleware::ProjectScope;
use crate::common::paging::Window;
use crate::common::scope;
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::DataStream;
use crate::routes::private::sensors;
use crate::routes::private::sensors::service::{
    close_sensor_deployment, create_sensor_for_stream, extract_vaisala_device_serial,
};
use crate::routes::private::sync::service as sync_service;
use crate::routes::private::{data_streams, site_parameters};

/// The most recent instants a stream holds, as the replicate rows they will be served as.
///
/// The routing block tells an operator which column becomes which replicate index; this shows the
/// same thing with the stream's own values in it, which is the difference between reading a
/// mapping and seeing what pairing will do. Readings exist before pairing (an unpaired stream
/// stores its data unattributed), so this works at review time. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/streams/{id}/preview",
    params(
        ("id" = Uuid, Path, description = "Stream UUID"),
        ("limit" = Option<u32>, Query, description = "Instants to return (default 3, max 20)"),
    ),
    responses(
        (status = 200, description = "Recent instants as replicate rows", body = StreamPreviewResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn stream_preview(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> AppResult<Json<StreamPreviewResponse>> {
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
    guard_stream_scope(&state, &stream, &scope).await?;

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(3)
        .clamp(1, 20);

    // The pinned column-to-index mapping, so an index is labelled with the column it came from
    // rather than left as a bare number.
    let columns: std::collections::HashMap<i16, String> =
        super::models::ReplicateSpec::from_metadata(&stream.metadata)
            .map(|spec| {
                spec.assignments
                    .into_iter()
                    .map(|a| (a.index, a.column))
                    .collect()
            })
            .unwrap_or_default();

    // The newest `limit` instants, then every replicate at those instants.
    let (sql, values) =
        preview_query(id, limit.unsigned_abs()).build(sea_orm::sea_query::PostgresQueryBuilder);
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let mut instants: Vec<PreviewInstant> = Vec::new();
    // The values each instant's sample would be computed from, beside the replicates it lists.
    let mut served: Vec<Vec<f64>> = Vec::new();
    for row in &rows {
        let row = PreviewRow::from_query_result(row, "")?;
        let time = row.time.with_timezone(&Utc);
        let replicate_index = row.replicate_index;
        let replicate = PreviewReplicate {
            replicate_index,
            column: columns.get(&replicate_index).cloned(),
            value: row.value,
            is_flagged: row.is_flagged,
            withdrawn: row.withdrawn,
        };
        match instants.last_mut() {
            Some(last) if last.time == time => last.replicates.push(replicate),
            _ => {
                instants.push(PreviewInstant {
                    time,
                    replicates: vec![replicate],
                    mean: None,
                    sd: None,
                    n: 0,
                });
                served.push(Vec::new());
            }
        }
        let counts = crate::routes::private::readings::service::counts_in_sample(
            row.is_flagged,
            row.withdrawn,
            row.unverified == Some(true),
        );
        if let (true, Some(value), Some(values)) = (counts, row.value, served.last_mut()) {
            values.push(value);
        }
    }

    // Statistics over the replicates that would be served, by the rule the samples trigger
    // applies, which is what makes the preview match the outcome.
    for (instant, values) in instants.iter_mut().zip(&served) {
        let stats = sync_service::group_stats(values);
        instant.n = stats.n;
        instant.mean = stats.mean;
        instant.sd = stats.sd;
    }

    Ok(Json(StreamPreviewResponse {
        stream_id: id,
        source_key: stream.source_key,
        instants,
    }))
}

/// A project-scoped key may only inspect a stream paired into its own project. An unpaired or
/// cross-project stream is reported as not-found (rather than 403) so a scoped key cannot even
/// confirm the existence of another project's streams.
async fn guard_stream_scope(
    state: &AppState,
    stream: &data_streams::Model,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<()> {
    if !scope.is_restricted() {
        return Ok(());
    }
    let stream_project = match stream.site_parameter_id {
        Some(sp_id) => state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT s.project_id FROM site_parameters sp JOIN sites s ON s.id = sp.site_id WHERE sp.id = $1",
                [sp_id.into()],
            ))
            .await?
            .map(|r| r.try_get::<Option<Uuid>>("", "project_id"))
            .transpose()?
            .flatten(),
        None => None,
    };
    if !scope.allows_project_opt(stream_project) {
        return Err(AppError::NotFound("Stream not found".to_string()));
    }
    Ok(())
}

/// Reading statistics for a single data stream: count, time range, latest value.
/// Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/streams/{id}/stats",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    responses(
        (status = 200, description = "Stream statistics", body = StreamStatsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn stream_stats(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<StreamStatsResponse>> {
    // Verify stream exists
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    guard_stream_scope(&state, &stream, &scope).await?;

    let stats = stream_stats_query(id)
        .into_model::<StoredStreamStats>()
        .one(&state.db)
        .await?
        .unwrap_or_default();
    let (count, withdrawn, min_time, max_time) = (
        stats.count,
        stats.withdrawn,
        stats.min_time.map(|t| t.with_timezone(&Utc)),
        stats.max_time.map(|t| t.with_timezone(&Utc)),
    );

    let latest_value: Option<f64> = latest_raw_value_query(id)
        .into_tuple::<f64>()
        .one(&state.db)
        .await?;

    Ok(Json(StreamStatsResponse {
        stream_id: id,
        reading_count: count,
        withdrawn_count: withdrawn,
        min_time,
        max_time,
        latest_value,
    }))
}

/// The stream's windowed-ingest ledger: one row per reconciliation pass, newest first.
/// Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/streams/{id}/receipts",
    params(("id" = Uuid, Path, description = "Stream UUID"), ReceiptsQuery),
    responses(
        (status = 200, description = "Ingest receipts", body = ReceiptsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn stream_receipts(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<ReceiptsQuery>,
) -> AppResult<Json<ReceiptsResponse>> {
    let stream = data_streams::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
    if scope.is_restricted() {
        let stream_project = match stream.site_parameter_id {
            Some(sp_id) => state
                .db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT s.project_id FROM site_parameters sp JOIN sites s ON s.id = sp.site_id WHERE sp.id = $1",
                    [sp_id.into()],
                ))
                .await?
                .map(|r| r.try_get::<Option<Uuid>>("", "project_id"))
                .transpose()?
                .flatten(),
            None => None,
        };
        if !scope.allows_project_opt(stream_project) {
            return Err(AppError::NotFound("Stream not found".to_string()));
        }
    }

    let window = Window::from_page(q.page, q.page_size, 50, 200);
    let total = i64::try_from(
        receipts::Entity::find()
            .filter(receipts::Column::StreamId.eq(id))
            .count(&state.db)
            .await?,
    )
    .unwrap_or(i64::MAX);
    let rows = receipts::Entity::find()
        .filter(receipts::Column::StreamId.eq(id))
        .order_by_desc(receipts::Column::At)
        .limit(window.limit)
        .offset(window.offset)
        .all(&state.db)
        .await?;
    let receipts: Vec<ReceiptRow> = rows
        .into_iter()
        .map(|row| ReceiptRow {
            id: row.id,
            at: row.at.with_timezone(&Utc),
            window_from: row.window_from.map(|t| t.with_timezone(&Utc)),
            window_to: row.window_to.map(|t| t.with_timezone(&Utc)),
            submitted: row.submitted,
            new_rows: row.new_rows,
            changed: row.changed,
            unchanged: row.unchanged,
            retained: row.retained,
            rejected_total: row.rejected_total,
            dropped: row.dropped,
            withdrawn: row.withdrawn,
            braked: row.braked,
        })
        .collect();
    Ok(Json(ReceiptsResponse {
        stream_id: id,
        total: u64::try_from(total).unwrap_or(0),
        receipts,
    }))
}

/// Upsert a data stream by (source_system, source_key). Used by sync microservices on
/// discovery to register streams before pairing. Requires `write_metadata`. `metadata` may be
/// omitted and defaults to an empty object, which the schema, taken from the client's own struct,
/// does not say.
#[utoipa::path(
    post,
    path = "/api/streams/register",
    request_body = RegisterStreamRequest,
    responses(
        (status = 200, description = "Stream registered (created or updated)", body = DataStream),
    ),
    tag = "streams"
)]
pub async fn register_stream(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(RegisterStreamRequest(mut payload)): Json<RegisterStreamRequest>,
) -> AppResult<Json<DataStream>> {
    payload.source_system = crate::common::provenance::source_system(&payload.source_system)?;
    crate::routes::private::readings::service::validate_measurement_type(
        payload.measurement_type.as_deref(),
    )?;
    let stored = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(&payload.source_system))
        .filter(data_streams::Column::SourceKey.eq(&payload.source_key))
        .one(&state.db)
        .await?;
    if let Some(declared) = payload.replicates.clone() {
        super::service::validate_declaration(&declared, payload.measurement_type.as_deref())?;
        // The stored column-to-index mapping is authoritative and append-only: readings carry
        // their index for life, so a re-registration keeps every known column's index, appends
        // genuinely new columns, and retires absent ones without reusing their indexes. Only the
        // register path authors it, which is why the caller's declaration cannot carry one.
        let prior = stored
            .as_ref()
            .and_then(|s| super::models::ReplicateSpec::from_metadata(&s.metadata));
        let assignments =
            super::service::pin_assignments(prior.as_ref(), &declared.source_columns)?;
        super::models::ReplicateSpec {
            declared,
            assignments,
        }
        .embed(&mut payload.metadata)?;
    }
    if let Some(sensor_id) = payload.sensor_id {
        // Which instrument produced a feed is the pairing plan's decision (Q195), so a service
        // speaking for a source cannot settle it on the way past. An operator registering a stream
        // by hand is that decision being made.
        if matches!(
            auth,
            crate::common::middleware::AuthContext::SyncService { .. }
        ) {
            return Err(AppError::Forbidden(format!(
                "{} cannot declare an instrument on a stream; the pairing plan decides which \
                 instrument a feed reports",
                auth.label()
            )));
        }
        validate_declared_sensor(&state.db, &scope, sensor_id, &payload.metadata).await?;
    }
    if let Some(places) = payload.decimal_places {
        if !(0..=10).contains(&places) {
            return Err(AppError::BadRequest(format!(
                "decimal_places {places} is out of range (expected 0 to 10)"
            )));
        }
        if !payload.metadata.is_object() {
            payload.metadata = serde_json::json!({});
        }
        payload.metadata[super::service::DECIMAL_PLACES_KEY] = serde_json::json!(places);
    }
    if let Some(granularity) = payload.instrument_granularity {
        if !payload.metadata.is_object() {
            payload.metadata = serde_json::json!({});
        }
        payload.metadata[super::service::INSTRUMENT_GRANULARITY_KEY] =
            serde_json::json!(granularity);
    }
    // Moving an already-attached feed to a different instrument changes the attribution of
    // everything it has ever written, so it is refused here and left to the explicit swap and
    // relink paths.
    if let Some(declared) = payload.sensor_id
        && let Some(current) = stored.as_ref().and_then(|s| s.sensor_id)
        && current != declared
    {
        return Err(AppError::Conflict(format!(
            "stream {} already reports instrument {current}; relink it explicitly rather than \
             on registration",
            stored.as_ref().map_or_else(Uuid::nil, |s| s.id)
        )));
    }

    // Register on (source_system, source_key). A byte-identical re-registration must be a no-op
    // write: the sync services re-run discovery every cycle, and rewriting an unchanged row
    // bumps updated_at and churns WAL for nothing. Only what the source describes is sent, so a
    // re-registration cannot move the pairing; a declared instrument attaches to a feed that has
    // none, and an omitted classification never clears an operator-set one.
    let mut active = data_streams::ActiveModel {
        source_system: Set(payload.source_system.clone()),
        source_key: Set(payload.source_key.clone()),
        source_name: Set(payload.source_name.clone()),
        source_path: Set(payload.source_path.clone()),
        metadata: Set(payload.metadata.clone()),
        ..Default::default()
    };
    if stored.is_none() || payload.sensor_id.is_some() {
        active.sensor_id = Set(payload.sensor_id);
    }
    if stored.is_none() || payload.measurement_type.is_some() {
        active.measurement_type = Set(payload.measurement_type.clone());
    }
    let registered = crate::common::provenance::register::<DataStream>(&state.db, active)
        .await?
        .0;

    let stream = data_streams::Entity::find_by_id(registered.id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch registered stream".to_string()))?;

    // Registration mints nothing. Which instrument produced a feed is a decision, and the plan is
    // where it is put to an operator (Q134): a row minted here answers it before anyone is asked,
    // and the plan then has to see through its own defaults to offer a real choice. The descriptor
    // it carries is the plan's evidence, not its conclusion, so the metadata is kept and the
    // column stays NULL until the pairing mints one. A reading that arrives meanwhile is staged
    // (B223), which is what makes the column's absence safe.

    // A feed that describes its device can report a different one than the instrument it is
    // attached to was minted with, which is a probe swap. The channel is the identity, so nothing
    // forks: the serials are refreshed and the change goes to the review queue for an operator.
    if let Some(sensor_id) = stream.sensor_id
        && let Err(e) = crate::routes::private::sensors::service::reconcile_source_identity(
            &state.db,
            sensor_id,
            stream.id,
            &stream.metadata,
        )
        .await
    {
        // Registration is the sync cycle's first call; failing it here would stop ingestion over a
        // metadata note.
        tracing::warn!(error = %e, stream = %stream.id, "device identity reconciliation failed");
    }

    Ok(Json(super::service::with_assignments(stream)))
}

/// Confine a caller-declared instrument to one the caller has a relationship to.
///
/// The relationship required of a caller confined to a project set is deployment: the instrument
/// must already be deployed into one of that caller's projects. Inventory that is deployed nowhere
/// belongs to no project, so nothing distinguishes another team's spare instrument from this
/// caller's, and attaching one makes every reading the feed writes resolve that instrument's
/// calibration windows. Wiring an undeployed instrument to its first feed is therefore an
/// unrestricted caller's operation, ie. an administrator or an unscoped sync service.
///
/// Two further conditions hold for every caller: the instrument exists, and it does not contradict
/// the device serial the stream's own metadata carries, when the instrument records one. That
/// serial check is a cross-check on feeds that describe their device, not the confinement:
/// metadata arrives in the same request, so a caller can always omit it, and the scope guard above
/// is what a restricted caller is held to.
pub async fn validate_declared_sensor(
    db: &DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    sensor_id: Uuid,
    metadata: &serde_json::Value,
) -> AppResult<()> {
    let sensor = sensors::Entity::find_by_id(sensor_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Sensor not found".to_string()))?;

    let project = scope::project_of_sensor(db, sensor_id).await?;
    scope::require_target_in_scope(scope, &project, scope::Unowned::Deny, "instrument")?;

    // Only an operator-entered serial can contradict: a source-registered instrument carries none
    // (its identity is the channel, and the feed's serials live in `metadata` as information), and
    // comparing against those would make a logger swap reject its own channel's next registration,
    // which is `reconcile_source_identity`'s job to record rather than refuse.
    if let Some(declared_serial) = extract_vaisala_device_serial(metadata)
        && let Some(recorded) = sensor.serial_number.as_deref()
        && recorded != declared_serial.as_str()
    {
        return Err(AppError::BadRequest(format!(
            "stream metadata reports device serial '{declared_serial}', which is not the serial of \
             instrument {sensor_id}"
        )));
    }
    Ok(())
}

/// A claim that waited out `lock_timeout` is another request pairing the same stream, which is a
/// conflict the caller can retry, not a server fault.
fn claim_error(e: sea_orm::DbErr) -> AppError {
    let message = e.to_string();
    if message.contains("55P03") || message.contains("lock timeout") {
        return AppError::Conflict("Stream is being paired by another request; retry".to_string());
    }
    AppError::Database(e)
}

/// Pair a stream to a site_parameter. Sets `site_parameter_id`/`paired_at` on the stream,
/// backfills existing unpaired readings with site_id/parameter_id, then re-derives each reading's
/// curve from the window covering its own time and refreshes aggregates, as a tracked job.
/// Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/streams/{id}/pair",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    request_body = PairStreamRequest,
    responses(
        (status = 200, description = "Stream paired, backfill count returned", body = PairStreamResponse),
        (status = 400, description = "Stream already paired"),
        (status = 404, description = "Stream or site parameter not found"),
        (status = 409, description = "Another request is pairing the same stream"),
    ),
    tag = "streams"
)]
pub async fn pair_stream(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Path(stream_id): Path<Uuid>,
    Json(payload): Json<PairStreamRequest>,
) -> AppResult<Json<PairStreamResponse>> {
    let db = &state.db;
    let now = Utc::now();

    // Claim first, then work, all in one transaction with the decompression cap lifted: the claim
    // is what stops two concurrent pairings of one stream both succeeding, and the transaction is
    // what stops a failed backfill leaving the stream paired with unattributed readings.
    let (sp_site_id, sp_parameter_id, backfilled, touched_events) =
        bulk_write::guarded(db, async |txn| {
            // A concurrent claim holds the row lock; wait a few seconds for it rather than either
            // failing instantly or hanging, then re-evaluate the claim predicate against its outcome.
            txn.execute_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SET LOCAL lock_timeout = '5s'".to_owned(),
            ))
            .await?;

            let sp = site_parameters::Entity::find_by_id(payload.site_parameter_id)
                .one(txn)
                .await?
                .ok_or_else(|| AppError::NotFound("Site parameter not found".to_string()))?;

            let existing = data_streams::Entity::find_by_id(stream_id)
                .one(txn)
                .await?
                .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
            if let Some(reason) = super::service::pairing_refusal(&existing.source_system) {
                return Err(AppError::BadRequest(reason));
            }

            let claimed =
                super::service::claim_stream(stream_id, payload.site_parameter_id, now.into())
                    .exec(txn)
                    .await
                    .map_err(claim_error)?
                    .rows_affected;

            let stream = data_streams::Entity::find_by_id(stream_id)
                .one(txn)
                .await?
                .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
            if claimed == 0 {
                return Err(AppError::BadRequest(
                    "Stream is already paired. Unpair it first.".to_string(),
                ));
            }
            super::service::declare_slot_decimal_places(
                txn,
                sp.id,
                super::service::declared_decimal_places(&stream.metadata),
            )
            .await?;

            // Create/reuse the sensor, then re-read the stream: it has gained a sensor_id. Pairing
            // never completes without an instrument: a slot's readings must name what measured them.
            let sensor_ctx =
                create_sensor_for_stream(txn, &stream, sp.parameter_id, sp.site_id, None).await?;
            let deployment_id = sensor_ctx.deployment_id;

            // Everything a pairing owes the slot: readings and status events attributed,
            // replicate groups materialised, spot instants attached as visits, deferred holds
            // promoted. One helper, so a stream paired here and the same stream paired through a
            // plan land in the same state.
            let done = super::flows::backfill(
                txn,
                crate::routes::private::sync::service::HoldScope::Stream(stream_id),
                deployment_id,
            )
            .await?;
            let (backfilled, touched_events) = (done.readings, done.touched_events);

            Ok((sp.site_id, sp.parameter_id, backfilled, touched_events))
        })
        .await?;

    // Attribution changed what the slot serves, so the pairing announces it like every other
    // path that changes stored reading values: `DataIngested` naming the site is what drops the
    // site's cached responses (`common/cache.rs:17-18`).
    let _ = state.events.send(crate::common::AppEvent::DataIngested {
        site_id: Some(sp_site_id),
        parameter_id: Some(sp_parameter_id),
        stream_id: Some(stream_id),
        count: usize::try_from(backfilled).unwrap_or(usize::MAX),
    });

    // Attribution is what made these readings visit values; the calculations that read them at
    // each manual visit run now (ADR 0007).
    crate::routes::private::collection_events::flows::enqueue_for(
        db,
        &touched_events,
        &crate::common::actor::label(&auth),
        crate::routes::private::collection_events::flows::Writer::Person,
    )
    .await?;

    // Window-reprocess the slot in the background (tracked): re-attributes the backfilled readings
    // to whichever sensor's deployment window covers each time, so pairing a stream into a slot with
    // a real deployment timeline is attributed by window, not by the single frozen sensor context,
    // resolves each reading's calibration and corrected value from the window covering its own time,
    // and refreshes continuous aggregates + cascades derived params.
    // Gated on the stream holding readings at all, not on the backfill having moved rows: a stream
    // re-paired after an unpair, or one whose readings arrived already attributed, backfills nothing
    // and still needs its window resolved against the slot it now feeds.
    let has_readings = crate::routes::private::readings::models::Entity::find()
        .filter(crate::routes::private::readings::models::Column::StreamId.eq(stream_id))
        .one(&state.db)
        .await?
        .is_some();
    if backfilled > 0 || has_readings {
        let slot_site = sp_site_id;
        let slot_param = sp_parameter_id;
        crate::routes::private::reprocessing_jobs::service::enqueue(
            db,
            "pairing_backfill",
            None,
            Some(stream_id),
            &serde_json::json!({ "site_id": slot_site, "parameter_id": slot_param }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // Re-fetch updated stream
    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(PairStreamResponse {
        stream: super::service::with_assignments(updated),
        backfilled,
    }))
}

/// Remove pairing from a stream. Clears `site_parameter_id`/`paired_at` on the stream and
/// nulls out `site_id`/`parameter_id`/`sample_id` on all readings (effectively hiding them from
/// continuous aggregates); samples left unreferenced are deleted. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/streams/{id}/unpair",
    params(("id" = Uuid, Path, description = "Stream UUID")),
    responses(
        (status = 200, description = "Stream unpaired, cleared count returned", body = UnpairStreamResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "streams"
)]
pub async fn unpair_stream(
    State(state): State<AppState>,
    Path(stream_id): Path<Uuid>,
) -> AppResult<Json<UnpairStreamResponse>> {
    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let Some(sp_id) = stream.site_parameter_id else {
        return Err(AppError::BadRequest("Stream is not paired".to_string()));
    };

    // Ahead of the teardown, and fatal rather than warn-logged: closing is idempotent (it matches
    // only an open deployment), so a retry after any later failure closes nothing twice.
    if let Some(sensor_id) = stream.sensor_id
        && let Some(sp) = site_parameters::Entity::find_by_id(sp_id).one(db).await?
    {
        close_sensor_deployment(db, sensor_id, sp.site_id, sp.parameter_id).await?;
    }

    let now = Utc::now();

    // Clear pairing on stream (keep sensor_id, sensor persists)
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(None);
    active.paired_at = Set(None);
    active.updated_at = Set(now.into());
    active.update(db).await?;

    // Release the stream's rows from the slot: one transaction, cap lifted, rollup rebuild queued
    // as a tracked job. The slot itself survives; only this stream stops feeding it.
    let retired = retire_slot(db, SlotScope::Stream(stream_id)).await?;
    crate::routes::private::collection_events::flows::enqueue_for(
        db,
        &retired.touched_events,
        &crate::common::actor::current().unwrap_or_else(|| "system".to_string()),
        crate::routes::private::collection_events::flows::Writer::Person,
    )
    .await?;
    let cleared = retired.touched.rows;

    // Open reviews lose their reviewer along with the slot; they wait as deferred until the
    // stream is paired again.
    crate::routes::private::sync::service::repoint_holds(
        db,
        crate::routes::private::sync::service::HoldScope::Stream(stream_id),
        false,
    )
    .await?;

    let updated = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to fetch updated stream".to_string()))?;

    Ok(Json(UnpairStreamResponse {
        stream: super::service::with_assignments(updated),
        cleared,
    }))
}

/// Classify data streams' measurement_type in bulk, the sensorless-stream counterpart of
/// `POST /sensors/retag_frequency` (portal imports like metalp/nomis carry no sensor to hang the
/// classification on). Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/streams/retag",
    request_body = RetagStreamsRequest,
    responses(
        (status = 200, description = "Streams reclassified", body = RetagStreamsResponse),
        (status = 400, description = "Invalid measurement_type or empty scope"),
    ),
    tag = "streams"
)]
pub async fn retag_streams(
    State(state): State<AppState>,
    Json(req): Json<RetagStreamsRequest>,
) -> AppResult<Json<RetagStreamsResponse>> {
    if req.stream_ids.is_empty() && req.source_system.is_none() {
        return Err(AppError::BadRequest(
            "provide stream_ids and/or source_system".to_string(),
        ));
    }
    if let Some(reason) =
        crate::routes::private::readings::service::retag_target_rejection(&req.measurement_type)
    {
        return Err(AppError::BadRequest(reason));
    }

    // "declared" writes nothing to `data_streams`; it aligns each reading with its own stream's
    // declaration, which for a family stream is already spot.
    let streams_updated =
        if req.measurement_type == crate::routes::private::readings::service::RETAG_DECLARED {
            0
        } else {
            if req.measurement_type != "spot" {
                let families = super::service::family_keys_in_streams(
                    &state.db,
                    &req.stream_ids,
                    req.source_system.as_deref(),
                )
                .await?;
                super::service::refuse_family_retag(&families, &req.measurement_type)?;
            }

            state
                .db
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE data_streams SET measurement_type = $1, updated_at = now() \
                 WHERE id = ANY($2) OR ($3::text IS NOT NULL AND source_system = $3)",
                    [
                        req.measurement_type.clone().into(),
                        req.stream_ids.clone().into(),
                        req.source_system.clone().into(),
                    ],
                ))
                .await?
                .rows_affected()
        };

    let job_id = if req.retag_existing {
        crate::routes::private::reprocessing_jobs::service::enqueue(
            &state.db,
            "measurement_retag",
            None,
            None,
            &serde_json::json!({
                "stream_ids": req.stream_ids,
                "source_system": req.source_system,
                "target": req.measurement_type,
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(RetagStreamsResponse {
        streams_updated,
        measurement_type: req.measurement_type,
        job_id,
    }))
}

/// What a stream looks like before anything is done with it: its shape, a sample of what it
/// carries, and the receipts of what has already been ingested from it.
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/streams/{id}/stats", get(stream_stats))
        .route("/streams/{id}/preview", get(stream_preview))
        .route("/streams/{id}/receipts", get(stream_receipts))
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_metadata,
        ))
}

/// Reshaping a stream that already exists: its cadence, the history it adopts, and the slot it is
/// paired to.
///
/// Registration is not here. `POST /streams/register` is one of the five `/x/register` routes that
/// a sync-service session token enrols through, which is a cross-cutting surface rather than this
/// component's, and it stays registered centrally with the rest of that family (Q143).
///
/// An Administrator action for humans, with the `write_metadata` token bit preserved so a
/// sync-service session token keeps pairing what it registered.
pub fn write_routes() -> Router<AppState> {
    Router::new()
        .route("/streams/retag", post(retag_streams))
        .route("/streams/{id}/pair", post(pair_stream))
        .route("/streams/{id}/unpair", post(unpair_stream))
        .layer(middleware::from_fn(
            crate::common::middleware::deny_scoped_token,
        ))
        .layer(middleware::from_fn(
            crate::common::middleware::require_admin_or_token_write_metadata,
        ))
}
