//! The notification HTTP surface: the admin roster, channel health, the delivery log, and every
//! `/notifications/me` handler a subscriber manages their own channels through.

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    routing::{get, post, put},
};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder, Statement, TransactionTrait,
};
use uuid::Uuid;

use super::models::*;
use super::service::*;
use std::collections::HashSet;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::AuthContext;
use crate::common::paging::Page;
use crate::error::{AppError, AppResult};

#[utoipa::path(
    post,
    path = "/api/notifications/test-send",
    request_body = TestSendRequest,
    responses((status = 200, description = "Test attempted", body = TestSendResponse)),
    tag = "notifications"
)]
pub async fn test_send(
    State(state): State<AppState>,
    Json(req): Json<TestSendRequest>,
) -> AppResult<Json<TestSendResponse>> {
    let recipient = req.recipient.trim().to_string();
    if recipient.is_empty() {
        return Err(AppError::BadRequest("recipient is required".to_string()));
    }

    let outcome = match req.channel.as_str() {
        "web_push" => {
            if !state.config.web_push_configured() {
                return Err(AppError::BadRequest(
                    "Web Push is not configured".to_string(),
                ));
            }
            let channel = WebPushChannel::new(&state.config).ok_or_else(|| {
                AppError::Internal("failed to build web push channel".to_string())
            })?;
            let msg = OutgoingMessage {
                kind: "test",
                subject: "RIVER Data test notification".to_string(),
                body: "✅ This is a test notification from RIVER Data.".to_string(),
                slot: None,
            };
            let results = channel.deliver(&state, &msg).await;
            if results.is_empty() {
                Err("no subscriptions found for this user".to_string())
            } else if let Some(err) = results.iter().find_map(|r| r.outcome.as_ref().err()) {
                Err(err.clone())
            } else {
                Ok(())
            }
        }
        other => {
            return Err(AppError::BadRequest(format!("unknown channel: {other}")));
        }
    };

    let (status, error) = match &outcome {
        Ok(()) => ("sent", None),
        Err(e) => ("failed", Some(e.as_str())),
    };
    log_delivery(
        &state.db,
        None,
        "test",
        &req.channel,
        &recipient,
        status,
        error,
    )
    .await;

    Ok(Json(TestSendResponse {
        channel: req.channel,
        all_sent: outcome.is_ok(),
        results: vec![TestResult {
            recipient,
            status: status.to_string(),
            error: error.map(str::to_string),
        }],
    }))
}

#[utoipa::path(
    get,
    path = "/api/notifications/subscribers",
    responses((status = 200, description = "Subscriber roster", body = [SubscriberRow])),
    tag = "notifications"
)]
pub async fn list_subscribers(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SubscriberRow>>> {
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            PG,
            "WITH subs AS ( \
                SELECT keycloak_sub FROM notification_subscribers \
                UNION \
                SELECT DISTINCT keycloak_sub FROM web_push_subscriptions \
             ) \
             SELECT s.keycloak_sub, \
                COALESCE(ns.web_push_enabled, true) AS web_push_enabled, \
                (SELECT COUNT(*) FROM notification_subscriptions nsub \
                   WHERE nsub.keycloak_sub = s.keycloak_sub) AS overrides, \
                (SELECT COUNT(*) FROM web_push_subscriptions wps \
                   WHERE wps.keycloak_sub = s.keycloak_sub) AS push_count \
             FROM subs s \
             LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = s.keycloak_sub \
             ORDER BY s.keycloak_sub"
                .to_string(),
        ))
        .await?;

    let out = rows
        .iter()
        .map(|r| SubscriberRow::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(out))
}

/// `GET /api/notifications/health`, latest persisted health per channel (admin-only).
#[utoipa::path(
    get,
    path = "/api/notifications/health",
    responses((status = 200, description = "Latest persisted health per channel", body = NotificationHealth)),
    tag = "notifications"
)]
pub async fn get_health(State(state): State<AppState>) -> AppResult<Json<NotificationHealth>> {
    Ok(Json(read_health(&state.db, &state.config).await))
}

/// `POST /api/notifications/health/refresh`, probe now, then return the fresh state (admin-only).
#[utoipa::path(
    post,
    path = "/api/notifications/health/refresh",
    responses((status = 200, description = "Health after probing every configured channel", body = NotificationHealth)),
    tag = "notifications"
)]
pub async fn refresh_health(State(state): State<AppState>) -> AppResult<Json<NotificationHealth>> {
    let _ = probe_once(&state.db, &state.config).await;
    Ok(Json(read_health(&state.db, &state.config).await))
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

#[utoipa::path(
    get,
    path = "/api/notifications/me",
    responses((status = 200, description = "My notification settings", body = MyNotifications)),
    tag = "notifications"
)]
pub async fn get_my_notifications(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<MyNotifications>> {
    let sub = require_sub(&auth)?;
    ensure_subscriber(&state, &sub).await?;
    Ok(Json(load(&state, &sub).await?))
}

#[utoipa::path(
    patch,
    path = "/api/notifications/me",
    request_body = UpdatePrefsRequest,
    responses((status = 200, description = "Updated settings", body = MyNotifications)),
    tag = "notifications"
)]
pub async fn update_my_notifications(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<UpdatePrefsRequest>,
) -> AppResult<Json<MyNotifications>> {
    let sub = require_sub(&auth)?;
    ensure_subscriber(&state, &sub).await?;
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            PG,
            "UPDATE notification_subscribers \
             SET web_push_enabled = COALESCE($2, web_push_enabled), \
                 updated_at = NOW() \
             WHERE keycloak_sub = $1",
            [sub.clone().into(), req.web_push_enabled.into()],
        ))
        .await?;
    Ok(Json(load(&state, &sub).await?))
}

