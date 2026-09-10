//! The post-write tail: what runs after a write to `readings` commits.
//!
//! Every write path finished with its own spelling of the same six steps, and each of B122, B123
//! and B124 was one path forgetting one of them. The steps are here once, in the order
//! `flags.rs` runs them, and the three axes the paths genuinely differ on are parameters:
//! how wide the cache invalidation goes, what window the rollups refresh over and whether a
//! failure there is fatal, and how alarm episodes are rebuilt.
//!
//! [`plan`] decides which steps run over which window; [`run`] performs them. The decision is
//! separated from the doing so it can be read back in a test without a database.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sea_orm::DatabaseConnection;

use crate::common::aggregates::{self, Window};
use crate::common::state::{AppState, EventSender, ResponseCache};
use crate::common::{AppEvent, cache};
use crate::error::AppResult;
use crate::routes::private::collection_events::flows::{self, TouchedEvent, Writer};

/// One slot a write landed in, and the stream it arrived through. An unpaired stream names
/// neither site nor parameter: its rows are stored and announced, and there is no slot to
/// invalidate, reconcile or roll up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot {
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub stream_id: Option<Uuid>,
}

impl Slot {
    #[must_use]
    pub fn paired(site_id: Uuid, parameter_id: Uuid) -> Self {
        Self {
            site_id: Some(site_id),
            parameter_id: Some(parameter_id),
            stream_id: None,
        }
    }

    #[must_use]
    pub fn through(mut self, stream_id: Uuid) -> Self {
        self.stream_id = Some(stream_id);
        self
    }

    fn slot(self) -> Option<(Uuid, Uuid)> {
        self.site_id.zip(self.parameter_id)
    }
}

/// What a write left behind. `rows` is the tail's own gate: a pass that moved nothing runs no
/// step, whatever it counted on the way.
#[derive(Debug, Clone, Default)]
pub struct Written {
    pub rows: u64,
    /// The count carried on `DataIngested`, which is the rows a consumer would fetch rather than
    /// everything the pass touched.
    pub announced: u64,
    pub span: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub slots: Vec<Slot>,
    pub touched_events: Vec<TouchedEvent>,
}

impl Written {
    #[must_use]
    pub fn new(rows: u64) -> Self {
        Self {
            rows,
            announced: rows,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn announced(mut self, announced: u64) -> Self {
        self.announced = announced;
        self
    }

    #[must_use]
    pub fn over(mut self, span: Option<(DateTime<Utc>, DateTime<Utc>)>) -> Self {
        self.span = span;
        self
    }

    #[must_use]
    pub fn at(mut self, slots: Vec<Slot>) -> Self {
        self.slots = slots;
        self
    }

    #[must_use]
    pub fn touching(mut self, touched_events: Vec<TouchedEvent>) -> Self {
        self.touched_events = touched_events;
        self
    }

    fn sites(&self) -> Vec<Uuid> {
        self.slots
            .iter()
            .filter_map(|s| s.site_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// How wide the cache invalidation goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    /// Every entry. For a write whose effect is not confined to the sites it names: sample
    /// formation and withdrawal rewrite served history for anyone reading it.
    All,
    /// The sites the write landed in.
    Sites,
}

/// The window the rollups are refreshed over, and whether a failure there is fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// No refresh: the write cannot have moved a rollup (spot rows are excluded from them).
    Skip,
    /// The span the write covers. A request path swallows the error, since the rows are committed
    /// and a 500 would replay a write that happened.
    Range { fatal: bool },
    /// From the earliest instant the write touched up to now, for a path that also moved rows it
    /// did not report.
    Since { fatal: bool },
}

/// How alarm episodes are rebuilt over what was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Episodes {
    /// Rebuilt inline, per slot. For paths that fire per sync cycle or per field entry, where one
    /// `reprocessing_jobs` row per write would be the noise.
    Inline,
    /// Enqueued as one `alarm_backfill` job over every slot.
    Job,
    None,
}

/// The three axes the write paths encode, stated once each.
#[derive(Debug, Clone, Copy)]
pub struct Axes {
    pub cache: Cache,
    pub refresh: Refresh,
    /// Whether each written slot is announced on the event bus, which is both the SSE feed and
    /// what the cache invalidator subscribes to.
    pub announce: bool,
    /// Whether the open-alarm state of the written slots is reconciled now rather than waiting for
    /// the periodic sweep.
    pub reconcile_alarms: bool,
    pub episodes: Episodes,
    /// Whether the derived values the written slots feed are recomputed over the same span. A
    /// derived value is computed from what its inputs served, so a decision that changes what an
    /// input serves leaves it stating a number nothing supports any more.
    pub recompute_derived: bool,
    /// Who wrote, which is what decides whether the visit's calculations run again.
    pub writer: Writer,
}

/// Which steps run, over which window. Everything here is decided from [`Written`] and [`Axes`]
/// alone.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub refresh: Option<Window>,
    pub refresh_fatal: bool,
    pub invalidate_all: bool,
    pub invalidate_sites: Vec<Uuid>,
    pub recompute_events: Vec<Uuid>,
    pub announce: Vec<Slot>,
    pub reconcile: Vec<(Uuid, Uuid)>,
    pub episodes: Episodes,
    pub episode_span: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The slots whose derived values are recomputed, over [`Plan::episode_span`]'s window.
    pub recompute_derived: Vec<(Uuid, Uuid)>,
}

impl Plan {
    /// The tail of a write that moved nothing.
    fn nothing() -> Self {
        Self {
            refresh: None,
            refresh_fatal: false,
            invalidate_all: false,
            invalidate_sites: Vec::new(),
            recompute_events: Vec::new(),
            announce: Vec::new(),
            reconcile: Vec::new(),
            episodes: Episodes::None,
            episode_span: None,
            recompute_derived: Vec::new(),
        }
    }
}

/// What the tail does for this write, before it does any of it.
#[must_use]
pub fn plan(written: &Written, axes: &Axes) -> Plan {
    if written.rows == 0 {
        return Plan::nothing();
    }
    let span = written.span;
    let refresh = match (axes.refresh, span) {
        (Refresh::Skip, _) | (_, None) => None,
        (Refresh::Range { .. }, Some((lo, hi))) => Some(Window::Range(lo, hi)),
        (Refresh::Since { .. }, Some((lo, _))) => Some(Window::Since(lo)),
    };
    let refresh_fatal = match axes.refresh {
        Refresh::Skip => false,
        Refresh::Range { fatal } | Refresh::Since { fatal } => fatal,
    };
    let recompute_events = if axes.writer == Writer::Chain {
        Vec::new()
    } else {
        written.touched_events.iter().map(|e| e.id).collect()
    };
    let episodes = if span.is_some() {
        axes.episodes
    } else {
        Episodes::None
    };
    Plan {
        refresh,
        refresh_fatal,
        invalidate_all: axes.cache == Cache::All,
        invalidate_sites: if axes.cache == Cache::All {
            Vec::new()
        } else {
            written.sites()
        },
        recompute_events,
        announce: if axes.announce {
            written.slots.clone()
        } else {
            Vec::new()
        },
        reconcile: if axes.reconcile_alarms {
            written
                .slots
                .iter()
                .filter_map(|s| s.slot())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        },
        episodes,
        episode_span: span,
        recompute_derived: if axes.recompute_derived && span.is_some() {
            written
                .slots
                .iter()
                .filter_map(|s| s.slot())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        },
    }
}

/// What the tail writes to. A request path holds the whole `AppState`; a tracked job holds its
/// `JobContext`'s connection and event sender, and reaches the response cache only if the process
/// built one.
pub struct Sink<'a> {
    pub db: &'a DatabaseConnection,
    pub events: &'a EventSender,
    pub cache: Option<&'a ResponseCache>,
}

