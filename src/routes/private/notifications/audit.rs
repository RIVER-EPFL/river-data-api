//! The inbound record: who used the bot, when, for what, and whether they were allowed to.
//!
//! `notification_log` covers only what this system *sends*. Nothing recorded what it *receives*,
//! so a linked chat could read site data indefinitely leaving no trace but a `last_verified_at`
//! stamp that outbound delivery also moves.
//!
//! No message body is stored. `command` is mapped to a fixed vocabulary and anything unrecognised
//! becomes `unknown`, so user-authored text never reaches the table: the trail answers "who did
//! what" without accumulating what people typed.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// How an inbound message was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Authorized, and the command ran.
    Ok,
    /// Authorized, but the command needs a role this user does not hold.
    Denied,
    /// No identity for this chat.
    Unlinked,
    /// The link exists but has been deactivated.
    Inactive,
    /// Keycloak says the user is gone, disabled, or holds no riverdata role.
    Revoked,
    /// Keycloak could not be reached, so the command was refused without a verdict.
    Unavailable,
    /// The message came from a different Telegram account than the one that claimed the link.
    WrongAccount,
    /// Refused for sending too fast.
    RateLimited,
}

impl Outcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Denied => "denied",
            Outcome::Unlinked => "unlinked",
            Outcome::Inactive => "inactive",
            Outcome::Revoked => "revoked",
            Outcome::Unavailable => "unavailable",
            Outcome::WrongAccount => "wrong_account",
            Outcome::RateLimited => "rate_limited",
        }
    }
}

/// Every command the bot answers. An inbound token outside this list is recorded as `unknown`
/// rather than stored, which is what keeps user-authored text out of the table.
const KNOWN_COMMANDS: [&str; 17] = [
    "start",
    "help",
    "ping",
    "status",
    "alarms",
    "sites",
    // The name the site listing shipped under, kept so existing traffic and stored rows stay
    // recognised rather than collapsing to `unknown`.
    "stations",
    "latest",
    "thresholds",
    "server",
    "battery",
    "grab",
    "mute",
    "unmute",
    "muted",
    "plot",
    "callback",
];

/// Map an inbound command to its audit vocabulary.
///
/// The legacy window commands (`/7d`) all record as `plot`: they are the same request with a fixed
/// window, and the window is not what an audit trail is for.
#[must_use]
pub fn command_name(cmd: &str) -> &'static str {
    if let Some(known) = KNOWN_COMMANDS.iter().find(|k| **k == cmd) {
        return known;
    }
    if super::plot_args::is_plot_command(cmd) {
        return "plot";
    }
    "unknown"
}

/// Record one inbound message.
///
/// Best-effort by design: an audit write must never cost a user their reply, so a failure is logged
/// and swallowed. That trade-off is only acceptable because this trail is for review, not for
/// enforcement, and nothing here gates access.
pub async fn record(
    db: &DatabaseConnection,
    chat_id: i64,
    chat_type: Option<&str>,
    identity_id: Option<Uuid>,
    keycloak_sub: Option<&str>,
    cmd: &str,
    outcome: Outcome,
) {
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "INSERT INTO telegram_command_audit \
             (chat_id, chat_type, identity_id, keycloak_sub, command, outcome) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            [
                chat_id.into(),
                chat_type.into(),
                identity_id.into(),
                keycloak_sub.into(),
                command_name(cmd).into(),
                outcome.as_str().into(),
            ],
        ))
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "telegram: failed to record command audit");
    }
}

/// Delete audit rows older than `retention_days`. `0` keeps them forever.
pub async fn prune(db: &DatabaseConnection, retention_days: i64) -> Result<u64, sea_orm::DbErr> {
    if retention_days <= 0 {
        return Ok(0);
    }
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM telegram_command_audit \
             WHERE created_at < NOW() - ($1 || ' days')::interval",
            [retention_days.to_string().into()],
        ))
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_commands_keep_their_name() {
        assert_eq!(command_name("plot"), "plot");
        assert_eq!(command_name("grab"), "grab");
    }

    #[test]
    fn test_legacy_window_commands_record_as_plot() {
        for cmd in ["1d", "7d", "30d", "6h"] {
            assert_eq!(command_name(cmd), "plot", "/{cmd} is a plot request");
        }
    }

    /// The data-minimisation property: whatever someone types, the stored value comes from our own
    /// vocabulary, never from their message.
    #[test]
    fn test_unrecognised_input_is_never_stored_verbatim() {
        for cmd in [
            "définitelynotacommand",
            "'; DROP TABLE readings; --",
            &"a".repeat(4000),
            "",
        ] {
            let stored = command_name(cmd);
            assert_eq!(stored, "unknown");
            assert!(KNOWN_COMMANDS.contains(&stored) || stored == "unknown");
        }
    }
}