#[derive(FromQueryResult)]
pub(super) struct ChannelCounts {
    kind: String,
    sent_1d: i64,
    sent_7d: i64,
    sent_30d: i64,
}

#[utoipa::path(
    get,
    path = "/api/notifications/channels",
    responses((status = 200, description = "Channels, with what each has sent lately", body = [ChannelView])),
    tag = "notifications"
)]
pub async fn list_channels(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Vec<ChannelView>>> {
    let sub = require_sub(&auth)?;
    let counts = state
        .db
        .query_all_raw(Statement::from_string(
            PG,
            // A send is a delivery the dispatcher recorded as `sent`; a failed or muted attempt is
            // not one, and the card says what the channel has sent lately. `created_at` is the
            // only time the row carries.
            "SELECT kind,
                    count(*) FILTER (WHERE status = 'sent'
                        AND created_at > now() - interval '1 day')::bigint AS sent_1d,
                    count(*) FILTER (WHERE status = 'sent'
                        AND created_at > now() - interval '7 days')::bigint AS sent_7d,
                    count(*) FILTER (WHERE status = 'sent'
                        AND created_at > now() - interval '30 days')::bigint AS sent_30d
               FROM notification_log GROUP BY kind"
                .to_string(),
        ))
        .await?;
    let mut by_kind = std::collections::HashMap::new();
    for row in &counts {
        let c = ChannelCounts::from_query_result(row, "")?;
        by_kind.insert(c.kind.clone(), c);
    }
    let mine = subscription::Entity::find()
        .filter(subscription::Column::KeycloakSub.eq(sub))
        .filter(subscription::Column::ProjectId.is_null())
        .filter(subscription::Column::SiteId.is_null())
        .filter(subscription::Column::ParameterId.is_null())
        .all(&state.db)
        .await?;
    let mut chosen = std::collections::HashMap::new();
    for row in mine {
        chosen.insert(row.channel, row.enabled);
    }
    Ok(Json(
        CHANNELS
            .iter()
            .map(|c| {
                let counts = by_kind.get(c.kind);
                ChannelView {
                    kind: c.kind.to_string(),
                    label: c.label.to_string(),
                    description: c.description.to_string(),
                    on_by_default: c.on_by_default,
                    subscribed: chosen.get(c.kind).copied().unwrap_or(c.on_by_default),
                    sent_1d: counts.map_or(0, |c| c.sent_1d),
                    sent_7d: counts.map_or(0, |c| c.sent_7d),
                    sent_30d: counts.map_or(0, |c| c.sent_30d),
                }
            })
            .collect(),
    ))
}

#[utoipa::path(
    put,
    path = "/api/notifications/me/subscriptions",
    request_body = SetSubscriptionsRequest,
    responses((status = 200, description = "Updated settings", body = MyNotifications)),
    tag = "notifications"
)]
pub async fn set_my_subscriptions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<SetSubscriptionsRequest>,
) -> AppResult<Json<MyNotifications>> {
    let sub = require_sub(&auth)?;

    let scope = auth.access_scope();
    let accessible: Option<HashSet<Uuid>> = match &scope {
        AccessScope::Unrestricted => None,
        AccessScope::Projects(projects) => Some((**projects).clone()),
    };

    for s in &req.subscriptions {
        if channel(&s.channel).is_none() {
            return Err(AppError::BadRequest(format!(
                "unknown notification channel: {}",
                s.channel
            )));
        }
        if let Some(pid) = s.project_id
            && !project_allowed(&accessible, pid)
        {
            return Err(AppError::Forbidden(format!(
                "project {pid} is not in your grant set"
            )));
        }
    }

    let txn = state.db.begin().await?;
    subscription::Entity::delete_many()
        .filter(subscription::Column::KeycloakSub.eq(sub.clone()))
        .exec(&txn)
        .await?;
    let rows: Vec<subscription::ActiveModel> = req
        .subscriptions
        .iter()
        .map(|s| subscription::ActiveModel {
            id: Set(Uuid::new_v4()),
            keycloak_sub: Set(sub.clone()),
            channel: Set(s.channel.clone()),
            project_id: Set(s.project_id),
            site_id: Set(s.site_id),
            parameter_id: Set(s.parameter_id),
            enabled: Set(s.enabled),
            ..Default::default()
        })
        .collect();
    if !rows.is_empty() {
        subscription::Entity::insert_many(rows).exec(&txn).await?;
    }
    txn.commit().await?;

    Ok(Json(load(&state, &sub).await?))
}

