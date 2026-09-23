//! Notification queries and the delivery machinery they share: live role resolution, the Web Push
//! channel, message rendering, and the trigger state the flows claim through.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use moka::future::Cache;
use sea_orm::sea_query::extension::postgres::PgFunc;
use sea_orm::sea_query::{
    Alias, Asterisk, Condition, Expr, ExprTrait, Func, OnConflict, Order, PgDateTruncUnit, Query,
    SelectStatement,
};
use sea_orm::{
    ActiveValue, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, ConnectionTrait,
    DatabaseConnection, DbErr, EntityTrait, FromQueryResult, JoinType, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, RelationTrait, TransactionTrait, TryInsertResult,
};
use std::fmt::Write as _;
use uuid::Uuid;

use super::models::subscriber::NotificationSubscriber;
use super::models::*;
use crate::common::AppState;
use crate::common::authz::Role;
use crate::common::grants::load_grants;
use crate::common::middleware::AuthContext;
use crate::common::paging::{Page, Window};
use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::routes::private::alarms::models::alarm_event;
use crate::routes::private::api_tokens::service as users;
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings::decision_model;
use crate::routes::private::reprocessing_jobs::models::job;
use crate::routes::private::sites::models as sites;
use crate::routes::private::sync::hold_model;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::events as sync_events;
use crate::routes::private::sync::models::services as sync_services;

pub(super) const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// Caches resolved roles for a short TTL. `resolve` returns `None` when Keycloak is unavailable
/// (fail closed) and `Some(Revoked)` for a definitive negative.
pub struct Authorizer {
    cache: Cache<String, RoleResolution>,
}

impl Default for Authorizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Authorizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(60))
                .build(),
        }
    }

    pub async fn resolve(&self, state: &AppState, sub: &str) -> Option<RoleResolution> {
        if let Some(cached) = self.cache.get(sub).await {
            return Some(cached);
        }
        let resolved = resolve_live(state, sub).await?;
        self.cache.insert(sub.to_string(), resolved.clone()).await;
        Some(resolved)
    }

    pub async fn invalidate(&self, sub: &str) {
        self.cache.invalidate(sub).await;
    }

    /// Seed a resolved role into the cache, bypassing live Keycloak resolution. For tests that
    /// reconcile against a known role set without a Keycloak backend.
    pub async fn prime(&self, sub: &str, resolution: RoleResolution) {
        self.cache.insert(sub.to_string(), resolution).await;
    }
}

pub(super) async fn resolve_live(state: &AppState, sub: &str) -> Option<RoleResolution> {
    let token = users::get_admin_token(state).await.ok()?;
    let client = users::admin_client(state).ok()?;
    let base = users::admin_base_url(state).ok()?;

    let resp = client
        .http_client
        .get(format!("{base}/users/{sub}"))
        .bearer_auth(&token)
        .send()
        .await
        .ok()?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Some(RoleResolution::Revoked);
    }
    if !resp.status().is_success() {
        return None;
    }
    let user: serde_json::Value = resp.json().await.ok()?;
    if user["enabled"].as_bool() != Some(true) {
        return Some(RoleResolution::Revoked);
    }

    let roles_resp = client
        .http_client
        .get(format!("{base}/users/{sub}/role-mappings/realm"))
        .bearer_auth(&token)
        .send()
        .await
        .ok()?;
    if !roles_resp.status().is_success() {
        return None;
    }
    let roles: Vec<serde_json::Value> = roles_resp.json().await.ok()?;
    let best = roles
        .iter()
        .filter_map(|r| r["name"].as_str())
        .map(|n| Role::from(n.to_string()))
        .max_by_key(Role::level);
    match best {
        Some(role) if role.grants_access() => Some(RoleResolution::Active(role)),
        _ => Some(RoleResolution::Revoked),
    }
}

/// Project ids `sub` may be notified for. `None` = unrestricted (administrators). `Some(set)` confines
/// a member to their granted projects; an empty set, a member with no grants, or a revoked/
/// unresolvable user, receives nothing (fail closed).
pub async fn accessible_project_ids(state: &AppState, sub: &str) -> Option<HashSet<Uuid>> {
    match state.authorizer.resolve(state, sub).await {
        Some(RoleResolution::Active(Role::Administrator)) => None,
        Some(RoleResolution::Active(_)) => {
            Some((*load_grants(&state.db, &state.grants_cache, sub).await).clone())
        }
        Some(RoleResolution::Revoked) | None => Some(HashSet::new()),
    }
}

/// Whether `project` is accessible given a resolved set. `None` (unrestricted) allows everything.
#[must_use]
pub fn project_allowed(accessible: &Option<HashSet<Uuid>>, project: Uuid) -> bool {
    accessible.as_ref().is_none_or(|ids| ids.contains(&project))
}

#[must_use]
pub fn severity_label(severity: i16) -> &'static str {
    match severity {
        2 => "ALARM",
        1 => "WARNING",
        _ => "INFO",
    }
}

pub(super) fn unit_suffix(units: Option<&str>) -> String {
    match units {
        Some(u) if !u.is_empty() => format!(" {u}"),
        _ => String::new(),
    }
}

#[must_use]
pub fn render_opened(events: &[PendingEvent], dashboard_base: Option<&str>) -> OutgoingMessage {
    let subject = format!("RIVER Data alarm: {} active", events.len());
    let mut body = format!("Alarm, {} active\n", events.len());
    for e in events {
        let _ = writeln!(
            body,
            "{} / {}: {:.2}{} ({})",
            e.site_name,
            e.parameter_name,
            e.value,
            unit_suffix(e.units.as_deref()),
            severity_label(e.severity)
        );
    }
    if let Some(base) = dashboard_base {
        let _ = write!(body, "View: {}", dashboard_link(base, "/alarms"));
    }
    OutgoingMessage {
        kind: "alarm_opened",
        key: None,
        subject,
        body,
        slot: None,
    }
}

#[must_use]
pub fn render_resolved(events: &[PendingEvent], dashboard_base: Option<&str>) -> OutgoingMessage {
    let subject = format!("RIVER Data resolved: {}", events.len());
    let mut body = format!("Resolved, {}\n", events.len());
    for e in events {
        let _ = writeln!(
            body,
            "{} / {} is back in range",
            e.site_name, e.parameter_name
        );
    }
    if let Some(base) = dashboard_base {
        let _ = write!(body, "View: {}", dashboard_link(base, "/alarms"));
    }
    OutgoingMessage {
        kind: "alarm_resolved",
        key: None,
        subject,
        body,
        slot: None,
    }
}

/// The `notification_state` kind a channel's health is recorded under, so the probe and the read
/// cannot drift apart.
pub(super) const CHANNEL_HEALTH_KIND: &str = "channel_health";

