//! Telegram bot poller: long-polls getUpdates, gates each command on the linked user's live
//! Keycloak role (anti-backdoor), and routes to the command handlers.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::{AccessScope, Role};

use super::access::accessible_project_ids;
use super::audit;
use super::authz::RoleResolution;
use super::commands;
use super::keyboard;
use super::telegram::{TelegramClient, Update};
use super::{Reply, plot_args};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const POLL_TIMEOUT_SECS: u32 = 25;

/// Published to clients as the autocomplete list behind `/`.
///
/// Read-only commands only: the privileged ones still work when typed and are listed by `/help`,
/// but offering everyone a menu entry they will be refused is noise.
const MENU_COMMANDS: [(&str, &str); 11] = [
    ("plot", "Chart a site or one parameter"),
    ("latest", "Latest reading per parameter at a site"),
    ("stations", "Sites you can see"),
    ("status", "Alarm summary"),
    ("alarms", "Open alarms"),
    ("thresholds", "Configured thresholds"),
    ("battery", "Voltage and depletion forecast"),
    ("muted", "Active alert mutes"),
    ("ping", "Liveness check"),
    ("help", "Everything I can do"),
    ("start", "Link this chat to your account"),
];

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
    let nudged = NudgeGate::new();
    tracing::info!("Telegram bot poller: starting");
    if let Err(e) = client.set_my_commands(&MENU_COMMANDS).await {
        tracing::warn!(error = %e, "telegram setMyCommands failed");
    }
    let mut offset = 0i64;
    loop {
        match client.get_updates(offset, POLL_TIMEOUT_SECS).await {
            Ok(updates) => {
                for u in updates {
                    offset = u.update_id + 1;
                    handle_update(&state, &client, &nudged, u).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram getUpdates failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// What a private chat gets when it sends something that isn't a command.
const NUDGE: &str = "I only respond to commands. Send /help to see what I can do.";

/// Remembers which chats were recently nudged, so a pasted multi-line message produces one reply
/// rather than one per line.
///
/// Poller-local rather than in `AppState`: the bot is a single task on a single replica by design
/// (`ENABLE_TELEGRAM_BOT`), so there is nothing to share it with.
struct NudgeGate {
    seen: moka::future::Cache<i64, ()>,
}

impl NudgeGate {
    fn new() -> Self {
        Self {
            seen: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(600))
                .build(),
        }
    }

    /// True at most once per chat per window.
    async fn allow(&self, chat_id: i64) -> bool {
        if self.seen.get(&chat_id).await.is_some() {
            return false;
        }
        self.seen.insert(chat_id, ()).await;
        true
    }
}

async fn handle_update(
    state: &AppState,
    client: &TelegramClient,
    nudged: &NudgeGate,
    u: Update,
) {
    let Some(chat_id) = u.chat_id else {
        return;
    };
    if let (Some(callback_id), Some(data)) = (u.callback_id.as_deref(), u.callback_data.as_deref()) {
        handle_callback(state, client, chat_id, &u, callback_id, data).await;
        return;
    }
    let Some(text) = u.text.as_deref() else {
        return;
    };
    let text = text.trim();
    let is_private = u.chat_type.as_deref() == Some("private");
    if !text.starts_with('/') {
        // A group must stay silent: a bot that answers every message is unusable in a shared chat,
        // and unrelated chatter should never reach these servers. In a 1:1 chat silence just reads
        // as a broken bot, so point at /help, at most once per chat per window.
        if is_private
            && nudged.allow(chat_id).await
            && let Err(e) = client.send_message(chat_id, NUDGE).await
        {
            tracing::warn!(error = %e, "telegram nudge failed");
        }
        return;
    }
    let (cmd, args) = parse_command(text);
    if let Some(reply) = route(
        state,
        chat_id,
        u.chat_type.as_deref(),
        is_private,
        u.username.as_deref(),
        &cmd,
        args,
    )
    .await
    {
        deliver(client, chat_id, reply, None).await;
    }
}

/// A tapped button.
///
/// Runs the same identity, role and scope resolution a typed command does: `callback_data` is
/// client-supplied and the button may have been sent before the tapper's access changed.
async fn handle_callback(
    state: &AppState,
    client: &TelegramClient,
    chat_id: i64,
    u: &Update,
    callback_id: &str,
    data: &str,
) {
    let Some(action) = keyboard::Action::parse(data) else {
        client
            .answer_callback(callback_id, Some("That button is out of date."))
            .await;
        return;
    };
    let ctx = match authorize(state, chat_id).await {
        Ok(ctx) => ctx,
        Err(refusal) => {
            audit::record(
                &state.db,
                chat_id,
                u.chat_type.as_deref(),
                None,
                None,
                "callback",
                refusal.outcome,
            )
            .await;
            client
                .answer_callback(callback_id, Some(&toast(&refusal.message)))
                .await;
            return;
        }
    };
    audit::record(
        &state.db,
        chat_id,
        u.chat_type.as_deref(),
        Some(ctx.identity_id),
        Some(&ctx.sub),
        "callback",
        audit::Outcome::Ok,
    )
    .await;
    client.answer_callback(callback_id, None).await;
    let reply = commands::callback(state, &ctx.scope, action).await;
    // A chart is replaced in place, so changing window doesn't stack images down the chat. Anything
    // else (a picker, an error) is a new message: a photo can't be edited into text.
    let edit = if u.has_photo && matches!(reply, Reply::Photo { .. }) {
        u.message_id
    } else {
        None
    };
    deliver(client, chat_id, reply, edit).await;
}

/// Telegram truncates a callback toast at 200 characters.
fn toast(text: &str) -> String {
    text.chars().take(190).collect()
}

/// Send a reply, editing `edit` in place when the reply is a chart replacing one.
async fn deliver(client: &TelegramClient, chat_id: i64, reply: Reply, edit: Option<i64>) {
    let sent = match reply {
        Reply::Text(t) => client.send_message(chat_id, &t).await,
        Reply::Menu { text, keyboard } => client.send_text(chat_id, &text, Some(&keyboard)).await,
        Reply::Photo {
            png,
            caption,
            keyboard,
        } => {
            let kb = keyboard.as_ref();
            let attempt = match edit {
                Some(message_id) => {
                    match client
                        .edit_photo(chat_id, message_id, png.clone(), &caption, kb)
                        .await
                    {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            tracing::debug!(error = %e, "telegram editMessageMedia failed, sending a new photo");
                            client.send_photo(chat_id, png, &caption, kb).await
                        }
                    }
                }
                None => client.send_photo(chat_id, png, &caption, kb).await,
            };
            match attempt {
                Ok(()) => Ok(()),
                Err(e) => {
                    // A Telegram-side image failure should still deliver the answer.
                    tracing::warn!(error = %e, "telegram sendPhoto failed, falling back to text");
                    client.send_text(chat_id, &caption, kb).await
                }
            }
        }
    };
    if let Err(e) = sent {
        tracing::warn!(error = %e, "telegram send failed");
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

/// Route one inbound command. Public for the integration tests, which drive it directly to assert
/// the authorization and audit behaviour a live chat would get.
pub async fn route(
    state: &AppState,
    chat_id: i64,
    chat_type: Option<&str>,
    is_private: bool,
    username: Option<&str>,
    cmd: &str,
    args: &str,
) -> Option<Reply> {
    // /start is the only command reachable before a chat is linked.
    if cmd == "start" {
        // Claiming a code binds THIS chat to the user's authority, so a group chat would hand every
        // member that user's project access, including people with no account at all.
        if !is_private {
            audit::record(
                &state.db,
                chat_id,
                chat_type,
                None,
                None,
                cmd,
                audit::Outcome::Denied,
            )
            .await;
            return Some(
                "Linking only works in a direct (1:1) chat with the bot. Open a chat with me and \
                 send /start <code> there."
                    .into(),
            );
        }
        let (claimed, reply) = commands::start(&state.db, chat_id, username, args).await;
        // A claim is the moment a chat gains an identity, so the row names who it became.
        let identity = if claimed {
            lookup_identity(&state.db, chat_id).await
        } else {
            None
        };
        audit::record(
            &state.db,
            chat_id,
            chat_type,
            identity.as_ref().map(|i| i.id),
            identity.as_ref().map(|i| i.sub.as_str()),
            cmd,
            if claimed {
                audit::Outcome::Ok
            } else {
                audit::Outcome::Denied
            },
        )
        .await;
        return Some(reply.into());
    }

    // A write/state-changing command in a group chat can't be attributed to an individual, refuse
    // before touching identity or the DB.
    if !is_private && is_write_command(cmd) {
        return Some(
            "This command changes data and only works in a direct (1:1) chat with the bot, \
             not in a group."
                .into(),
        );
    }

    let ctx = match authorize(state, chat_id).await {
        Ok(ctx) => ctx,
        Err(refusal) => {
            audit::record(
                &state.db,
                chat_id,
                chat_type,
                None,
                None,
                cmd,
                refusal.outcome,
            )
            .await;
            return Some(Reply::Text(refusal.message));
        }
    };
    let (role, scope) = (ctx.role, ctx.scope);

    // Commands that answer with an image or a keyboard sit outside the text-only match below.
    // Placed after identity, role and scope resolution so they inherit every gate rather than
    // re-implementing one, and `scope` confines site resolution exactly as it does for /latest.
    // A refusal below sets `outcome`, so one message produces exactly one audit row.
    let mut outcome = audit::Outcome::Ok;
    let reply = if plot_args::is_plot_command(cmd) {
        commands::plot(state, &scope, cmd, args).await
    } else if cmd == "stations" {
        commands::stations(&state.db, &scope).await
    } else {
        Reply::Text(text_command(state, &scope, &role, username, chat_id, cmd, args, &mut outcome).await)
    };
    audit::record(
        &state.db,
        chat_id,
        chat_type,
        Some(ctx.identity_id),
        Some(&ctx.sub),
        cmd,
        outcome,
    )
    .await;
    Some(reply)
}

/// The commands that answer with text. `outcome` is set to `Denied` where a role gate refuses, so
/// the audit trail distinguishes "ran it" from "tried it".
#[allow(clippy::too_many_arguments)]
async fn text_command(
    state: &AppState,
    scope: &AccessScope,
    role: &RoleResolution,
    username: Option<&str>,
    chat_id: i64,
    cmd: &str,
    args: &str,
    outcome: &mut audit::Outcome,
) -> String {
    let cutoff = state.config.battery_cutoff_volts;
    match cmd {
        "help" => commands::help(),
        "ping" => commands::ping(),
        "status" => commands::status(&state.db, scope).await,
        "alarms" => commands::alarms(&state.db, scope).await,
        "latest" => commands::latest(&state.db, scope, args).await,
        "thresholds" => commands::thresholds(&state.db, scope, args).await,
        // Sync-service internals (endpoints, last_error) are operational data, Administrators only.
        "server" => {
            if role.allows_admin() {
                commands::server(&state.db).await
            } else {
                *outcome = audit::Outcome::Denied;
                "This command requires an administrator role.".to_string()
            }
        }
        "battery" => commands::battery(&state.db, scope, args, cutoff).await,
        // Grab-sample submission is a data write: require the same River level as HTTP `/grab_samples`
        // (interns are read-only) and confine the site to the caller's scope.
        "grab" => {
            if role.allows_level(&Role::River) {
                commands::grab(state, scope, args, username, chat_id).await
            } else {
                *outcome = audit::Outcome::Denied;
                "Submitting grab samples requires at least the River role.".to_string()
            }
        }
        "mute" | "unmute" | "muted" => {
            if role.allows_admin() {
                let by = username.map_or_else(
                    || format!("telegram:{chat_id}"),
                    |u| format!("telegram:{u}"),
                );
                match cmd {
                    "mute" => commands::mute(&state.db, args, &by).await,
                    "unmute" => commands::unmute(&state.db, args).await,
                    _ => commands::muted(&state.db).await,
                }
            } else {
                *outcome = audit::Outcome::Denied;
                "This command requires an administrator role.".to_string()
            }
        }
        other => format!("Unknown command /{other}. Try /help."),
    }
}

/// What a chat is allowed to do, resolved fresh.
struct Context {
    identity_id: Uuid,
    sub: String,
    role: RoleResolution,
    scope: AccessScope,
}

/// A refused message: what to tell the user, and what to record.
struct Refusal {
    message: String,
    outcome: audit::Outcome,
}

impl Refusal {
    fn new(outcome: audit::Outcome, message: &str) -> Self {
        Self {
            message: message.to_string(),
            outcome,
        }
    }
}

/// Resolve the chat's linked user, their current role and their project confinement.
///
/// Anti-backdoor: this runs on every inbound message and every button tap. A role revoked in
/// Keycloak deactivates the identity here; an unreachable authorization service refuses rather than
/// falling back to a cached answer. `Err` carries the refusal to show the user.
async fn authorize(state: &AppState, chat_id: i64) -> Result<Context, Refusal> {
    let Some(identity) = lookup_identity(&state.db, chat_id).await else {
        return Err(Refusal::new(
            audit::Outcome::Unlinked,
            "This chat isn't linked. Ask an administrator for a code, then send /start <code>.",
        ));
    };
    if !identity.is_active {
        return Err(Refusal::new(
            audit::Outcome::Inactive,
            "This chat's access has been revoked. Ask an administrator to re-link.",
        ));
    }
    let role = match state.authorizer.resolve(state, &identity.sub).await {
        Some(RoleResolution::Revoked) => {
            deactivate(&state.db, identity.id).await;
            return Err(Refusal::new(
                audit::Outcome::Revoked,
                "Your access has been revoked.",
            ));
        }
        None => {
            return Err(Refusal::new(
                audit::Outcome::Unavailable,
                "Authorization service is unavailable. Try again shortly.",
            ));
        }
        Some(role) => role,
    };
    stamp_verified(&state.db, identity.id).await;

    // The same project set that gates HTTP reads and alert delivery. `None` (administrator) means
    // unrestricted; a member is confined to their granted projects so a bot read can't surface data
    // they can't see in the portal.
    let scope = match accessible_project_ids(state, &identity.sub).await {
        None => AccessScope::Unrestricted,
        Some(projects) => AccessScope::Projects(std::sync::Arc::new(projects)),
    };
    Ok(Context {
        identity_id: identity.id,
        sub: identity.sub,
        role,
        scope,
    })
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
