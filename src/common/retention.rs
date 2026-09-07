//! How long each stored record is kept, in one place, with the reason for each.
//!
//! Five records answer "what happened to this value": the decisions taken on it, the holds raised
//! about it, the receipt of the pass that carried it, the sync event that ran that pass, and the
//! job that reprocessed it. They are pruned on four different clocks by two different jobs, so the
//! shortest horizon among them is how far back that answer reaches. Stating them together is what
//! makes that visible; whether they should share one horizon is Q119.

use crate::config::Config;

/// How long one record is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    Days(u32),
    /// The record is the curation trail itself; nothing prunes it.
    Forever,
    /// The horizon is configured to 0, which switches that prune off.
    Disabled,
}

impl Kept {
    #[must_use]
    pub const fn days(days: u32) -> Self {
        if days == 0 { Self::Disabled } else { Self::Days(days) }
    }

    /// The horizon in days, or None when nothing prunes the record.
    #[must_use]
    pub const fn horizon_days(self) -> Option<u32> {
        match self {
            Self::Days(d) => Some(d),
            Self::Forever | Self::Disabled => None,
        }
    }
}

/// One stored record, how long it is kept, what prunes it, and why the horizon is what it is.
#[derive(Debug, Clone, Copy)]
pub struct Record {
    pub table: &'static str,
    /// Which rows of the table this horizon covers, when one table has more than one.
    pub rows: &'static str,
    pub kept: Kept,
    /// The job name that prunes it, or "nothing".
    pub pruned_by: &'static str,
    pub reason: &'static str,
}

/// Every retention horizon the process runs under, resolved from configuration.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// High-volume tracked jobs (janitor, ingest, refresh, alarm backfill).
    pub job_maintenance: Kept,
    /// Tracked jobs a person or a metadata change caused.
    pub job_operator: Kept,
    /// Ceiling on maintenance job rows between daily prunes; 0 disables it.
    pub job_maintenance_max_rows: u64,
    pub sync_events: Kept,
    pub ingest_receipts: Kept,
}

impl Retention {
    #[must_use]
    pub const fn from_config(config: &Config) -> Self {
        Self {
            job_maintenance: Kept::days(config.job_maintenance_retention_days),
            job_operator: Kept::days(config.janitor_retention_days),
            job_maintenance_max_rows: config.job_maintenance_max_rows,
            sync_events: Kept::days(config.sync_event_retention_days),
            ingest_receipts: Kept::days(config.ingest_receipt_retention_days),
        }
    }

    /// The records above plus the two nothing prunes, in the order a value's history is read.
    #[must_use]
    pub fn records(self) -> Vec<Record> {
        vec![
            Record {
                table: "reading_decisions",
                rows: "every row",
                kept: Kept::Forever,
                pruned_by: "nothing",
                reason: "the append-only curation record: every flag, correction and rollback a \
                         person made is the answer to what happened to a value",
            },
            Record {
                table: "replicate_audit_holds",
                rows: "every row",
                kept: Kept::Forever,
                pruned_by: "nothing",
                reason: "a hold carries the disagreement and the ruling taken on it, which is a \
                         decision rather than a log line",
            },
            Record {
                table: "ingest_receipts",
                rows: "rows no stored reading resolves to",
                kept: self.ingest_receipts,
                pruned_by: "sync_ledger_retention",
                reason: "a receipt says how a stored value arrived, so age alone does not release \
                         one: a receipt whose window still covers a reading is kept whatever its age",
            },
            Record {
                table: "sync_events",
                rows: "rows that are not running",
                kept: self.sync_events,
                pruned_by: "sync_ledger_retention",
                reason: "one row per cycle per service, so the table accretes steadily; the pass's \
                         own accounting survives it in the receipt",
            },
            Record {
                table: "reprocessing_jobs",
                rows: "category operator and metadata",
                kept: self.job_operator,
                pruned_by: "janitor",
                reason: "a job a person or a metadata change caused is what explains a value \
                         moving, so it outlives the maintenance rows by an order of magnitude",
            },
            Record {
                table: "reprocessing_jobs",
                rows: "category maintenance",
                kept: self.job_maintenance,
                pruned_by: "janitor",
                reason: "the janitor, ingest, refresh and alarm-backfill rows are high volume and \
                         individually uninteresting; a count cap holds the table between prunes",
            },
            Record {
                table: "reprocessing_job_logs",
                rows: "every row",
                kept: self.job_maintenance,
                pruned_by: "janitor",
                reason: "the timeline of a run cascade-deletes with the run it explains, so it \
                         cannot outlive it",
            },
        ]
    }

    /// The shortest horizon among the records a value's history is assembled from, which is how
    /// far back that history is complete.
    #[must_use]
    pub fn history_horizon_days(self) -> Option<u32> {
        self.records()
            .iter()
            .filter_map(|r| r.kept.horizon_days())
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Retention {
        Retention {
            job_maintenance: Kept::days(14),
            job_operator: Kept::days(180),
            job_maintenance_max_rows: 50_000,
            sync_events: Kept::days(90),
            ingest_receipts: Kept::days(365),
        }
    }

    #[test]
    fn a_zero_horizon_is_a_disabled_prune_rather_than_an_immediate_one() {
        assert_eq!(Kept::days(0), Kept::Disabled);
        assert_eq!(Kept::days(0).horizon_days(), None);
        assert_eq!(Kept::Forever.horizon_days(), None);
    }

    #[test]
    fn the_curation_record_is_the_one_nothing_prunes() {
        let kept: Vec<_> = defaults()
            .records()
            .into_iter()
            .filter(|r| r.kept == Kept::Forever)
            .map(|r| r.table)
            .collect();
        assert_eq!(kept, vec!["reading_decisions", "replicate_audit_holds"]);
    }

    #[test]
    fn the_history_reaches_back_only_as_far_as_its_shortest_lived_record() {
        assert_eq!(defaults().history_horizon_days(), Some(14));
    }
}