/// Probe every configured channel and upsert its health row, returning how many were probed. A
/// no-op when nothing is configured. The walk is a handful of channels, so it reports no progress
/// step: the job is over before a bar could move.
pub async fn probe_once(db: &DatabaseConnection, config: &Config) -> usize {
    let channels = build_channels(config);
    for ch in &channels {
        let (healthy, detail) = match ch.check_health().await {
            Ok(d) => (true, d),
            Err(e) => (false, e),
        };
        let row = state::ActiveModel {
            kind: ActiveValue::Set(CHANNEL_HEALTH_KIND.to_string()),
            subject_key: ActiveValue::Set(ch.name().to_string()),
            state: ActiveValue::Set(if healthy { "healthy" } else { "unhealthy" }.to_string()),
            last_notified_at: ActiveValue::Set(Utc::now()),
            detail: ActiveValue::Set(Some(detail)),
        };
        let res = state::Entity::insert(row)
            .on_conflict(
                OnConflict::columns([state::Column::Kind, state::Column::SubjectKey])
                    .update_columns([
                        state::Column::State,
                        state::Column::Detail,
                        state::Column::LastNotifiedAt,
                    ])
                    .to_owned(),
            )
            .exec(db)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, channel = ch.name(), "failed to upsert channel health");
        }
    }
    channels.len()
}

pub(super) async fn read_health(db: &DatabaseConnection, config: &Config) -> NotificationHealth {
    let known = [("web_push", config.web_push_configured())];
    let mut channels = Vec::with_capacity(known.len());
    for (name, available) in known {
        let row = state::Entity::find()
            .filter(state::Column::Kind.eq(CHANNEL_HEALTH_KIND))
            .filter(state::Column::SubjectKey.eq(name))
            .one(db)
            .await
            .ok()
            .flatten();
        // A health report degrades rather than fails: a row it cannot read is a channel whose
        // state is unknown, which is what `None` says on every one of these three.
        let (healthy, detail, checked_at) = match row {
            Some(r) => (
                Some(r.state == "healthy"),
                r.detail,
                Some(r.last_notified_at),
            ),
            None => (None, None, None),
        };
        channels.push(ChannelHealth {
            name: name.to_string(),
            available,
            healthy,
            detail,
            checked_at,
        });
    }
    let (undeliverable_24h, failed_24h) = recent_failures(db).await;
    NotificationHealth {
        no_channel_configured: channels.iter().all(|c| !c.available),
        channels,
        undeliverable_24h,
        failed_24h,
    }
}

/// How many deliveries reached nobody in the last day, by kind of failure.
pub(super) async fn recent_failures(db: &DatabaseConnection) -> (i64, i64) {
    let counted = log::Entity::find()
        .select_only()
        .column_as(count_where(log_status("undeliverable")), "undeliverable")
        .column_as(count_where(log_status("failed")), "failed")
        .filter(log::Column::CreatedAt.gt(Utc::now() - chrono::Duration::hours(24)))
        .into_tuple::<(i64, i64)>()
        .one(db)
        .await;
    // An aggregate always returns its one row, so `None` is unreachable; the default stands
    // because this function reports health and must not fail.
    match counted {
        Ok(Some(counts)) => counts,
        Ok(None) => (0, 0),
        Err(e) => {
            tracing::warn!(error = %e, "failed to count recent notification failures");
            (0, 0)
        }
    }
}

/// What one channel has sent lately, by the day, week and month.
#[derive(FromQueryResult)]
pub(super) struct ChannelCounts {
    pub(super) kind: String,
    pub(super) sent_1d: i64,
    pub(super) sent_7d: i64,
    pub(super) sent_30d: i64,
}

/// A send is a delivery the dispatcher recorded as `sent`; a failed or muted attempt is not one.
/// `created_at` is the only time the row carries.
pub(super) async fn sent_counts_by_kind(
    db: &DatabaseConnection,
) -> Result<Vec<ChannelCounts>, DbErr> {
    let sent_within = |days: i64| {
        count_where(
            log_status("sent")
                .add(log::Column::CreatedAt.gt(Utc::now() - chrono::Duration::days(days))),
        )
    };
    log::Entity::find()
        .select_only()
        .column(log::Column::Kind)
        .column_as(sent_within(1), "sent_1d")
        .column_as(sent_within(7), "sent_7d")
        .column_as(sent_within(30), "sent_30d")
        .group_by(log::Column::Kind)
        .into_model::<ChannelCounts>()
        .all(db)
        .await
}

/// The statuses `dispatcher::deliver` writes. A filter naming anything else is refused rather than
/// silently returning nothing.
pub(super) const STATUSES: [&str; 5] = ["sent", "failed", "muted", "undeliverable", "skipped"];

pub(super) const MAX_LIMIT: u64 = 200;

