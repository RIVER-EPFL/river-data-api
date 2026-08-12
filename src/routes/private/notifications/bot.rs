//! Telegram bot poller: long-polls getUpdates, gates each command on the linked user's live
//! Keycloak role (anti-backdoor), and routes to the command handlers.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::{AccessScope, Role};

use super::access::accessible_project_ids;
use super::authz::RoleResolution;
use super::commands;
use super::telegram::{TelegramClient, Update};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const POLL_TIMEOUT_SECS: u32 = 25;

struct Identity {
    id: Uuid,
    sub: String,
    is_active: bool,
}

/// Long-poll loop. No-op (returns) when no bot token is configured.
pub async fn run(state: AppState) {
    let Some(token) = state.config.telegram_bot_token.clone() else {
        return;
    };
    let client = TelegramClient::new(token);
    tracing::info!("Telegram bot poller: starting");
    let mut offset = 0i64;
    loop {
        match client.get_updates(offset, POLL_TIMEOUT_SECS).await {
            Ok(updates) => {
                for u in updates {
                    offset = u.update_id + 1;
                    handle_update(&state, &client, u).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram getUpdates failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn handle_update(state: &AppState, client: &TelegramClient, u: Update) {
    let (Some(chat_id), Some(text)) = (u.chat_id, u.text) else {
        return;
    };
    let text = text.trim();
    if !text.starts_with('/') {
        return;
    }
    let is_private = u.chat_type.as_deref() == Some("private");
    let (cmd, args) = parse_command(text);
    if let Some(reply) = route(state, chat_id, is_private, u.username.as_deref(), &cmd, args).await {
        if let Err(e) = client.send_message(chat_id, &reply).await {
            tracing::warn!(error = %e, "telegram send_message failed");
        }
    }
}

/// Commands that write data or change state. They are refused outside a 1:1 `private` chat because in
/// a group every member shares the linked chat id, so the acting individual can't be identified.
fn is_write_command(cmd: &str) -> bool {
    matches!(cmd, "grab" | "mute" | "unmute")
}

fn parse_command(text: &str) -> (String, &str) {
    let body = text.trim_start_matches('/');
    let (head, rest) = match body.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (body, ""),
    };
    // Strip a "@botname" suffix (present when commands are sent in groups).
    let cmd = head.split('@').next().unwrap_or(head).to_lowercase();
    (cmd, rest)
}

async fn route(
    state: &AppState,
    chat_id: i64,
    is_private: bool,
    username: Option<&str>,
    cmd: &str,
    args: &str,
) -> Option<String> {
    // /start is the only command reachable before a chat is linked.
    if cmd == "start" {
        return Some(commands::start(&state.db, chat_id, username, args).await);
    }

    // A write/state-changing command in a group chat can't be attributed to an individual, refuse
    // before touching identity or the DB.
    if !is_private && is_write_command(cmd) {
        return Some(
            "This command changes data and only works in a direct (1:1) chat with the bot, \
             not in a group."
                .to_string(),
        );
    }

    let Some(identity) = lookup_identity(&state.db, chat_id).await else {
        return Some(
            "This chat isn't linked. Ask an administrator for a code, then send /start <code>."
                .to_string(),
        );
    };
    if !identity.is_active {
        return Some(
            "This chat's access has been revoked. Ask an administrator to re-link.".to_string(),
        );
    }

    // Anti-backdoor: resolve the linked user's CURRENT role on every command.
    let role = match state.authorizer.resolve(state, &identity.sub).await {
        Some(RoleResolution::Revoked) => {
            deactivate(&state.db, identity.id).await;
            return Some("Your access has been revoked.".to_string());
        }
        None => {
            return Some("Authorization service is unavailable. Try again shortly.".to_string());
        }
        Some(role) => role,
    };
    stamp_verified(&state.db, identity.id).await;

    // The linked user's project confinement, the same set that gates HTTP reads and alert delivery.
    // `None` (administrator) means unrestricted; a member is confined to their granted projects so a
    // bot read can't surface data they can't see in the portal.
    let scope = match accessible_project_ids(state, &identity.sub).await {
        None => AccessScope::Unrestricted,
        Some(projects) => AccessScope::Projects(std::sync::Arc::new(projects)),
    };

    let cutoff = state.config.battery_cutoff_volts;
    let reply = match cmd {
        "help" => commands::help(),
        "ping" => commands::ping(),
        "status" => commands::status(&state.db, &scope).await,
        "alarms" => commands::alarms(&state.db, &scope).await,
        "stations" => commands::stations(&state.db, &scope).await,
        "latest" => commands::latest(&state.db, &scope, args).await,
        "thresholds" => commands::thresholds(&state.db, &scope, args).await,
        // Sync-service internals (endpoints, last_error) are operational data, Administrators only.
        "server" => {
            if role.allows_admin() {
                commands::server(&state.db).await
            } else {
                "This command requires an administrator role.".to_string()
            }
        }
        "battery" => commands::battery(&state.db, &scope, args, cutoff).await,
        // Grab-sample submission is a data write: require the same River level as HTTP `/grab_samples`
        // (interns are read-only) and confine the site to the caller's scope.
        "grab" => {
            if role.allows_level(&Role::River) {
                commands::grab(state, &scope, args, username, chat_id).await
            } else {
                "Submitting grab samples requires at least the River role.".to_string()
            }
        }
        "mute" | "unmute" | "muted" => {
            if role.allows_admin() {
                let by = username
                    .map_or_else(|| format!("telegram:{chat_id}"), |u| format!("telegram:{u}"));
                match cmd {
                    "mute" => commands::mute(&state.db, args, &by).await,
                    "unmute" => commands::unmute(&state.db, args).await,
                    _ => commands::muted(&state.db).await,
                }
            } else {
                "This command requires an administrator role.".to_string()
            }
        }
        other => format!("Unknown command /{other}. Try /help."),
    };
    Some(reply)
}

async fn lookup_identity(db: &DatabaseConnection, chat_id: i64) -> Option<Identity> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT id, linked_keycloak_sub, is_active FROM telegram_identities \
             WHERE telegram_chat_id = $1",
            [chat_id.into()],
        ))
        .await
        .ok()??;
    Some(Identity {
        id: row.try_get("", "id").ok()?,
        sub: row.try_get("", "linked_keycloak_sub").ok()?,
        is_active: row.try_get("", "is_active").ok()?,
    })
}

async fn deactivate(db: &DatabaseConnection, id: Uuid) {
    let _ = db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE telegram_identities SET is_active = FALSE, updated_at = NOW() WHERE id = $1",
            [id.into()],
        ))
        .await;
}

async fn stamp_verified(db: &DatabaseConnection, id: Uuid) {
    let _ = db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE telegram_identities SET last_verified_at = NOW(), updated_at = NOW() \
             WHERE id = $1",
            [id.into()],
        ))
        .await;
}