impl<'a> From<&'a AppState> for Sink<'a> {
    fn from(state: &'a AppState) -> Self {
        Self {
            db: &state.db,
            events: &state.events,
            cache: Some(&state.response_cache),
        }
    }
}

/// Run the tail. Call it after the guarded write has committed: an aggregate refresh is a
/// procedure with its own transaction control, and so are the jobs the reactive hook enqueues.
pub async fn run<'a>(
    sink: impl Into<Sink<'a>>,
    written: &Written,
    axes: &Axes,
    actor: &str,
) -> AppResult<Plan> {
    let sink = sink.into();
    let plan = plan(written, axes);

    if let Some(window) = plan.refresh {
        match aggregates::refresh(sink.db, window).await {
            Ok(()) => {}
            Err(e) if plan.refresh_fatal => return Err(e),
            Err(e) => tracing::warn!(error = %e, "aggregate refresh after a write failed"),
        }
    }

    if let Some(response_cache) = sink.cache {
        if plan.invalidate_all {
            cache::invalidate_all(response_cache, "write rewrote served history");
        }
        for site_id in &plan.invalidate_sites {
            cache::invalidate_site(response_cache, *site_id);
        }
    }

    if !plan.recompute_events.is_empty() {
        flows::enqueue_for(sink.db, &written.touched_events, actor, axes.writer).await?;
    }

    for slot in &plan.announce {
        let _ = sink.events.send(AppEvent::DataIngested {
            site_id: slot.site_id,
            parameter_id: slot.parameter_id,
            stream_id: slot.stream_id,
            count: usize::try_from(written.announced).unwrap_or(usize::MAX),
        });
    }

    if !plan.reconcile.is_empty() {
        crate::routes::private::alarms::flows::reconcile_and_notify(
            sink.db,
            sink.events,
            &plan.reconcile,
        )
        .await;
    }

    if let Some((lo, hi)) = plan.episode_span {
        match plan.episodes {
            Episodes::Inline => {
                for (site_id, parameter_id) in written.slots.iter().filter_map(|s| s.slot()) {
                    if let Err(e) = crate::routes::private::alarms::flows::evaluate_alarm_episodes(
                        sink.db,
                        site_id,
                        parameter_id,
                        lo,
                        hi,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, %site_id, %parameter_id, "alarm episode reconstruction failed");
                    }
                }
            }
            Episodes::Job => {
                let slots: Vec<serde_json::Value> = written
                    .slots
                    .iter()
                    .filter_map(|s| s.slot())
                    .map(|(site_id, parameter_id)| serde_json::json!([site_id, parameter_id]))
                    .collect();
                crate::routes::private::reprocessing_jobs::worker::enqueue(
                    sink.db,
                    "alarm_backfill",
                    None,
                    None,
                    &serde_json::json!({
                        "slots": slots,
                        "start": lo.to_rfc3339(),
                        "end": hi.to_rfc3339(),
                    }),
                    None,
                )
                .await?;
            }
            Episodes::None => {}
        }
    }

    if let (false, Some((lo, hi))) = (plan.recompute_derived.is_empty(), plan.episode_span) {
        let sites: BTreeSet<Uuid> = plan.recompute_derived.iter().map(|(s, _)| *s).collect();
        let parameters: BTreeSet<Uuid> = plan.recompute_derived.iter().map(|(_, p)| *p).collect();
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            sink.db,
            "derived_recompute",
            None,
            None,
            &serde_json::json!({
                "site_ids": sites.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "parameter_ids": parameters.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "start": lo.to_rfc3339(),
                "end": hi.to_rfc3339(),
            }),
            None,
        )
        .await?;
    }

    Ok(plan)
}

#[cfg(test)]
#[path = "tests/tail.rs"]
mod tests;