/// The rows this file's two queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
pub(super) struct MessageRow {
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
pub(super) struct RecipientRow {
    alarm_event_id: Option<Uuid>,
    kind: String,
    at: DateTime<Utc>,
    channel: String,
    recipient: String,
    status: String,
    error: Option<String>,
    created_at: DateTime<Utc>,
}

pub(super) fn validate(value: &str, field: &str) -> AppResult<()> {
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

    let rows = MessageRow::find_by_statement(PG.build(&message_page(q, limit, offset)))
        .all(db)
        .await?;
    let mut messages = Vec::with_capacity(rows.len());
    for row in rows {
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

    let total = db
        .query_one_raw(PG.build(&message_count(q)))
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
pub(super) async fn attach_recipients(
    db: &DatabaseConnection,
    messages: &mut [DeliveryMessage],
) -> AppResult<()> {
    let oldest = messages.iter().map(|m| m.at).min().expect("page not empty");
    let newest = messages.iter().map(|m| m.at).max().expect("page not empty");
    let rows = log::Entity::find()
        .select_only()
        .columns([log::Column::AlarmEventId, log::Column::Kind])
        .column_as(sent_second(), "at")
        .columns([
            log::Column::Channel,
            log::Column::Recipient,
            log::Column::Status,
            log::Column::Error,
            log::Column::CreatedAt,
        ])
        .filter(log::Column::CreatedAt.gte(oldest))
        .filter(log::Column::CreatedAt.lt(newest + chrono::Duration::seconds(1)))
        .order_by_asc(log::Column::CreatedAt)
        .into_model::<RecipientRow>()
        .all(db)
        .await?;

    for row in rows {
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

/// `COUNT(*) FILTER (WHERE condition)` as a bigint, which is what every reader decodes a count as.
pub(super) fn count_where(condition: Condition) -> Expr {
    Expr::from(Func::count(Expr::col(Asterisk)).filter(condition)).cast_as(Alias::new("bigint"))
}

/// `COUNT(*)` as a bigint.
fn count_all() -> Expr {
    Expr::from(Func::count(Expr::col(Asterisk))).cast_as(Alias::new("bigint"))
}

/// The delivery rows written with one status.
fn log_status(status: &str) -> Condition {
    Condition::all().add(log::Column::Status.eq(status))
}

/// The second a delivery row was written in. The rows one send writes share it, so it is the
/// message's time.
fn sent_second() -> Expr {
    PgFunc::date_trunc(
        PgDateTruncUnit::Second,
        Expr::col((log::Entity, log::Column::CreatedAt)),
    )
    .into()
}

/// The message time as a group selects it. The groups are keyed on this output column rather than
/// on the expression again, which would bind its unit a second time and read to Postgres as a
/// different expression.
fn at() -> Expr {
    Expr::col(Alias::new("at"))
}

/// One group per message a delivery filter keeps, selecting its time. The page and its count
/// both start from here, so a message's counts and its page position agree.
fn message_groups(q: &DeliveryQuery) -> SelectStatement {
    let mut groups = Query::select();
    groups
        .expr_as(sent_second(), Alias::new("at"))
        .from(log::Entity)
        .add_group_by([
            Expr::col((log::Entity, log::Column::AlarmEventId)),
            Expr::col((log::Entity, log::Column::Kind)),
            at(),
        ]);
    if let Some(kind) = &q.kind {
        groups.and_where(log::Column::Kind.eq(kind.as_str()));
    }
    if let Some(status) = &q.status {
        groups.and_having(count_where(log_status(status)).gt(0));
    }
    groups
}

/// One page of messages, newest first, each with the slot its alarm event names and a count per
/// status.
fn message_page(q: &DeliveryQuery, limit: u64, offset: u64) -> SelectStatement {
    let mut page = message_groups(q);
    page.column((log::Entity, log::Column::AlarmEventId))
        .column((log::Entity, log::Column::Kind))
        .expr_as(
            Expr::col((sites::Entity, sites::Column::Name)),
            Alias::new("site_name"),
        )
        .expr_as(
            Expr::col((parameters::Entity, parameters::Column::Name)),
            Alias::new("parameter_name"),
        )
        .expr_as(count_all(), Alias::new("total"));
    for status in STATUSES {
        page.expr_as(count_where(log_status(status)), Alias::new(status));
    }
    page.left_join(
        alarm_event::Entity,
        Expr::col((alarm_event::Entity, alarm_event::Column::Id))
            .equals((log::Entity, log::Column::AlarmEventId)),
    )
    .left_join(
        sites::Entity,
        Expr::col((sites::Entity, sites::Column::Id))
            .equals((alarm_event::Entity, alarm_event::Column::SiteId)),
    )
    .left_join(
        parameters::Entity,
        Expr::col((parameters::Entity, parameters::Column::Id))
            .equals((alarm_event::Entity, alarm_event::Column::ParameterId)),
    )
    .add_group_by([
        Expr::col((sites::Entity, sites::Column::Name)),
        Expr::col((parameters::Entity, parameters::Column::Name)),
    ])
    .order_by_expr(at(), Order::Desc)
    .order_by((log::Entity, log::Column::Kind), Order::Asc)
    .limit(limit)
    .offset(offset);
    page
}

/// How many messages the filter keeps in all, under the column `c`.
fn message_count(q: &DeliveryQuery) -> SelectStatement {
    let groups = message_groups(q);
    Query::select()
        .expr_as(count_all(), Alias::new("c"))
        .from_subquery(groups, Alias::new("g"))
        .to_owned()
}

pub struct WebPushChannel {
    client: reqwest::Client,
    /// The signing key, or why the configured keypair cannot sign.
    key: Result<::web_push::PartialVapidSignatureBuilder, String>,
    vapid_subject: String,
}

impl WebPushChannel {
    /// The channel, when a VAPID keypair and subject are configured. A keypair that is configured
    /// but unusable still builds one, so the health probe reports why and every send fails with it.
    pub fn new(config: &Config) -> Option<Self> {
        let key = web_push_key(config)?;
        let subject = config.vapid_subject.as_ref()?;
        Some(Self {
            client: reqwest::Client::new(),
            key,
            vapid_subject: subject.clone(),
        })
    }
}

/// The configured VAPID signing key, `None` when no keypair is configured.
pub fn web_push_key(
    config: &Config,
) -> Option<Result<::web_push::PartialVapidSignatureBuilder, String>> {
    let pem = config.vapid_private_key_pem.as_deref()?;
    let public_key = config.vapid_public_key.as_deref()?;
    Some(vapid_key(pem, public_key))
}

/// The signing key in `pem`, refused unless `public_key` (base64url, as browsers are handed it) is
/// its public half: a browser subscribed with any other key has every push refused.
pub fn vapid_key(
    pem: &str,
    public_key: &str,
) -> Result<::web_push::PartialVapidSignatureBuilder, String> {
    use base64::Engine;
    let key = ::web_push::VapidSignatureBuilder::from_pem_no_sub(std::io::Cursor::new(pem))
        .map_err(|e| format!("VAPID_PRIVATE_KEY_PEM is not an EC private key: {e}"))?;
    let derived = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.get_public_key());
    if derived != public_key.trim().trim_end_matches('=') {
        return Err("VAPID_PUBLIC_KEY is not the public half of VAPID_PRIVATE_KEY_PEM".to_string());
    }
    Ok(key)
}

#[derive(sea_orm::FromQueryResult)]
pub struct Subscription {
    pub id: Uuid,
    #[sea_orm(alias = "sub")]
    pub keycloak_sub: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// Splits the subscriptions a kind reached into those whose holder's live role is in the channel's
/// audience, and the holders it refused. A role that cannot be resolved is refused: the audience is
/// not confirmed. A channel open to every level resolves nobody.
pub async fn within_audience(
    state: &AppState,
    kind: &str,
    subscriptions: Vec<Subscription>,
) -> (Vec<Subscription>, Vec<String>) {
    if channel(kind).is_none_or(|c| c.audience.level() <= Role::Intern.level()) {
        return (subscriptions, Vec::new());
    }
    let mut admitted = Vec::with_capacity(subscriptions.len());
    let mut refused = Vec::new();
    for sub in subscriptions {
        let resolution = state.authorizer.resolve(state, &sub.keycloak_sub).await;
        match resolution.as_ref().and_then(RoleResolution::role) {
            Some(role) if admits(kind, role) => admitted.push(sub),
            _ => refused.push(sub.keycloak_sub),
        }
    }
    (admitted, refused)
}

/// The path the dashboard is served under, `paths.base` in `river-data-ui/svelte.config.js`.
/// `DASHBOARD_BASE_URL` is the origin alone, so every link a message carries is built through
/// [`dashboard_link`].
const DASHBOARD_PATH: &str = "/admin";

/// `path` in the dashboard at `base`.
fn dashboard_link(base: &str, path: &str) -> String {
    format!("{}{DASHBOARD_PATH}{path}", base.trim_end_matches('/'))
}

/// The tag a device collapses notifications by: a later message with the same tag replaces the
/// earlier one. A slot-scoped message is one per kind and slot, a keyed one per kind and key, and
/// a digest one per kind.
pub(super) fn push_tag(msg: &OutgoingMessage) -> String {
    match (&msg.slot, &msg.key) {
        (Some(s), _) => format!("{}:{}:{}", msg.kind, s.site_id, s.parameter_id),
        (None, Some(key)) => format!("{}:{key}", msg.kind),
        (None, None) => msg.kind.to_string(),
    }
}

pub(super) fn deep_link_url(base: Option<&str>, slot: &Option<Slot>) -> Option<String> {
    let base = base?;
    Some(match slot {
        Some(s) => dashboard_link(
            base,
            &format!("/sites/{}?focus={}", s.site_id, s.parameter_id),
        ),
        None => dashboard_link(base, "/alarms"),
    })
}

/// The push subscriptions a message reaches: subscribers who have the channel on, are in the
/// message's group, and have not turned that group off for this slot.
///
/// A group with no row for a subscriber reads as its own default (`alarms` on, `sync` off), so the
/// audience is right before anyone has visited the preferences page. `group` is `None` for a kind
/// with no audience of its own, which is narrowed by the channel toggle alone.
pub async fn slot_subscriptions(
    db: &DatabaseConnection,
    slot: &Option<Slot>,
    kind: &str,
) -> Result<Vec<Subscription>, String> {
    let mut audience = enabled_subscriptions();
    // A kind no channel answers for is addressed to whoever asked for it, never fanned out by
    // subscription: the test send is the only one.
    if channel(kind).is_some() {
        audience.and_where(group_subscribed(slot, kind));
    }
    read_subscriptions(db, &audience).await
}

/// Every push device whose holder has not switched push off. A holder with no subscriber row
/// has not switched it off.
fn enabled_subscriptions() -> SelectStatement {
    let device = |c: push_subscription::Column| Expr::col((push_subscription::Entity, c));
    Query::select()
        .expr_as(device(push_subscription::Column::Id), Alias::new("id"))
        .expr_as(
            device(push_subscription::Column::KeycloakSub),
            Alias::new("sub"),
        )
        .expr_as(
            device(push_subscription::Column::Endpoint),
            Alias::new("endpoint"),
        )
        .expr_as(
            device(push_subscription::Column::P256dh),
            Alias::new("p256dh"),
        )
        .expr_as(device(push_subscription::Column::Auth), Alias::new("auth"))
        .from(push_subscription::Entity)
        .left_join(
            subscriber::Entity,
            Expr::col((subscriber::Entity, subscriber::Column::KeycloakSub)).equals((
                push_subscription::Entity,
                push_subscription::Column::KeycloakSub,
            )),
        )
        .and_where(
            Func::coalesce([
                Expr::col((subscriber::Entity, subscriber::Column::WebPushEnabled)),
                Expr::val(true),
            ])
            .into(),
        )
        .to_owned()
}

/// Whether the device's holder has this channel on for the slot. The most specific row the
/// holder keeps for the channel wins: parameter, then site, then project, then the channel-wide
/// row. With none of them the channel's own default stands.
fn group_subscribed(slot: &Option<Slot>, kind: &str) -> Expr {
    use subscription::Column as C;
    let row = |c: C| Expr::col((subscription::Entity, c));
    let mut scopes = Condition::any().add(
        Condition::all()
            .add(row(C::ProjectId).is_null())
            .add(row(C::SiteId).is_null())
            .add(row(C::ParameterId).is_null()),
    );
    if let Some(s) = slot {
        scopes = scopes
            .add(
                Condition::all()
                    .add(row(C::SiteId).eq(s.site_id))
                    .add(row(C::ParameterId).eq(s.parameter_id)),
            )
            .add(
                Condition::all()
                    .add(row(C::SiteId).eq(s.site_id))
                    .add(row(C::ParameterId).is_null()),
            );
        if let Some(project_id) = s.project_id {
            scopes = scopes.add(
                Condition::all()
                    .add(row(C::ProjectId).eq(project_id))
                    .add(row(C::SiteId).is_null())
                    .add(row(C::ParameterId).is_null()),
            );
        }
    }
    let most_specific = Query::select()
        .expr(row(C::Enabled))
        .from(subscription::Entity)
        .cond_where(
            Condition::all()
                .add(row(C::KeycloakSub).equals((
                    push_subscription::Entity,
                    push_subscription::Column::KeycloakSub,
                )))
                .add(row(C::Channel).eq(kind))
                .add(scopes),
        )
        .order_by_expr(row(C::ParameterId).is_not_null(), Order::Desc)
        .order_by_expr(row(C::SiteId).is_not_null(), Order::Desc)
        .order_by_expr(row(C::ProjectId).is_not_null(), Order::Desc)
        .limit(1)
        .to_owned();
    Func::coalesce([Expr::expr(most_specific), Expr::val(on_by_default(kind))]).into()
}

pub(super) async fn read_subscriptions(
    db: &DatabaseConnection,
    audience: &SelectStatement,
) -> Result<Vec<Subscription>, String> {
    Subscription::find_by_statement(PG.build(audience))
        .all(db)
        .await
        .map_err(|e| e.to_string())
}

pub(super) async fn prune_subscription(db: &DatabaseConnection, id: Uuid) {
    if let Err(e) = push_subscription::Entity::delete_by_id(id).exec(db).await {
        tracing::warn!(error = %e, "web_push: failed to prune expired subscription");
    }
}

pub(super) async fn stamp_success(db: &DatabaseConnection, id: Uuid) {
    let _ = push_subscription::Entity::update_many()
        .col_expr(
            push_subscription::Column::LastSuccessAt,
            Expr::current_timestamp(),
        )
        .filter(push_subscription::Column::Id.eq(id))
        .exec(db)
        .await;
}

pub async fn send_push(
    client: &reqwest::Client,
    key: &Result<::web_push::PartialVapidSignatureBuilder, String>,
    vapid_subject: &str,
    sub: &Subscription,
    payload: &[u8],
) -> Result<(), String> {
    let info = ::web_push::SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);

    let mut sig_builder = key
        .clone()
        .map_err(|e| format!("VAPID key: {e}"))?
        .add_sub_info(&info);
    sig_builder.add_claim("sub", serde_json::Value::String(vapid_subject.to_string()));
    let sig = sig_builder
        .build()
        .map_err(|e| format!("VAPID sign: {e}"))?;

    let mut builder = ::web_push::WebPushMessageBuilder::new(&info);
    builder.set_payload(::web_push::ContentEncoding::Aes128Gcm, payload);
    builder.set_vapid_signature(sig);
    builder.set_ttl(86400);

    let message = builder.build().map_err(|e| format!("build: {e}"))?;

    let http_req = ::web_push::request_builder::build_request::<Vec<u8>>(message);
    let (parts, body) = http_req.into_parts();

    let mut req = client.request(
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap(),
        parts.uri.to_string(),
    );
    for (name, value) in &parts.headers {
        req = req.header(name.as_str(), value.as_bytes());
    }
    req = req.body(body);

    let resp = req.send().await.map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    match status.as_u16() {
        410 => Err("EndpointNotValid".to_string()),
        404 => Err("EndpointNotFound".to_string()),
        401 => Err(format!("Unauthorized: {body}")),
        413 => Err("PayloadTooLarge".to_string()),
        code => Err(format!("HTTP {code}: {body}")),
    }
}

#[async_trait::async_trait]
impl NotificationChannel for WebPushChannel {
    fn name(&self) -> &'static str {
        "web_push"
    }

    async fn check_health(&self) -> Result<String, String> {
        match &self.key {
            Ok(_) => Ok("VAPID keypair valid".to_string()),
            Err(e) => Err(e.clone()),
        }
    }

    async fn deliver(
        &self,
        state: &AppState,
        msg: &OutgoingMessage,
    ) -> Result<Vec<DeliveryResult>, String> {
        let db = &state.db;
        let subscriptions = slot_subscriptions(db, &msg.slot, msg.kind)
            .await
            .map_err(|e| format!("failed to load subscriptions: {e}"))?;

        let (subscriptions, refused) = within_audience(state, msg.kind, subscriptions).await;
        for recipient in &refused {
            log_delivery(
                db,
                None,
                msg.kind,
                self.name(),
                recipient,
                "skipped",
                Some("below the channel's audience"),
            )
            .await;
        }

        let project = msg.slot.as_ref().and_then(|s| s.project_id);

        let url = deep_link_url(state.config.dashboard_base_url.as_deref(), &msg.slot);

        let payload = serde_json::json!({
            "title": msg.subject,
            "body": msg.body,
            "url": url,
            "tag": push_tag(msg),
        });
        let payload_bytes = payload.to_string().into_bytes();

        let mut results = Vec::with_capacity(subscriptions.len());
        for sub in &subscriptions {
            if let Some(p) = project
                && !project_allowed(&accessible_project_ids(state, &sub.keycloak_sub).await, p)
            {
                continue;
            }

            let outcome = send_push(
                &self.client,
                &self.key,
                &self.vapid_subject,
                sub,
                &payload_bytes,
            )
            .await;

            match &outcome {
                Ok(()) => stamp_success(db, sub.id).await,
                Err(e) if e.contains("EndpointNotValid") || e.contains("EndpointNotFound") => {
                    tracing::info!(endpoint = %sub.endpoint, "web_push: pruning expired subscription");
                    prune_subscription(db, sub.id).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, endpoint = %sub.endpoint, "web_push: delivery failed");
                }
            }

            results.push(DeliveryResult {
                recipient: sub.keycloak_sub.clone(),
                outcome: outcome.map_err(|e| e.to_string()),
            });
        }
        Ok(results)
    }
}

pub(super) struct Row {
    pub(super) id: Uuid,
    pub(super) slot: (Uuid, Uuid),
    pub(super) project_id: Uuid,
    pub(super) event: PendingEvent,
}

/// One outbox row, as either arm of [`fetch_pending`] selects it. Both arms name the same columns,
/// so the two differ in which severity and value they carry, not in shape. `units` is the
/// parameter's own, which is legitimately null.
#[derive(FromQueryResult)]
pub(super) struct PendingRow {
    pub(super) id: Uuid,
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
    pub(super) project_id: Uuid,
    pub(super) site_name: String,
    pub(super) parameter_name: String,
    pub(super) units: Option<String>,
    pub(super) severity: i16,
    pub(super) value: f64,
}

/// Build the enabled channels from config. Empty when nothing is configured (the API runs fine
/// without notifications, the dispatcher then just stamps the outbox and sends nothing).
pub fn build_channels(config: &Config) -> Vec<Box<dyn NotificationChannel>> {
    let mut channels: Vec<Box<dyn NotificationChannel>> = Vec::new();
    if let Some(ch) = WebPushChannel::new(config) {
        channels.push(Box::new(ch));
        tracing::info!("Notifications: Web Push channel enabled");
    }
    channels
}

pub(super) async fn fetch_pending(
    db: &DatabaseConnection,
    opened: bool,
) -> Result<Vec<Row>, DbErr> {
    use alarm_event::Column as C;
    let pending = alarm_event::Entity::find()
        .select_only()
        .columns([C::Id, C::SiteId, C::ParameterId])
        .column_as(sites::Column::ProjectId, "project_id")
        .column_as(sites::Column::Name, "site_name")
        .column_as(parameters::Column::Name, "parameter_name")
        .column_as(parameters::Column::DefaultUnits, "units")
        .join(JoinType::InnerJoin, alarm_event::Relation::Site.def())
        .join(JoinType::InnerJoin, alarm_event::Relation::Parameter.def());
    let pending = if opened {
        pending
            .column_as(C::Severity, "severity")
            .column_as(C::LastValue, "value")
            .filter(C::NotifiedAt.is_null())
            .filter(C::ResolvedAt.is_null())
            .order_by_asc(C::StartedAt)
    } else {
        pending
            .column_as(C::MaxSeverity, "severity")
            .column_as(
                Expr::from(Func::coalesce([
                    Expr::col((alarm_event::Entity, C::ResolvedValue)),
                    Expr::col((alarm_event::Entity, C::LastValue)),
                ])),
                "value",
            )
            .filter(C::ResolvedAt.is_not_null())
            .filter(C::ResolutionNotifiedAt.is_null())
            .order_by_asc(C::ResolvedAt)
    };
    let rows = pending.into_model::<PendingRow>().all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| Row {
            id: r.id,
            slot: (r.site_id, r.parameter_id),
            project_id: r.project_id,
            event: PendingEvent {
                site_name: r.site_name,
                parameter_name: r.parameter_name,
                units: r.units,
                severity: r.severity,
                value: r.value,
            },
        })
        .collect())
}

