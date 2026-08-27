//! Discard-and-refetch repair for replicate-family streams whose stored `replicate_index` values
//! no longer name the source column each reading came from.
//!
//! A reading's replicate index is the source's column position and nothing renumbers it, so a
//! stream that carries renumbered indexes cannot be corrected in place: the mapping back to the
//! source positions is not recoverable from the stored rows. The repair therefore deletes the
//! stream's readings, rewinds its sync cursor and asks the owning sync service to send them again.
//!
//! Two conditions bound what that can destroy, and both are enforced before anything is deleted.
//! A replicate-family stream has a single author, the sync service that owns its source system, so
//! a refetch reproduces the same rows. And the job refuses, without touching any stream in the
//! scope, when a targeted stream is paired to a site parameter or holds a flagged reading:
//! attribution and flag decisions are not reproducible from the source.

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ConnectionTrait, DbErr, Set, Statement};
use uuid::Uuid;

use super::job::Job;
use super::lifecycle::JobContext;
use crate::common::bulk_write;
use crate::routes::private::data_streams::replicates::METADATA_KEY;
use crate::routes::private::sync::commands_model as sync_commands;
use river_data_core::models::CommandStatus;

pub const TRIGGER_TYPE: &str = "replicate_reindex_repair";

/// The sync client's command name for a full-history refetch of named streams. Spelled out here
/// because `river_data_core::commands::RESYNC_STREAMS` postdates the core release this crate
/// builds against.
const RESYNC_STREAMS: &str = "resync_streams";

/// Used when the job runs outside a process that published its config (the worker-pool tests).
const FALLBACK_COMMAND_EXPIRY_SECS: i64 = 3600;

struct Inputs {
    stream_ids: Vec<Uuid>,
    source_system: Option<String>,
    dry_run: bool,
}

fn job_inputs(params: &serde_json::Value) -> Result<Inputs, DbErr> {
    let stream_ids: Vec<Uuid> = params
        .get("stream_ids")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        })
        .unwrap_or_default();
    let source_system = params
        .get("source_system")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    if stream_ids.is_empty() && source_system.is_none() {
        return Err(DbErr::Custom(
            "replicate reindex repair needs stream_ids or source_system".to_string(),
        ));
    }
    let dry_run = params
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(Inputs {
        stream_ids,
        source_system,
        dry_run,
    })
}

#[derive(Debug, Clone)]
struct Target {
    id: Uuid,
    source_key: String,
    source_system: String,
    paired: bool,
    flagged: i64,
    readings: i64,
    has_cursor: bool,
}

/// The replicate families in the requested scope, with everything the refusals and the per-stream
/// decisions need. A family is identified the same way the reconciliation identifies one: the
/// `:reps` suffix the sync service appends plus the registered spec.
async fn family_targets<C: ConnectionTrait>(
    conn: &C,
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> Result<Vec<Target>, DbErr> {
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.id, s.source_key, s.source_system,
                    s.site_parameter_id IS NOT NULL AS paired,
                    s.last_data_time IS NOT NULL AS has_cursor,
                    (SELECT COUNT(*)::bigint FROM readings r WHERE r.stream_id = s.id) AS readings,
                    (SELECT COUNT(*)::bigint FROM readings r
                     WHERE r.stream_id = s.id AND r.is_flagged IS TRUE) AS flagged
             FROM data_streams s
             WHERE s.source_key LIKE '%:reps'
               AND s.metadata -> $3 IS NOT NULL
               AND (s.id = ANY($1) OR ($2::text IS NOT NULL AND s.source_system = $2))
             ORDER BY s.source_key",
            [
                stream_ids.to_vec().into(),
                source_system.map(ToString::to_string).into(),
                METADATA_KEY.into(),
            ],
        ))
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Target {
                id: row.try_get("", "id")?,
                source_key: row.try_get("", "source_key")?,
                source_system: row.try_get("", "source_system")?,
                paired: row.try_get("", "paired")?,
                flagged: row.try_get("", "flagged")?,
                readings: row.try_get("", "readings")?,
                has_cursor: row.try_get("", "has_cursor")?,
            })
        })
        .collect()
}

