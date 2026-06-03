use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use crate::common::AppState;

pub async fn event_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| async {
        match result {
            Ok(event) => {
                let json = serde_json::to_string(&event).ok()?;
                let event_type = match &event {
                    crate::common::AppEvent::JobCreated { .. } => "job_created",
                    crate::common::AppEvent::JobProgress { .. } => "job_progress",
                    crate::common::AppEvent::JobCompleted { .. } => "job_completed",
                    crate::common::AppEvent::DataIngested { .. } => "data_ingested",
                    crate::common::AppEvent::AlarmStateChanged { .. } => "alarm_state_changed",
                };
                Some(Ok(Event::default().event(event_type).data(json)))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