/// Deliver to every channel, log each attempt, and decide whether to stamp the outbox: stamp when
/// nothing was attempted (no channels/recipients) or at least one delivery succeeded; otherwise leave
/// it for the next tick to retry.
///
/// Every path through here writes a `notification_log` row, including the two that send nothing: a
/// muted slot and a deployment with no channel configured at all. The stamp says the dispatcher is
/// finished with the message, not that anyone was told.
///
/// This is the single gate every notification passes through, so the mute check lives here rather
/// than in each caller: a slot-keyed message for a muted slot is dropped before any channel sees it
/// and reported as delivered, which is what stamps the outbox and leaves the trigger dedup state in
/// place. A message with no slot (a sync-failure digest) has nothing to mute against and always
/// goes out.
pub(super) async fn deliver(
    state: &AppState,
    channels: &[Box<dyn NotificationChannel>],
    msg: &OutgoingMessage,
    single_event_id: Option<Uuid>,
) -> bool {
    let db = &state.db;
    if let Some(slot) = &msg.slot {
        match is_muted(db, slot.site_id, slot.parameter_id).await {
            Ok(true) => {
                // Suppression is an outcome, not an absence: without this row the delivery log
                // reads the same as a slot nobody subscribes to.
                log_delivery(
                    db,
                    single_event_id,
                    msg.kind,
                    "all",
                    &format!("slot:{}:{}", slot.site_id, slot.parameter_id),
                    "muted",
                    None,
                )
                .await;
                return true;
            }
            Ok(false) => {}
            // An unreadable mute table must not silently unmute a slot, nor drop the alert: leave
            // the message unsent and unstamped so the next tick reassesses it.
            Err(e) => {
                tracing::warn!(error = %e, "mute lookup failed, deferring delivery");
                return false;
            }
        }
    }
    // A message with nowhere to go is an outcome, not an absence. Notifications are complementary
    // and never block ingestion, so the outbox row is still stamped; without this row the log reads
    // exactly like a clean delivery and nothing anywhere says the alarm went nowhere.
    if channels.is_empty() {
        log_delivery(
            db,
            single_event_id,
            msg.kind,
            "-",
            "-",
            "undeliverable",
            Some("no notification channel is configured"),
        )
        .await;
        return true;
    }

    let mut attempted = 0usize;
    let mut any_success = false;
    for ch in channels {
        // A channel that could not say who to tell has failed everyone it would have told, so it
        // counts as a failed attempt and, alone, leaves the message for the next tick.
        let results = match ch.deliver(state, msg).await {
            Ok(results) => results,
            Err(e) => {
                tracing::warn!(channel = ch.name(), error = %e, "recipient lookup failed");
                attempted += 1;
                log_delivery(
                    db,
                    single_event_id,
                    msg.kind,
                    ch.name(),
                    "-",
                    "failed",
                    Some(&e),
                )
                .await;
                continue;
            }
        };
        if results.is_empty() {
            log_delivery(
                db,
                single_event_id,
                msg.kind,
                ch.name(),
                "-",
                "skipped",
                None,
            )
            .await;
            continue;
        }
        for r in results {
            attempted += 1;
            let (status, error) = match &r.outcome {
                Ok(()) => {
                    any_success = true;
                    ("sent", None)
                }
                Err(e) => ("failed", Some(e.as_str())),
            };
            log_delivery(
                db,
                single_event_id,
                msg.kind,
                ch.name(),
                &r.recipient,
                status,
                error,
            )
            .await;
        }
    }
    attempted == 0 || any_success
}