fn refuse_unsafe_targets(targets: &[Target]) -> Result<(), DbErr> {
    let paired: Vec<&str> = targets
        .iter()
        .filter(|t| t.paired)
        .map(|t| t.source_key.as_str())
        .collect();
    if !paired.is_empty() {
        return Err(DbErr::Custom(format!(
            "these streams are paired to a site parameter and cannot be discarded and refetched: \
             {}. Unpair them first, or narrow the scope",
            paired.join(", ")
        )));
    }
    let flagged: Vec<&str> = targets
        .iter()
        .filter(|t| t.flagged > 0)
        .map(|t| t.source_key.as_str())
        .collect();
    if !flagged.is_empty() {
        return Err(DbErr::Custom(format!(
            "these streams hold flagged readings, which a refetch would not reproduce: {}. \
             Unflag them first, or narrow the scope",
            flagged.join(", ")
        )));
    }
    Ok(())
}

/// The service to address a resync to: the instance of the source's type that reported a heartbeat
/// most recently. `service_type` matches `data_streams.source_system` by convention, so a source
/// with no enrolled service is a reportable outcome rather than an error.
async fn owning_service<C: ConnectionTrait>(
    conn: &C,
    source_system: &str,
) -> Result<Option<Uuid>, DbErr> {
    let row = conn
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM sync_services WHERE service_type = $1
             ORDER BY last_heartbeat DESC NULLS LAST LIMIT 1",
            [source_system.into()],
        ))
        .await?;
    row.map(|r| r.try_get::<Uuid>("", "id")).transpose()
}

/// Ask a service to send the named streams again from the start of history. The readings are gone
/// by the time this runs, so the ingest has nothing to conflict with and `overwrite` stays off.
async fn issue_resync(
    db: &sea_orm::DatabaseConnection,
    service_id: Uuid,
    source_keys: &[String],
) -> Result<(), DbErr> {
    let expiry_secs = crate::common::global_app_state()
        .and_then(|s| i64::try_from(s.config.as_ref().sync_command_expiry_secs).ok())
        .unwrap_or(FALLBACK_COMMAND_EXPIRY_SECS);
    let cmd = sync_commands::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(service_id),
        command: Set(RESYNC_STREAMS.to_string()),
        payload: Set(Some(serde_json::json!({
            "source_keys": source_keys,
            "overwrite": false,
        }))),
        status: Set(CommandStatus::Pending.to_string()),
        result: Set(None),
        created_at: Set(Utc::now().into()),
        expires_at: Set((Utc::now() + chrono::Duration::seconds(expiry_secs)).into()),
        acknowledged_at: Set(None),
        completed_at: Set(None),
    };
    cmd.insert(db).await?;
    Ok(())
}

pub struct ReplicateReindexRepair;

#[async_trait]
impl Job for ReplicateReindexRepair {
    fn name(&self) -> &'static str {
        TRIGGER_TYPE
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let inputs = job_inputs(ctx.params())?;
        let db = ctx.db();

        let targets =
            family_targets(db, &inputs.stream_ids, inputs.source_system.as_deref()).await?;
        if targets.is_empty() {
            return Err(DbErr::Custom(
                "no replicate family streams matched the requested scope".to_string(),
            ));
        }
        refuse_unsafe_targets(&targets)?;

        let total = i32::try_from(targets.len()).unwrap_or(i32::MAX);
        let mut repaired = 0i64;
        let mut readings_deleted = 0i64;
        let mut skipped = 0i64;
        let mut streams = Vec::with_capacity(targets.len());
        let mut refetch: Vec<(String, String)> = Vec::new();

