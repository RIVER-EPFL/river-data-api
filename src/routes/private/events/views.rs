use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use tokio::sync::Mutex;
use tokio_stream::wrappers::BroadcastStream;

use crate::common::AppState;
use crate::common::middleware::{AuthContext, ProjectScope, scope_site_ids};
use crate::common::scope::Unowned;

use super::models::Lens;
use super::service::event_type;

/// `GET /api/events`, the live stream of job and alarm events as Server-Sent Events. The
/// connection stays open and each event carries the kind in its `event` field; a project-scoped
/// API token sees only its own project's events.
#[utoipa::path(
    get,
    path = "/api/events",
    responses((status = 200, description = "An open `text/event-stream` of job and alarm events", content_type = "text/event-stream")),
    tag = "events"
)]
pub async fn event_stream(
    State(state): State<AppState>,
    auth: Option<axum::Extension<AuthContext>>,
    ProjectScope(scope): ProjectScope,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let lens = if scope.is_restricted() {
        // A project-scoped API token is confined to one project's data by construction; a member is
        // a person operating the installation, so the jobs and alarm transitions that carry no
        // project are theirs to see.
        let scoped_token = matches!(
            auth.as_ref().map(|axum::Extension(ctx)| ctx),
            Some(AuthContext::ApiToken {
                project_scope: Some(_),
                ..
            })
        );
        let unowned = if scoped_token {
            Unowned::Deny
        } else {
            Unowned::Allow
        };
        Lens::Confined {
            sites: Arc::new(
                scope_site_ids(&state.db, &scope)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            ),
            scope,
            unowned,
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    } else {
        Lens::Everything
    };

    let lens = Arc::new(lens);
    // The stream ends when the process stops, else the graceful shutdown waits on it forever.
    let mut stopping = state.shutdown.subscribe();
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let lens = lens.clone();
        let db = state.db.clone();
        async move {
            let event = result.ok()?;
            if !lens.admits(&db, &event).await {
                return None;
            }
            let json = serde_json::to_string(&event).ok()?;
            Some(Ok(Event::default().event(event_type(&event)).data(json)))
        }
    });

    let stream = stream.take_until(async move {
        let _ = stopping.wait_for(|stopped| *stopped).await;
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