pub(super) async fn log_delivery(
    db: &DatabaseConnection,
    alarm_event_id: Option<Uuid>,
    kind: &str,
    channel: &str,
    recipient: &str,
    status: &str,
    error: Option<&str>,
) {
    let row = log::ActiveModel {
        id: Set(Uuid::new_v4()),
        alarm_event_id: Set(alarm_event_id),
        kind: Set(kind.to_string()),
        channel: Set(channel.to_string()),
        recipient: Set(recipient.to_string()),
        status: Set(status.to_string()),
        error: Set(error.map(str::to_string)),
        created_at: NotSet,
    };
    let res = log::Entity::insert(row).exec_without_returning(db).await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "failed to write notification_log row");
    }
}

/// Atomically claim one outbox event by stamping its sent-marker column iff still NULL. The single
/// replica whose UPDATE flips it from NULL wins and sends; a peer that lost the race updates no
/// row and skips.
pub(super) async fn claim_event(
    db: &DatabaseConnection,
    column: alarm_event::Column,
    id: Uuid,
) -> Result<bool, DbErr> {
    let claimed = alarm_event::Entity::update_many()
        .col_expr(column, Expr::current_timestamp())
        .col_expr(alarm_event::Column::UpdatedAt, Expr::current_timestamp())
        .filter(alarm_event::Column::Id.eq(id))
        .filter(column.is_null())
        .exec(db)
        .await?;
    Ok(claimed.rows_affected == 1)
}

