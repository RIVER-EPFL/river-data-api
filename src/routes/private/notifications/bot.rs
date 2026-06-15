//! Telegram bot poller: long-polls getUpdates, gates each command on the linked user's live
//! Keycloak role (anti-backdoor), and routes to the command handlers.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;

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
    let (cmd, args) = parse_command(text);
    if let Some(reply) = route(state, chat_id, u.username.as_deref(), &cmd, args).await {
        if let Err(e) = client.send_message(chat_id, &reply).await {
            tracing::warn!(error = %e, "telegram send_message failed");
        }
    }
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
    username: Option<&str>,
    cmd: &str,
    args: &str,
) -> Option<String> {
    // /start is the only command reachable before a chat is linked.
    if cmd == "start" {
        return Some(commands::start(&state.db, chat_id, username, args).await);
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

    let cutoff = state.config.battery_cutoff_volts;
    let reply = match cmd {
        "help" => commands::help(),
        "ping" => commands::ping(),
        "status" => commands::status(&state.db).await,
        "alarms" => commands::alarms(&state.db).await,
        "stations" => commands::stations(&state.db).await,
        "latest" => commands::latest(&state.db, args).await,
        "thresholds" => commands::thresholds(&state.db, args).await,
        "server" => commands::server(&state.db).await,
        "battery" => commands::battery(&state.db, args, cutoff).await,
        "grab" => commands::grab(state, args, username, chat_id).await,
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
