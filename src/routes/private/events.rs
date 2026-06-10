use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, scope_site_ids};

pub async fn event_stream(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Snapshot the scoped principal's sites once at connect. A project-scoped key only receives
    // data events for its own project; project-less events (jobs, the alarm summary) are global and
    // suppressed for scoped keys. Unscoped principals (Keycloak users, unscoped tokens) get the full
    // operator firehose. The snapshot is connection-lifetime: a site added mid-connection appears on
    // reconnect, which is acceptable for a live event feed.
    let scope_sites: Option<Arc<HashSet<Uuid>>> = if scope.is_some() {
        Some(Arc::new(
            scope_site_ids(&state.db, scope)
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        ))
    } else {
        None
    };

    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let scope_sites = scope_sites.clone();
        async move {
            let event = result.ok()?;

            // For a scoped key, forward only in-project data events and drop everything else.
            if let Some(sites) = &scope_sites {
                let in_scope = matches!(
                    &event,
                    crate::common::AppEvent::DataIngested { site_id: Some(sid), .. } if sites.contains(sid)
                );
                if !in_scope {
                    return None;
                }
            }

            let json = serde_json::to_string(&event).ok()?;
            let event_type = match &event {
                crate::common::AppEvent::JobCreated { .. } => "job_created",
                crate::common::AppEvent::JobProgress { .. } => "job_progress",
                crate::common::AppEvent::JobCompleted { .. } => "job_completed",
                crate::common::AppEvent::JobLog { .. } => "job_log",
                crate::common::AppEvent::DataIngested { .. } => "data_ingested",
                crate::common::AppEvent::AlarmStateChanged { .. } => "alarm_state_changed",
            };
            Some(Ok(Event::default().event(event_type).data(json)))
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
