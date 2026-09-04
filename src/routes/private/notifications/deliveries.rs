//! The delivery log, read back by message. Every path through `dispatcher::deliver` writes one
//! `notification_log` row per (message, channel, recipient), including the ones that sent nothing:
//! `muted`, `undeliverable` and `skipped`. Grouping those rows back into the message they came from
//! is what tells an admin who an alarm actually reached.
//!
//! The rows of one message share `(alarm_event_id, kind)` and are written inside one `deliver` call,
//! so the group key is that pair plus the second the rows were written in.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// The statuses `dispatcher::deliver` writes. A filter naming anything else is refused rather than
/// silently returning nothing.
const STATUSES: [&str; 5] = ["sent", "failed", "muted", "undeliverable", "skipped"];

const MAX_LIMIT: u64 = 200;

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeliveryQuery {
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    /// Keep only messages carrying at least one row of this status.
    pub status: Option<String>,
    /// Keep only messages of this kind (`alarm_opened`, `sync_stale`, `test`, ...).
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryCounts {
    pub total: i64,
    pub sent: i64,
    pub failed: i64,
    pub muted: i64,
    pub undeliverable: i64,
    pub skipped: i64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecipient {
    pub channel: String,
    pub recipient: String,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryMessage {
    pub alarm_event_id: Option<Uuid>,
    pub kind: String,
    /// The second the message's rows were written in, which is the group key.
    pub at: DateTime<Utc>,
    pub site_name: Option<String>,
    pub parameter_name: Option<String>,
    pub counts: DeliveryCounts,
    pub recipients: Vec<DeliveryRecipient>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryLogPage {
    pub messages: Vec<DeliveryMessage>,
    /// Messages matching the filter, not rows.
    pub total: i64,
}

fn validate(value: &str, field: &str) -> AppResult<()> {
    if field == "status" && !STATUSES.contains(&value) {
        return Err(AppError::BadRequest(format!(
            "unknown status: {value}, expected one of {}",
            STATUSES.join(", ")
        )));
    }
    Ok(())
}

/// One page of delivery messages, newest first, with the recipients of each.
pub async fn list_deliveries(
    db: &DatabaseConnection,
    q: &DeliveryQuery,
) -> AppResult<DeliveryLogPage> {
    let limit = q.limit.unwrap_or(50).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0);
    if let Some(status) = &q.status {
        validate(status, "status")?;
    }

    // Both queries filter the same way, so a message's counts and its page position agree.
    let mut filters = String::new();
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(kind) = &q.kind {
        values.push(kind.clone().into());
        filters.push_str(&format!(" AND nl.kind = ${}", values.len()));
    }
    let having = match &q.status {
        Some(status) => {
            values.push(status.clone().into());
            format!(
                " HAVING COUNT(*) FILTER (WHERE nl.status = ${}) > 0",
                values.len()
            )
        }
        None => String::new(),
    };

    let group_sql = format!(
        "SELECT nl.alarm_event_id, nl.kind, date_trunc('second', nl.created_at) AS at, \
                s.name AS site_name, p.name AS parameter_name, \
                COUNT(*)::bigint AS total, \
                COUNT(*) FILTER (WHERE nl.status = 'sent')::bigint AS sent, \
                COUNT(*) FILTER (WHERE nl.status = 'failed')::bigint AS failed, \
                COUNT(*) FILTER (WHERE nl.status = 'muted')::bigint AS muted, \
                COUNT(*) FILTER (WHERE nl.status = 'undeliverable')::bigint AS undeliverable, \
                COUNT(*) FILTER (WHERE nl.status = 'skipped')::bigint AS skipped \
         FROM notification_log nl \
         LEFT JOIN alarm_events ae ON ae.id = nl.alarm_event_id \
         LEFT JOIN sites s ON s.id = ae.site_id \
         LEFT JOIN parameters p ON p.id = ae.parameter_id \
         WHERE true{filters} \
         GROUP BY nl.alarm_event_id, nl.kind, at, s.name, p.name{having} \
         ORDER BY at DESC, nl.kind \
         LIMIT {limit} OFFSET {offset}"
    );

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            PG,
            &group_sql,
            values.clone(),
        ))
        .await?;

    let mut messages = Vec::with_capacity(rows.len());
    for r in rows {
        messages.push(DeliveryMessage {
            alarm_event_id: r.try_get("", "alarm_event_id")?,
            kind: r.try_get("", "kind")?,
            at: r.try_get("", "at")?,
            site_name: r.try_get("", "site_name")?,
            parameter_name: r.try_get("", "parameter_name")?,
            counts: DeliveryCounts {
                total: r.try_get("", "total")?,
                sent: r.try_get("", "sent")?,
                failed: r.try_get("", "failed")?,
                muted: r.try_get("", "muted")?,
                undeliverable: r.try_get("", "undeliverable")?,
                skipped: r.try_get("", "skipped")?,
            },
            recipients: Vec::new(),
        });
    }

    let count_sql = format!(
        "SELECT COUNT(*)::bigint AS c FROM ( \
            SELECT 1 FROM notification_log nl WHERE true{filters} \
            GROUP BY nl.alarm_event_id, nl.kind, date_trunc('second', nl.created_at){having} \
         ) g"
    );
    let total = db
        .query_one_raw(Statement::from_sql_and_values(PG, &count_sql, values))
        .await?
        .map(|r| r.try_get::<i64>("", "c"))
        .transpose()?
        .unwrap_or(0);

    if !messages.is_empty() {
        attach_recipients(db, &mut messages).await?;
    }
    Ok(DeliveryLogPage { messages, total })
}

/// Fill each message on the page with its own rows. The page is contiguous in time, so one scan of
/// that span serves every message on it.
async fn attach_recipients(
    db: &DatabaseConnection,
    messages: &mut [DeliveryMessage],
) -> AppResult<()> {
    let oldest = messages.iter().map(|m| m.at).min().expect("page not empty");
    let newest = messages.iter().map(|m| m.at).max().expect("page not empty");
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            PG,
            "SELECT alarm_event_id, kind, date_trunc('second', created_at) AS at, \
                    channel, recipient, status, error, created_at \
             FROM notification_log \
             WHERE created_at >= $1 AND created_at < $2 + INTERVAL '1 second' \
             ORDER BY created_at",
            [oldest.into(), newest.into()],
        ))
        .await?;

    for r in rows {
        let key: (Option<Uuid>, String, DateTime<Utc>) = (
            r.try_get("", "alarm_event_id")?,
            r.try_get("", "kind")?,
            r.try_get("", "at")?,
        );
        let Some(msg) = messages
            .iter_mut()
            .find(|m| (m.alarm_event_id, m.kind.as_str(), m.at) == (key.0, key.1.as_str(), key.2))
        else {
            continue;
        };
        msg.recipients.push(DeliveryRecipient {
            channel: r.try_get("", "channel")?,
            recipient: r.try_get("", "recipient")?,
            status: r.try_get("", "status")?,
            error: r.try_get("", "error")?,
            created_at: r.try_get("", "created_at")?,
        });
    }
    Ok(())
}

/// `GET /api/notifications/deliveries`, the delivery log by message (admin-only).
#[utoipa::path(
    get,
    path = "/api/notifications/deliveries",
    responses((status = 200, description = "Delivery log by message", body = DeliveryLogPage)),
    tag = "notifications"
)]
pub async fn list_delivery_log(
    State(state): State<AppState>,
    Query(q): Query<DeliveryQuery>,
) -> AppResult<Json<DeliveryLogPage>> {
    Ok(Json(list_deliveries(&state.db, &q).await?))
}
