//! Periodic identity reconciliation, the anti-backdoor backstop, plus idle-link expiry.
//!
//! Re-resolves every active linked identity against Keycloak and deactivates any whose user is gone,
//! disabled, or no longer holds a riverdata role. This bounds the revocation window even for users
//! who never issue another command (a command-active user is already caught within the authz cache
//! TTL; an alert-only user would otherwise keep receiving alerts indefinitely). Keycloak-unavailable
//! resolutions are skipped, never deactivated, so an outage can't mass-unlink.
//!
//! # Idle expiry
//!
//! A link nobody has used should lapse rather than sit forever receiving site data. "Used" means
//! any activity, sent *or* received: `last_verified_at` is stamped both on an inbound command
//! (`bot::stamp_verified`) and on a successful outbound delivery (`telegram::stamp_delivered`).
//! Counting only inbound commands would cut off exactly the people this exists for, the ones who
//! link once and only ever receive alarms.
//!
//! The revocation check above runs first and unconditionally. `expiry_exempt` holds a link open
//! against *inactivity only*; it never shields a revoked user.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;

use super::authz::RoleResolution;
use super::telegram::TelegramClient;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// `notification_state.kind` for the "your link is about to expire" claim, so the warning is sent
/// once rather than on every five-minute tick for a week.
const WARN_KIND: &str = "telegram_link_idle";

/// What a sweep did, for the job's `detail.counts`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SweepOutcome {
    pub revoked: usize,
    pub warned: usize,
    pub expired: usize,
    pub purged: usize,
    /// Audit rows dropped past their retention window.
    pub audit_pruned: usize,
}

impl SweepOutcome {
    /// The headline number the job worker persists as `readings_updated`.
    #[must_use]
    pub fn total(self) -> usize {
        self.revoked + self.warned + self.expired + self.purged + self.audit_pruned
    }
}

/// One reconciliation pass, driven by the scheduled `identity_reconcile` job.
pub async fn sweep(state: &AppState) -> Result<SweepOutcome, sea_orm::DbErr> {
    let mut outcome = SweepOutcome::default();

    // 1. Revocation, first and unconditional: an exempt link is still cut off when its user goes.
    let rows = state
        .db
        .query_all(Statement::from_string(
            PG,
            "SELECT id, linked_keycloak_sub FROM telegram_identities WHERE is_active".to_string(),
        ))
        .await?;

    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        let sub: String = row.try_get("", "linked_keycloak_sub")?;
        if let Some(RoleResolution::Revoked) = state.authorizer.resolve(state, &sub).await {
            deactivate(&state.db, id).await?;
            state.authorizer.invalidate(&sub).await;
            outcome.revoked += 1;
        }
    }

    // 2. Idle expiry, only for links the revocation pass left alone.
    let idle_days = state.config.telegram_link_idle_days;
    if idle_days > 0 {
        outcome.warned = warn_expiring(state, idle_days).await?;
        outcome.expired = expire_idle(state, idle_days).await?;
    }
    let purge_days = state.config.telegram_link_purge_days;
    if purge_days > 0 {
        outcome.purged = purge(&state.db, purge_days).await?;
    }

    // 3. Audit retention. Same cadence, same table family, no new schedule.
    outcome.audit_pruned = usize::try_from(
        super::audit::prune(&state.db, state.config.telegram_audit_retention_days).await?,
    )
    .unwrap_or(0);

    Ok(outcome)
}

/// Warn once, `warn_days` before a link would lapse.
///
/// This is the safety valve for the case where no alarm fires anywhere for a whole idle window:
/// the warning is itself a real message, and a single `/ping` resets the clock.
async fn warn_expiring(state: &AppState, idle_days: i64) -> Result<usize, sea_orm::DbErr> {
    let warn_days = state.config.telegram_link_warn_days;
    if warn_days <= 0 || warn_days >= idle_days {
        return Ok(0);
    }
    let Some(client) = telegram_client(state) else {
        return Ok(0);
    };

    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, linked_keycloak_sub, telegram_chat_id, last_verified_at \
             FROM telegram_identities \
             WHERE is_active AND NOT expiry_exempt AND telegram_chat_id IS NOT NULL \
               AND last_verified_at IS NOT NULL \
               AND last_verified_at < NOW() - ($1 || ' days')::interval",
            [(idle_days - warn_days).into()],
        ))
        .await?;

    let mut warned = 0;
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        let sub: String = row.try_get("", "linked_keycloak_sub")?;
        let chat_id: i64 = row.try_get("", "telegram_chat_id")?;
        let last: Option<DateTime<Utc>> = row.try_get("", "last_verified_at").ok();

        // A departed collaborator's chat is still an outbound message: resolve authority first.
        if matches!(
            state.authorizer.resolve(state, &sub).await,
            Some(RoleResolution::Revoked) | None
        ) {
            continue;
        }

        if !claim_warning(&state.db, id).await? {
            continue;
        }

        let days_left = last.map_or(warn_days, |t| {
            (idle_days - (Utc::now() - t).num_days()).max(0)
        });
        let msg = format!(
            "Your Telegram link has been idle and expires in {days_left} day{}.\n\
             Send /ping to keep it active.",
            if days_left == 1 { "" } else { "s" }
        );
        // Sent directly, NOT through TelegramChannel::deliver: that stamps last_verified_at, which
        // would reset the very clock this message is warning about and the link would never lapse.
        if let Err(e) = client.send_message(chat_id, &msg).await {
            tracing::warn!(error = %e, "telegram: idle-link warning failed");
            release_warning(&state.db, id).await?;
            continue;
        }
        warned += 1;
    }
    Ok(warned)
}

