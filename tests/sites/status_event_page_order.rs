//! `GET /sites/{id}/status_events` pages with LIMIT/OFFSET, and one timestamp carries one row per
//! stream, so the sort must be a total order or a tied row can repeat on one page and vanish from
//! the next. These walk the pages in both directions and at the edges of the range.
//!
//! Run: cargo test --test sites -- --test-threads=1

use std::collections::HashSet;

use serde_json::json;
use serial_test::serial;

use crate::common::e2e;

const TIMESTAMPS: usize = 3;
const STREAMS: usize = 4;

struct Fixture {
    app: axum::Router,
    token: String,
    site_id: String,
}

impl Fixture {
    fn range(&self) -> String {
        "start=2025-09-01T00:00:00Z&end=2025-09-02T00:00:00Z".to_string()
    }

    async fn values(&self, extra: &str) -> Vec<String> {
        let uri = format!(
            "/api/sites/{}/status_events?{}&{extra}",
            self.site_id,
            self.range()
        );
        let (status, body) = crate::common::get_json_with_token(&self.app, &uri, &self.token).await;
        assert_eq!(status, 200, "{uri} ({status}): {body}");
        body["events"]
            .as_array()
            .unwrap_or_else(|| panic!("events must be an array: {body}"))
            .iter()
            .map(|e| {
                e["value"]
                    .as_str()
                    .unwrap_or_else(|| panic!("event has no value: {body}"))
                    .to_string()
            })
            .collect()
    }
}

/// Four parameters emitting at the same three timestamps, so every timestamp carries a four-row
/// tie. The batch endpoint mints one stream per (site, parameter).
async fn fixture() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let project_id = e2e::create_project(&app, &token, "Tied Events", "tied-events", false).await;
    let site_id = e2e::create_site(&app, &token, &project_id, "Tied Site", "tied-site").await;

    let mut events = Vec::new();
    for stream in 0..STREAMS {
        let parameter_id = e2e::create_parameter(
            &app,
            &token,
            &format!("TiedP{stream}"),
            &format!("Tied Parameter {stream}"),
            "state",
        )
        .await;
        e2e::assign_site_parameter_minimal(&app, &token, &site_id, &parameter_id).await;
        for hour in 0..TIMESTAMPS {
            events.push(json!({
                "site_id": site_id,
                "parameter_id": parameter_id,
                "time": format!("2025-09-01T{hour:02}:00:00Z"),
                "value": format!("h{hour}-s{stream}"),
            }));
        }
    }

    let (status, ingested) = crate::common::post_json_parse_with_token(
        &app,
        "/api/status_events/batch",
        &json!({ "events": events }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "seed status events ({status}): {ingested}");

    Fixture {
        app,
        token,
        site_id,
    }
}

#[tokio::test]
#[serial]
async fn descending_pages_are_the_ascending_walk_reversed() {
    let f = fixture().await;
    let total = TIMESTAMPS * STREAMS;

    let ascending = f.values("").await;
    assert_eq!(
        ascending.len(),
        total,
        "the unpaged read returns every event"
    );

    let descending = f.values("order=desc").await;
    let mut ascending_reversed = ascending.clone();
    ascending_reversed.reverse();
    assert_eq!(
        descending, ascending_reversed,
        "the descending order is the ascending one reversed, ties included"
    );

    let mut paged = Vec::new();
    for offset in 0..total {
        let page = f
            .values(&format!("order=desc&limit=1&offset={offset}"))
            .await;
        assert_eq!(page.len(), 1, "one row per page at offset {offset}");
        paged.push(page.into_iter().next().unwrap());
    }
    assert_eq!(
        paged, descending,
        "paging descending walks the same total order as the unpaged descending read"
    );
    assert_eq!(
        paged.iter().collect::<HashSet<_>>().len(),
        total,
        "no row is repeated across the descending pages: {paged:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_multi_row_page_covers_a_tie_without_overlap() {
    let f = fixture().await;
    let total = TIMESTAMPS * STREAMS;

    // A page size that does not divide the tie width, so a page boundary lands mid-tie.
    let page_size = 5;
    let mut walked = Vec::new();
    let mut offset = 0;
    while offset < total {
        let page = f
            .values(&format!("limit={page_size}&offset={offset}"))
            .await;
        assert!(!page.is_empty(), "page at offset {offset} is empty");
        walked.extend(page);
        offset += page_size;
    }

    assert_eq!(
        walked.len(),
        total,
        "the pages cover every row exactly once"
    );
    assert_eq!(
        walked.iter().collect::<HashSet<_>>().len(),
        total,
        "and none of them twice: {walked:?}"
    );
    assert_eq!(walked, f.values("").await, "in the unpaged order");
}

#[tokio::test]
#[serial]
async fn an_offset_past_the_end_returns_an_empty_page_with_the_full_total() {
    let f = fixture().await;
    let total = TIMESTAMPS * STREAMS;

    let uri = format!(
        "/api/sites/{}/status_events?{}&limit=5&offset={}",
        f.site_id,
        f.range(),
        total + 10
    );
    let (status, body) = crate::common::get_json_with_token(&f.app, &uri, &f.token).await;
    assert_eq!(status, 200, "({status}): {body}");
    assert!(
        body["events"].as_array().expect("events array").is_empty(),
        "past the end there is nothing to return: {body}"
    );
    assert_eq!(
        body["total"],
        json!(total),
        "total still counts the whole match set: {body}"
    );
}
