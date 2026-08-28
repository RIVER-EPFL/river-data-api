use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::access::{accessible_project_ids, project_allowed};
use super::{DeliveryResult, NotificationChannel, OutgoingMessage, Slot};
use crate::common::AppState;
use crate::config::Config;

pub struct WebPushChannel {
    client: reqwest::Client,
    vapid_pem: Vec<u8>,
    vapid_subject: String,
}

impl WebPushChannel {
    pub fn new(config: &Config) -> Option<Self> {
        let pem = config.vapid_private_key_pem.as_ref()?;
        let subject = config.vapid_subject.as_ref()?;
        Some(Self {
            client: reqwest::Client::new(),
            vapid_pem: pem.as_bytes().to_vec(),
            vapid_subject: subject.clone(),
        })
    }
}

pub struct Subscription {
    pub id: Uuid,
    pub keycloak_sub: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

fn deep_link_url(base: Option<&str>, slot: &Option<Slot>) -> Option<String> {
    let base = base?.trim_end_matches('/');
    match slot {
        Some(s) => Some(format!("{}/sites/{}?focus={}", base, s.site_id, s.parameter_id)),
        None => Some(format!("{}/alarms", base)),
    }
}

pub async fn slot_subscriptions(
    db: &DatabaseConnection,
    slot: &Option<Slot>,
) -> Result<Vec<Subscription>, String> {
    let rows = match slot {
        Some(s) => {
            db.query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT wps.id, wps.keycloak_sub AS sub, wps.endpoint, wps.p256dh, wps.auth \
                 FROM web_push_subscriptions wps \
                 LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = wps.keycloak_sub \
                 WHERE COALESCE(ns.is_active, true) AND COALESCE(ns.web_push_enabled, true) \
                   AND COALESCE(( \
                     SELECT subq.enabled FROM notification_subscriptions subq \
                     WHERE subq.keycloak_sub = wps.keycloak_sub \
                       AND ( (subq.site_id = $1 AND subq.parameter_id = $2) \
                          OR (subq.site_id = $1 AND subq.parameter_id IS NULL) \
                          OR ($3::uuid IS NOT NULL AND subq.project_id = $3 \
                              AND subq.site_id IS NULL AND subq.parameter_id IS NULL) ) \
                     ORDER BY (subq.parameter_id IS NOT NULL) DESC, (subq.site_id IS NOT NULL) DESC \
                     LIMIT 1 \
                   ), true)",
                [s.site_id.into(), s.parameter_id.into(), s.project_id.into()],
            ))
            .await
        }
        None => {
            db.query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT wps.id, wps.keycloak_sub AS sub, wps.endpoint, wps.p256dh, wps.auth \
                 FROM web_push_subscriptions wps \
                 LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = wps.keycloak_sub \
                 WHERE COALESCE(ns.is_active, true) AND COALESCE(ns.web_push_enabled, true)"
                    .to_string(),
            ))
            .await
        }
    }
    .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(Subscription {
            id: row.try_get("", "id").map_err(|e| e.to_string())?,
            keycloak_sub: row.try_get("", "sub").map_err(|e| e.to_string())?,
            endpoint: row.try_get("", "endpoint").map_err(|e| e.to_string())?,
            p256dh: row.try_get("", "p256dh").map_err(|e| e.to_string())?,
            auth: row.try_get("", "auth").map_err(|e| e.to_string())?,
        });
    }
    Ok(out)
}

async fn prune_subscription(db: &DatabaseConnection, id: Uuid) {
    if let Err(e) = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM web_push_subscriptions WHERE id = $1",
            [id.into()],
        ))
        .await
    {
        tracing::warn!(error = %e, "web_push: failed to prune expired subscription");
    }
}

async fn stamp_success(db: &DatabaseConnection, id: Uuid) {
    let _ = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE web_push_subscriptions SET last_success_at = NOW() WHERE id = $1",
            [id.into()],
        ))
        .await;
}

pub async fn send_push(
    client: &reqwest::Client,
    vapid_pem: &[u8],
    vapid_subject: &str,
    sub: &Subscription,
    payload: &[u8],
) -> Result<(), String> {
    let info = ::web_push::SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);

    let mut sig_builder = ::web_push::VapidSignatureBuilder::from_pem(
        std::io::Cursor::new(vapid_pem),
        &info,
    )
    .map_err(|e| format!("VAPID build: {e}"))?;
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
        Ok("VAPID key loaded".to_string())
    }

    async fn deliver(&self, state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        let db = &state.db;
        let subscriptions = match slot_subscriptions(db, &msg.slot).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "web_push: failed to load subscriptions");
                return Vec::new();
            }
        };

        let project = msg.slot.as_ref().and_then(|s| s.project_id);

        let url = deep_link_url(
            state.config.dashboard_base_url.as_deref(),
            &msg.slot,
        );

        let payload = serde_json::json!({
            "title": msg.subject,
            "body": msg.body,
            "url": url,
            "tag": msg.kind,
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
                &self.vapid_pem,
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
        results
    }
}