/// Release a claim after an all-channel send failure so the next tick retries it (at-least-once).
pub(super) async fn release_claim(
    db: &DatabaseConnection,
    column: alarm_event::Column,
    id: Uuid,
) -> Result<(), DbErr> {
    alarm_event::Entity::update_many()
        .col_expr(
            column,
            Expr::value(Option::<chrono::DateTime<chrono::Utc>>::None),
        )
        .filter(alarm_event::Column::Id.eq(id))
        .exec(db)
        .await?;
    Ok(())
}

pub(super) fn require_sub(auth: &AuthContext) -> AppResult<String> {
    auth.keycloak_sub().map(str::to_string).ok_or_else(|| {
        AppError::Forbidden("notification preferences require a Keycloak login".to_string())
    })
}

/// The caller's level for choosing channels; a caller with no realm role is below every audience.
pub(super) fn caller_role(auth: &AuthContext) -> Role {
    auth.highest_role()
        .unwrap_or_else(|| Role::Unknown(String::new()))
}

// --- The roster ---

/// What the roster reports over the stored row: how many push devices the person has registered.
/// The generated list is the roster, so the count is attached to it here rather than spelled in a
/// route of its own.
pub struct SubscriberOperations;

impl CRUDOperations for SubscriberOperations {
    type Resource = NotificationSubscriber;

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut NotificationSubscriber,
    ) -> Result<(), ApiError> {
        let counts = push_device_counts(db, std::slice::from_ref(&entity.keycloak_sub)).await?;
        entity.push_subscription_count =
            Some(counts.get(&entity.keycloak_sub).copied().unwrap_or(0));
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<<NotificationSubscriber as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        if entities.is_empty() {
            return Ok(());
        }
        let subs: Vec<String> = entities.iter().map(|e| e.keycloak_sub.clone()).collect();
        let counts = push_device_counts(db, &subs).await?;
        for entity in entities.iter_mut() {
            entity.push_subscription_count =
                Some(counts.get(&entity.keycloak_sub).copied().unwrap_or(0));
        }
        Ok(())
    }
}

/// Registered push devices per login, for the logins asked about. A login with no device is absent
/// from the result rather than carrying a zero.
pub async fn push_device_counts<C: ConnectionTrait>(
    db: &C,
    subs: &[String],
) -> Result<std::collections::HashMap<String, i64>, ApiError> {
    #[derive(FromQueryResult)]
    struct DeviceCount {
        keycloak_sub: String,
        devices: i64,
    }

    let rows = push_subscription::Entity::find()
        .select_only()
        .column(push_subscription::Column::KeycloakSub)
        .column_as(push_subscription::Column::Id.count(), "devices")
        .filter(push_subscription::Column::KeycloakSub.is_in(subs.to_vec()))
        .group_by(push_subscription::Column::KeycloakSub)
        .into_model::<DeviceCount>()
        .all(db)
        .await
        .map_err(ApiError::database)?;
    Ok(rows
        .into_iter()
        .map(|r| (r.keycloak_sub, r.devices))
        .collect())
}

