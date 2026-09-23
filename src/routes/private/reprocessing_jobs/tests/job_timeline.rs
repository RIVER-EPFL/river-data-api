use super::*;
use sea_orm::QueryTrait;

fn sql(job_id: Uuid, after_seq: i64, limit: u64) -> String {
    timeline(job_id, after_seq, limit)
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string()
}

#[test]
fn test_timeline_filters_one_job_from_after_seq() {
    let job_id = Uuid::nil();
    let out = sql(job_id, 41, 1000);
    assert!(
        out.contains(r#""job_id" = '00000000-0000-0000-0000-000000000000'"#),
        "{out}"
    );
    assert!(out.contains(r#""seq" > 41"#), "{out}");
}

#[test]
fn test_timeline_is_ordered_oldest_first_and_capped() {
    let out = sql(Uuid::nil(), -1, 5000);
    assert!(
        out.contains(r#"ORDER BY "reprocessing_job_logs"."seq" ASC"#),
        "{out}"
    );
    assert!(out.contains("LIMIT 5000"), "{out}");
}

/// The whole timeline is `after_seq` unset, which the handler spells as -1: `seq` starts at 0.
#[test]
fn test_timeline_from_the_start_admits_seq_zero() {
    let out = sql(Uuid::nil(), -1, 1000);
    assert!(out.contains(r#""seq" > -1"#), "{out}");
}

#[test]
fn test_cancel_request_frees_dedupe_key_of_queued_job() {
    let out = cancel_request(Uuid::nil())
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    assert!(
        out.contains(
            r#""dedupe_key" = (CASE WHEN ("reprocessing_jobs"."status" = 'queued') THEN NULL ELSE "dedupe_key" END)"#
        ),
        "{out}"
    );
}
