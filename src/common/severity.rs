//! One severity for records whose sources each spell failure their own way.
//!
//! Six tables say "this went wrong" in six vocabularies: a job's `status` and `error_message`, a
//! job log's `level`, a delivery's `status` and `error`, a receipt's `rejected_total` and
//! `braked`, a hold's `status`, and a sync event's `status`. A reader that wants "show me today's
//! failures" cannot filter on any one of them. The severity is derived here at the read, per
//! source shape: storing it would be a seventh spelling that can disagree with the six.

/// How a record reads to someone scanning for what went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Something did not happen and nobody was told by the record itself.
    Error,
    /// It happened, with part of it refused or held back.
    Warning,
    /// It happened.
    Info,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }

    /// A tracked job: `reprocessing_jobs.status`. The status alone decides it, because
    /// `error_message` survives a retry that later succeeded and so does not say how the run
    /// ended.
    #[must_use]
    pub fn job(status: &str) -> Self {
        match status {
            "failed" => Self::Error,
            "cancelled" => Self::Warning,
            _ => Self::Info,
        }
    }

    /// One timeline entry: `reprocessing_job_logs.level`.
    #[must_use]
    pub fn job_log(level: &str) -> Self {
        match level {
            "error" => Self::Error,
            "warn" => Self::Warning,
            _ => Self::Info,
        }
    }

    /// One delivery attempt: `notification_log.status` with `error`. `muted` and `skipped` are
    /// the dispatcher declining to send, which is not a failure.
    #[must_use]
    pub fn notification(status: &str, error: Option<&str>) -> Self {
        if status == "failed" || present(error) {
            Self::Error
        } else {
            Self::Info
        }
    }

    /// One windowed ingest pass: `ingest_receipts`. The brake is the pass that did not apply what
    /// it read; rejected rows are a pass that applied the rest.
    #[must_use]
    pub fn ingest_receipt(rejected_total: i64, braked: bool) -> Self {
        if braked {
            Self::Error
        } else if rejected_total > 0 {
            Self::Warning
        } else {
            Self::Info
        }
    }

    /// One review-queue entry: `replicate_audit_holds.status`. A hold nobody has ruled on is the
    /// open question; `deferred` is waiting on a pairing, and the terminal statuses are decided.
    #[must_use]
    pub fn audit_hold(status: &str) -> Self {
        match status {
            "pending" => Self::Error,
            "deferred" => Self::Warning,
            _ => Self::Info,
        }
    }

    /// One alarm episode: `alarm_events.max_severity` (1 = warning, 2 = alarm) and whether it is
    /// still open. A closed episode is what happened, not what is wrong now.
    #[must_use]
    pub fn alarm(max_severity: i16, resolved: bool) -> Self {
        if resolved {
            Self::Info
        } else if max_severity >= 2 {
            Self::Error
        } else {
            Self::Warning
        }
    }

    /// One sync cycle: `sync_events.status`. `partial` is the cycle that carried some of what it
    /// read.
    #[must_use]
    pub fn sync_event(status: &str) -> Self {
        match status {
            "failed" => Self::Error,
            "partial" | "cancelled" => Self::Warning,
            _ => Self::Info,
        }
    }
}

fn present(message: Option<&str>) -> bool {
    message.is_some_and(|m| !m.trim().is_empty())
}

#[cfg(test)]
#[path = "tests/severity.rs"]
mod tests;
