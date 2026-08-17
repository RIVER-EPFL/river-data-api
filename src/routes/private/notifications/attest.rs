//! The attestation clock: proof that a linked chat's owner still holds their Keycloak account.
//!
//! `last_verified_at` records Telegram activity, but an alert stream moves it without the human
//! doing anything, so a chat that receives a daily alarm renews itself forever. That makes it a
//! credential with no practical expiry, and a stolen phone keeps working indefinitely.
//!
//! The second clock is reset only by an authenticated portal request. That factor matters
//! precisely because it sits outside Telegram: any confirmation the user could complete inside a
//! chat is equally available to whoever holds the phone, and so proves nothing.
//!
//! Renewal is passive. The stamp happens on any Keycloak-authenticated request, so opening the
//! dashboard is enough, and a short-TTL cache keeps that to one write per user per hour.

use std::sync::OnceLock;
use std::time::Duration;

use moka::future::Cache;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// How long a stamp is suppressed after being written, so the auth path stays a cache lookup.
const STAMP_INTERVAL_SECS: u64 = 3600;

fn recently_stamped() -> &'static Cache<String, ()> {
    static CACHE: OnceLock<Cache<String, ()>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(STAMP_INTERVAL_SECS))
            .build()
    })
}

/// Record that `sub` proved control of their account.
///
/// Called from the authenticated request path, so it must stay cheap: the common case is a moka
/// hit and no database work at all. Only claimed, active links are touched, so a user with no
/// Telegram link costs one no-op statement an hour.
pub async fn stamp(db: &DatabaseConnection, sub: &str) {
    if recently_stamped().get(sub).await.is_some() {
        return;
    }
    recently_stamped().insert(sub.to_string(), ()).await;

    // Deliberately not reactivating: a lapsed link must be re-claimed deliberately. In the case
    // this clock exists for, the lapsed chat is the attacker's, and the owner logging in would
    // hand it straight back.
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE telegram_identities \
             SET last_attested_at = NOW(), updated_at = NOW() \
             WHERE linked_keycloak_sub = $1 AND telegram_chat_id IS NOT NULL AND is_active",
            [sub.into()],
        ))
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "telegram: failed to stamp attestation");
    }
}
