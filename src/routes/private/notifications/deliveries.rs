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
use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::paging::{Page, Window};
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

/// The rows this file's two queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct MessageRow {
    alarm_event_id: Option<Uuid>,
    kind: String,
    at: DateTime<Utc>,
    site_name: Option<String>,
    parameter_name: Option<String>,
    total: i64,
    sent: i64,
    failed: i64,
    muted: i64,
    undeliverable: i64,
    skipped: i64,
}

#[derive(FromQueryResult)]
struct RecipientRow {
    alarm_event_id: Option<Uuid>,
    kind: String,
    at: DateTime<Utc>,
    channel: String,
    recipient: String,
    status: String,
    error: Option<String>,
    created_at: DateTime<Utc>,
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
) -> AppResult<Page<DeliveryMessage>> {
    let window = Window::from_limit_offset(q.limit, q.offset, 50, MAX_LIMIT);
    let (limit, offset) = (window.limit, window.offset);
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
        let row = MessageRow::from_query_result(&r, "")?;
        messages.push(DeliveryMessage {
            alarm_event_id: row.alarm_event_id,
            kind: row.kind,
            at: row.at,
            site_name: row.site_name,
            parameter_name: row.parameter_name,
            counts: DeliveryCounts {
                total: row.total,
                sent: row.sent,
                failed: row.failed,
                muted: row.muted,
                undeliverable: row.undeliverable,
                skipped: row.skipped,
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
    Ok(Page::new(
        messages,
        u64::try_from(total).unwrap_or(0),
        Some(window),
    ))
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
        let row = RecipientRow::from_query_result(&r, "")?;
        let Some(msg) = messages.iter_mut().find(|m| {
            (m.alarm_event_id, m.kind.as_str(), m.at)
                == (row.alarm_event_id, row.kind.as_str(), row.at)
        }) else {
            continue;
        };
        msg.recipients.push(DeliveryRecipient {
            channel: row.channel,
            recipient: row.recipient,
            status: row.status,
            error: row.error,
            created_at: row.created_at,
        });
    }
    Ok(())
}

/// `GET /api/notifications/deliveries`, the delivery log by message (admin-only).
#[utoipa::path(
    get,
    path = "/api/notifications/deliveries",
    responses((status = 200, description = "Delivery log by message", body = Page<DeliveryMessage>)),
    tag = "notifications"
)]
pub async fn list_delivery_log(
    State(state): State<AppState>,
    Query(q): Query<DeliveryQuery>,
) -> AppResult<Json<Page<DeliveryMessage>>> {
    Ok(Json(list_deliveries(&state.db, &q).await?))
}