/// Deactivate links idle past the threshold, telling the user why.
///
/// The existing revocation message only appears when the user next sends a command, which an
/// inactive user by definition will not. A proactive notice is the only way they learn, and
/// unlike a security revocation this is benign and should be specific and actionable.
async fn expire_idle(state: &AppState, idle_days: i64) -> Result<usize, sea_orm::DbErr> {
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, linked_keycloak_sub, telegram_chat_id FROM telegram_identities \
             WHERE is_active AND NOT expiry_exempt AND last_verified_at IS NOT NULL \
               AND last_verified_at < NOW() - ($1 || ' days')::interval",
            [idle_days.into()],
        ))
        .await?;

    let client = telegram_client(state);
    let mut expired = 0;
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        let sub: String = row.try_get("", "linked_keycloak_sub")?;
        let chat_id: Option<i64> = row.try_get("", "telegram_chat_id").ok();

        deactivate(&state.db, id).await?;
        clear_warning(&state.db, id).await?;
        expired += 1;

        let still_authorized = !matches!(
            state.authorizer.resolve(state, &sub).await,
            Some(RoleResolution::Revoked) | None
        );
        if let (Some(client), Some(chat_id), true) = (client.as_ref(), chat_id, still_authorized) {
            let base = state.config.dashboard_base_url.as_deref().unwrap_or("");
            let where_to = if base.is_empty() {
                "your dashboard settings".to_string()
            } else {
                format!("{base}/settings")
            };
            let msg = format!(
                "Your Telegram link has expired after {idle_days} days without use, so this chat \
                 will no longer receive River Data alerts.\n\
                 To reconnect, generate a new code at {where_to} and send /start <code>."
            );
            // Direct send again: going through the channel would stamp activity on a link we have
            // just expired.
            if let Err(e) = client.send_message(chat_id, &msg).await {
                tracing::warn!(error = %e, "telegram: idle-link expiry notice failed");
            }
        }
    }
    Ok(expired)
}

/// Delete rows long past expiry. No message: the user was told at the warning and at expiry.
async fn purge(db: &DatabaseConnection, purge_days: i64) -> Result<usize, sea_orm::DbErr> {
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM telegram_identities \
             WHERE NOT is_active AND NOT expiry_exempt AND last_verified_at IS NOT NULL \
               AND last_verified_at < NOW() - ($1 || ' days')::interval",
            [purge_days.into()],
        ))
        .await?;
    Ok(res.rows_affected() as usize)
}

fn telegram_client(state: &AppState) -> Option<TelegramClient> {
    state
        .config
        .telegram_bot_token
        .clone()
        .map(TelegramClient::new)
}

/// Claim the one-per-lapse warning. False when it was already sent.
async fn claim_warning(db: &DatabaseConnection, id: Uuid) -> Result<bool, sea_orm::DbErr> {
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "INSERT INTO notification_state (kind, subject_key, state, last_notified_at) \
             VALUES ($1, $2, 'warned', NOW()) ON CONFLICT (kind, subject_key) DO NOTHING",
            [WARN_KIND.into(), id.to_string().into()],
        ))
        .await?;
    Ok(res.rows_affected() > 0)
}

async fn release_warning(db: &DatabaseConnection, id: Uuid) -> Result<(), sea_orm::DbErr> {
    clear_warning(db, id).await
}

/// Drop the warning claim so a link that lapses again warns again.
pub async fn clear_warning(db: &DatabaseConnection, id: Uuid) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_sql_and_values(
        PG,
        "DELETE FROM notification_state WHERE kind = $1 AND subject_key = $2",
        [WARN_KIND.into(), id.to_string().into()],
    ))
    .await?;
    Ok(())
}

async fn deactivate(db: &DatabaseConnection, id: Uuid) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_sql_and_values(
        PG,
        "UPDATE telegram_identities SET is_active = FALSE, updated_at = NOW() WHERE id = $1",
        [id.into()],
    ))
    .await?;
    Ok(())
}