        for (done, target) in targets.iter().enumerate() {
            if ctx.is_cancelled() {
                ctx.info("Cancelled between streams; completed repairs stand")
                    .await;
                break;
            }
            ctx.set_progress(i32::try_from(done).unwrap_or(i32::MAX), Some(total))
                .await;

            let mut entry = serde_json::json!({
                "stream": target.source_key,
                "stream_id": target.id,
                "readings": target.readings,
            });

            if target.readings == 0 && !target.has_cursor {
                skipped += 1;
                entry["status"] = serde_json::json!("nothing_to_repair");
                streams.push(entry);
                continue;
            }

            if inputs.dry_run {
                entry["status"] = serde_json::json!("would_repair");
                streams.push(entry);
                continue;
            }

            let removed = bulk_write::guarded(db, async |txn| {
                let removed = bulk_write::mutation(
                    txn,
                    Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "DELETE FROM readings WHERE stream_id = $1",
                        [target.id.into()],
                    ),
                )
                .await?
                .rows;
                txn.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE data_streams SET last_data_time = NULL, updated_at = NOW() \
                     WHERE id = $1",
                    [target.id.into()],
                ))
                .await?;
                Ok(removed)
            })
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;

            repaired += 1;
            readings_deleted += i64::try_from(removed).unwrap_or(0);
            refetch.push((target.source_system.clone(), target.source_key.clone()));
            entry["status"] = serde_json::json!("repaired");
            entry["readings_deleted"] = serde_json::json!(removed);
            streams.push(entry);
        }

        let mut commands_issued = 0i64;
        let mut sources_without_service = 0i64;
        let mut sources: Vec<String> = refetch.iter().map(|(sys, _)| sys.clone()).collect();
        sources.sort_unstable();
        sources.dedup();
        for source in &sources {
            let keys: Vec<String> = refetch
                .iter()
                .filter(|(sys, _)| sys == source)
                .map(|(_, key)| key.clone())
                .collect();
            match owning_service(db, source).await? {
                Some(service_id) => {
                    issue_resync(db, service_id, &keys).await?;
                    commands_issued += 1;
                }
                None => {
                    sources_without_service += 1;
                    ctx.log(
                        "warn",
                        "No sync service is enrolled for this source; the rewound cursor will \
                         refetch on the next scheduled cycle instead",
                        serde_json::json!({ "source_system": source, "streams": keys.len() }),
                    )
                    .await;
                }
            }
        }

        ctx.set_detail(serde_json::json!({
            "scope": {
                "source_system": inputs.source_system,
                "stream_ids": inputs.stream_ids,
                "dry_run": inputs.dry_run,
            },
            "counts": {
                "streams_targeted": targets.len(),
                "streams_repaired": repaired,
                "readings_deleted": readings_deleted,
                "streams_skipped": skipped,
                "commands_issued": commands_issued,
                "sources_without_service": sources_without_service,
            },
            "streams": streams,
        }))
        .await;

        if let Some(state) = crate::common::global_app_state() {
            state.response_cache.invalidate_all();
        }
        Ok(readings_deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(key: &str, paired: bool, flagged: i64) -> Target {
        Target {
            id: Uuid::new_v4(),
            source_key: key.to_string(),
            source_system: "cnet".to_string(),
            paired,
            flagged,
            readings: 10,
            has_cursor: true,
        }
    }

    #[test]
    fn a_scope_names_streams_or_a_source() {
        assert!(job_inputs(&serde_json::json!({})).is_err());
        let by_source = job_inputs(&serde_json::json!({"source_system": "cnet"})).unwrap();
        assert_eq!(by_source.source_system.as_deref(), Some("cnet"));
        assert!(by_source.stream_ids.is_empty());
        assert!(!by_source.dry_run);

        let id = Uuid::new_v4();
        let by_ids = job_inputs(&serde_json::json!({
            "stream_ids": [id.to_string()],
            "dry_run": true,
        }))
        .unwrap();
        assert_eq!(by_ids.stream_ids, vec![id]);
        assert!(by_ids.dry_run);
    }

    #[test]
    fn a_blank_source_system_is_not_a_scope() {
        assert!(job_inputs(&serde_json::json!({"source_system": "  "})).is_err());
    }

    #[test]
    fn a_paired_stream_refuses_the_whole_scope() {
        let targets = [target("a:reps", false, 0), target("b:reps", true, 0)];
        let message = refuse_unsafe_targets(&targets).unwrap_err().to_string();
        assert!(message.contains("b:reps"));
        assert!(!message.contains("a:reps"));
    }

    #[test]
    fn a_flagged_reading_refuses_the_whole_scope() {
        let targets = [target("a:reps", false, 0), target("c:reps", false, 3)];
        let message = refuse_unsafe_targets(&targets).unwrap_err().to_string();
        assert!(message.contains("c:reps"));
    }

    #[test]
    fn an_unpaired_unflagged_scope_is_accepted() {
        let targets = [target("a:reps", false, 0), target("b:reps", false, 0)];
        refuse_unsafe_targets(&targets).unwrap();
    }
}
