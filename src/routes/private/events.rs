use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use sea_orm::DatabaseConnection;
use tokio::sync::Mutex;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::common::authz::AccessScope;
use crate::common::middleware::{AuthContext, ProjectScope, scope_site_ids};
use crate::common::scope::{Unowned, project_of_job, require_row_in_scope};
use crate::common::{AppEvent, AppState};

/// Per-connection view of the event bus.
///
/// One rule decides every frame: an event that carries a project is forwarded only when that
/// project is in the caller's scope; an event that carries none is operational telemetry, forwarded
/// to a member and withheld from a project-scoped API token, whose whole purpose is one project's
/// data. `JobLog` is the exception in both directions: it is the only variant carrying free text,
/// which can name another project's stream, so a restricted principal never receives it.
enum Lens {
    /// An administrator, an unscoped token or a sync token: every frame, and no database work.
    Everything,
    Confined {
        scope: AccessScope,
        /// The caller's sites, snapshotted at connect. A site added mid-connection appears on
        /// reconnect, which is acceptable for a live feed.
        sites: Arc<HashSet<Uuid>>,
        /// How a project-less event is treated.
        unowned: Unowned,
        /// Decisions already taken for a job id. A job's target is fixed once it has run, and this
        /// keeps a job's create/progress/log/complete burst to one resolution per connection.
        jobs: Arc<Mutex<HashMap<Uuid, bool>>>,
    },
}

/// Cap on the memoised job decisions of one connection, so a stream held open for days cannot grow
/// without bound.
const JOB_MEMO_LIMIT: usize = 1024;

impl Lens {
    async fn admits(&self, db: &DatabaseConnection, event: &AppEvent) -> bool {
        let Lens::Confined {
            scope,
            sites,
            unowned,
            jobs,
        } = self
        else {
            return true;
        };
        match event {
            AppEvent::DataIngested {
                site_id: Some(id), ..
            } => sites.contains(id),
            // An unpaired stream's readings belong to no site and so to no project.
            AppEvent::DataIngested { site_id: None, .. } | AppEvent::AlarmStateChanged { .. } => {
                *unowned == Unowned::Allow
            }
            AppEvent::JobLog { .. } => false,
            AppEvent::JobCreated { job_id }
            | AppEvent::JobProgress { job_id, .. }
            | AppEvent::JobCompleted { job_id, .. } => {
                admits_job(db, scope, *unowned, jobs, *job_id).await
            }
        }
    }
}

/// Resolve a job's project once per connection. A resolution error withholds the frame.
async fn admits_job(
    db: &DatabaseConnection,
    scope: &AccessScope,
    unowned: Unowned,
    memo: &Mutex<HashMap<Uuid, bool>>,
    job_id: Uuid,
) -> bool {
    if let Some(decided) = memo.lock().await.get(&job_id) {
        return *decided;
    }
    let Ok(row) = project_of_job(db, job_id).await else {
        return false;
    };
    let admitted = require_row_in_scope(scope, &row, unowned, "job").is_ok();
    let mut memo = memo.lock().await;
    if memo.len() >= JOB_MEMO_LIMIT {
        memo.clear();
    }
    memo.insert(job_id, admitted);
    admitted
}

fn event_type(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::JobCreated { .. } => "job_created",
        AppEvent::JobProgress { .. } => "job_progress",
        AppEvent::JobCompleted { .. } => "job_completed",
        AppEvent::JobLog { .. } => "job_log",
        AppEvent::DataIngested { .. } => "data_ingested",
        AppEvent::AlarmStateChanged { .. } => "alarm_state_changed",
    }
}

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

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