pub(super) async fn ensure_subscriber(state: &AppState, sub: &str) -> AppResult<()> {
    subscriber::Entity::insert(subscriber::ActiveModel {
        id: Set(Uuid::new_v4()),
        keycloak_sub: Set(sub.to_string()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(subscriber::Column::KeycloakSub)
            .do_nothing()
            .to_owned(),
    )
    .try_insert()
    .exec(&state.db)
    .await?;
    Ok(())
}

/// The subscriber row for this login, or `None` before their first visit.
pub(super) async fn find_subscriber(
    db: &DatabaseConnection,
    sub: &str,
) -> AppResult<Option<subscriber::Model>> {
    Ok(subscriber::Entity::find()
        .filter(subscriber::Column::KeycloakSub.eq(sub))
        .one(db)
        .await?)
}

pub(super) async fn load(state: &AppState, sub: &str) -> AppResult<MyNotifications> {
    // No subscriber row means the default: every channel is on until somebody turns one off.
    let web_push_enabled = find_subscriber(&state.db, sub)
        .await?
        .is_none_or(|row| row.web_push_enabled);

    let push_count = i64::try_from(
        push_subscription::Entity::find()
            .filter(push_subscription::Column::KeycloakSub.eq(sub))
            .count(&state.db)
            .await?,
    )
    .unwrap_or(i64::MAX);

    let sub_rows = subscription::Entity::find()
        .filter(subscription::Column::KeycloakSub.eq(sub))
        .all(&state.db)
        .await?;
    let mut subscriptions = Vec::with_capacity(sub_rows.len());
    for row in sub_rows {
        subscriptions.push(SubscriptionScope {
            channel: row.channel,
            project_id: row.project_id,
            site_id: row.site_id,
            parameter_id: row.parameter_id,
            enabled: row.enabled,
        });
    }

    Ok(MyNotifications {
        web_push_enabled,
        push_subscription_count: push_count,
        subscriptions,
    })
}

pub(super) fn endpoint_tail(endpoint: &str) -> String {
    let count = endpoint.chars().count();
    endpoint.chars().skip(count.saturating_sub(12)).collect()
}

pub(super) async fn send_to_user(
    state: &AppState,
    keycloak_sub: &str,
    title: &str,
    body: &str,
) -> AppResult<Vec<PushAttempt>> {
    let rows = push_subscription::Entity::find()
        .filter(push_subscription::Column::KeycloakSub.eq(keycloak_sub))
        .order_by_asc(push_subscription::Column::CreatedAt)
        .all(&state.db)
        .await?;

    if rows.is_empty() {
        return Err(AppError::BadRequest(
            "no push subscriptions registered".to_string(),
        ));
    }

    let Some(channel) = WebPushChannel::new(&state.config) else {
        return Err(AppError::Internal("VAPID not configured".to_string()));
    };

    // A unique tag per send: notifications sharing a tag replace each other, so a fixed
    // "test" tag makes the second test silently overwrite the first instead of alerting.
    let payload = serde_json::json!({
        "title": title,
        "body": body,
        "tag": format!("test-{}", Utc::now().timestamp_millis()),
    })
    .to_string();

    let mut attempts = Vec::with_capacity(rows.len());

    for row in rows {
        // A row that will not decode must not silence the devices queued behind it.
        let push_subscription::Model {
            id,
            endpoint,
            p256dh,
            auth: auth_key,
            user_agent,
            ..
        } = row;

        let tail = endpoint_tail(&endpoint);
        let sub = Subscription {
            id,
            keycloak_sub: keycloak_sub.to_string(),
            endpoint,
            p256dh,
            auth: auth_key,
        };

        let attempt = match send_push(
            &channel.client,
            &channel.key,
            &channel.vapid_subject,
            &sub,
            payload.as_bytes(),
        )
        .await
        {
            Ok(()) => {
                stamp_success(&state.db, id).await;
                PushAttempt {
                    id,
                    endpoint_tail: tail.clone(),
                    user_agent,
                    status: "sent".to_string(),
                    error: None,
                    pruned: false,
                }
            }
            Err(e) => {
                // 410 and 404 mean the push service has retired the endpoint: it can never
                // deliver again, so the row goes rather than failing on every future send.
                let gone = e.contains("EndpointNotValid") || e.contains("EndpointNotFound");
                if gone {
                    prune_subscription(&state.db, id).await;
                }
                tracing::warn!(error = %e, endpoint_tail = %tail, pruned = gone, "push: delivery failed");
                PushAttempt {
                    id,
                    endpoint_tail: tail.clone(),
                    user_agent,
                    status: "failed".to_string(),
                    error: Some(e),
                    pruned: gone,
                }
            }
        };

        log_delivery(
            &state.db,
            None,
            "test",
            "web_push",
            &tail,
            &attempt.status,
            attempt.error.as_deref(),
        )
        .await;

        attempts.push(attempt);
    }

    Ok(attempts)
}

/// A notification's stored state and when it last fired.
pub(super) async fn state_get(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
) -> Result<Option<(String, DateTime<Utc>)>, DbErr> {
    Ok(state::Entity::find()
        .filter(state::Column::Kind.eq(kind))
        .filter(state::Column::SubjectKey.eq(key))
        .one(db)
        .await?
        .map(|r| (r.state, r.last_notified_at)))
}

pub(super) async fn state_upsert(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
    state: &str,
) -> Result<(), DbErr> {
    let row = state::ActiveModel {
        kind: Set(kind.to_string()),
        subject_key: Set(key.to_string()),
        state: Set(state.to_string()),
        last_notified_at: Set(Utc::now()),
        detail: NotSet,
    };
    state::Entity::insert(row)
        .on_conflict(
            OnConflict::columns([state::Column::Kind, state::Column::SubjectKey])
                .update_columns([state::Column::State, state::Column::LastNotifiedAt])
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    Ok(())
}

pub(super) async fn state_clear(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
) -> Result<(), DbErr> {
    state::Entity::delete_many()
        .filter(state::Column::Kind.eq(kind))
        .filter(state::Column::SubjectKey.eq(key))
        .exec(db)
        .await?;
    Ok(())
}

// Multi-replica claims: each transition is committed to `notification_state` BEFORE the send so that
// at 2-3 replicas exactly one replica sends. The unique (kind, subject_key) key arbitrates the race:
// the conflict action's own `WHERE` decides the winner, and an insert that neither stored nor
// updated a row is the loser's answer (`TryInsertResult::Conflicted`).

fn firing(kind: &str, key: &str, at: DateTime<Utc>) -> state::ActiveModel {
    state::ActiveModel {
        kind: Set(kind.to_string()),
        subject_key: Set(key.to_string()),
        state: Set("firing".to_string()),
        last_notified_at: Set(at),
        detail: NotSet,
    }
}

/// Whether the insert stored or updated a row: the claim was won.
async fn claim_won(
    db: &DatabaseConnection,
    insert: sea_orm::Insert<state::ActiveModel>,
) -> Result<bool, DbErr> {
    Ok(matches!(
        insert.try_insert().exec(db).await?,
        TryInsertResult::Inserted(_)
    ))
}

/// Claim a fresh firing transition: insert the dedup row iff absent. Winner sends.
pub async fn claim_insert(db: &DatabaseConnection, kind: &str, key: &str) -> Result<bool, DbErr> {
    let insert = state::Entity::insert(firing(kind, key, Utc::now())).on_conflict(
        OnConflict::columns([state::Column::Kind, state::Column::SubjectKey])
            .do_nothing()
            .to_owned(),
    );
    claim_won(db, insert).await
}

/// Claim a resolve transition: delete the dedup row. Winner sends the recovery message.
pub async fn claim_clear(db: &DatabaseConnection, kind: &str, key: &str) -> Result<bool, DbErr> {
    let res = state::Entity::delete_many()
        .filter(state::Column::Kind.eq(kind))
        .filter(state::Column::SubjectKey.eq(key))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

/// Claim a (re-)notify with a suppression window: win iff there is no prior alert or the last one was
/// more than `within_hours` ago. Atomically advances the timestamp so a single replica re-notifies.
pub async fn claim_renotify(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
    within_hours: i64,
) -> Result<bool, DbErr> {
    let now = Utc::now();
    let insert = state::Entity::insert(firing(kind, key, now)).on_conflict(
        OnConflict::columns([state::Column::Kind, state::Column::SubjectKey])
            .update_columns([state::Column::LastNotifiedAt, state::Column::State])
            .action_and_where(
                Expr::col((state::Entity, state::Column::LastNotifiedAt))
                    .lt(now - chrono::Duration::hours(within_hours)),
            )
            .to_owned(),
    );
    claim_won(db, insert).await
}

/// Claim by advancing a watermark: win iff the stored timestamp still equals `expected` (the value
/// just read), or the row is absent. A replica that already advanced it wins the compare-and-swap and
/// the loser skips, so a digest is sent once.
pub async fn claim_cas(
    db: &DatabaseConnection,
    kind: &str,
    key: &str,
    expected: DateTime<Utc>,
) -> Result<bool, DbErr> {
    let insert = state::Entity::insert(firing(kind, key, Utc::now())).on_conflict(
        OnConflict::columns([state::Column::Kind, state::Column::SubjectKey])
            .update_columns([state::Column::LastNotifiedAt, state::Column::State])
            .action_and_where(
                Expr::col((state::Entity, state::Column::LastNotifiedAt)).eq(expected),
            )
            .to_owned(),
    );
    claim_won(db, insert).await
}

/// Readings each sync service reported bringing in since `since`, from the per-cycle count the
/// services already write. Services that synced nothing are absent rather than zero.
pub(super) async fn arrivals_by_source(
    db: &DatabaseConnection,
    since: DateTime<Utc>,
) -> Result<Vec<(String, i64)>, DbErr> {
    sync_events::Entity::find()
        .select_only()
        .column_as(sync_services::Column::ServiceType, "source_system")
        .column_as(
            Expr::from(Func::sum(Expr::col((
                sync_events::Entity,
                sync_events::Column::ReadingsSynced,
            ))))
            .cast_as(Alias::new("bigint")),
            "n",
        )
        .join(
            JoinType::InnerJoin,
            sync_events::Relation::SyncService.def(),
        )
        .filter(
            sync_events::Column::StartedAt.gt(sea_orm::prelude::DateTimeWithTimeZone::from(since)),
        )
        .filter(sync_events::Column::ReadingsSynced.gt(0))
        .group_by(sync_services::Column::ServiceType)
        .order_by_asc(sync_services::Column::ServiceType)
        .into_tuple::<(String, i64)>()
        .all(db)
        .await
}

/// What one job kind reports having done since `since`: the named `detail.counts` key summed over
/// its completed runs, or the headline number each run returns when no key is named.
pub(super) async fn job_total_since(
    db: &DatabaseConnection,
    trigger_type: &str,
    key: Option<&str>,
    since: DateTime<Utc>,
) -> Result<i64, DbErr> {
    use sea_orm::sea_query::extension::postgres::PgExpr as _;
    let per_run = match key {
        Some(k) => Expr::col((job::Entity, job::Column::Detail))
            .get_json_field("counts")
            .cast_json_field(k)
            .cast_as(Alias::new("bigint")),
        None => Expr::col((job::Entity, job::Column::ReadingsUpdated)),
    };
    let total = Expr::from(Func::coalesce([
        Expr::from(Func::sum(Func::coalesce([per_run, Expr::val(0)]))),
        Expr::val(0),
    ]))
    .cast_as(Alias::new("bigint"));
    let n = job::Entity::find()
        .select_only()
        .column_as(total, "n")
        .filter(job::Column::TriggerType.eq(trigger_type))
        .filter(job::Column::Status.eq("completed"))
        .filter(job::Column::CompletedAt.gt(sea_orm::prelude::DateTimeWithTimeZone::from(since)))
        .into_tuple::<i64>()
        .one(db)
        .await?;
    Ok(n.unwrap_or(0))
}

/// Readings a system-made change of one kind touched since `since`, counted from the ledger rows
/// that change writes.
pub(super) async fn decisions_since(
    db: &DatabaseConnection,
    kind: &str,
    since: DateTime<Utc>,
) -> Result<i64, DbErr> {
    let n = decision_model::Entity::find()
        .filter(decision_model::Column::Kind.eq(kind))
        .filter(decision_model::Column::At.gt(sea_orm::prelude::DateTimeWithTimeZone::from(since)))
        .count(db)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

/// The hold kinds a sync import records as a tag on the data rather than as work for a person.
const IMPORT_TAG_KINDS: [HoldKind; 2] = [HoldKind::ReplicateStats, HoldKind::CurveClaimStripped];

/// Discrepancy tags recorded since `since`, counted by the source system of the stream that
/// raised them (`None` for a tag no stream produced) and by kind.
pub(super) async fn import_tags_since(
    db: &DatabaseConnection,
    since: DateTime<Utc>,
) -> Result<Vec<(Option<String>, String, i64)>, DbErr> {
    let source = Expr::col((data_streams::Entity, data_streams::Column::SourceSystem));
    let kind = Expr::col((hold_model::Entity, hold_model::Column::Kind));
    let query = Query::select()
        .expr_as(source.clone(), Alias::new("source_system"))
        .expr_as(kind.clone(), Alias::new("kind"))
        .expr_as(
            Expr::col((hold_model::Entity, hold_model::Column::Id)).count(),
            Alias::new("n"),
        )
        .from(hold_model::Entity)
        .left_join(
            data_streams::Entity,
            Expr::col((data_streams::Entity, data_streams::Column::Id))
                .equals((hold_model::Entity, hold_model::Column::StreamId)),
        )
        .and_where(kind.clone().is_in(IMPORT_TAG_KINDS.map(HoldKind::as_str)))
        .and_where(
            Expr::col((hold_model::Entity, hold_model::Column::CreatedAt))
                .gt(sea_orm::prelude::DateTimeWithTimeZone::from(since)),
        )
        .add_group_by([source, kind])
        .order_by(
            (data_streams::Entity, data_streams::Column::SourceSystem),
            Order::Asc,
        )
        .order_by((hold_model::Entity, hold_model::Column::Kind), Order::Asc)
        .to_owned();
    let rows = db.query_all_raw(PG.build(&query)).await?;
    rows.iter()
        .map(|r| {
            let TagCount {
                source_system,
                kind,
                n,
            } = TagCount::from_query_result(r, "")?;
            Ok((source_system, kind, n))
        })
        .collect()
}

#[derive(FromQueryResult)]
struct TagCount {
    source_system: Option<String>,
    kind: String,
    n: i64,
}

/// The digest of discrepancy tags a sync import recorded, or `None` when there were none. It
/// informs rather than asks: a tag is a mark on the data, not a decision owed.
#[must_use]
pub fn render_import_tags(counts: &[(Option<String>, String, i64)]) -> Option<OutgoingMessage> {
    let total: i64 = counts.iter().map(|(_, _, n)| n).sum();
    if total == 0 {
        return None;
    }
    let listed = counts
        .iter()
        .map(|(source, kind, n)| {
            format!(
                "{n} {kind} from {}",
                source.as_deref().unwrap_or("no stream")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(OutgoingMessage {
        kind: "import_tags",
        key: None,
        subject: format!("RIVER Data: {total} discrepancy tag(s) recorded at import"),
        body: format!(
            "Synced data did not match itself where it was imported: {listed}. The values are \
             stored as the source sent them, tagged for reading under Data Streams, Audits."
        ),
        // Tags span every stream a sync touched, so the digest carries no single scope.
        slot: None,
    })
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;

/// Whether the `(site, parameter)` slot is muted right now.
///
/// Every notification carrying a slot passes through here before delivery, so muting one slot
/// suppresses every slot-keyed alert for it, not just the threshold alarms.
pub async fn is_muted(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<bool, DbErr> {
    let found = mutes::Entity::find()
        .filter(mutes::Column::SiteId.eq(site_id))
        .filter(mutes::Column::ParameterId.eq(parameter_id))
        .filter(mutes::in_force())
        .one(db)
        .await?;
    Ok(found.is_some())
}