#[utoipa::path(post, path = "/api/notifications/me/push", tag = "notifications")]
pub async fn register_push_subscription(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<RegisterPushRequest>,
) -> AppResult<Json<PushSubscriptionRow>> {
    let sub = require_sub(&auth)?;
    // A device implies a subscriber: the roster is the subscriber table, so a person who registers
    // one without ever opening their settings still has a row there.
    ensure_subscriber(&state, &sub).await?;
    let row = push_subscription::Entity::insert(push_subscription::ActiveModel {
        id: Set(Uuid::new_v4()),
        keycloak_sub: Set(sub),
        endpoint: Set(body.endpoint),
        p256dh: Set(body.p256dh),
        auth: Set(body.auth),
        user_agent: Set(body.user_agent),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(push_subscription::Column::Endpoint)
            .update_columns([
                push_subscription::Column::KeycloakSub,
                push_subscription::Column::P256dh,
                push_subscription::Column::Auth,
                push_subscription::Column::UserAgent,
            ])
            .to_owned(),
    )
    .exec_with_returning(&state.db)
    .await?;

    Ok(Json(PushSubscriptionRow {
        id: row.id,
        endpoint: row.endpoint,
        user_agent: row.user_agent,
        created_at: row.created_at,
        last_success_at: row.last_success_at,
    }))
}

#[utoipa::path(get, path = "/api/notifications/me/push", tag = "notifications")]
pub async fn list_push_subscriptions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Vec<PushSubscriptionRow>>> {
    let sub = require_sub(&auth)?;
    let out = push_subscription::Entity::find()
        .filter(push_subscription::Column::KeycloakSub.eq(sub))
        .order_by_desc(push_subscription::Column::CreatedAt)
        .all(&state.db)
        .await?
        .into_iter()
        .map(|row| PushSubscriptionRow {
            id: row.id,
            endpoint: row.endpoint,
            user_agent: row.user_agent,
            created_at: row.created_at,
            last_success_at: row.last_success_at,
        })
        .collect();
    Ok(Json(out))
}

#[utoipa::path(delete, path = "/api/notifications/me/push", tag = "notifications")]
pub async fn delete_push_subscription(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<DeletePushRequest>,
) -> AppResult<StatusCode> {
    let sub = require_sub(&auth)?;
    push_subscription::Entity::delete_many()
        .filter(push_subscription::Column::KeycloakSub.eq(sub))
        .filter(push_subscription::Column::Endpoint.eq(body.endpoint))
        .exec(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/notifications/me/push/test", tag = "notifications")]
pub async fn test_push(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Vec<PushAttempt>>> {
    let sub = require_sub(&auth)?;
    let attempts = send_to_user(
        &state,
        &sub,
        "Test notification",
        "Push notifications are working.",
    )
    .await?;
    Ok(Json(attempts))
}

#[utoipa::path(post, path = "/api/notifications/me/push/ping", tag = "notifications")]
pub async fn schedule_ping(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<PingRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let sub = require_sub(&auth)?;
    let seconds = body.seconds.clamp(5, 3600);
    let owned_sub = sub.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
        let _ = send_to_user(
            &state,
            &owned_sub,
            "Ping",
            &format!("Your {seconds}-second ping."),
        )
        .await;
    });
    Ok(Json(serde_json::json!({ "seconds": seconds })))
}

/// Notification oversight: per-channel health, the delivery log, a one-off test send and the
/// subscriber roster. Administrator only, the same gate the rest of the roster carries.
pub fn oversight_routes() -> Router<AppState> {
    Router::new()
        .route("/notifications/health", get(get_health))
        .route("/notifications/deliveries", get(list_delivery_log))
        .route("/notifications/health/refresh", post(refresh_health))
        .route("/notifications/test-send", post(test_send))
        .route("/notifications/subscribers", get(list_subscribers))
        .layer(middleware::from_fn(
            crate::common::middleware::require_admin,
        ))
}

/// Self-service notification preferences: any Keycloak user manages their OWN settings.
///
/// The gate is `require_read_data` rather than an admin one because every handler binds to the
/// caller's JWT `sub`; an API token has no user sub and each handler refuses one itself.
pub fn subscriber_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/notifications/me",
            get(get_my_notifications).patch(update_my_notifications),
        )
        .route("/notifications/me/subscriptions", put(set_my_subscriptions))
        .route(
            "/notifications/me/push",
            post(register_push_subscription)
                .get(list_push_subscriptions)
                .delete(delete_push_subscription),
        )
        .route("/notifications/channels", get(list_channels))
        .route("/notifications/me/push/test", post(test_push))
        .route("/notifications/me/push/ping", post(schedule_ping))
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_data,
        ))
}
