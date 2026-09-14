use std::collections::HashMap;

use sea_orm::DatabaseConnection;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::common::AppEvent;
use crate::common::authz::AccessScope;
use crate::common::scope::{Unowned, project_of_job, require_row_in_scope};

use super::models::{JOB_MEMO_LIMIT, Lens};

impl Lens {
    pub(super) async fn admits(&self, db: &DatabaseConnection, event: &AppEvent) -> bool {
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

pub(super) fn event_type(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::JobCreated { .. } => "job_created",
        AppEvent::JobProgress { .. } => "job_progress",
        AppEvent::JobCompleted { .. } => "job_completed",
        AppEvent::JobLog { .. } => "job_log",
        AppEvent::DataIngested { .. } => "data_ingested",
        AppEvent::AlarmStateChanged { .. } => "alarm_state_changed",
    }
}
