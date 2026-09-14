use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::common::authz::AccessScope;
use crate::common::scope::Unowned;

pub use crate::common::AppEvent;

/// Per-connection view of the event bus.
///
/// One rule decides every frame: an event that carries a project is forwarded only when that
/// project is in the caller's scope; an event that carries none is operational telemetry, forwarded
/// to a member and withheld from a project-scoped API token, whose whole purpose is one project's
/// data. `JobLog` is the exception in both directions: it is the only variant carrying free text,
/// which can name another project's stream, so a restricted principal never receives it.
pub(super) enum Lens {
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
pub(super) const JOB_MEMO_LIMIT: usize = 1024;
